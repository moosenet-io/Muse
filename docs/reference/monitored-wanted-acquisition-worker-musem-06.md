## Monitored "wanted" acquisition worker (MUSEM-06)

`src/acquisition/worker.rs` is the Sonarr/Radarr-style background engine: `run_wanted_pass`
scans `repo::acquisition::list_wanted` (MUSEM-01) across every library and, for each item not
searched within `Config::wanted_search_cooldown_secs` (default 6h) of its
`monitored_items.last_search_at`, runs a real on-demand Prowlarr search (MUSEM-03) and turns the
result into an `Availability` signal for `arr::request::classify_tier` — reused verbatim, never a
second tiering rule. Only an `AutoApprovable` item (the operator opted in via
`Config::arr_request_auto_tier_enabled` AND the search actually confirmed it's grabbable now)
reaches `acquisition::fulfill_request` (MUSEM-05) at all; every other item still gets a
`media_requests` row persisted at `RequestStatus::Requested` for manual/operator follow-up —
never a silent grab, including when there's no capability to even search (no Prowlarr configured,
or no `quality_profile_id` on the monitored row).

**Exactly one real Prowlarr search per wanted item.** `fulfill_request` normally runs its own
on-demand search; the worker instead hands it the candidates it already fetched via a new
`FulfillOptions::prefetched_candidates` field, so an `AutoApprovable` item is never searched
twice. This is what makes `Config::wanted_max_searches_per_pass` (default 20) an honest bound on
actual Prowlarr calls, not just on how many items get *considered* — on top of the shared
`ProwlarrClient` `RateLimiter`'s own hourly search budget. `Config::wanted_max_grabs_per_pass`
(default 5) separately caps grabs.

**Idempotent across passes.** A worker-originated grab's `download_queue` row carries BOTH
`request_id` and `monitored_item_id` (`FulfillOptions::monitored_item_id` →
`DownloadSource::Both`, threaded through `fulfill_request`/`grab_and_persist`) — not just
`request_id` the way `POST /requests`' grabs do — so `repo::acquisition::
is_monitored_item_active_in_queue`'s `monitored_item_id`-keyed check actually finds it, and an
item already `queued`/`downloading` is skipped on every subsequent pass, never re-searched or
re-grabbed.

**Create-once pending requests, keyed off real existence, not a timestamp proxy.** Every
worker-created `media_requests` row also carries `monitored_item_id` (migration
`0105_media_requests_monitored_item.sql`, `NULLABLE` — `POST /requests`/`approve`/
`AcquisitionSink` still pass `NULL`, no monitored item involved). Before persisting a `Requested`
row for a non-grabbed outcome (no-capability, `NeedsReview`, `Blocked`), the worker checks
`repo::acquisition::has_open_worker_request_for_monitored_item` — an existing non-terminal
(`requested`/`approved`/`searching`/`grabbed`) request for that monitored item — and only creates
a new one when none exists. An earlier version keyed this off `monitored_items.last_search_at IS
NULL` ("first encounter"); that was wrong, because a FAILED search also sets `last_search_at`
without creating a request, so a pass-1 search failure could permanently suppress the request a
later, successful pass should have created. `last_search_at` is now purely a cooldown timer.

The `AutoApprovable` branch applies the same "at most one worker-created request per monitored
item" invariant, but by REUSE rather than refusal: `repo::acquisition::
get_open_worker_request_for_monitored_item` fetches any existing open request for the item, and
if one exists (e.g. a prior pass left it `Requested` because auto-tier was off, or availability
wasn't confirmed grabbable yet), `fulfill_request` is called against THAT row — transitioning it to
`Grabbed`/`Failed` in place — instead of creating a second request. A fresh request is only created
when none exists yet. This branch's own protection against a double GRAB remains the
`download_queue.monitored_item_id` check; the reuse-vs-create logic here is purely about never
leaving a stale duplicate `Requested` row behind when an item's classification improves.

Non-blocking: an unreachable Prowlarr/qBittorrent, a DB hiccup, or a metadata row deleted mid-pass
is logged and the pass moves on to the next item — never a panic, never an aborted pass.

`run_wanted_pass` is itself gated on `ExperienceSettings.acquisition.enabled` (the same master
gate `fulfill_request` enforces unbypassably as its own first action) — checked again here purely
so a gate-off deployment short-circuits to a cheap no-op without even listing libraries.

**Scheduled inside the maintenance chain** (`crate::maintenance::run_maintenance_pass`,
`src/maintenance/mod.rs`), right after the arr-ingest step and before embed/taste/divergence/
enrichment — dependency order, since arr ingest is what refreshes the `media_items`/`media_files`
rows `list_wanted`'s cutoff comparison reads. `MaintenanceSummary.wanted` carries the pass's own
tally (`grabbed`/`needs_review`/`already_queued_skipped`/`cooldown_skipped`/`no_capability_skipped`/
`metadata_missing_skipped`/`search_failed`/`errors`/`grab_cap_skipped`/`search_cap_skipped`), same
"ran and did nothing" vs "not run" posture every other maintenance-pass step already follows.

