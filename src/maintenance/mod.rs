//! MUSE-31: the background maintenance pipeline.
//!
//! MUSE-30's docs audit found several write-path routines that existed but
//! had **no scheduled caller** at all: `arr::ingest::run`,
//! `embed::pipeline::embed_stale`, `taste_model::recompute_taste`,
//! `radar::divergence::recompute_divergence`, and
//! `enrichment::EnrichmentService::enrich_media_item`. With nothing ever
//! calling them, a freshly-deployed Muse silently never populates
//! `embeddings`/`taste_profile`/`taste_divergence` — emptying `/recommend`'s
//! taste tier and the `friday_evening`/`zeitgeist` proactive generators,
//! which both read `taste_profile`/`taste_divergence`.
//!
//! [`run_maintenance_pass`] is the fix: one dependency-ordered pass that
//! ties those routines together —
//!
//! 1. **arr ingest** (`arr::ingest::run`) — only when `state.arr_instances`
//!    is non-empty. Populates `media_items`/`media_metadata`/etc., which
//!    every step below reads.
//! 2. **`embed_stale`** — only when `state.embed` is configured. Populates
//!    `embeddings`, which `taste_model`'s `overall_centroid` and MUSE-09
//!    recall both depend on. Bounded by `Config::embed_batch_size` (see
//!    `embed::pipeline::embed_stale`'s own paging docs for why a bounded
//!    batch is safe to call repeatedly).
//! 3. **Per account** (`repo::account::list`): `taste_model::recompute_taste`
//!    (Chord is optional — taste computation works without an LLM, only the
//!    `model_notes` prose degrades to `None`) then
//!    `radar::divergence::recompute_divergence`. Divergence is computed
//!    *after* taste since both read the same freshly-ingested/embedded
//!    corpus, and divergence additionally needs a `population_profile` (see
//!    step 4's sibling worker, or `recompute_divergence`'s own
//!    compute-if-stale fallback).
//! 4. **Bounded enrichment** — only when `state.enrichment.any_source_configured()`.
//!    For each account, up to `Config::maintenance_enrichment_limit` gap-
//!    analysis candidates (`curation::candidates::gather_gap_candidates` —
//!    engaged-with shows with more content beyond the library, the same
//!    "worth digging into" set the recommend engine already surfaces) are
//!    enriched via `EnrichmentService::enrich_media_item`. Deduped across
//!    accounts within one pass so the same title isn't enriched twice just
//!    because two accounts are both watching it.
//! 5. **MUSEL-A2 metadata resolve** — only when at least one
//!    `metadata::MetadataProvider` is configured (`state.tmdb` and/or a
//!    freshly-built `metadata::tvdb::TvdbClient::from_config`). Runs after
//!    arr ingest (which is what actually creates/updates rows with known
//!    provider ids) so there's something to resolve, and before `embed`
//!    for the SAME reason `embed` runs after taste/divergence's data
//!    dependency: a title enriched with a real `overview` embeds into a
//!    richer, more useful vector than a bare title/year string. Bounded by
//!    the same `Config::maintenance_enrichment_limit` knob step 4 uses
//!    (`repo::media_metadata::find_needing_enrichment`) — up to that many
//!    movie rows plus that many show rows per pass, oldest-unsynced-first.
//!
//! Every step is independently error-isolated and never panics: a failure
//! in one step (or one account, or one instance) is logged and the pass
//! moves on to the next step/account — matching the graceful-degrade
//! posture of every other worker in this crate (`prowlarr::worker`,
//! `proactive::scheduler`, `tuner::scheduler`). A deployment with nothing
//! configured at all (`arr_instances` empty, no `embed`/`enrichment`
//! sources, zero accounts) runs a harmless no-op pass.
//!
//! [`spawn_maintenance_worker`] wraps [`run_maintenance_pass`] in a
//! `tokio::time::interval` loop (cadence `Config::maintenance_tick_secs`) —
//! this is the worker `workers::spawn_workers` starts unconditionally
//! (same posture as `tuner::scheduler`/`proactive::scheduler`: harmless
//! no-op tick when nothing is configured yet).
//!
//! [`spawn_trending_worker`] is a second, independent interval worker
//! (cadence `Config::trending_tick_secs`, default daily) that runs
//! `trending::snapshot_trending` + `compute_population_distributions` when
//! `state.tmdb` is configured — the trending/population corpus
//! `taste_divergence` reads. Kept separate from the maintenance pass
//! because it has its own, much coarser cadence (TMDb's trending page
//! doesn't meaningfully change every 30 minutes) and its own upstream
//! (TMDb, not arr/Ollama/SearXNG/news).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use crate::http::AppState;
use crate::taste_model::chord_client::ChordClient;

