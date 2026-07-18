//! Report-pull worker (MUSE-17): the scheduled background task that ties
//! together everything MUSE-16 shipped -- per-indexer polite RSS-mode
//! pulls (`ProwlarrClient::rss_pull`), the deterministic release-name parser
//! (`parse::parse_release_name`), release upsert (`repo::release::upsert`),
//! best-effort title resolution (`repo::media_metadata::find_by_title_year`),
//! and the per-title availability rollup (`repo::availability::recompute`).
//! Also prunes expired `releases` rows (spec S3.6) at the end of each tick.
//!
//! Layout:
//! - [`run_tick`] is one full pass over all enabled indexers -- DB + network
//!   dependent, exercised end-to-end by the `#[tokio::test]` at the bottom
//!   (gated on `MUSE_TEST_DATABASE_URL`, per the crate's live-DB test
//!   convention -- see `src/integration_tests.rs`).
//! - [`spawn_report_pull_worker`] wraps `run_tick` in a `tokio::time::interval`
//!   loop and is what `workers.rs` spawns at startup when Prowlarr is
//!   configured.
//! - `pull_if_due` is the network-touching-but-DB-free slice used by the
//!   scheduling/etiquette unit tests below (httpmock only, no Postgres).
//!
//! A Prowlarr outage or a single bad release never aborts the tick: each
//! indexer's pull, each release's upsert, and each title's rollup are
//! independently best-effort -- a failure is logged and the tick moves on,
//! so one flaky indexer or one malformed row can't starve the rest (spec
//! S4b: "if Prowlarr outage ... just logs and retries next tick").

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::error::MuseError;
use crate::http::AppState;
use crate::models::indexer::Indexer;
use crate::models::media_metadata::MediaKind;
use crate::models::release::NewRelease;
use crate::repo;

use super::client::ProwlarrClient;
use super::models::ProwlarrRelease;
use super::parse::{parse_release_name, ParsedRelease};
use super::scheduler;

/// Outcome counters for one `run_tick` pass -- returned so the caller (the
/// spawn loop, or a test) can log/assert without re-deriving them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickSummary {
    pub indexers_polled: usize,
    pub indexers_skipped: usize,
    pub releases_seen: usize,
    pub releases_resolved: usize,
    pub availability_recomputed: usize,
    pub releases_pruned: u64,
}

/// One full report-pull pass: for every enabled indexer that's both
/// etiquette-eligible (has a relevant category to pull) and due
/// (`scheduler::is_due`), RSS-pull, parse, upsert, and track which titles
/// need an availability rollup; then prune expired releases.
///
/// Returns `Ok(TickSummary::default())` with no work done when Prowlarr
/// isn't configured (`state.prowlarr.is_none()`) -- same graceful-degrade
/// posture as the rest of the Prowlarr integration (MUSE-16).
pub async fn run_tick(state: &AppState) -> crate::error::MuseResult<TickSummary> {
    let Some(prowlarr) = state.prowlarr.as_ref() else {
        return Ok(TickSummary::default());
    };

    let indexers = repo::indexer::list_enabled(&state.pool).await?;
    let now = Utc::now();

    let mut summary = TickSummary::default();
    let mut titles_to_recompute: HashSet<i64> = HashSet::new();

    for indexer in &indexers {
        let categories = relevant_categories(&indexer.categories, &state.config);

        let releases = match pull_if_due(prowlarr, indexer, &categories, now).await {
            None => {
                summary.indexers_skipped += 1;
                continue;
            }
            Some(Err(MuseError::Conflict(msg))) => {
                tracing::debug!(indexer = %indexer.name, reason = %msg, "report-pull skipped this tick");
                summary.indexers_skipped += 1;
                continue;
            }
            Some(Err(e)) => {
                tracing::warn!(indexer = %indexer.name, error = %e, "prowlarr report-pull failed; will retry next tick");
                summary.indexers_skipped += 1;
                continue;
            }
            Some(Ok(releases)) => releases,
        };

        summary.indexers_polled += 1;
        summary.releases_seen += releases.len();

        for release in &releases {
            let parsed = parse_release_name(&release.title);
            let kind = infer_media_kind(release, &parsed, &state.config);

            let media_metadata_id = resolve_title(state, kind, &parsed).await;
            if let Some(id) = media_metadata_id {
                summary.releases_resolved += 1;
                titles_to_recompute.insert(id);
            }

            let new_release = build_new_release(indexer, release, &parsed, media_metadata_id, &state.config, now);
            if let Err(e) = repo::release::upsert(&state.pool, &new_release).await {
                tracing::warn!(error = %e, guid = %release.guid, indexer = %indexer.name, "failed to upsert release; skipping this one");
            }
        }

        if let Err(e) = repo::indexer::mark_rss_pulled(&state.pool, indexer.id).await {
            tracing::warn!(error = %e, indexer = %indexer.name, "failed to record report-pull timestamp");
        }
    }

    for media_metadata_id in &titles_to_recompute {
        match repo::availability::recompute(&state.pool, *media_metadata_id).await {
            Ok(_) => summary.availability_recomputed += 1,
            Err(e) => {
                tracing::warn!(error = %e, media_metadata_id, "availability rollup failed for this title")
            }
        }
    }

    match repo::release::prune_expired(&state.pool).await {
        Ok(pruned) => {
            summary.releases_pruned = pruned;
            if pruned > 0 {
                tracing::info!(pruned, "pruned expired releases");
            }
        }
        Err(e) => tracing::warn!(error = %e, "expired-release pruning failed"),
    }

    Ok(summary)
}

