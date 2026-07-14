# Muse

**Muse is an AI-native media curation & taste companion** — a private, local-inference-first
"brain" for a Plex library. It owns taste modeling, curation, metadata, availability intelligence,
proactive recommendations, and a pseudo-TV channel director, backed by a **mandatory Postgres +
pgvector** database, while Plex stays the playback surface and qBittorrent stays the acquisition
tool.

Muse is built **strangler-fig**: it keeps qBittorrent (acquisition) and Plex (consumption) and
owns the *brain* (taste, curation, metadata, release selection, organization). Each phase ships
independent value and *arr/Tautulli are retired one function at a time — import (the only
high-blast-radius part) is dead last. Everything shipped so far is **Phase 0 + Phase 0.5:
read-only, benign playback only, zero blast radius against the live stack**.

It is a peer to the rest of the constellation — **Harmony** (build orchestrator), **Chord**
(inference proxy/orchestrator), **Terminus** (tool hub), and **Lumina** (assistant). Terminus's
`media_*` tools re-point at Muse as phases land, and Lumina consumes Muse's proactive-content
outbox.

See [`specs/S96-muse-foundation.md`](specs/S96-muse-foundation.md) for the founding spec, and the
[`docs/`](docs/) set for grounded reference material:

- [`docs/architecture.md`](docs/architecture.md) — constellation position, data flow, module layering
- [`docs/schema.md`](docs/schema.md) — the Postgres data model, grouped by concern, with the shipped divergences from the spec
- [`docs/runbooks.md`](docs/runbooks.md) — operational runbooks (Tautulli replacement, Prowlarr etiquette, adding Muse as a Plex tuner, the taste/embedding pipeline, the proactive→Lumina contract)
- [`docs/behavior-spec.md`](docs/behavior-spec.md) — the behavioral contract (taste derivation, curation ranking, proactive triggers/cooldowns, the pseudo-TV director, degradation invariants)