/// Outcome of one [`run_maintenance_pass`] call. Every counter is a plain
/// "how much happened", not a pass/fail — failures are counted separately
/// (`*_failed`) rather than aborting the pass, per the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceSummary {
    /// `false` when the arr-ingest step was skipped this pass (no
    /// instances configured) — every `arr_*` counter below stays `0` in
    /// that case rather than a vacuous ingest having run.
    pub arr_ran: bool,
    pub arr_instances_ok: usize,
    pub arr_instances_skipped: usize,
    pub arr_movies_upserted: usize,
    pub arr_series_upserted: usize,

    /// MUSEM-06: the monitored "wanted" acquisition worker's own tally for
    /// this pass — see `crate::acquisition::worker::run_wanted_pass`. A
    /// `WantedPassSummary::default()` (every counter zero) means the
    /// worker's own master-acquisition-gate check short-circuited it to a
    /// no-op, same "ran but did nothing" posture `embed_ran`/`arr_ran`
    /// establish for their own steps.
    pub wanted: crate::acquisition::worker::WantedPassSummary,

    /// MUSEL-A2: `false` when no `metadata::MetadataProvider` was
    /// configured this pass (neither TMDb nor TVDB) — the resolve step is
    /// skipped entirely rather than running with zero providers (which
    /// `metadata::resolve::resolve_and_merge` would itself just no-op on
    /// anyway; skipping here also avoids the `find_needing_enrichment`
    /// queries for no reason).
    pub metadata_resolve_ran: bool,
    pub metadata_resolved: usize,
    pub metadata_resolve_failed: usize,

    pub embed_ran: bool,
    pub embedded: usize,
    pub embed_skipped_unchanged: usize,
    pub embed_failed: usize,

    pub accounts_considered: usize,
    pub taste_recomputed: usize,
    pub taste_failed: usize,
    pub divergence_recomputed: usize,
    pub divergence_failed: usize,

    pub enrichment_ran: bool,
    pub enrichment_attempted: usize,
    pub enrichment_failed: usize,
}