/// Spawn the report-pull worker's background loop. A no-op (never spawns a
/// task) if Prowlarr isn't configured -- callers should check
/// `state.prowlarr.is_some()` themselves if they want to log that
/// distinction (see `workers.rs`), but calling this unconditionally is also
/// safe: `run_tick` degrades cleanly every tick instead.
pub fn spawn_report_pull_worker(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    let tick_interval = StdDuration::from_secs(state.config.prowlarr_tick_interval_secs.max(1));

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick_interval);
        loop {
            interval.tick().await;
            match run_tick(&state).await {
                Ok(summary) => {
                    if summary.indexers_polled > 0 || summary.releases_pruned > 0 {
                        tracing::info!(
                            indexers_polled = summary.indexers_polled,
                            indexers_skipped = summary.indexers_skipped,
                            releases_seen = summary.releases_seen,
                            releases_resolved = summary.releases_resolved,
                            availability_recomputed = summary.availability_recomputed,
                            releases_pruned = summary.releases_pruned,
                            "report-pull tick complete"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "report-pull tick failed entirely; will retry next tick");
                }
            }
        }
    })
}

/// The network-touching, DB-free slice of one indexer's tick: decides
/// whether this indexer is due (categories known + `scheduler::is_due`) and,
/// if so, performs the RSS pull. Returns `None` when there's nothing to do
/// (no relevant category, or not yet due) so the caller can distinguish
/// "skipped by our own schedule" from "attempted and failed/rate-limited".
async fn pull_if_due(
    prowlarr: &ProwlarrClient,
    indexer: &Indexer,
    categories: &[i32],
    now: DateTime<Utc>,
) -> Option<crate::error::MuseResult<Vec<ProwlarrRelease>>> {
    if categories.is_empty() {
        return None;
    }
    if !scheduler::is_due(indexer.last_rss_pull_at, indexer.polite_min_interval_secs, now) {
        return None;
    }

    let min_interval = StdDuration::from_secs(indexer.polite_min_interval_secs.max(0) as u64);
    Some(prowlarr.rss_pull(indexer.prowlarr_id, categories, min_interval).await)
}

