# arr

The multi-instance Radarr/Sonarr (*arr API v3) ingest client (84 KG nodes, MUSE-05). The
operator's fleet is **8 *arr instances** sharded by root folder/purpose — 5 Radarr
(`radarr`, `radarr_foreign`, `radarr_anime`, `radarr_uhd`, `radarr_animated`) + 3 Sonarr
(`sonarr`, `sonarr_anime`, `sonarr_animated`). This module is a *pure, read-only* HTTP
client plus an ingest routine that maps *arr responses onto the MUSE-02 core schema via
the `repo` layer — for N configured instances instead of one server.

Two hard rules from the founding spec (§1), stated in the module doc and structurally
held:

1. **Never write to *arr.** Phase 0 is acquisition-read-only.
2. **Never hardcode instance URLs/keys.** The fleet is described by
   `config::ArrInstanceConfig`, loaded from `MUSE_ARR_INSTANCES` (JSON) at runtime.

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `arr::client::ArrClient` | struct | `src/arr/client.rs` | Typed, read-only *arr v3 API client (one per instance) |
| `arr::client::ArrClient::movies` | fn | `src/arr/client.rs` | Fetches a Radarr instance's movie list |
| `arr::config::load_arr_instances` | fn | `src/arr/config.rs` | Parses `MUSE_ARR_INSTANCES` JSON into the instance fleet; malformed input degrades to zero instances, never blocks startup |
| `arr::ingest::run` | fn | `src/arr/ingest.rs` | The ingest pass: for each instance, pull → map onto `libraries`/`media_metadata`/`media_items`/`seasons`/`episodes`/`media_files`; an unreachable instance is logged and skipped (`IngestSummary`), never aborting the rest of the fleet |
| `arr::request::classify_tier` | fn | `src/arr/request.rs` | MUSEX-14: pure tiered-safety classification of a "please get this" ask → `RequestTier` (`AutoApprovable`/`NeedsReview`/`Blocked`) |
| `arr::request::MockMediaRequestSink` | struct | `src/arr/request.rs` | Test double for the `MediaRequestSink` seam |

## How it connects

`main` parses the fleet at boot (`Config::arr_instances`) into `AppState`;
`maintenance`'s scheduled pass calls `ingest::run` when the fleet is non-empty.
`classify_tier` is the safety gate the acquisition orchestrator
(`acquisition::fulfill_request`) runs before any grab, and `premiere::engagement`'s
budgets modulate — never bypass — it. The `MediaRequestSink` trait seam is implemented
for real by `acquisition::AcquisitionSink`; this module itself ships no live
*arr-writing implementation.

## Configuration

- `MUSE_ARR_INSTANCES` — JSON array describing the instance fleet (name, kind, URL, key).
- `RADARR_URL`/`RADARR_API_KEY`, `SONARR_URL`/`SONARR_API_KEY` — single-instance
  convenience keys read by `Config`.
- `MUSE_ARR_REQUEST_AUTO_TIER_ENABLED` — whether `classify_tier` may ever return
  `AutoApprovable` (default `false`: every missing-title request needs manual review
  until an operator opts in).

## Notes and gaps

- One instance (`radarr_animated`) is currently offline in the operator's fleet;
  `ingest::run` is built to degrade around exactly that.
- The auto-tier flag changes only classification, never whether this module gains a live
  write call — the read-only rule holds regardless of its value.
- Not covered here: the request lifecycle after classification — see the
  [acquisition orchestrator page](acquisition-orchestrator-request-lifecycle-musem-05.md).
