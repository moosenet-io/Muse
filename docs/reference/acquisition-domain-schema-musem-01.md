## Acquisition domain schema (MUSEM-01)

Muse today is a *read-only* observer of the operator's Radarr/Sonarr/Prowlarr fleet (`arr/`,
`prowlarr/`). `migrations/0104_acquisition_domain.sql` lays the **schema + repository foundation**
(`src/models/acquisition.rs`, `src/repo/acquisition.rs`) for a native write-path — monitoring
("wanted"), requests, the download queue, typed history, and a blocklist — mirroring the
Radarr/Sonarr data model (`quality` is a compound `{quality, revision}` value, custom formats are a
named scored-matcher registry, history is typed `jsonb`, provider IDs are a keyed map). **This item
is schema/repo only: no workers, no HTTP endpoints, nothing wired into a running deployment yet**
(see "Wiring status" below and the later MUSEM items for the write path itself).

Tables added:

| Table | Purpose |
|---|---|
| `monitored_items` | The "wanted" driver — monitoring a title within a `library`, decoupled from whether a `media_items`/file exists yet |
| `media_requests` | <media-service>-style request lifecycle (`requested → approved/denied → searching → grabbed → available`) |
| `download_queue` | One row per in-flight/terminal grab; requires at least one of `request_id`/`monitored_item_id` (`download_queue_has_source` CHECK) |
| `history_events` | Typed history (`event_type`-keyed `jsonb` payload), correlated to a download via `download_id` |
| `blocklist` | Releases/hashes a future decision engine must never re-grab |

The pre-existing quality-domain tables (`quality_definitions`, `quality_profiles`, `custom_formats`,
`quality_profile_formats` — added in MUSE-02, `src/models/quality.rs` / `src/repo/quality.rs`) are
reused by FK, **not redefined** — see the migration's header comment for why. `media_requests.kind`
and the `status`/`event_type` columns are plain `text` (not a Postgres enum type) so a future
`'music'` kind or a new status value never needs an `ALTER TYPE`; `src/models/acquisition.rs`
provides the validated Rust-side enums (`RequestStatus`, `QueueStatus`, `HistoryEventType`) with
`as_str()`/`FromStr` conversions.

The hot "wanted" scan is `repo::acquisition::list_wanted(pool, library_id)`: everything monitored
in a library that either has no file yet, or whose best on-disk file quality is strictly below its
quality profile's cutoff (compared via `quality_definitions.sort_order`, never raw quality-tier
ids, which are historical/non-contiguous per the blueprint).