/// The Newznab category ids worth pulling for this indexer: the movie+tv
/// categories Muse cares about (`Config::prowlarr_movie_categories` /
/// `_tv_categories`), narrowed to what the indexer's synced capabilities
/// (`indexer.categories`, MUSE-16 §4b-A) say it actually supports.
///
/// If `indexer.categories` is empty (capabilities never synced, or the
/// indexer genuinely advertises none), this falls back to requesting the
/// full configured set rather than silently never polling that indexer:
/// asking Prowlarr for a category an indexer doesn't support is a normal
/// empty-result no-op, not an etiquette violation -- it's not an extra
/// network call to a tracker, just an unmatched filter on the one call we
/// were making anyway.
fn relevant_categories(indexer_categories: &[i32], config: &crate::config::Config) -> Vec<i32> {
    let requested: Vec<i32> = config
        .prowlarr_movie_categories
        .iter()
        .chain(config.prowlarr_tv_categories.iter())
        .copied()
        .collect();

    if indexer_categories.is_empty() {
        return requested;
    }

    requested
        .into_iter()
        .filter(|c| indexer_categories.contains(c))
        .collect()
}

/// Best-effort media kind for one release: prefer the indexer-reported
/// Newznab category (authoritative when present), falling back to whether
/// the deterministic parser found a season marker (a season/episode-shaped
/// release is a TV release even if the tracker's own categorization is
/// missing/ambiguous).
fn infer_media_kind(release: &ProwlarrRelease, parsed: &ParsedRelease, config: &crate::config::Config) -> MediaKind {
    let category_ids = release.category_ids();

    if category_ids.iter().any(|c| config.prowlarr_tv_categories.contains(c)) {
        return MediaKind::Show;
    }
    if category_ids.iter().any(|c| config.prowlarr_movie_categories.contains(c)) {
        return MediaKind::Movie;
    }

    if parsed.season.is_some() {
        MediaKind::Show
    } else {
        MediaKind::Movie
    }
}

/// Best-effort resolve a parsed release to an existing `media_metadata`
/// title (spec: "resolve a parsed release to a media_metadata title ... by
/// parsed title+year -> existing media_metadata; leave NULL if no confident
/// match"). Requires BOTH a parsed title and a parse confidence at or above
/// `Config::prowlarr_resolve_min_confidence` -- a poorly-parsed release name
/// (garbled title, no attributes recognized) is left unresolved rather than
/// risking a wrong match on partial/garbage text.
///
/// Deliberately title-level only, not episode-level: MUSE-17's scope (per
/// the founding item) is the media_metadata resolution step; per-episode
/// resolution of `releases.episode_id` is left for a follow-up (noted in
/// the build report) rather than guessed at here.
async fn resolve_title(state: &AppState, kind: MediaKind, parsed: &ParsedRelease) -> Option<i64> {
    if parsed.confidence < state.config.prowlarr_resolve_min_confidence {
        return None;
    }
    let title = parsed.title.as_deref()?;

    match repo::media_metadata::find_by_title_year(&state.pool, kind, title, parsed.year).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, title, "media_metadata resolution lookup failed; leaving release unresolved");
            None
        }
    }
}

