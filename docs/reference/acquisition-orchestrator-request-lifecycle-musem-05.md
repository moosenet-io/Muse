## Acquisition orchestrator + request lifecycle (MUSEM-05)

`src/acquisition/` is the first thing in this crate that assembles MUSEM-01..04 into a real,
callable path: `acquisition::fulfill_request(deps, request) -> FulfillOutcome` runs, for one
`media_requests` row, a genuine on-demand Prowlarr search (MUSEM-03, narrowed to the request's
`media_kind`'s configured categories) → `decision::decide_release` (MUSEM-04) against the
request's `quality_profile_id` → on `Decision::Grab`, `DownloadClient::add` (MUSEM-02) → persists
a `download_queue` row (MUSEM-01, status `Queued`, `client_hash` from the grab receipt) and a
`Grabbed` `history_events` row; on `Decision::Reject` (or a download-client error), marks the
request `Failed` and records a `DownloadFailed` history row — never a phantom queue row, never a
panic. A request with no `quality_profile_id`, or no Prowlarr configured, degrades to
`FulfillOutcome::Skipped` and is left `Requested` for manual follow-up, rather than erroring.

`acquisition::AcquisitionSink` is the FIRST real (non-`Noop`) implementation of
`arr::request::MediaRequestSink` this crate ships: `submit(draft)` creates the `media_requests` row
for the draft and immediately `fulfill_request`s it. `arr::request::submit_if_appropriate`
guarantees `submit` is only ever reached for `RequestTier::AutoApprovable` — see
`src/acquisition/mod.rs`'s module doc for how `POST /requests` (below) computes a REAL
availability signal (from the on-demand search itself, not a fabrication) so that tier is
actually reachable now, closing the gap `conversational::handle_conversational_request`'s own doc
flagged as "never `AutoApprovable` in practice" and named as the natural follow-up.

`decision::`/`prowlarr::search_releases`/`download::qbit::QbitClient` have no dependency on each
other beyond what `acquisition::fulfill_request` wires — each still merges/tests independently.

