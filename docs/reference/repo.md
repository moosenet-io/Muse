# repo

The sqlx query layer over the MUSE-02 arr-shaped core schema and everything layered on
top of it — the **only place raw SQL lives** in the crate (279 KG nodes, the largest
subsystem). One module per table group, each exposing plain async functions over a
`PgPool`. All queries use **runtime** sqlx (`sqlx::query`/`sqlx::query_as`), never the
compile-time `query!` macros, because the crate must build without a live database (a
MUSE-02 build constraint).

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `repo::play_event::insert` | fn | `src/repo/play_event.rs` | Appends one raw playback event; the highest-ranked repo symbol in the KG — both tracker ingest paths (webhook + poller) funnel through it |
| `repo::library::create` | fn | `src/repo/library.rs` | Creates a `libraries` row |
| `repo::account::create` | fn | `src/repo/account.rs` | Creates an `accounts` row |
| `repo::media_metadata::upsert_by_tmdb` | fn | `src/repo/media_metadata.rs` | Title-level upsert keyed on TMDb id — arr ingest, trending, and metadata resolution all converge here |
| `repo::media_item::upsert` | fn | `src/repo/media_item.rs` | Library-item upsert (a title as owned in a specific library) |
| `repo::settings::save` / `repo::settings::load` | fn | `src/repo/settings.rs` | Persist/load the `ExperienceSettings` control-panel row (load returns defaults when no row exists) |
| `repo::media_metadata::get` | fn | `src/repo/media_metadata.rs` | Point lookup by id |
| `repo::play_event::list_tautulli_snapshot_events` | fn | `src/repo/play_event.rs` | Reads Tautulli-origin history rows for the parity report CLI |

## Module inventory

One module per table group (`src/repo/mod.rs`): `account`, `acquisition`
(media_requests + monitored_items), `artwork_cache`, `availability`, `channel`,
`embedding`, `episode`, `external_enrichment`, `friend_opt_in`, `indexer`,
`interstitial`, `library`, `media_file`, `media_item`, `media_metadata`, `persona`,
`play_event`, `play_session`, `premiere_discussion`, `proactive_item`, `quality`,
`release`, `season`, `settings`, and more — mirroring the 47 migrations in
`migrations/`.

## How it connects

Everything above the foundation layer calls into `repo`: the tracker persists
`play_events`/`play_sessions` through it, arr ingest and the library scanner upsert the
core schema through it, the Prowlarr worker upserts `indexers`/`releases`/`availability`,
the embed pipeline reads/writes `embeddings`, taste/persona/curation read the derived
taste tables, and the acquisition orchestrator records request lifecycles in
`repo::acquisition`. `repo` itself depends only on `models` (row types), `error`, and
the pool. No module outside `repo` writes SQL.

## Configuration

None directly — `repo` receives an already-built `PgPool`. The pool itself is built from
`MUSE_DATABASE_URL` (lazily, in `src/db.rs`); live-DB tests gate on
`MUSE_TEST_DATABASE_URL`.

## Notes and gaps

- The per-table modules are deliberately thin; business logic (scoring, folding,
  classification) lives in the calling subsystems, keeping this layer mechanical and
  round-trip-testable.
- This page does not enumerate every function of every table module — see the schema
  overview in [docs/schema.md](../schema.md) for the table-by-table data model.