/// Run one full maintenance pass in dependency order. Never returns `Err`
/// and never panics — see the module doc comment for the per-step
/// error-isolation posture. Safe to call repeatedly on a schedule (each
/// step is itself idempotent/incremental) or on demand (the `POST
/// /ops/maintenance` route calls this directly to prime a fresh deploy).
pub async fn run_maintenance_pass(state: &AppState) -> MaintenanceSummary {
    let mut summary = MaintenanceSummary::default();

    // --- (a) arr ingest -----------------------------------------------
    if !state.arr_instances.is_empty() {
        summary.arr_ran = true;
        let ingest_summary = crate::arr::ingest::run(&state.pool, &state.arr_instances).await;
        summary.arr_instances_ok = ingest_summary.instances_ok.len();
        summary.arr_instances_skipped = ingest_summary.instances_skipped.len();
        summary.arr_movies_upserted = ingest_summary.movies_upserted;
        summary.arr_series_upserted = ingest_summary.series_upserted;
        for (instance, error) in &ingest_summary.instances_skipped {
            tracing::warn!(instance = %instance, error = %error, "MUSE-31: maintenance pass — arr instance skipped");
        }
    } else {
        tracing::debug!("MUSE-31: maintenance pass — no arr instances configured; skipping ingest step");
    }

    // --- (a2) MUSEM-06: monitored "wanted" acquisition worker --------------
    //
    // Scheduled right after arr ingest (dependency order: ingest is what
    // populates/refreshes `media_items`/`media_files`, which `list_wanted`'s
    // cutoff comparison reads) and before embed/taste/divergence/enrichment,
    // which don't depend on it and it doesn't depend on them. Gated on the
    // master acquisition setting internally (`run_wanted_pass` checks it
    // itself, and `fulfill_request` enforces it again unbypassably) — a
    // deployment with acquisition off runs a harmless, cheap no-op here,
    // same posture as every other optional step in this pass.
    let wanted_deps = crate::acquisition::worker::WantedPassDeps {
        pool: &state.pool,
        config: &state.config,
        prowlarr: state.prowlarr.as_ref(),
        download: state.download.as_ref().map(|c| c as &dyn crate::download::DownloadClient),
    };
    summary.wanted = crate::acquisition::worker::run_wanted_pass(&wanted_deps).await;

    // --- (a3) MUSEL-A2: metadata resolve + enrichment upsert --------------
    //
    // `state.tmdb` is already built at startup (MUSE-19); TVDB has no
    // equivalent `AppState` field yet, so it's built fresh here from
    // `state.config` each pass — cheap (just a `reqwest::Client` +
    // an empty token cache) and the same "construct fresh per pass" posture
    // step (c) already uses for `ChordClient::from_config` below. Both are
    // `None` (skip this whole step) when unconfigured — graceful degrade,
    // same as every other step in this pass.
    let tvdb = crate::metadata::tvdb::TvdbClient::from_config(&state.config);
    let mut named_providers: Vec<crate::metadata::resolve::NamedProvider<'_>> = Vec::new();
    if let Some(tmdb) = state.tmdb.as_ref() {
        named_providers.push(crate::metadata::resolve::NamedProvider::new(
            crate::metadata::resolve::TMDB,
            tmdb,
        ));
    }
    if let Some(tvdb) = tvdb.as_ref() {
        named_providers.push(crate::metadata::resolve::NamedProvider::new(
            crate::metadata::resolve::TVDB,
            tvdb,
        ));
    }

    if !named_providers.is_empty() {
        summary.metadata_resolve_ran = true;
        let (resolved, failed) =
            run_metadata_resolve_pass(&state.pool, &named_providers, state.config.maintenance_enrichment_limit).await;
        summary.metadata_resolved = resolved;
        summary.metadata_resolve_failed = failed;
    } else {
        tracing::debug!(
            "MUSE-31/MUSEL-A2: maintenance pass — no metadata providers configured; skipping resolve step"
        );
    }

    // --- (b) embed_stale -------------------------------------------------
    if let Some(embed_client) = state.embed.as_ref() {
        summary.embed_ran = true;
        match crate::embed::embed_stale(&state.pool, embed_client, state.config.embed_batch_size).await {
            Ok(outcome) => {
                summary.embedded = outcome.embedded;
                summary.embed_skipped_unchanged = outcome.skipped_unchanged;
                summary.embed_failed = outcome.failed;
            }
            Err(e) => {
                tracing::warn!(error = %e, "MUSE-31: maintenance pass — embed_stale failed this pass; will retry next tick");
            }
        }
    } else {
        tracing::debug!("MUSE-31: maintenance pass — no embed client configured; skipping embed step");
    }

    // --- (c) per-account taste + divergence recompute ---------------------
    let chord = ChordClient::from_config(&state.config);

    let accounts = match crate::repo::account::list(&state.pool).await {
        Ok(accounts) => accounts,
        Err(e) => {
            tracing::warn!(error = %e, "MUSE-31: maintenance pass — could not list accounts; skipping taste/divergence/enrichment steps");
            return summary;
        }
    };
    summary.accounts_considered = accounts.len();

    let mut enrichment_targets: HashSet<(i64, String)> = HashSet::new();

    for account in &accounts {
        match crate::taste_model::recompute_taste(&state.pool, chord.as_ref(), account.id).await {
            Ok(_) => summary.taste_recomputed += 1,
            Err(e) => {
                summary.taste_failed += 1;
                tracing::warn!(error = %e, account_id = account.id, "MUSE-31: maintenance pass — recompute_taste failed for this account; continuing");
            }
        }

        match crate::radar::recompute_divergence(&state.pool, account.id).await {
            Ok(_) => summary.divergence_recomputed += 1,
            Err(e) => {
                summary.divergence_failed += 1;
                tracing::warn!(error = %e, account_id = account.id, "MUSE-31: maintenance pass — recompute_divergence failed for this account; continuing");
            }
        }

        // --- (d) bounded enrichment: gather this account's gap-analysis
        // candidates now (cheap, DB-only) so the actual enrichment calls
        // below can be deduped across every account in one pass.
        if state.enrichment.any_source_configured() {
            match crate::curation::candidates::gather_gap_candidates(
                &state.pool,
                account.id,
                state.config.maintenance_enrichment_limit,
            )
            .await
            {
                Ok(candidates) => {
                    for candidate in candidates {
                        if let Some(media_item_id) = candidate.media_item_id {
                            enrichment_targets.insert((media_item_id, candidate.title.clone()));
                        }
                    }
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    account_id = account.id,
                    "MUSE-31: maintenance pass — gap-candidate lookup failed for enrichment sourcing; continuing"
                ),
            }
        }
    }

    // --- (d) bounded enrichment pass, deduped across accounts -------------
    //
    // Already bounded by construction: each account contributed at most
    // `Config::maintenance_enrichment_limit` candidates above, and this set
    // dedups them across accounts (never grows the bound, only shrinks it
    // via overlap) -- no further truncation needed here.
    if state.enrichment.any_source_configured() {
        summary.enrichment_ran = true;
        for (media_item_id, title) in enrichment_targets {
            match state.enrichment.enrich_media_item(&state.pool, media_item_id, &title).await {
                Ok(_) => summary.enrichment_attempted += 1,
                Err(e) => {
                    summary.enrichment_attempted += 1;
                    summary.enrichment_failed += 1;
                    tracing::warn!(error = %e, media_item_id, title = %title, "MUSE-31: maintenance pass — enrichment failed for this title; continuing");
                }
            }
        }
    }

    tracing::info!(
        arr_ran = summary.arr_ran,
        arr_movies_upserted = summary.arr_movies_upserted,
        arr_series_upserted = summary.arr_series_upserted,
        wanted_grabbed = summary.wanted.grabbed,
        wanted_needs_review = summary.wanted.needs_review,
        metadata_resolve_ran = summary.metadata_resolve_ran,
        metadata_resolved = summary.metadata_resolved,
        embedded = summary.embedded,
        embed_skipped_unchanged = summary.embed_skipped_unchanged,
        accounts_considered = summary.accounts_considered,
        taste_recomputed = summary.taste_recomputed,
        divergence_recomputed = summary.divergence_recomputed,
        enrichment_attempted = summary.enrichment_attempted,
        "MUSE-31: maintenance pass complete"
    );

    summary
}

