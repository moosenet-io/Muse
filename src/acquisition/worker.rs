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

use crate::acquisition::{fulfill_request, media_kind_str, search_candidates, AcquisitionDeps, FulfillOptions};
use crate::arr::request::{classify_tier, RequestTier};
use crate::config::Config;
use crate::download::DownloadClient;
use crate::error::MuseResult;
use crate::models::acquisition::{MediaRequest, NewMediaRequest, WantedItem};
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

    // Review finding (codex, MUSEM-06 REQUEST_CHANGES, second follow-up):
    // create-once for every non-grabbed persist path (no-capability AND
    // NeedsReview/Blocked below) must be keyed off whether a request
    // ACTUALLY EXISTS for this monitored item, not off `last_search_at`
    // being non-`NULL`. The original `last_search_at.is_none()` proxy was
    // wrong: `last_search_at` is ALSO set after a FAILED search (see
    // below), which creates no request at all -- so a pass-1 search
    // failure followed by a pass-2 success-but-`NeedsReview` would
    // permanently suppress the request this item genuinely needed, since
    // `last_search_at` was already non-`NULL` from the failed pass. Fixed
    // by asking the real question directly against `media_requests` (see
    // `repo::acquisition::has_open_worker_request_for_monitored_item`'s
    // doc for what "open" means).
    let has_open_request =
        match repo::acquisition::has_open_worker_request_for_monitored_item(deps.pool, item.monitored_item_id)
            .await
        {
            Ok(has_open_request) => has_open_request,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    monitored_item_id = item.monitored_item_id,
                    "MUSEM-06: wanted pass — could not check for an existing open request; skipping item"
                );
                return (ItemOutcome::Error, false);
            }
        };

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

    let media_kind = media_kind_str(metadata.kind);

    // "Has capability at all" -- same honest re-reading `crate::http::requests`
    // already applies to `classify_tier`'s `has_matching_arr_instance` param
    // for this Prowlarr-native (not *arr-fleet) grab path: a configured
    // Prowlarr client and a resolvable quality profile for this item. Without
    // either, this item can never even be searched, so there's no on-demand
    // search to run here -- but review finding 3 (codex, MUSEM-06
    // REQUEST_CHANGES) still applies: this must NOT silently vanish, it must
    // persist a `Requested` `media_request` for the operator, same "persist
    // but never act" posture as every other non-`AutoApprovable` outcome
    // below. That persist is create-ONCE (`!has_open_request`, computed
    // above) -- so this item's cooldown/create-once discipline is honored on
    // subsequent passes too and no duplicate request is created.
    let Some(prowlarr) = deps.prowlarr else {
        persist_pending_request_once(deps, item, media_kind, has_open_request, "no Prowlarr client configured")
            .await;
        return (ItemOutcome::NoCapability, false);
    };
    if item.quality_profile_id.is_none() {
        persist_pending_request_once(
            deps,
            item,
            media_kind,
            has_open_request,
            "no quality_profile_id set on this monitored item",
        )
        .await;
        return (ItemOutcome::NoCapability, false);
    }

    // The real, on-demand search -- never a fabricated availability signal,
    // same posture as `crate::acquisition`'s module doc and
    // `crate::http::requests::create_request_handler`.
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

    // Never silently grab: only `AutoApprovable` (operator opted in AND
    // this real search confirmed it's grabbable now) reaches
    // `fulfill_request` at all -- `NeedsReview`/`Blocked` leave a
    // `RequestStatus::Requested` request for the operator, the exact
    // "persist but never act" posture `crate::arr::request::
    // submit_if_appropriate` and `create_request_handler` already
    // establish.
    //
    // Follow-up finding (opus, MUSEM-06 REQUEST_CHANGES): this persist must
    // be create-ONCE too, same `!has_open_request` guard as the
    // no-capability branches above -- otherwise a monitored item that keeps
    // classifying `NeedsReview` (auto-tier off, or never confirmed
    // grabbable) gets a brand-new `Requested` request every ~cooldown-
    // interval forever. Net invariant: at most one worker-created
    // `Requested` request is ever persisted per monitored item across
    // EVERY non-grabbed outcome (no-capability, NeedsReview, Blocked).
    if tier != RequestTier::AutoApprovable {
        if !has_open_request {
            if let Err(e) = create_wanted_request(deps, item, media_kind, &format!("{tier:?}")).await {
                tracing::warn!(
                    error = %e,
                    monitored_item_id = item.monitored_item_id,
                    "MUSEM-06: wanted pass — could not persist media_request; continuing"
                );
                return (ItemOutcome::Error, true);
            }
        } else {
            tracing::debug!(
                monitored_item_id = item.monitored_item_id,
                ?tier,
                "MUSEM-06: wanted pass — item already has an open request from a prior pass; not \
                 creating a second one"
            );
        }
        return (ItemOutcome::NeedsReview, true);
    }

    // `AutoApprovable` always needs a request row to hand `fulfill_request`
    // (the actual grab attempt), regardless of `has_open_request` -- the
    // create-once guard above is about not spamming PENDING requests for an
    // outcome that never acts; a real grab attempt is a distinct event, and
    // this branch's own idempotency comes from the `download_queue`
    // `monitored_item_id` check at the top of this function (a second pass
    // never even reaches here for an item that's already queued).
    let request = match create_wanted_request(deps, item, media_kind, &format!("{tier:?}")).await {
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

    // Review finding 1 (opus) + finding 2 (codex, MUSEM-06 REQUEST_CHANGES):
    // thread `monitored_item_id` through so the resulting `download_queue`
    // row is idempotency-visible (see `FulfillOptions`'s doc), and hand
    // `fulfill_request` the candidates THIS call already fetched above so it
    // never runs a second, redundant on-demand search for the same title --
    // exactly one real Prowlarr search per wanted item, which is what makes
    // `wanted_max_searches_per_pass` an honest bound on actual search calls.
    let options = FulfillOptions {
        monitored_item_id: Some(item.monitored_item_id),
        prefetched_candidates: Some(candidates),
    };
    match fulfill_request(&deps.acquisition_deps(), &request, &options).await {
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

/// Review finding 3 (codex, MUSEM-06 REQUEST_CHANGES): a wanted item this
/// pass could not even attempt to search (no Prowlarr configured, or no
/// `quality_profile_id` on the monitored row) must still not silently
/// vanish -- persists a `Requested` `media_request` so the operator can see
/// and manually act on it, same "persist but never act" posture as every
/// other non-`AutoApprovable` outcome.
///
/// ## Follow-up finding (codex, twice): create-once, keyed off real existence
/// The FIRST version of this persisted a new request on EVERY pass a
/// no-capability item was considered (nothing here ever touched
/// `last_search_at`, so the cooldown guard never applied to it either) --
/// a deployment with no Prowlarr, or one monitored item missing a quality
/// profile, would spam a fresh `Requested` row every maintenance tick
/// forever. The SECOND version fixed that with a `last_search_at IS NULL`
/// ("first encounter") proxy -- which was itself wrong, because a FAILED
/// search (see the search-error branch above) also sets `last_search_at`
/// without ever creating a request, so that proxy could permanently
/// suppress a request a later, successful pass should have created. Fixed
/// for real by asking the actual question: does an open (non-terminal)
/// `media_requests` row already exist for this monitored item
/// (`repo::acquisition::has_open_worker_request_for_monitored_item`,
/// computed once by the caller and passed in as `has_open_request`)?
/// - ALWAYS touches `last_search_at` (best-effort, same as the searched
///   path) -- purely a cooldown timer now, so the cooldown guard at the
///   top of `process_wanted_item` skips this item on subsequent passes
///   within the cooldown window, same as any other processed item.
/// - Creates the `media_request` row ONLY when `has_open_request` is
///   `false`. Every later encounter still marks the item processed
///   (cooldown-gated) but never creates a second request while an earlier
///   one is still open.
///
/// Best-effort throughout: a failure to touch `last_search_at` or persist
/// the request is logged, never escalated (the item was already going to
/// be counted `NoCapability` either way).
async fn persist_pending_request_once(
    deps: &WantedPassDeps<'_>,
    item: &WantedItem,
    media_kind: &str,
    has_open_request: bool,
    reason: &str,
) {
    touch_last_search_best_effort(deps.pool, item.monitored_item_id).await;

    if has_open_request {
        tracing::debug!(
            monitored_item_id = item.monitored_item_id,
            reason,
            "MUSEM-06: wanted pass — no-capability item already has an open request from a prior \
             pass; not creating a second one"
        );
        return;
    }

    if let Err(e) = create_wanted_request(deps, item, media_kind, "Blocked").await {
        tracing::warn!(
            error = %e,
            monitored_item_id = item.monitored_item_id,
            reason,
            "MUSEM-06: wanted pass — could not persist a pending media_request for a no-capability item"
        );
    }
}

/// The one place a wanted-worker-originated `media_requests` row gets built
/// — shared by the no-capability fallback above and the main tier-
/// classified path, so every such row carries the exact same `note` shape
/// (`"MUSEM-06 wanted worker: monitored_item_id={id}"`) callers/tests can
/// key off of.
async fn create_wanted_request(
    deps: &WantedPassDeps<'_>,
    item: &WantedItem,
    media_kind: &str,
    tier_label: &str,
) -> MuseResult<MediaRequest> {
    let new_request = NewMediaRequest {
        provider_ids: serde_json::json!({}),
        media_kind: media_kind.to_string(),
        title: item.title.clone(),
        requested_by: Some("wanted-worker".to_string()),
        tier: Some(tier_label.to_string()),
        quality_profile_id: item.quality_profile_id,
        note: Some(format!("MUSEM-06 wanted worker: monitored_item_id={}", item.monitored_item_id)),
        // Review finding (codex, MUSEM-06 REQUEST_CHANGES): every worker-
        // created request (both the NeedsReview/Blocked persist path and
        // the AutoApprovable grab path -- this helper backs both, see its
        // call sites) correlates back to the monitored item it came from.
        // This is what `has_open_worker_request_for_monitored_item` keys
        // off of for the create-once guard.
        monitored_item_id: Some(item.monitored_item_id),
    };
    repo::acquisition::create_request(deps.pool, &new_request).await
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

    /// Follow-up finding (opus, MUSEM-06 REQUEST_CHANGES): the create-once
    /// guard on the no-capability path wasn't enough -- a capability-present
    /// item that keeps classifying `NeedsReview` (auto-tier disabled here)
    /// must ALSO only ever get ONE persisted `Requested` request, not a new
    /// one every time it's re-processed. Calls `process_wanted_item` TWICE
    /// directly with a near-zero cooldown (rather than `run_wanted_pass`
    /// twice back to back, which the ordinary cooldown guard would just
    /// short-circuit on its own -- see the no-capability twice-test for
    /// that version) so the second call genuinely re-reaches the
    /// `classify_tier` branch, proving THIS specific create-once guard does
    /// the work, not the cooldown.
    #[tokio::test]
    async fn needs_review_persists_a_requested_request_only_once_across_two_encounters() {
        let Some(pool) = test_pool_or_skip(
            "needs_review_persists_a_requested_request_only_once_across_two_encounters",
        )
        .await
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
                .body(search_response_body(
                    "musem06-needsreview-twice-guid",
                    &format!("{}.2020.1080p.WEB-DL", item.title),
                ));
        });
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        // arr_request_auto_tier_enabled stays false (Config::default()) --
        // every encounter classifies NeedsReview regardless of availability.
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

        // A cooldown of zero never blocks -- the second call below
        // genuinely reaches classify_tier again rather than being skipped
        // by the ordinary cooldown guard.
        let no_cooldown = chrono::Duration::zero();

        let (outcome1, searched1) = process_wanted_item(&deps, &settings, &item, no_cooldown).await;
        assert_eq!(outcome1, ItemOutcome::NeedsReview);
        assert!(searched1);

        let (outcome2, searched2) = process_wanted_item(&deps, &settings, &item, no_cooldown).await;
        assert_eq!(outcome2, ItemOutcome::NeedsReview);
        assert!(
            searched2,
            "the second encounter must still genuinely search (cooldown was bypassed on purpose, \
             so this proves the create-once guard -- not the cooldown -- prevents the duplicate)"
        );

        assert_eq!(download.added_count(), 0, "NeedsReview must never grab");

        let request_count: i64 = sqlx::query_scalar("SELECT count(*) FROM media_requests WHERE note = $1")
            .bind(format!(
                "MUSEM-06 wanted worker: monitored_item_id={}",
                item.monitored_item_id
            ))
            .fetch_one(&pool)
            .await
            .expect("count media_requests rows");
        assert_eq!(
            request_count, 1,
            "a second NeedsReview encounter must never create a second media_request"
        );
    }

    /// THE GAP (codex, MUSEM-06 REQUEST_CHANGES, second follow-up): the
    /// `last_search_at IS NULL` ("first encounter") create-once proxy was
    /// wrong because a FAILED search also sets `last_search_at` without
    /// ever creating a request. Sequence proven fixed here: pass 1's search
    /// fails (touches `last_search_at`, creates NO request) -> pass 2's
    /// search succeeds and classifies `NeedsReview` -> a request MUST still
    /// be created on pass 2, because `has_open_worker_request_for_monitored_item`
    /// correctly reports "no open request yet" (unlike the old
    /// `last_search_at.is_none()` proxy, which would have wrongly reported
    /// "already surfaced" and silently suppressed this request forever).
    #[tokio::test]
    async fn failed_search_then_successful_needs_review_still_persists_a_request_on_the_second_pass() {
        let Some(pool) = test_pool_or_skip(
            "failed_search_then_successful_needs_review_still_persists_a_request_on_the_second_pass",
        )
        .await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let item = seed_wanted_item(&pool, &library, Some(profile_id)).await;

        let config = test_config();
        assert!(!config.arr_request_auto_tier_enabled, "auto-tier stays off for both passes");
        let download = MockDownloadClient::new();
        let mut settings = ExperienceSettings::default();
        settings.acquisition.enabled = true;
        let no_cooldown = chrono::Duration::zero();

        // Pass 1: Prowlarr unreachable -- the search fails. Nothing listens
        // on this address, same pattern as prowlarr_unreachable_search_failed_item_skipped.
        let unreachable_prowlarr = ProwlarrClient::new("http://127.0.0.1:1", "test-key").expect("client");
        let deps1 = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&unreachable_prowlarr),
            download: Some(&download),
        };
        let (outcome1, searched1) = process_wanted_item(&deps1, &settings, &item, no_cooldown).await;
        assert_eq!(outcome1, ItemOutcome::SearchFailed);
        assert!(searched1);
        assert!(
            request_for_monitored_item(&pool, item.monitored_item_id).await.is_none(),
            "a failed search must never persist a request"
        );
        let monitored_after_pass1 = repo::acquisition::get_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("reload monitored_item after pass 1");
        assert!(
            monitored_after_pass1.last_search_at.is_some(),
            "even a failed search must still touch last_search_at (it's a cooldown timer, not \
             the create-once key anymore)"
        );

        // Pass 2: Prowlarr reachable now -- the search succeeds and
        // classifies NeedsReview (auto-tier is off). This is THE GAP: the
        // old last_search_at-based guard would have wrongly treated pass
        // 1's failed search as "already surfaced" and never created this
        // request.
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body(search_response_body(
                    "musem06-failed-then-review-guid",
                    &format!("{}.2020.1080p.WEB-DL", item.title),
                ));
        });
        let working_prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let deps2 = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&working_prowlarr),
            download: Some(&download),
        };
        let (outcome2, searched2) = process_wanted_item(&deps2, &settings, &item, no_cooldown).await;
        assert_eq!(outcome2, ItemOutcome::NeedsReview);
        assert!(searched2);
        assert_eq!(download.added_count(), 0);

        let request = request_for_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect(
                "THE GAP: a request must be created on the second pass even though pass 1's \
                 failed search already touched last_search_at",
            );
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

        // Review finding 3 (codex, MUSEM-06 REQUEST_CHANGES): a no-capability
        // item must still persist a `Requested` request for the operator,
        // never silently vanish.
        let request = request_for_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("a media_request row must have been persisted even with no capability");
        assert_eq!(request.status, RequestStatus::Requested.as_str());

        // Follow-up finding (codex): the no-capability path must also mark
        // the item processed (same as the searched path), so the
        // create-once guard and the cooldown guard both actually apply on
        // a later pass.
        let monitored = repo::acquisition::get_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("reload monitored_item");
        assert!(
            monitored.last_search_at.is_some(),
            "a no-capability item must still have last_search_at set after being processed"
        );
    }

    #[tokio::test]
    async fn no_prowlarr_configured_is_no_capability_and_persists_a_requested_request() {
        let Some(pool) =
            test_pool_or_skip("no_prowlarr_configured_is_no_capability_and_persists_a_requested_request").await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        let profile_id = seed_profile_allowing_web_1080p(&pool).await;
        let item = seed_wanted_item(&pool, &library, Some(profile_id)).await;

        let config = test_config();
        let download = MockDownloadClient::new();
        let mut settings = ExperienceSettings::default();
        settings.acquisition.enabled = true;
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: None,
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

        let request = request_for_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("a media_request row must have been persisted even with no Prowlarr configured");
        assert_eq!(request.status, RequestStatus::Requested.as_str());

        let monitored = repo::acquisition::get_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("reload monitored_item");
        assert!(
            monitored.last_search_at.is_some(),
            "a no-capability item must still have last_search_at set after being processed"
        );
    }

    /// Follow-up finding (codex): the no-capability persist must be
    /// create-ONCE, not repeated every pass -- otherwise a deployment with
    /// no Prowlarr (or one monitored item missing a quality profile) spams
    /// a fresh `Requested` row every maintenance tick forever. Runs
    /// `run_wanted_pass` TWICE against a no-quality-profile item and
    /// asserts exactly one request exists after both passes, plus
    /// `last_search_at` got set (proving the second pass was skipped via
    /// the ordinary cooldown guard, not by re-hitting the no-capability
    /// branch a second time).
    #[tokio::test]
    async fn no_capability_persists_a_requested_request_only_once_across_two_passes() {
        let Some(pool) = test_pool_or_skip(
            "no_capability_persists_a_requested_request_only_once_across_two_passes",
        )
        .await
        else {
            return;
        };
        save_settings(&pool, true).await;
        let library = seed_library(&pool).await;
        // No quality profile -- a no-capability item on every pass.
        let item = seed_wanted_item(&pool, &library, None).await;

        let config = test_config();
        let download = MockDownloadClient::new();
        let server = MockServer::start(); // deliberately no mock registered -- never reached
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        run_wanted_pass(&deps).await;

        let request_count_after_pass1: i64 =
            sqlx::query_scalar("SELECT count(*) FROM media_requests WHERE note = $1")
                .bind(format!(
                    "MUSEM-06 wanted worker: monitored_item_id={}",
                    item.monitored_item_id
                ))
                .fetch_one(&pool)
                .await
                .expect("count media_requests rows after pass 1");
        assert_eq!(request_count_after_pass1, 1, "the first pass must create exactly one request");

        let monitored_after_pass1 = repo::acquisition::get_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("reload monitored_item after pass 1");
        assert!(
            monitored_after_pass1.last_search_at.is_some(),
            "the no-capability path must mark the item processed after pass 1"
        );

        // Pass 2: within the (default) cooldown window -- the ordinary
        // cooldown guard should skip this item entirely now that
        // last_search_at is set, so no second request gets created.
        run_wanted_pass(&deps).await;

        let request_count_after_pass2: i64 =
            sqlx::query_scalar("SELECT count(*) FROM media_requests WHERE note = $1")
                .bind(format!(
                    "MUSEM-06 wanted worker: monitored_item_id={}",
                    item.monitored_item_id
                ))
                .fetch_one(&pool)
                .await
                .expect("count media_requests rows after pass 2");
        assert_eq!(
            request_count_after_pass2, 1,
            "a second pass must never create a second request for the same no-capability item"
        );

        let monitored_after_pass2 = repo::acquisition::get_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("reload monitored_item after pass 2");
        assert_eq!(
            monitored_after_pass2.last_search_at, monitored_after_pass1.last_search_at,
            "pass 2 must be skipped by the ordinary cooldown guard before reaching the \
             no-capability branch again"
        );
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

    /// Review finding 1 (opus, MUSEM-06 REQUEST_CHANGES) — the serious one:
    /// a worker-originated grab's `download_queue` row must carry
    /// `monitored_item_id` (via `FulfillOptions`, threaded through
    /// `fulfill_request`/`grab_and_persist`), not just `request_id` — proven
    /// end to end here by running `run_wanted_pass` TWICE against the same
    /// seeded item and asserting the second pass writes no second queue row,
    /// no second request, and never even re-searches (touches
    /// `last_search_at`). Every assertion below is scoped to this test's own
    /// `monitored_item_id`/exact `note` text, so it stays correct regardless
    /// of whatever else is wanted in the shared test database concurrently.
    #[tokio::test]
    async fn second_pass_skips_an_item_whose_first_pass_grab_is_still_queued() {
        let Some(pool) =
            test_pool_or_skip("second_pass_skips_an_item_whose_first_pass_grab_is_still_queued").await
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
                .body(search_response_body(
                    "musem06-idempotent-guid",
                    &format!("{}.2020.1080p.WEB-DL", item.title),
                ));
        });
        let prowlarr = ProwlarrClient::new(server.base_url(), "test-key").expect("client");
        let config = Config {
            arr_request_auto_tier_enabled: true,
            ..test_config()
        };
        let download = MockDownloadClient::new();
        let deps = WantedPassDeps {
            pool: &pool,
            config: &config,
            prowlarr: Some(&prowlarr),
            download: Some(&download),
        };

        // Pass 1: exercises the full pass (not just process_wanted_item
        // directly) so the grab really goes through fulfill_request's real
        // enqueue path.
        run_wanted_pass(&deps).await;

        let queue_count_after_pass1: i64 =
            sqlx::query_scalar("SELECT count(*) FROM download_queue WHERE monitored_item_id = $1")
                .bind(item.monitored_item_id)
                .fetch_one(&pool)
                .await
                .expect("count download_queue rows after pass 1");
        assert_eq!(
            queue_count_after_pass1, 1,
            "the worker's grab must write a download_queue row with monitored_item_id set -- if \
             this is 0, the row was written with only request_id set (the original bug) and this \
             query can never find it"
        );

        let monitored_after_pass1 = repo::acquisition::get_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("reload monitored_item after pass 1");
        assert!(
            monitored_after_pass1.last_search_at.is_some(),
            "pass 1 must have actually searched (and thus touched last_search_at)"
        );

        // Pass 2: the already-queued idempotency check must short-circuit
        // this item before anything else -- no second search, no second
        // grab, no duplicate rows.
        run_wanted_pass(&deps).await;

        let queue_count_after_pass2: i64 =
            sqlx::query_scalar("SELECT count(*) FROM download_queue WHERE monitored_item_id = $1")
                .bind(item.monitored_item_id)
                .fetch_one(&pool)
                .await
                .expect("count download_queue rows after pass 2");
        assert_eq!(
            queue_count_after_pass2, 1,
            "a second pass must never write a second download_queue row for an item that's \
             already active in the queue"
        );

        let request_count: i64 = sqlx::query_scalar("SELECT count(*) FROM media_requests WHERE note = $1")
            .bind(format!(
                "MUSEM-06 wanted worker: monitored_item_id={}",
                item.monitored_item_id
            ))
            .fetch_one(&pool)
            .await
            .expect("count media_requests rows for this monitored item");
        assert_eq!(
            request_count, 1,
            "a second pass must never persist a second media_request for an already-queued item"
        );

        let monitored_after_pass2 = repo::acquisition::get_monitored_item(&pool, item.monitored_item_id)
            .await
            .expect("reload monitored_item after pass 2");
        assert_eq!(
            monitored_after_pass2.last_search_at, monitored_after_pass1.last_search_at,
            "the already-queued short-circuit happens BEFORE the cooldown/search logic, so a \
             second pass must never touch last_search_at for an already-queued item"
        );
    }
}