> **Accuracy note.** This documentation is written against the shipped code, not the aspirational
> spec. Where a subsystem is a wired-but-untriggered seam or diverges from the founding spec, it is
> marked as such. In particular, three write-path recompute functions (embeddings, taste profile,
> taste-divergence radar) are fully implemented and tested but have **no scheduled worker or route
> calling them yet** — see [Wiring status](#wiring-status-what-actually-runs) below and the docs.

## Architecture at a glance

Muse is a single `axum` HTTP service (`muse` binary) plus a handful of background workers, all
over `sqlx`→Postgres. It is local-inference-first: embeddings run on a local `nomic-embed-text`
via Ollama, and curation/taste reasoning routes through Chord to a local model on <host>. Secrets are
materialized into the process environment from <secret-manager> at runtime (the fleet `.env`-from-vault
pattern) — the code only ever reads `std::env::var`, never authors a secret literal.

Every external dependency is **optional and graceful**: an unconfigured or down Plex / Prowlarr /
TMDb / Ollama / Chord / SearXNG / news endpoint degrades that one feature (a skipped tier, an
empty result, a `None` client) — it never blocks startup and never turns into a `500`.

Subsystem map (source module → concern):

| Module | Concern |
|---|---|
| `arr/` | Radarr/Sonarr multi-instance library ingest (metadata + files) |
| `plex/` | Read-only Plex API client (libraries, sessions, ratings, watchlist, artwork) |
| `tracker/` | **Native Tautulli-replacement**: webhook receiver + session poller + reconstruction |
| `tautulli/` | One-time Tautulli history backfill importer |
| `prowlarr/` | Availability report-pull worker (indexer sync, RSS pull, release parse, rollup) |
| `trending/` | TMDb trending/population feed + streaming-availability |
| `embed/` | Local embedding pipeline (`nomic-embed-text` → pgvector) + cosine recall primitive |
| `recall/` | Vector-recall search API (`/query/resolve`, `/query/similar`) |
| `taste_model/` | Behavioral taste model (signals → profile + context centroids + LLM notes) |
| `radar/` | You-vs-masses `taste_divergence` computation |
| `curation/` | Recommend engine (`/recommend`, `/recommend/on_deck`, `/recommend/gaps`) |
| `proactive/` | Five proactive-content generators → `proactive_items` → Lumina |
| `enrichment/` | External enrichment cache (forum sentiment, "does it get good", renewal/trailer news) |
| `channels/` | On-demand pseudo-TV lineup composer + presets |
| `tuner/` | HDHomeRun-emulation linear tuner (discover/lineup/M3U/XMLTV) + rolling-grid scheduler |
| `streaming/` | ffmpeg channel streaming engine (`/auto/v{id}`) |
| `web/` | Channel-guide web page + JSON API + artwork proxy |
| `plex_control/` | Plex Companion cast/play-queue client (library-only, unwired — see below) |

## Running

Build and tests run on the fleet build host (<host> / a glibc-matched host), not the dev box — see
the project's `moosenet-spec` build pipeline. The service needs **PostgreSQL 17 with the `vector`
(pgvector) and `pg_trgm` extensions** on a `muse` database. sqlx queries are checked at runtime
(no offline query cache), so migrations must apply before first use; `db::migrate` runs them
best-effort at startup.

```
cargo run
```

`GET /health` returns `{status, version, db}` and **never 500s** even when the database is down (a
2-second probe reports `db:"up"|"down"`).

### Configuration (environment variables)

All configuration is read from the environment (materialized from <secret-manager> at runtime — **never
hardcode secrets**). Every field below is read by `Config::from_env` (`src/config.rs`). Examples use
RFC 5737 documentation IPs (`192.0.2.x`) and placeholder hostnames — never real infra.

| Variable | Default | Purpose |
|---|---|---|
| `MUSE_DATABASE_URL` | *(none)* | Postgres connection string for the pgvector-enabled `muse` DB. Pool connects lazily; service starts even if the DB is unreachable. |
| `MUSE_BIND_ADDR` | `0.0.0.0:8090` | HTTP bind address. |
| `MUSE_LOG_LEVEL` | `info` | `tracing`/`EnvFilter` level. |
| `PLEX_URL` | *(none)* | Plex Media Server base URL (read-only). e.g. `http://192.0.2.10:32400`. |
| `PLEX_TOKEN` | *(none)* | Plex API token (read-only). Used server-side only; never exposed to the browser. |
| `MUSE_PLEX_POLL_SECS` | *(none → 10)* | Session-poller cadence in seconds. Unset/unparseable falls back to the poller's own 10s default. |
| `TAUTULLI_URL` | *(none)* | Tautulli base URL for the one-time history backfill. |
| `TAUTULLI_API_KEY` | *(none)* | Tautulli API key. |
| `RADARR_URL` | *(none)* | Single-instance Radarr base URL (library ingest). |
| `RADARR_API_KEY` | *(none)* | Radarr API key. |
| `SONARR_URL` | *(none)* | Single-instance Sonarr base URL (library ingest). |
| `SONARR_API_KEY` | *(none)* | Sonarr API key. |
| `PROWLARR_URL` | *(none)* | Prowlarr base URL (availability report-pull). |
| `PROWLARR_API_KEY` | *(none)* | Prowlarr API key. When both this and `PROWLARR_URL` are set, the report-pull worker is spawned. |
| `TMDB_API_KEY` | *(none)* | TMDb API key (metadata, trending feed, `/query/resolve` beyond-the-library tier). |
| `MUSE_OLLAMA_URL` | *(none)* | Ollama base URL serving `nomic-embed-text` for local embeddings. |
| `CHORD_URL` | *(none)* | Chord OpenAI-compatible base URL for routed local-model reasoning (rationale, taste notes, channel composition). |
| `MUSE_SEARXNG_URL` | *(none)* | Fleet SearXNG base URL for forum/critic sentiment + "does it get good" enrichment. |
| `MUSE_NEWS_URL` | *(none)* | News-search endpoint base URL for renewal/trailer enrichment. |
| `MUSE_NEWS_API_KEY` | *(none)* | Optional bearer key for `MUSE_NEWS_URL` (many self-hosted aggregators need none). |
| `MUSE_ARR_INSTANCES` | *(none)* | JSON array describing the multi-instance *arr fleet (see below). Malformed JSON degrades ingest to zero instances, never fatal. |
| `MUSE_PROWLARR_TICK_INTERVAL_SECS` | `60` | How often the report-pull worker checks which indexers are due. |
| `MUSE_PROWLARR_MOVIE_CATEGORIES` | `2000` | Comma-separated Newznab parent category ids treated as movies. |
| `MUSE_PROWLARR_TV_CATEGORIES` | `5000` | Comma-separated Newznab parent category ids treated as TV. |
| `MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE` | `0.5` | Minimum release-name parse confidence before resolving a release to a title. |
| `MUSE_RELEASE_EXPIRY_DAYS` | `21` | How long a rolling `releases` snapshot row lives before it's pruned. |
| `MUSE_PUBLIC_URL` | *(none)* | LAN-reachable base URL advertised by the tuner (`/discover.json`, `/muse.m3u` stream URLs). Degrades to `http://{bind_addr}` (only correct when `bind_addr` is a real LAN address). |
| `MUSE_HDHR_DEVICE_ID` | `MUSE0001` | HDHomeRun-emulation device id advertised in `/discover.json`. |
| `MUSE_CHANNEL_GUIDE_WINDOW_HOURS` | `48` | Rolling linear-guide window the director keeps `channel_programs` filled to; also the XMLTV render window. |
| `MUSE_CHANNEL_SCHEDULER_TICK_SECS` | `900` | How often the linear-channel scheduler tops off the guide window. |
| `MUSE_RECALL_VECTOR_MAX_DISTANCE` | `0.4` | Max pgvector cosine distance a `/query/resolve` vector-tier match may have and still count as "confident". |
| `MUSE_FFMPEG_PATH` | `ffmpeg` | Path (or `$PATH` command name) to the ffmpeg binary for the streaming engine. |
| `MUSE_MEDIA_ROOT` | `""` (empty) | Base path prepended to stored `relative_path`/`file_path` values for ffmpeg. Empty means "use stored paths as-is" (correct when they're already absolute). |
| `MUSE_PROACTIVE_TICK_INTERVAL_SECS` | `3600` | How often the proactive generator worker runs the five generators for every account. |
| `MUSE_TEST_DATABASE_URL` | *(none)* | **Test-only.** Points the DB-gated integration/live tests at a scratch Postgres; unset → those tests skip cleanly instead of failing. |

That is **34 runtime environment variables** (plus the test-only `MUSE_TEST_DATABASE_URL`).

`MUSE_ARR_INSTANCES` JSON entry shape (`src/arr/config.rs`):

```json
[
  {"name": "radarr", "kind": "radarr", "base_url": "http://192.0.2.11:7878",
   "api_key": "<from-vault>", "library_kind": "movie", "root_folder": "/media/Movies/"},
  {"name": "sonarr", "kind": "sonarr", "base_url": "http://192.0.2.12:8989",
   "api_key": "<from-vault>", "library_kind": "tv"}
]
```

## HTTP API surface (20 routes)

Every route below is real. The `/ingest/*`, `/query/*`, and `/proactive/*` nests each also carry a
`fallback` that answers **`501 Not Implemented`** for any un-mounted sub-path (a documented seam for
future spec items). Error→status mapping lives in `src/error.rs`.

**Health**
- `GET /health` — `{status:"ok", version, db:"up"|"down"}`, never 500s.

**Ingest**
- `POST /ingest/plex-webhook` — native Plex tracker webhook receiver (multipart, `payload` field). Always `200`.

**Query / recall**
- `POST /query/resolve` — free-text → library item, via a vector → trigram → (opt-in) TMDb ladder. `{tier, results}`.
- `POST /query/similar` — "more like this" for a `media_item_id`, vector-first with genre fallback. `{tier, results}`.

**Recommend / curation**
- `POST /recommend` — full ranked list (on-deck + gap + taste + availability-aware). `{items}`.
- `GET /recommend/on_deck?account_id=&limit=` — continue-watching only.
- `GET /recommend/gaps?account_id=&limit=` — gap analysis only.

**Proactive (Lumina's poll surface)**
- `GET /proactive/pending?account_id=&limit=` — eligible undelivered nudges. `{items}`.
- `POST /proactive/{id}/ack` — body `{"outcome":"sent"|"dismissed"}`; other values → `400`. `{item}`.

**Web guide + artwork**
- `GET /` and `GET /guide` — self-contained EPG-style channel-guide HTML page.
- `GET /api/channels` — channel summaries.
- `GET /api/channels/{id}/lineup` — a channel's program lineup (window `now-2h … now+24h`).
- `GET /art/{kind}/{id}?variant=poster` — artwork proxy (Postgres-cached; never leaks the Plex token; serves a 1×1 placeholder rather than 404).

**Linear tuner (HDHomeRun-emulation) + streaming**
- `GET /discover.json` — HDHomeRun device descriptor.
- `GET /lineup_status.json` — static scan status.
- `GET /lineup.json` — channel lineup (GuideNumber/GuideName/URL per `mode='linear'` channel).
- `GET /muse.m3u` — M3U playlist alternative.
- `GET /xmltv.xml` — XMLTV EPG generated from `channel_programs`.
- `GET /auto/v{channel_id}` — continuous MPEG-TS stream (join-mid-stream). `501` if ffmpeg is missing, `503` if nothing is scheduled "now".

## Wiring status: what actually runs

Not every implemented subsystem is triggered in a running deployment. This matters for operators —
some capabilities need a manual invocation or a future worker/route to become live.

**Live HTTP routes:** all 20 listed above.

**Background workers spawned at startup** (`src/workers.rs`):
- Plex session poller (`tracker::poller`) — always spawned; no-ops if Plex unconfigured.
- Prowlarr report-pull worker — spawned **only when Prowlarr is configured**.
- Linear-tuner scheduler (`tuner::scheduler`) — always spawned; no-ops with zero linear channels.
- Proactive generator worker (`proactive::scheduler`) — always spawned; no-ops with zero accounts.

**Implemented but NOT triggered by any worker or route (seams awaiting wiring):**
- `embed::pipeline::embed_stale` — the embedding **write** path. Nothing schedules it, so embeddings are never written in a running deployment unless invoked manually. (The read primitive `embed::nearest` *is* live, used by recall + curation.)
- `taste_model::recompute_taste` — signal→profile recompute. No scheduled caller, so `taste_profile`/`taste_context_centroids` are never populated automatically.
- `radar::divergence::recompute_divergence` — the you-vs-masses radar. No caller, no HTTP surface at all; `taste_divergence` is never computed automatically.
- `arr::ingest::run` — library ingest. Parsed `MUSE_ARR_INSTANCES` is held in state, but no worker or route runs the ingest.
- `tautulli::backfill::run` — one-time history import. No route/CLI/worker; intended to be driven by an orchestrator/ops step.
- `trending::snapshot_trending` — TMDb trending ingest. `main.rs` notes it as a "follow-on wiring item".
- `channels::compose_channel_run` — the on-demand pseudo-TV director. Fully implemented + tested, but no HTTP route mounts it (the *linear* tuner uses its own `tuner::scheduler` grid-filler instead).
- `enrichment::EnrichmentService::enrich_media_item` — external-enrichment cache population. Wired object on `AppState`, but nothing calls it outside tests.
- `plex_control::*` — Plex Companion cast/play-queue client. Declared as a module but **not mounted anywhere** and never called — library-only, and never exercised against a real Plex server.

Consequence: in a fresh deployment with a fully populated library and Ollama/Chord configured,
`/recommend`'s taste tier, `proactive`'s `friday_evening`, and `zeitgeist` will silently return
empty until the three recompute write-paths are given a scheduled caller. See
[`docs/runbooks.md`](docs/runbooks.md) for how to drive them and
[`docs/behavior-spec.md`](docs/behavior-spec.md) for the full contract.

## Testing

The suite runs green with **no live database** — every DB-touching integration test is gated on
`MUSE_TEST_DATABASE_URL` and skips cleanly (does not fail) when it's unset. Point it at a scratch
Postgres 17 (`vector` + `pg_trgm`) database to actually exercise migrations and the repo layer:

```
MUSE_TEST_DATABASE_URL=postgres://user:pass@192.0.2.30/muse_test cargo test
```

Env-var-reading config tests use `serial_test` (process-global env), so run the suite
single-threaded (`cargo test -- --test-threads=1`) to avoid cross-test env races. HTTP-client tests
use `httpmock` and need no live upstream. The integration tests live *inside* the binary crate
(there is no `[lib]` target) so they can reach `crate::repo`/`crate::models`.

### Endpoint contract/integration harness (`src/endpoint_tests.rs`)

A router-level test suite exercises every route mounted in `http::router` through a real axum
`Router` via `tower::ServiceExt::oneshot` (an actual HTTP request/response round trip, not a bare
handler call) — `/health`, `/query/resolve`, `/query/similar`, `/recommend` +
`/recommend/on_deck` + `/recommend/gaps`, `/proactive/pending` + `/proactive/{id}/ack`,
`/api/channels` + `/api/channels/{id}/lineup`, `/art/{kind}/{id}`, `/channels/{id}/compose`,
`/ops/*`, and the `/ingest` \| `/query` \| `/proactive` fallback-501 contract. Each route gets a
contract test (status + response shape), an error-path test, and — gated on
`MUSE_TEST_DATABASE_URL` like the rest of the suite — a happy-path test against real seeded rows.
The gated `db_gated::read_endpoints_never_mutate_the_database` test snapshots every watched
table's row count, exercises every Phase-0 read endpoint back to back, and asserts the counts are
unchanged — the executable form of this service's read-only-until-Phase-1 posture. The two
intentionally-mutating Phase-0 endpoints (`/channels/{id}/compose`, `/proactive/{id}/ack`) get the
opposite assertion in their own tests: that they write exactly the one row they claim to. No test
in this module ever configures a real Plex/Prowlarr/TMDb/Tautulli/arr/Ollama/Chord client, so it
can never reach a live upstream even by accident — same posture as every other test in this crate.
Run with `cargo test endpoint_tests`.

### Golden-response regression baseline (MUSET-02)

On top of the contract/error-path/happy-path coverage above, the `golden`/`golden_support`
modules at the bottom of `src/endpoint_tests.rs` (plus a nested `golden` module inside
`db_gated` for the endpoints whose representative response needs a seeded row) add a
**golden-response baseline**: the exact, canonicalized JSON — or raw bytes, for the artwork
proxy's placeholder PNG — a representative request currently returns, committed under
[`tests/golden/`](tests/golden) and diffed on every `cargo test` run. This catches drift in
response *content*, not just status code. Non-deterministic fields (timestamps, generated ids,
per-run-unique fixture names) are redacted to a stable `"<redacted>"` placeholder via JSON
Pointer before comparison, rather than skipped — the rest of the shape is still asserted
exactly.

- **Regenerate a baseline** after an intentionally changed response:
  `MUSE_UPDATE_GOLDEN=1 cargo test endpoint_tests`. Never hand-edit a `tests/golden/*.json`
  file — golden tests are otherwise strictly read-only/comparison-only.
- **DB-independent goldens** (`health`, `/query/resolve`'s empty-query short-circuit, the
  `/ops/ingest/*` unconfigured 503s, the `/ingest`\|`/query`\|`/proactive` 501 fallback, the two
  `/proactive/{id}/ack` and `/channels/{id}/compose` 400 error bodies, and the artwork proxy's
  placeholder PNG) run in the default `cargo test` invocation, no `MUSE_TEST_DATABASE_URL`
  needed.
- **DB-gated goldens** (the `/recommend` family for a signal-empty account, an `/api/channels`
  + `/api/channels/{id}/lineup` shape for a real seeded channel, and a
  `/proactive/{id}/ack`-dismissed persisted+returned shape) skip cleanly without
  `MUSE_TEST_DATABASE_URL`, same posture as the rest of the suite.
- **Proof the mechanism actually catches drift**:
  `golden::golden_diff_mechanism_detects_a_deliberately_altered_response` exercises the real
  comparison function against a deliberately mutated response (in a self-contained scratch
  file, never a committed golden) and asserts it panics.