/// Assemble the full upsert payload for one release from the raw Prowlarr
/// report + the deterministic parse, including a rolling `expires_at`
/// (`Config::release_expiry_days` from `now`) -- refreshed on every re-seen
/// upsert, which is what makes the pruning in `run_tick` a true rolling
/// snapshot rather than a one-shot TTL from first sight.
fn build_new_release(
    indexer: &Indexer,
    release: &ProwlarrRelease,
    parsed: &ParsedRelease,
    media_metadata_id: Option<i64>,
    config: &crate::config::Config,
    now: DateTime<Utc>,
) -> NewRelease {
    NewRelease {
        media_metadata_id,
        episode_id: None,
        indexer_id: indexer.id,
        guid: release.guid.clone(),
        title: release.title.clone(),
        info_url: release.info_url.clone(),
        download_url: release.download_url.clone(),
        info_hash: release.info_hash.clone(),
        size_bytes: release.size,
        publish_date: release.publish_date,
        seeders: release.seeders,
        leechers: release.leechers,
        grabs: release.grabs,
        freeleech: release.is_freeleech() || parsed.freeleech,
        freeleech_pct: None,
        categories: release.category_ids(),
        parsed_title: parsed.title.clone(),
        parsed_year: parsed.year,
        quality: parsed.quality.clone(),
        resolution: parsed.resolution.clone(),
        source: parsed.source.clone(),
        video_codec: parsed.video_codec.clone(),
        audio_codec: parsed.audio_codec.clone(),
        audio_channels: None,
        hdr: parsed.hdr.clone(),
        edition: parsed.edition.clone(),
        release_group: parsed.release_group.clone(),
        proper_repack: parsed.proper_repack,
        languages: Vec::new(),
        subtitles: Vec::new(),
        parse_confidence: Some(parsed.confidence),
        expires_at: Some(now + ChronoDuration::days(config.release_expiry_days.max(0))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::models::ProwlarrCategory;
    use chrono::TimeZone;
    use httpmock::prelude::*;

    fn test_indexer(id: i64, prowlarr_id: i32, categories: Vec<i32>, last_rss_pull_at: Option<DateTime<Utc>>) -> Indexer {
        let now = Utc.timestamp_opt(0, 0).unwrap();
        Indexer {
            id,
            prowlarr_id,
            name: format!("indexer-{prowlarr_id}"),
            protocol: Some("torrent".to_string()),
            privacy: Some("public".to_string()),
            enabled: true,
            categories,
            last_rss_pull_at,
            polite_min_interval_secs: 900,
            created_at: now,
            updated_at: now,
        }
    }

    fn test_config() -> crate::config::Config {
        crate::config::Config {
            prowlarr_movie_categories: vec![2000],
            prowlarr_tv_categories: vec![5000],
            ..Default::default()
        }
    }

    #[test]
    fn relevant_categories_narrows_to_indexer_supported_set() {
        let config = test_config();
        let cats = relevant_categories(&[2000, 3000], &config);
        assert_eq!(cats, vec![2000]);
    }

    #[test]
    fn relevant_categories_falls_back_to_requested_when_indexer_categories_unknown() {
        let config = test_config();
        let cats = relevant_categories(&[], &config);
        assert_eq!(cats, vec![2000, 5000]);
    }

    #[test]
    fn relevant_categories_empty_when_no_overlap() {
        let config = test_config();
        let cats = relevant_categories(&[9999], &config);
        assert!(cats.is_empty());
    }

    #[test]
    fn infer_media_kind_prefers_category_over_parse() {
        let config = test_config();
        let release = ProwlarrRelease {
            guid: "g".into(),
            title: "Some.Movie.2020.1080p".into(),
            indexer_id: 1,
            indexer: None,
            protocol: None,
            size: None,
            publish_date: None,
            info_url: None,
            download_url: None,
            info_hash: None,
            seeders: None,
            leechers: None,
            grabs: None,
            categories: vec![ProwlarrCategory {
                id: 5000,
                name: None,
            }],
            indexer_flags: vec![],
            imdb_id_raw: None,
            tmdb_id_raw: None,
            tvdb_id_raw: None,
        };
        // No season parsed, but category says TV -> Show wins.
        let parsed = parse_release_name(&release.title);
        assert_eq!(infer_media_kind(&release, &parsed, &config), MediaKind::Show);
    }

    #[test]
    fn infer_media_kind_falls_back_to_parsed_season_when_category_unknown() {
        let config = test_config();
        let release = ProwlarrRelease {
            guid: "g".into(),
            title: "Show.Name.S01E02.720p.WEB-DL".into(),
            indexer_id: 1,
            indexer: None,
            protocol: None,
            size: None,
            publish_date: None,
            info_url: None,
            download_url: None,
            info_hash: None,
            seeders: None,
            leechers: None,
            grabs: None,
            categories: vec![],
            indexer_flags: vec![],
            imdb_id_raw: None,
            tmdb_id_raw: None,
            tvdb_id_raw: None,
        };
        let parsed = parse_release_name(&release.title);
        assert_eq!(infer_media_kind(&release, &parsed, &config), MediaKind::Show);
    }

    #[test]
    fn infer_media_kind_defaults_to_movie_with_no_signal() {
        let config = test_config();
        let release = ProwlarrRelease {
            guid: "g".into(),
            title: "Ambiguous.Release.Name".into(),
            indexer_id: 1,
            indexer: None,
            protocol: None,
            size: None,
            publish_date: None,
            info_url: None,
            download_url: None,
            info_hash: None,
            seeders: None,
            leechers: None,
            grabs: None,
            categories: vec![],
            indexer_flags: vec![],
            imdb_id_raw: None,
            tmdb_id_raw: None,
            tvdb_id_raw: None,
        };
        let parsed = parse_release_name(&release.title);
        assert_eq!(infer_media_kind(&release, &parsed, &config), MediaKind::Movie);
    }

    #[tokio::test]
    async fn pull_if_due_skips_when_no_relevant_categories() {
        let server = MockServer::start();
        let client = ProwlarrClient::new(server.base_url(), "key").unwrap();
        let indexer = test_indexer(1, 10, vec![2000], None);

        let result = pull_if_due(&client, &indexer, &[], Utc::now()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn pull_if_due_skips_when_not_due() {
        let server = MockServer::start();
        let client = ProwlarrClient::new(server.base_url(), "key").unwrap();
        let now = Utc::now();
        let indexer = test_indexer(1, 10, vec![2000], Some(now));

        // last_rss_pull_at == now, interval 900s -> not due yet.
        let result = pull_if_due(&client, &indexer, &[2000], now).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn pull_if_due_pulls_when_due() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/search")
                .query_param("indexerIds", "10")
                .query_param("categories", "2000");
            then.status(200)
                .header("content-type", "application/json")
                .body("[]");
        });
        let client = ProwlarrClient::new(server.base_url(), "key").unwrap();
        let indexer = test_indexer(1, 10, vec![2000], None);

        let result = pull_if_due(&client, &indexer, &[2000], Utc::now()).await;
        mock.assert();
        assert!(matches!(result, Some(Ok(_))));
    }

    /// Defense-in-depth check (reuses the MUSE-16 `rate_limit` paused-clock
    /// pattern): even though the DB-backed `scheduler::is_due` check has no
    /// notion of elapsed time within a single `now` value, the
    /// `ProwlarrClient`'s own in-process `RateLimiter` still refuses an
    /// immediate second pull of the same indexer, and allows it again once
    /// the polite interval has actually elapsed.
    // Not `start_paused`: this test issues real HTTP to an httpmock server,
    // and tokio's auto-advancing paused clock fires reqwest's request timeout
    // before the mock I/O completes. Real time is fine here — the two
    // `pull_if_due` calls land microseconds apart, far inside the 900s polite
    // interval, so the client's own `RateLimiter` gates the second one. The
    // time-based *reset* (a later pull succeeding once the interval elapses)
    // is already covered by MUSE-16's `rate_limit` unit tests with a paused
    // clock and no HTTP; here we only prove `pull_if_due` delegates to it.
    #[tokio::test]
    async fn client_rate_limiter_still_gates_a_second_pull_within_the_same_tick() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body("[]");
        });
        let client = ProwlarrClient::new(server.base_url(), "key").unwrap();
        let indexer = test_indexer(1, 42, vec![2000], None);
        let now = Utc::now();

        let first = pull_if_due(&client, &indexer, &[2000], now).await;
        assert!(matches!(first, Some(Ok(_))), "first pull should succeed");

        let second = pull_if_due(&client, &indexer, &[2000], now).await;
        assert!(
            matches!(second, Some(Err(MuseError::Conflict(_)))),
            "immediate second pull should be rate limited by the client itself"
        );
    }

    // --- Live-DB test (MUSE-17): full pull -> parse -> upsert -> rollup ---
    //
    // Gated on MUSE_TEST_DATABASE_URL exactly like
    // `src/integration_tests.rs::core_schema_migrates_and_round_trips` --
    // logs and returns cleanly (does not fail) when unset, per the crate's
    // "must pass with no live database" build constraint.
    #[tokio::test]
    async fn report_pull_tick_persists_releases_and_recomputes_availability() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set -- skipping report_pull_tick_persists_releases_and_recomputes_availability"
            );
            return;
        };

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let prowlarr_id: i32 = (uuid::Uuid::new_v4().as_u128() % 1_000_000) as i32;

        // A known title to resolve against. NOTE: the title must match what
        // the mock release name (`Worker.Test.Movie.2020...`) parses to
        // exactly (case-insensitive title + year), since MUSE-17 resolution
        // is deliberately exact-match, not fuzzy. The `suffix` provides
        // per-run isolation via the unique `tmdb_id` below, not the title.
        let title = format!("Worker Test Movie {suffix}");
        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &crate::models::media_metadata::NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: title.clone(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: None,
                year: Some(2020),
                images: serde_json::json!({}),
            },
        )
        .await
        .expect("create media_metadata");

        let indexer = repo::indexer::upsert(
            &pool,
            &crate::models::indexer::NewIndexer {
                prowlarr_id,
                name: format!("worker-test-indexer-{suffix}"),
                protocol: Some("torrent".to_string()),
                privacy: Some("public".to_string()),
                enabled: true,
                categories: vec![2000],
                polite_min_interval_secs: 1, // short so the test doesn't need to wait
            },
        )
        .await
        .expect("create indexer");

        let server = MockServer::start();
        // Suffix in BOTH the seeded title and the release name so exact-match
        // resolution (find_by_title_year) is unambiguous even when the shared
        // test DB has accumulated other tests' titles. The parser reads
        // everything before the year token as the title, so the suffix token
        // lands in the parsed title and matches the seeded metadata exactly.
        let release_title = format!("Worker.Test.Movie.{suffix}.2020.1080p.BluRay.x264-TEST");
        let guid = format!("guid-{suffix}");
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/search")
                .query_param("indexerIds", prowlarr_id.to_string());
            then.status(200)
                .header("content-type", "application/json")
                .body(format!(
                    r#"[{{"guid": "{guid}", "title": "{release_title}", "indexerId": {prowlarr_id},
                        "size": 12345678, "seeders": 10, "leechers": 1,
                        "categories": [{{"id": 2000, "name": "Movies"}}]}}]"#
                ));
        });

        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").unwrap();

        let config = crate::config::Config {
            database_url: Some(database_url.clone()),
            prowlarr_movie_categories: vec![2000],
            prowlarr_tv_categories: vec![5000],
            prowlarr_resolve_min_confidence: 0.3,
            release_expiry_days: 21,
            ..Default::default()
        };

        let state = AppState {
            pool: pool.clone(),
            config,
            plex: None,
            prowlarr: Some(prowlarr),
            arr_instances: Vec::new(),
            enrichment: crate::enrichment::EnrichmentService::from_config(
                &crate::config::Config::default(),
            ),
            tmdb: None,
            embed: None,
            download: None,
        };

        let summary = run_tick(&state).await.expect("run_tick should succeed");

        assert_eq!(summary.indexers_polled, 1);
        assert_eq!(summary.releases_seen, 1);
        assert_eq!(summary.releases_resolved, 1, "release should resolve to the seeded title");
        assert_eq!(summary.availability_recomputed, 1);

        let stored_indexer = repo::indexer::get(&pool, indexer.id).await.expect("reload indexer");
        assert!(stored_indexer.last_rss_pull_at.is_some(), "mark_rss_pulled should have run");

        let releases = repo::release::list_by_media_metadata(&pool, metadata.id)
            .await
            .expect("list releases for resolved title");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].guid, guid);
        assert_eq!(releases[0].parsed_title.as_deref(), Some(format!("Worker Test Movie {suffix}").as_str()));
        assert_eq!(releases[0].parsed_year, Some(2020));

        let availability = repo::availability::get(&pool, metadata.id).await.expect("availability rollup exists");
        assert_eq!(availability.release_count, 1);
        assert_eq!(availability.best_seeders, Some(10));

        // A second tick within the polite interval should skip the same
        // indexer (still rate-limited by the client) rather than re-pulling.
        let second_summary = run_tick(&state).await.expect("second tick should not error");
        assert_eq!(second_summary.indexers_polled, 0);
        assert!(second_summary.indexers_skipped >= 1);
    }
}
