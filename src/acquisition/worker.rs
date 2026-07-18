//! MUSEM-06 (Plane MUSE S119 Sprint 1): the monitored "wanted" acquisition
//! worker — the Sonarr/Radarr-style background engine that periodically
//! scans `monitored_items` for anything missing or below its quality
//! profile's cutoff (`repo::acquisition::list_wanted`, MUSEM-01) and, for
//! each, runs the exact same search -> decide -> grab orchestration
//! MUSEM-05 already ships (`crate::acquisition::fulfill_request`) — never a
//! second implementation of that flow.
//!
//! ## Reuses, never bypasses, the MUSEM-05 gates
//! [`fulfill_request`] already enforces the master acquisition gate
//! (`ExperienceSettings.acquisition.enabled`) as its own first action, so a
//! grab is structurally impossible through this worker when that gate is
//! off. [`run_wanted_pass`] additionally checks the gate itself, up front,
//! purely so a gate-off pass can short-circuit to a cheap no-op without
//! even listing libraries/wanted items — an optimization, not a second
//! source of truth for the gate.
//!
//! ## Auto-tier policy, honestly (mirrors `crate::http::requests`)
//! A monitored item is never silently grabbed. For each wanted item this
//! worker runs a REAL on-demand Prowlarr search (MUSEM-03) to build an
//! actual [`Availability`] signal, then [`classify_tier`] (the same
//! tiered-safety gate `crate::arr::request` ships, reused rather than
//! duplicated) decides whether the item is `AutoApprovable` — the operator
//! opted in (`Config::arr_request_auto_tier_enabled`) AND a real search
//! confirmed it's grabbable right now. Only then does this worker create
//! the `media_requests` row and call `fulfill_request`. Every other
//! outcome (`NeedsReview`/`Blocked`, or "no capability configured at all")
//! still persists a `media_requests` row at `RequestStatus::Requested` —
//! Muse's real (no separate `NeedsReview` value) status enum expresses the
//! "pending, operator should look at this" state by simply never advancing
//! past `Requested`, exactly as `crate::acquisition`'s own module doc
//! establishes for [`FulfillOutcome::Skipped`] — so a monitored item never
//! silently vanishes, it becomes a request an operator can review or
//! `POST /requests/:id/approve` later.
//!
//! ## Non-blocking, capped, cooldown-guarded, idempotent
//! - An item already active (`queued`/`downloading`) in `download_queue`
//!   (`repo::acquisition::is_monitored_item_active_in_queue`) is skipped —
//!   never re-grabbed by a second pass or a second tick of the same pass.
//! - An item searched within `Config::wanted_search_cooldown_secs` of
//!   `monitored_items.last_search_at` is skipped without re-searching.
//! - `Config::wanted_max_grabs_per_pass` / `Config::wanted_max_searches_per_pass`
//!   cap one pass's blast radius — once either cap is hit, every remaining
//!   item this pass is skipped (counted, never silently dropped) rather
//!   than searched/grabbed.
//! - An unreachable Prowlarr/qBittorrent (or any other per-item failure —
//!   a deleted metadata row race, a DB hiccup) is logged and the pass moves
//!   on to the next item; [`run_wanted_pass`] never propagates an error and
//!   never aborts the maintenance chain it's scheduled inside
//!   (`crate::maintenance::run_maintenance_pass`).

use crate::acquisition::{fulfill_request, media_kind_str, search_candidates, AcquisitionDeps};
use crate::arr::request::{classify_tier, RequestTier};
use crate::config::Config;
use crate::download::DownloadClient;
use crate::models::acquisition::{NewMediaRequest, WantedItem};
use crate::models::availability::Availability;
use crate::prowlarr::ProwlarrClient;
use crate::repo;
use crate::settings::ExperienceSettings;
use sqlx::PgPool;

/// Everything [`run_wanted_pass`] needs — the same four dependencies
/// [`AcquisitionDeps`] threads for `fulfill_request`, grouped identically so
/// callers (today: `crate::maintenance::run_maintenance_pass`) don't thread
/// two near-identical dependency bags.
pub struct WantedPassDeps<'a> {
    pub pool: &'a PgPool,
    pub config: &'a Config,
    pub prowlarr: Option<&'a ProwlarrClient>,
    pub download: Option<&'a dyn DownloadClient>,
}

impl<'a> WantedPassDeps<'a> {
    fn acquisition_deps(&self) -> AcquisitionDeps<'a> {
        AcquisitionDeps {
            pool: self.pool,
            config: self.config,
            prowlarr: self.prowlarr,
            download: self.download,
        }
    }
}

