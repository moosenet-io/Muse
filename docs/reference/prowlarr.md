# prowlarr

The read-only Prowlarr availability client (109 KG nodes, MUSE-16/17 + MUSEM-03).
Mirrors the *arr report-pull mechanism so Muse knows what's actually grabbable *now* —
not just what exists in a catalog. This module makes no writes to Prowlarr and never
grabs anything; persistence goes through `repo::indexer`/`repo::release`/
`repo::availability`.

## Key types and functions

| Symbol | Kind | File | What it does |
|---|---|---|---|
| `prowlarr::client::ProwlarrClient` | struct | `src/prowlarr/client.rs` | Typed, read-only Prowlarr v1 API client: indexer listing, RSS-mode report-pull, bounded targeted search |
| `prowlarr::client::ProwlarrClient::targeted_search` | fn | `src/prowlarr/client.rs` | On-demand "search this specific title now" (prefers TMDb id over free text; enforces the hourly cap) |
| `prowlarr::rate_limit::RateLimiter` | struct | `src/prowlarr/rate_limit.rs` | The polite-interval + hourly-cap guard every request funnels through — one shared instance per client, so report-pull and on-demand search share the same budget |
| `prowlarr::parse::parse_release_name` | fn | `src/prowlarr/parse.rs` | Deterministic release-name parser v0 populating `releases.parsed_*` with a confidence score |
| `prowlarr::parse::parse_season_episode_token` | fn | `src/prowlarr/parse.rs` | S/E token extraction inside the parser |
| `prowlarr::scheduler::is_due` | fn | `src/prowlarr/scheduler.rs` | DB-backed per-indexer scheduling decision (each indexer has its own `polite_min_interval_secs`) |
| `prowlarr::worker::run_tick` / `spawn_report_pull_worker` | fn | `src/prowlarr/worker.rs` | The scheduled report-pull worker: pull → parse → upsert → availability rollup → prune, per due indexer |
| `prowlarr::search::search_releases` | fn | `src/prowlarr/search.rs` | MUSEM-03 entry point feeding candidate `SearchRelease`s to the release-decision engine; persists nothing itself |

## How it connects

`main` constructs the client from config; `workers::spawn_workers` spawns the
report-pull worker **only when Prowlarr is configured**. The worker writes through
`repo`; the availability rollup feeds `curation` and the `grab_window` proactive
generator. `search_releases` is called by `acquisition::fulfill_request` and the
monitored-wanted worker, whose candidates then flow into `decision::decide_release`.
Release names below the resolve-confidence threshold are still stored
(negative-space discovery) but left unresolved rather than risking a wrong match.

## Configuration

- `PROWLARR_URL`, `PROWLARR_API_KEY` — enable the client.
- `MUSE_PROWLARR_TICK_INTERVAL_SECS` — worker wake cadence (default 60s; the per-indexer
  polite interval is the real etiquette gate).
- `MUSE_PROWLARR_MOVIE_CATEGORIES` / `MUSE_PROWLARR_TV_CATEGORIES` — Newznab parent
  category ids (defaults: 2000s / 5000s).
- `MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE` — minimum parse confidence before resolving a
  release to a `media_metadata` title (default 0.5).
- `MUSE_RELEASE_EXPIRY_DAYS` — rolling snapshot retention before prune (default 21).
- `MUSE_PROWLARR_SEARCH_MAX_PER_HOUR` — hourly cap on on-demand targeted searches
  (default 30), shared with report-pull through the single `RateLimiter`.

## Notes and gaps

- Tracker etiquette is layered: per-indexer polite intervals, the shared hourly cap, and
  (for the wanted worker) additional per-pass search/grab caps on top.
- The parser is deliberately v0-deterministic — no ML, exhaustively unit-tested; its
  confidence score is what gates title resolution.
- Not covered here: how a found release becomes a grab — see the
  [release-decision engine](release-decision-engine.md) and
  [acquisition orchestrator](acquisition-orchestrator-request-lifecycle-musem-05.md)
  pages.