/// MUSEL-A2: resolve + persist enrichment for up to `limit` candidate rows
/// per `MediaKind` (movies and shows considered separately, each bounded
/// independently — worst case `2 * limit` rows touched this pass).
/// Returns `(resolved, failed)`. Never panics; a single row's resolve or
/// persist failure is logged and the pass continues (same error-isolation
/// posture as every other step in [`run_maintenance_pass`]).
async fn run_metadata_resolve_pass(
    pool: &sqlx::PgPool,
    providers: &[crate::metadata::resolve::NamedProvider<'_>],
    limit: i64,
) -> (usize, usize) {
    let mut resolved = 0usize;
    let mut failed = 0usize;

    for repo_kind in [
        crate::models::media_metadata::MediaKind::Movie,
        crate::models::media_metadata::MediaKind::Show,
    ] {
        let candidates = match crate::repo::media_metadata::find_needing_enrichment(pool, repo_kind, limit).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, kind = ?repo_kind, "MUSEL-A2: maintenance pass — could not list enrichment candidates; skipping this kind");
                continue;
            }
        };

        let resolve_kind = match repo_kind {
            crate::models::media_metadata::MediaKind::Movie => crate::metadata::MediaKind::Movie,
            crate::models::media_metadata::MediaKind::Show => crate::metadata::MediaKind::Series,
        };

        for row in candidates {
            let mut ids = crate::metadata::resolve::ResolveIds::new().with_title(row.title.clone());
            if let Some(tmdb_id) = &row.tmdb_id {
                ids = ids.with_id(crate::metadata::resolve::TMDB, tmdb_id.clone());
            }
            if let Some(tvdb_id) = &row.tvdb_id {
                ids = ids.with_id(crate::metadata::resolve::TVDB, tvdb_id.clone());
            }
            if let Some(imdb_id) = &row.imdb_id {
                ids = ids.with_id(crate::metadata::resolve::IMDB, imdb_id.clone());
            }

            let resolved_match = match crate::metadata::resolve::resolve_and_merge(&ids, resolve_kind, providers).await
            {
                Ok(Some(resolved_match)) => resolved_match,
                Ok(None) => {
                    tracing::debug!(media_metadata_id = row.id, "MUSEL-A2: maintenance pass — nothing resolved this pass; continuing");
                    continue;
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(error = %e, media_metadata_id = row.id, "MUSEL-A2: maintenance pass — resolve_and_merge failed; continuing");
                    continue;
                }
            };

            // Review finding 1 (S119b codex REQUEST_CHANGES): an automated
            // background pass must never persist a `TitleSearch` (lowest-
            // confidence, free-text) match as if it were authoritative.
            // Only `MatchConfidence::Id` (resolved via a provider's own id
            // lookup) is written to `media_metadata` here. A
            // `TitleSearch` hit is discoverable in logs (see
            // `resolve_and_merge`'s own `tracing::warn!` when it takes
            // that path) for a future manual-review surface, but this
            // unattended pass skips persisting it rather than risking a
            // wrong-confident enrichment landing on the row unattended.
            if resolved_match.confidence != crate::metadata::resolve::MatchConfidence::Id {
                tracing::info!(
                    media_metadata_id = row.id,
                    "MUSEL-A2: maintenance pass — resolved via low-confidence title search; \
                     skipping persistence (never auto-persisted as authoritative)"
                );
                continue;
            }

            match crate::repo::media_metadata::apply_enrichment(pool, row.id, &resolved_match.metadata).await {
                Ok(_) => resolved += 1,
                Err(e) => {
                    failed += 1;
                    tracing::warn!(error = %e, media_metadata_id = row.id, "MUSEL-A2: maintenance pass — apply_enrichment failed; continuing");
                }
            }
        }
    }

    (resolved, failed)
}