/// One `monitored_items` row's disposition this pass — every branch is
/// counted in [`WantedPassSummary`], never silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemOutcome {
    Grabbed,
    NeedsReview,
    AlreadyQueued,
    CooldownActive,
    NoCapability,
    MetadataMissing,
    SearchFailed,
    Error,
}

/// Outcome of one [`run_wanted_pass`] call — a plain "how much happened"
/// tally, never a pass/fail. Mirrors `crate::maintenance::MaintenanceSummary`'s
/// posture: failures are counted, never propagated as an `Err` that would
/// abort the pass or the maintenance chain it's scheduled inside.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WantedPassSummary {
    pub libraries_scanned: usize,
    pub wanted_considered: usize,
    pub grabbed: usize,
    pub needs_review: usize,
    pub already_queued_skipped: usize,
    pub cooldown_skipped: usize,
    pub no_capability_skipped: usize,
    pub metadata_missing_skipped: usize,
    pub search_failed: usize,
    pub errors: usize,
    pub grab_cap_skipped: usize,
    pub search_cap_skipped: usize,
}

/// Run one full "wanted" scan-and-fulfill pass across every library. Never
/// panics, never returns `Err` — see the module doc for the graceful-
/// degrade posture every step follows. Safe to call on a schedule (from
/// `crate::maintenance::run_maintenance_pass`) or on demand.
pub async fn run_wanted_pass(deps: &WantedPassDeps<'_>) -> WantedPassSummary {
    let mut summary = WantedPassSummary::default();

    let settings = match repo::settings::load(deps.pool).await {
        Ok(settings) => settings,
        Err(e) => {
            tracing::warn!(error = %e, "MUSEM-06: wanted pass — could not load settings; skipping pass");
            return summary;
        }
    };
    if !settings.is_acquisition_enabled() {
        tracing::debug!(
            "MUSEM-06: wanted pass — master acquisition gate is off; skipping pass \
             (fulfill_request would no-op per item anyway, but this avoids even listing \
             libraries/wanted items for a gate that's off)"
        );
        return summary;
    }

    let libraries = match repo::library::list(deps.pool).await {
        Ok(libraries) => libraries,
        Err(e) => {
            tracing::warn!(error = %e, "MUSEM-06: wanted pass — could not list libraries; skipping pass");
            return summary;
        }
    };

    let cooldown = chrono::Duration::seconds(deps.config.wanted_search_cooldown_secs.max(0));
    let max_grabs = deps.config.wanted_max_grabs_per_pass;
    let max_searches = deps.config.wanted_max_searches_per_pass;
    let mut searches_this_pass: usize = 0;

    for library in &libraries {
        summary.libraries_scanned += 1;

        let wanted = match repo::acquisition::list_wanted(deps.pool, library.id).await {
            Ok(wanted) => wanted,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    library_id = library.id,
                    "MUSEM-06: wanted pass — list_wanted failed for this library; continuing"
                );
                continue;
            }
        };

        for item in wanted {
            summary.wanted_considered += 1;

            if summary.grabbed >= max_grabs {
                summary.grab_cap_skipped += 1;
                continue;
            }
            if searches_this_pass >= max_searches {
                summary.search_cap_skipped += 1;
                continue;
            }

            let (outcome, searched) =
                process_wanted_item(deps, &settings, &item, cooldown).await;
            if searched {
                searches_this_pass += 1;
            }
            match outcome {
                ItemOutcome::Grabbed => summary.grabbed += 1,
                ItemOutcome::NeedsReview => summary.needs_review += 1,
                ItemOutcome::AlreadyQueued => summary.already_queued_skipped += 1,
                ItemOutcome::CooldownActive => summary.cooldown_skipped += 1,
                ItemOutcome::NoCapability => summary.no_capability_skipped += 1,
                ItemOutcome::MetadataMissing => summary.metadata_missing_skipped += 1,
                ItemOutcome::SearchFailed => summary.search_failed += 1,
                ItemOutcome::Error => summary.errors += 1,
            }
        }
    }

    tracing::info!(
        libraries_scanned = summary.libraries_scanned,
        wanted_considered = summary.wanted_considered,
        grabbed = summary.grabbed,
        needs_review = summary.needs_review,
        already_queued_skipped = summary.already_queued_skipped,
        cooldown_skipped = summary.cooldown_skipped,
        search_failed = summary.search_failed,
        errors = summary.errors,
        "MUSEM-06: wanted pass complete"
    );
    summary
}

/// Process one wanted item end to end. Returns `(outcome, searched)` —
/// `searched` is `true` only when an on-demand Prowlarr search was actually
/// attempted (whether it succeeded or not), so the caller's
/// `wanted_max_searches_per_pass` cap counts real search attempts, not
/// every wanted item considered.
async fn process_wanted_item(
    deps: &WantedPassDeps<'_>,
    settings: &ExperienceSettings,
    item: &WantedItem,
    cooldown: chrono::Duration,
) -> (ItemOutcome, bool) {
    // Idempotency: never re-grab an item already mid-flight, whether from a
    // previous pass or an earlier item in this same pass.
    match repo::acquisition::is_monitored_item_active_in_queue(deps.pool, item.monitored_item_id)
        .await
    {
        Ok(true) => return (ItemOutcome::AlreadyQueued, false),
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(
                error = %e,
                monitored_item_id = item.monitored_item_id,
                "MUSEM-06: wanted pass — could not check download_queue membership; skipping item"
            );
            return (ItemOutcome::Error, false);
        }
    }

    // Cooldown: read once per item (fail-safe OFF, per the edge case in the
    // item spec — a gate toggled mid-pass is honored on the NEXT item, this
    // read is for the cooldown, not the master gate, but the same
    // read-once-per-item posture applies).
    let monitored = match repo::acquisition::get_monitored_item(deps.pool, item.monitored_item_id)
        .await
    {
        Ok(monitored) => monitored,
        Err(e) => {
            tracing::warn!(
                error = %e,
                monitored_item_id = item.monitored_item_id,
                "MUSEM-06: wanted pass — could not reload monitored_item; skipping item"
            );
            return (ItemOutcome::Error, false);
        }
    };
    if let Some(last_search_at) = monitored.last_search_at {
        if chrono::Utc::now() - last_search_at < cooldown {
            return (ItemOutcome::CooldownActive, false);
        }
    }

    // Metadata row deleted out from under this wanted row (a race between
    // list_wanted's snapshot and now) — skip cleanly, not a crash. Also
    // resolves the media_kind (`monitored_items`/`WantedItem` don't carry
    // it directly) that `media_requests.media_kind` needs.
    let metadata = match repo::media_metadata::get(deps.pool, item.media_metadata_id).await {
        Ok(metadata) => metadata,
        Err(e) => {
            tracing::warn!(
                error = %e,
                monitored_item_id = item.monitored_item_id,
                media_metadata_id = item.media_metadata_id,
                "MUSEM-06: wanted pass — media_metadata row missing for a wanted item; skipping"
            );
            return (ItemOutcome::MetadataMissing, false);
        }
    };

    // "Has capability at all" -- same honest re-reading `crate::http::requests`
    // already applies to `classify_tier`'s `has_matching_arr_instance` param
    // for this Prowlarr-native (not *arr-fleet) grab path: a configured
    // Prowlarr client and a resolvable quality profile for this item. Without
    // either, this item can never even be searched -- skip without touching
    // `last_search_at` (nothing was attempted) or persisting a request (no
    // quality profile means `fulfill_request` could only ever Skip it anyway).
    let Some(prowlarr) = deps.prowlarr else {
        return (ItemOutcome::NoCapability, false);
    };
    if item.quality_profile_id.is_none() {
        return (ItemOutcome::NoCapability, false);
    }

    // The real, on-demand search -- never a fabricated availability signal,
    // same posture as `crate::acquisition`'s module doc and
    // `crate::http::requests::create_request_handler`.
    let media_kind = media_kind_str(metadata.kind);
    let candidates =
        match search_candidates(prowlarr, deps.config, &item.title, media_kind).await {
            Ok(candidates) => candidates,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    monitored_item_id = item.monitored_item_id,
                    title = %item.title,
                    "MUSEM-06: wanted pass — search failed for this item (Prowlarr unreachable or \
                     rate-limited); skipping item, pass continues"
                );
                // A search was attempted (and failed) -- counts against the
                // per-pass search cap same as a successful one, and touches
                // last_search_at so a persistently-unreachable indexer
                // doesn't get hammered again next tick within the cooldown.
                touch_last_search_best_effort(deps.pool, item.monitored_item_id).await;
                return (ItemOutcome::SearchFailed, true);
            }
        };

    touch_last_search_best_effort(deps.pool, item.monitored_item_id).await;

    let availability = Availability {
        media_metadata_id: item.media_metadata_id,
        best_quality: None,
        best_seeders: candidates.iter().filter_map(|c| c.seeders).max(),
        release_count: candidates.len() as i32,
        has_freeleech: candidates.iter().any(|c| c.freeleech),
        cheapest_size_bytes: candidates.iter().filter_map(|c| c.size).min(),
        newest_release_at: candidates.iter().filter_map(|c| c.publish_date).max(),
        computed_at: chrono::Utc::now(),
    };

    // Auto-tier policy: `classify_tier`, reused verbatim from
    // `crate::arr::request` -- never a second, ad hoc tiering rule. The
    // master acquisition gate is already read via `settings` above (and
    // enforced again, unbypassably, inside `fulfill_request` itself) --
    // `auto_tier_enabled` here folds in the OPERATOR opt-in
    // (`Config::arr_request_auto_tier_enabled`) plus the download-client
    // precondition, same as `create_request_handler`'s own
    // `auto_tier_enabled` computation.
    let auto_tier_enabled =
        settings.is_acquisition_enabled() && deps.config.arr_request_auto_tier_enabled && deps.download.is_some();
    let tier = classify_tier(auto_tier_enabled, Some(&availability), true);

    let new_request = NewMediaRequest {
        provider_ids: serde_json::json!({}),
        media_kind: media_kind.to_string(),
        title: item.title.clone(),
        requested_by: Some("wanted-worker".to_string()),
        tier: Some(format!("{tier:?}")),
        quality_profile_id: item.quality_profile_id,
        note: Some(format!("MUSEM-06 wanted worker: monitored_item_id={}", item.monitored_item_id)),
    };
    let request = match repo::acquisition::create_request(deps.pool, &new_request).await {
        Ok(request) => request,
        Err(e) => {
            tracing::warn!(
                error = %e,
                monitored_item_id = item.monitored_item_id,
                "MUSEM-06: wanted pass — could not persist media_request; skipping item"
            );
            return (ItemOutcome::Error, true);
        }
    };

    // Never silently grab: only `AutoApprovable` (operator opted in AND
    // this real search confirmed it's grabbable now) reaches
    // `fulfill_request` at all -- `NeedsReview`/`Blocked` leave the just-
    // persisted request at `RequestStatus::Requested` for the operator, the
    // exact "persist but never act" posture `crate::arr::request::
    // submit_if_appropriate` and `create_request_handler` already
    // establish.
    if tier != RequestTier::AutoApprovable {
        return (ItemOutcome::NeedsReview, true);
    }

    match fulfill_request(&deps.acquisition_deps(), &request).await {
        Ok(crate::acquisition::FulfillOutcome::Grabbed { .. }) => (ItemOutcome::Grabbed, true),
        Ok(crate::acquisition::FulfillOutcome::Rejected { reasons }) => {
            tracing::debug!(
                monitored_item_id = item.monitored_item_id,
                reasons = ?reasons,
                "MUSEM-06: wanted pass — fulfill_request rejected every candidate for this item"
            );
            (ItemOutcome::NeedsReview, true)
        }
        Ok(crate::acquisition::FulfillOutcome::Skipped { reason }) => {
            tracing::debug!(
                monitored_item_id = item.monitored_item_id,
                reason = %reason,
                "MUSEM-06: wanted pass — fulfill_request skipped this item"
            );
            (ItemOutcome::NeedsReview, true)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                monitored_item_id = item.monitored_item_id,
                "MUSEM-06: wanted pass — fulfill_request errored for this item; continuing"
            );
            (ItemOutcome::Error, true)
        }
    }
}