/// Spawn the maintenance worker's background loop. Always spawned (same
/// posture as `tuner::scheduler`/`proactive::scheduler`): a deployment with
/// nothing configured yet just ticks a harmless no-op pass.
pub fn spawn_maintenance_worker(state: Arc<AppState>) {
    let tick = StdDuration::from_secs(state.config.maintenance_tick_secs.max(1));
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        // Skip the immediate first tick so a hot-reload/restart doesn't
        // hammer every account's recompute before the process has even
        // finished booting other workers — same posture as
        // `proactive::scheduler::spawn`.
        interval.tick().await;
        loop {
            interval.tick().await;
            run_maintenance_pass(&state).await;
        }
    });
}

/// Spawn the daily trending/population worker's background loop. Always
/// spawned; the tick itself is a no-op when `state.tmdb` isn't configured
/// (see [`run_trending_pass`]).
pub fn spawn_trending_worker(state: Arc<AppState>) {
    let tick = StdDuration::from_secs(state.config.trending_tick_secs.max(1));
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.tick().await;
        loop {
            interval.tick().await;
            run_trending_pass(&state).await;
        }
    });
}

/// One trending/population pass: `snapshot_trending` (writes
/// `trending_snapshots` + `streaming_availability` + a `population_profile`
/// rollup) followed by a fresh `compute_population_distributions` — only
/// when `state.tmdb` is configured. Never fails the caller; every error is
/// logged and swallowed, matching every other worker's posture.
pub async fn run_trending_pass(state: &AppState) {
    let Some(tmdb) = state.tmdb.as_ref() else {
        tracing::debug!("MUSE-31: trending worker — no tmdb client configured; skipping");
        return;
    };

    match crate::trending::snapshot_trending(&state.pool, tmdb, crate::trending::DEFAULT_REGION).await {
        Ok(summary) => {
            tracing::info!(
                snapshots_written = summary.snapshots_written,
                providers_written = summary.providers_written,
                degraded = summary.degraded,
                "MUSE-31: trending snapshot pass complete"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "MUSE-31: trending worker — snapshot_trending failed this tick; will retry next tick");
            return;
        }
    }

    if let Err(e) =
        crate::radar::compute_population_distributions(&state.pool, crate::trending::DEFAULT_REGION).await
    {
        tracing::warn!(error = %e, "MUSE-31: trending worker — compute_population_distributions failed this tick");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure guard-logic checks: a `MaintenanceSummary::default()` reports
    /// every step as not-run, matching the "nothing configured -> harmless
    /// no-op pass" posture without needing a live DB/AppState at all.
    #[test]
    fn default_summary_reports_every_step_as_not_run() {
        let summary = MaintenanceSummary::default();
        assert!(!summary.arr_ran);
        assert!(!summary.embed_ran);
        assert!(!summary.enrichment_ran);
        assert_eq!(summary.accounts_considered, 0);
        assert_eq!(summary.taste_recomputed, 0);
        assert_eq!(summary.divergence_recomputed, 0);
    }

    // --- live-DB test: proves the pipeline actually populates -------------
    //
    // Gated on MUSE_TEST_DATABASE_URL: skips cleanly (does NOT fail) when
    // unset, matching every other live-DB test in this crate. Seeds one
    // account + one library item + a hand-inserted embedding (checkerboard
    // vector, unique to this test's own suffix — no live Ollama/Chord/arr/
    // enrichment needed), runs `run_maintenance_pass` with `embed`/`arr`/
    // enrichment sources left unconfigured (so only steps (c) taste +
    // divergence actually do anything this pass), and asserts
    // `taste_profile`/`taste_divergence` rows now exist for the account —
    // proving the pipeline populates them end-to-end when scheduled.
    #[tokio::test]
    async fn maintenance_pass_populates_taste_profile_and_divergence_for_seeded_account() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 maintenance_pass_populates_taste_profile_and_divergence_for_seeded_account \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        use crate::models::account::NewAccount;
        use crate::models::embedding::{EmbeddingEntityKind, NewEmbedding};
        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::watch_stats::NewWatchStats;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();

        let account = crate::repo::account::create(
            &pool,
            &NewAccount {
                plex_account_id: Some(format!("muse31-test-account-{suffix}")),
                username: Some(format!("muse31_test_{suffix}")),
                friendly_name: Some("MUSE-31 Test Account".to_string()),
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account");

        let library = crate::repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("muse31-test-library-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/muse31-test".to_string(),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let genre_id: i64 = sqlx::query_scalar::<_, i64>("INSERT INTO genres (name) VALUES ($1) RETURNING id")
            .bind(format!("muse31-genre-{suffix}"))
            .fetch_one(&pool)
            .await
            .expect("insert genre");

        let metadata = crate::repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("muse31-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("MUSE-31 Test Movie {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(100),
                year: Some(2021),
                images: serde_json::json!({}),
            },
        )
        .await
        .expect("upsert media_metadata");

        sqlx::query("INSERT INTO media_metadata_genres (media_metadata_id, genre_id) VALUES ($1, $2)")
            .bind(metadata.id)
            .bind(genre_id)
            .execute(&pool)
            .await
            .expect("tag item with genre");

        let item = crate::repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/media/muse31-test/movie-{suffix}.mkv"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("muse31-test-rk-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        crate::repo::watch_stats::upsert_watch_stats(
            &pool,
            &NewWatchStats {
                account_id: account.id,
                media_item_id: item.id,
                play_count: 3,
                finished_count: 3,
                rewatch_count: 2,
                total_watched_ms: 3 * 100 * 60 * 1000,
                avg_percent: Some(0.95),
                last_watched_at: Some(chrono::Utc::now()),
                abandoned: false,
                first_watched_at: Some(chrono::Utc::now() - chrono::Duration::days(30)),
            },
        )
        .await
        .expect("upsert watch_stats");

        // Hand-inserted checkerboard embedding (unique to this item, no live
        // Ollama needed) -- state.embed is left None below, so embed_stale
        // never runs this pass; this only proves the DOWNSTREAM consumers
        // (overall_centroid aggregation) don't choke on a real row existing.
        let mut vector = vec![0.0_f32; 1024];
        for (i, v) in vector.iter_mut().enumerate() {
            *v = if i % 2 == 0 { 1.0 } else { -1.0 };
        }
        crate::repo::embedding::upsert(
            &pool,
            &NewEmbedding::qwen3(
                EmbeddingEntityKind::MediaItem,
                item.id,
                vector,
                Some(format!("MUSE-31 Test Movie {suffix} (2021)\nType: movie")),
            ),
        )
        .await
        .expect("insert checkerboard embedding");

        // Before the pass: no taste_profile / taste_divergence row yet.
        assert!(
            crate::repo::taste::get_profile(&pool, account.id)
                .await
                .expect("get_profile query")
                .is_none(),
            "precondition: no taste_profile should exist before the pass"
        );
        assert!(
            crate::radar::latest_divergence(&pool, account.id)
                .await
                .expect("latest_divergence query")
                .is_none(),
            "precondition: no taste_divergence should exist before the pass"
        );

        let state = AppState {
            pool: pool.clone(),
            config: crate::config::Config::default(),
            plex: None,
            prowlarr: None,
            arr_instances: Vec::new(),
            enrichment: crate::enrichment::EnrichmentService::from_config(&crate::config::Config::default()),
            tmdb: None,
            embed: None,
            download: None,
        };

        let summary = run_maintenance_pass(&state).await;

        assert!(!summary.arr_ran, "no arr instances configured -> step should be skipped");
        assert!(!summary.embed_ran, "no embed client configured -> step should be skipped");
        assert!(!summary.enrichment_ran, "no enrichment source configured -> step should be skipped");
        assert!(
            summary.accounts_considered >= 1,
            "the seeded account must have been considered"
        );

        // Scoped to this test's own account_id -- never asserts over a
        // global/unscoped count, per the shared muse_test DB's accumulation
        // rule.
        let profile = crate::repo::taste::get_profile(&pool, account.id)
            .await
            .expect("get_profile query")
            .expect("taste_profile row should now exist for the seeded account");
        assert_eq!(profile.account_id, account.id);

        let divergence = crate::radar::latest_divergence(&pool, account.id)
            .await
            .expect("latest_divergence query")
            .expect("taste_divergence row should now exist for the seeded account");
        assert_eq!(divergence.account_id, account.id);
    }
}