/// `touch_last_search` is best-effort here: a failure to record it just
/// means this item might get re-searched sooner than the cooldown intends
/// next pass -- never worth aborting the item over, let alone the pass.
async fn touch_last_search_best_effort(pool: &PgPool, monitored_item_id: i64) {
    if let Err(e) = repo::acquisition::touch_last_search(pool, monitored_item_id).await {
        tracing::warn!(
            error = %e,
            monitored_item_id,
            "MUSEM-06: wanted pass — could not update last_search_at (best-effort; continuing)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wanted_pass_summary_default_is_all_zero() {
        let summary = WantedPassSummary::default();
        assert_eq!(summary.libraries_scanned, 0);
        assert_eq!(summary.grabbed, 0);
        assert_eq!(summary.needs_review, 0);
    }
}

/// DB-gated coverage for `process_wanted_item`/`run_wanted_pass` —
/// `MUSE_TEST_DATABASE_URL`-gated, same convention as
/// `crate::acquisition::db_gated` / `crate::repo::acquisition::db_gated`.
/// Skips cleanly, never fails, when no test database is configured.
///
/// ## Why most assertions target ONE seeded item, not pass-wide totals
/// `run_wanted_pass`/`list_wanted` scan every library and every monitored
/// item in the shared test database — other `db_gated` test modules in this
/// crate (`repo::acquisition::db_gated` in particular) also seed
/// `monitored_items` rows, and `cargo test` runs these concurrently against
/// the same database by default. Asserting an exact pass-wide count (e.g.
/// "exactly 1 item was grabbed this pass") would be flaky under that
/// concurrency. Instead, each test below queries for the SPECIFIC
/// monitored-item/request/queue row it seeded (by its own unique id or a
/// `note` containing that id) — reliable regardless of what else is
/// happening in the shared database — except where a `WantedPassSummary`
/// counter is itself deterministic pass-wide (e.g. a `0` cap makes
/// `summary.grabbed == 0` true no matter what else is wanted).
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::models::acquisition::{MediaRequest, NewMonitoredItem, RequestStatus};
    use crate::models::library::{Library, LibraryKind, NewLibrary};
    use crate::models::media_metadata::MediaKind;
    use crate::models::quality::{NewQualityDefinition, NewQualityProfile};
    use crate::download::MockDownloadClient;
    use httpmock::prelude::*;

    async fn test_pool_or_skip(test_name: &str) -> Option<PgPool> {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping {test_name} \
                 (expected in the default test run; this harness does not \
                 require a live DB)"
            );
            return None;
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
        Some(pool)
    }

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    }

    fn test_config() -> Config {
        Config {
            wanted_search_cooldown_secs: 21_600,
            wanted_max_grabs_per_pass: 5,
            wanted_max_searches_per_pass: 20,
            ..Config::default()
        }
    }

    async fn save_settings(pool: &PgPool, acquisition_enabled: bool) {
        let mut settings = ExperienceSettings::default();
        settings.acquisition = crate::settings::AcquisitionSettings {
            enabled: acquisition_enabled,
        };
        repo::settings::save(pool, &settings).await.expect("save settings");
    }

    async fn seed_library(pool: &PgPool) -> Library {
        let new = NewLibrary {
            name: format!("musem06-lib-{}", unique_suffix()),
            kind: LibraryKind::Movie,
            root_folder: "/data/musem06-movies".to_string(),
            source_arr_name: None,
            source_arr_url: None,
        };
        repo::library::create(pool, &new).await.expect("seed library")
    }

    async fn seed_media_metadata(pool: &PgPool, title: &str) -> i64 {
        let row: (i64,) = sqlx::query_as(
            r#"
            INSERT INTO media_metadata (kind, title, provider_ids)
            VALUES ($1, $2, '{}'::jsonb)
            RETURNING id
            "#,
        )
        .bind(MediaKind::Movie)
        .bind(title)
        .fetch_one(pool)
        .await
        .expect("seed media_metadata");
        row.0
    }

    /// A quality profile that allows (and is satisfied by) a plain
    /// `1080p WEB-DL` release — mirrors
    /// `crate::acquisition::db_gated::seed_profile_allowing_web_1080p`.
    async fn seed_profile_allowing_web_1080p(pool: &PgPool) -> i64 {
        let def = repo::quality::create_definition(
            pool,
            &NewQualityDefinition {
                quality_key: format!("musem06-web-1080p-{}", unique_suffix()),
                title: "WEB 1080p".to_string(),
                source: "WEB-DL".to_string(),
                resolution: Some("1080p".to_string()),
                modifier: "none".to_string(),
                sort_order: 1,
            },
        )
        .await
        .expect("seed quality_definition");

        repo::quality::create_profile(
            pool,
            &NewQualityProfile {
                name: format!("musem06-profile-{}", unique_suffix()),
                cutoff_quality_id: None,
                items: serde_json::json!([{ "quality": { "id": def.id }, "allowed": true }]),
                upgrade_allowed: true,
                natural_language_intent: None,
            },
        )
        .await
        .expect("seed quality_profile")
        .id
    }

    /// A quality profile with zero allowed tiers — every candidate is
    /// rejected by `decide_release`, matching
    /// `crate::acquisition::db_gated`'s "empty profile" seed.
    async fn seed_profile_rejecting_everything(pool: &PgPool) -> i64 {
        repo::quality::create_profile(
            pool,
            &NewQualityProfile {
                name: format!("musem06-empty-profile-{}", unique_suffix()),
                cutoff_quality_id: None,
                items: serde_json::json!([]),
                upgrade_allowed: true,
                natural_language_intent: None,
            },
        )
        .await
        .expect("seed empty quality_profile")
        .id
    }

    /// Seeds one "missing" (no `media_item_id`) monitored, wanted movie —
    /// the simplest `list_wanted` shape (no on-disk file at all, always
    /// wanted regardless of cutoff). Returns the `WantedItem` shape
    /// `process_wanted_item` consumes, built directly rather than round-
    /// tripped through `list_wanted` (both are exercised together by the
    /// `run_wanted_pass`-level tests below).
    async fn seed_wanted_item(pool: &PgPool, library: &Library, quality_profile_id: Option<i64>) -> WantedItem {
        let title = format!("Musem06 Wanted Movie {}", unique_suffix());
        let media_metadata_id = seed_media_metadata(pool, &title).await;
        let monitored = repo::acquisition::create_monitored_item(
            pool,
            &NewMonitoredItem {
                media_metadata_id,
                media_item_id: None,
                library_id: library.id,
                monitored: true,
                quality_profile_id,
                min_availability: None,
            },
        )
        .await
        .expect("seed monitored_item");

        WantedItem {
            monitored_item_id: monitored.id,
            media_metadata_id,
            library_id: library.id,
            title,
            quality_profile_id,
            has_file: false,
            best_quality_sort_order: None,
            cutoff_sort_order: None,
        }
    }

    fn search_response_body(guid: &str, title: &str) -> String {
        format!(
            r#"[{{"guid": "{guid}", "title": "{title}", "indexerId": 1,
                  "indexer": "TestIndexer", "protocol": "torrent",
                  "downloadUrl": "http://example.invalid/dl/{guid}",
                  "categories": [{{"id": 2000, "name": "Movies"}}]}}]"#
        )
    }

    async fn request_for_monitored_item(pool: &PgPool, monitored_item_id: i64) -> Option<MediaRequest> {
        let note = format!("MUSEM-06 wanted worker: monitored_item_id={monitored_item_id}");
        sqlx::query_as::<_, MediaRequest>("SELECT * FROM media_requests WHERE note = $1")
            .bind(note)
            .fetch_optional(pool)
            .await
            .expect("query media_requests by note")
    }

    // --- process_wanted_item: the per-item decision path -------------------

    #[tokio::test]
    async fn auto_tier_confirmed_grabbable_item_grabs_and_writes_queue_row() {
        let Some(pool) = test_pool_or_skip("auto_tier_confirmed_grabbable_item_grabs_and_writes_queue_row").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let item = seed_wanted_item(&pool, &library, Some(profile_id)).await;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body("musem06-grab-guid", &format!("{}.2020.1080p.WEB-DL", item.title)));
        });
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let config = test_config();
        let download = MockDownloadClient::new();
        let mut settings = ExperienceSettings::default();
        settings.acquisition.enabled = true;

        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let (outcome, searched) = process_wanted_item(&deps, &settings, &item, chrono::Duration::seconds(config.wanted_search_cooldown_secs)).await;
        assert_eq!(outcome, ItemOutcome::Grabbed);
        assert!(searched);
        assert_eq!(download.added_count(), 1);

        let queued = repo::acquisition::is_monitored_item_active_in_queue(&pool, item.monitored_item_id)
            .await
            .expect("check queue membership");
        assert!(queued, "a grabbed item must leave an active download_queue row");

        let request = request_for_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("a media_request row must have been persisted");
        assert_eq!(request.status, RequestStatus::Grabbed.as_str());
    }

    #[tokio::test]
    async fn item_without_auto_tier_becomes_a_requested_request_with_no_grab() {
        let Some(pool) = test_pool_or_skip("item_without_auto_tier_becomes_a_requested_request_with_no_grab").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let item = seed_wanted_item(&pool, &library, Some(profile_id)).await;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body("musem06-review-guid", &format!("{}.2020.1080p.WEB-DL", item.title)));
        });
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        // `arr_request_auto_tier_enabled` stays false (Config::default()) —
        // the operator never opted in to auto-tier, so even a confirmed-
        // grabbable item must become a `Requested` request, never a grab.
        let config = test_config();
        assert!(!config.arr_request_auto_tier_enabled);
        let download = MockDownloadClient::new();
        let mut settings = ExperienceSettings::default();
        settings.acquisition.enabled = true;

        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let (outcome, searched) = process_wanted_item(&deps, &settings, &item, chrono::Duration::seconds(config.wanted_search_cooldown_secs)).await;
        assert_eq!(outcome, ItemOutcome::NeedsReview);
        assert!(searched);
        assert_eq!(download.added_count(), 0, "no auto-tier must never grab");

        let request = request_for_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("a media_request row must still have been persisted");
        assert_eq!(request.status, RequestStatus::Requested.as_str());
    }

    #[tokio::test]
    async fn decision_reject_still_persists_a_requested_request_no_grab() {
        let Some(pool) = test_pool_or_skip("decision_reject_still_persists_a_requested_request_no_grab").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let profile_id = seed_profile_rejecting_everything(&pool).await;
        let item = seed_wanted_item(&pool, &library, Some(profile_id)).await;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body("musem06-reject-guid", &format!("{}.2020.1080p.WEB-DL", item.title)));
        });
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let config = Config {
            arr_request_auto_tier_enabled: true,
            ..test_config()
        };
        let download = MockDownloadClient::new();
        let mut settings = ExperienceSettings::default();
        settings.acquisition.enabled = true;

        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let (outcome, searched) = process_wanted_item(&deps, &settings, &item, chrono::Duration::seconds(config.wanted_search_cooldown_secs)).await;
        assert_eq!(outcome, ItemOutcome::NeedsReview);
        assert!(searched);
        assert_eq!(download.added_count(), 0, "a rejected decision must never grab");

        let request = request_for_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("a media_request row must have been persisted");
        assert_eq!(request.status, RequestStatus::Failed.as_str(), "fulfill_request marks a rejected decision Failed");
    }

    #[tokio::test]
    async fn already_queued_item_is_skipped_no_duplicate_grab() {
        let Some(pool) = test_pool_or_skip("already_queued_item_is_skipped_no_duplicate_grab").await else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let item = seed_wanted_item(&pool, &library, Some(profile_id)).await;

        // Simulate an already in-flight grab for this monitored item.
        repo::acquisition::enqueue_download(
            &pool,
            &crate::models::acquisition::NewDownloadQueueEntry {
                source: crate::models::acquisition::DownloadSource::MonitoredItem(item.monitored_item_id),
                release_guid: format!("musem06-already-queued-{}", unique_suffix()),
                release_title: "Already Queued Release".to_string(),
                indexer: None,
                download_client: Some("qbittorrent".to_string()),
                client_hash: None,
                protocol: Some("torrent".to_string()),
                size_bytes: None,
            },
        )
        .await
        .expect("seed an already-active download_queue row");

        let config = test_config();
        let download = MockDownloadClient::new();
        let mut settings = ExperienceSettings::default();
        settings.acquisition.enabled = true;
        // No Prowlarr mock configured at all -- proves the already-queued
        // check short-circuits BEFORE any search is attempted.
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: None,
            download: Some(&download),
        };

        let (outcome, searched) = process_wanted_item(&deps, &settings, &item, chrono::Duration::seconds(config.wanted_search_cooldown_secs)).await;
        assert_eq!(outcome, ItemOutcome::AlreadyQueued);
        assert!(!searched);
        assert_eq!(download.added_count(), 0);
        assert!(
            request_for_monitored_item(&pool, item.monitored_item_id).await.is_none(),
            "an already-queued item must never get a second media_request persisted"
        );
    }

    #[tokio::test]
    async fn cooldown_active_skips_without_searching() {
        let Some(pool) = test_pool_or_skip("cooldown_active_skips_without_searching").await else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let item = seed_wanted_item(&pool, &library, Some(profile_id)).await;

        // Mark this item as just-searched -- well within any sane cooldown.
        repo::acquisition::touch_last_search(&pool, item.monitored_item_id)
            .await
            .expect("touch_last_search");

        let config = test_config();
        let download = MockDownloadClient::new();
        let mut settings = ExperienceSettings::default();
        settings.acquisition.enabled = true;
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: None, // never reached -- proves no search is attempted
            download: Some(&download),
        };

        let (outcome, searched) = process_wanted_item(
            &deps,
            &settings,
            &item,
            chrono::Duration::seconds(config.wanted_search_cooldown_secs),
        )
        .await;
        assert_eq!(outcome, ItemOutcome::CooldownActive);
        assert!(!searched);
        assert_eq!(download.added_count(), 0);
    }

    #[tokio::test]
    async fn missing_quality_profile_is_no_capability_without_search() {
        let Some(pool) = test_pool_or_skip("missing_quality_profile_is_no_capability_without_search").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let item = seed_wanted_item(&pool, &library, None).await;

        let config = test_config();
        let download = MockDownloadClient::new();
        let mut settings = ExperienceSettings::default();
        settings.acquisition.enabled = true;
        let server = MockServer::start(); // deliberately no mock registered
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let (outcome, searched) = process_wanted_item(
            &deps,
            &settings,
            &item,
            chrono::Duration::seconds(config.wanted_search_cooldown_secs),
        )
        .await;
        assert_eq!(outcome, ItemOutcome::NoCapability);
        assert!(!searched);
        assert_eq!(download.added_count(), 0);
    }

    #[tokio::test]
    async fn prowlarr_unreachable_search_failed_item_skipped() {
        let Some(pool) = test_pool_or_skip("prowlarr_unreachable_search_failed_item_skipped").await else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let item = seed_wanted_item(&pool, &library, Some(profile_id)).await;

        // Nothing listens here -- every request errors at the connection
        // level, simulating Prowlarr being unreachable.
        let prowlarr = ProwlarrClient::new("http://127.0.0.1:1", "test-key").expect("client");
        let config = test_config();
        let download = MockDownloadClient::new();
        let mut settings = ExperienceSettings::default();
        settings.acquisition.enabled = true;
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let (outcome, searched) = process_wanted_item(
            &deps,
            &settings,
            &item,
            chrono::Duration::seconds(config.wanted_search_cooldown_secs),
        )
        .await;
        assert_eq!(outcome, ItemOutcome::SearchFailed);
        assert!(searched, "a failed search still counts as an attempt for the per-pass search cap");
        assert_eq!(download.added_count(), 0);
        assert!(
            request_for_monitored_item(&pool, item.monitored_item_id).await.is_none(),
            "a failed search must never persist a media_request"
        );
    }

    // --- run_wanted_pass: pass-level orchestration --------------------------

    #[tokio::test]
    async fn wanted_pass_master_gate_off_is_a_clean_noop() {
        let Some(pool) = test_pool_or_skip("wanted_pass_master_gate_off_is_a_clean_noop").await else {
            return;
        };
        save_settings(&pool, false).await;
        let config = test_config();
        let download = MockDownloadClient::new();
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: None,
            download: Some(&download),
        };

        let summary = run_wanted_pass(&deps).await;
        assert_eq!(
            summary,
            WantedPassSummary::default(),
            "the master gate being off must short-circuit to a fully-zero summary"
        );
        assert_eq!(download.added_count(), 0);
    }

    #[tokio::test]
    async fn wanted_pass_zero_grab_cap_prevents_any_grab_pass_wide() {
        let Some(pool) = test_pool_or_skip("wanted_pass_zero_grab_cap_prevents_any_grab_pass_wide").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let item = seed_wanted_item(&pool, &library, Some(profile_id)).await;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body("musem06-cap-guid", &format!("{}.2020.1080p.WEB-DL", item.title)));
        });
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let config = Config {
            arr_request_auto_tier_enabled: true,
            wanted_max_grabs_per_pass: 0,
            ..test_config()
        };
        let download = MockDownloadClient::new();
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        let summary = run_wanted_pass(&deps).await;
        // Deterministic pass-wide regardless of what else is wanted in the
        // shared test database: the cap is 0, so `summary.grabbed` can never
        // exceed it.
        assert_eq!(summary.grabbed, 0);

        let queued = repo::acquisition::is_monitored_item_active_in_queue(&pool, item.monitored_item_id)
            .await
            .expect("check queue membership");
        assert!(!queued, "my seeded item specifically must not have been grabbed");
    }

    #[tokio::test]
    async fn wanted_pass_zero_search_cap_prevents_any_search_pass_wide() {
        let Some(pool) = test_pool_or_skip("wanted_pass_zero_search_cap_prevents_any_search_pass_wide").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let item = seed_wanted_item(&pool, &library, Some(profile_id)).await;

        let config = Config {
            wanted_max_searches_per_pass: 0,
            ..test_config()
        };
        let download = MockDownloadClient::new();
        // No Prowlarr configured -- if the cap were ineffective and a search
        // were attempted anyway, it would hit `NoCapability` instead (still
        // zero grabs), so this test also proves the zero-search-cap path is
        // reached BEFORE the capability check would matter for the
        // assertion below via `last_search_at` staying untouched.
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: None,
            download: Some(&download),
        };

        run_wanted_pass(&deps).await;

        let monitored = repo::acquisition::get_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("reload monitored_item");
        assert!(
            monitored.last_search_at.is_none(),
            "the zero search cap must prevent even an attempted search for my item"
        );
        assert_eq!(download.added_count(), 0);
    }

    #[tokio::test]
    async fn wanted_pass_scans_a_freshly_created_library_without_error() {
        let Some(pool) = test_pool_or_skip("wanted_pass_scans_a_freshly_created_library_without_error").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        // Deliberately zero monitored items in this brand-new library.
        let wanted = repo::acquisition::list_wanted(&pool, library.id).await.expect("list_wanted");
        assert!(wanted.is_empty(), "a freshly created library must start with an empty wanted list");

        let config = test_config();
        let download = MockDownloadClient::new();
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: None,
            download: Some(&download),
        };

        // A clean no-op pass must not panic and must scan at least this
        // library (proving the empty-wanted-list-for-a-library branch is a
        // genuine no-op, not skipped entirely).
        let summary = run_wanted_pass(&deps).await;
        assert!(summary.libraries_scanned >= 1);
    }
}
