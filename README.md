# Muse

Muse is a standalone Rust service and AI-native media curation & taste companion — a private,
local-inference-first "brain" for a Plex library. It owns taste modeling, curation, metadata,
availability intelligence, and proactive recommendations, backed by a mandatory Postgres +
pgvector database, while Plex stays the playback surface. It's a peer to Harmony (build
orchestrator), Chord (inference proxy), Terminus (tool hub), and Lumina (assistant) — Terminus's
`media_*` tools consume Muse as it lands. See `specs/S96-muse-foundation.md` for the full
architecture, schema, and phased roadmap; this is the Phase 0 (MUSE-01) service scaffold — HTTP
skeleton, config, and a lazy DB pool, with no domain logic or schema yet.

## Running

Configuration is read from environment variables (materialized from <secret-manager> at runtime in the
fleet — never hardcode secrets):

| Variable | Default | Notes |
|---|---|---|
| `MUSE_DATABASE_URL` | none | Postgres connection string (pgvector-enabled `muse` DB). Pool connects lazily; the service still starts if the DB is unreachable. |
| `MUSE_BIND_ADDR` | `0.0.0.0:8090` | HTTP bind address. |
| `MUSE_LOG_LEVEL` | `info` | `tracing`/`EnvFilter` level. |
| `PLEX_URL` / `PLEX_TOKEN` | none | Plex API (read-only, Phase 0). |
| `TAUTULLI_URL` / `TAUTULLI_API_KEY` | none | One-time history backfill source. |
| `RADARR_URL` / `RADARR_API_KEY` | none | Library ingest source. |
| `SONARR_URL` / `SONARR_API_KEY` | none | Library ingest source. |
| `PROWLARR_URL` / `PROWLARR_API_KEY` | none | Availability/release report-pull. |
| `TMDB_API_KEY` | none | Metadata + trending/population feed. |
| `MUSE_OLLAMA_URL` | none | Local embeddings (`nomic-embed-text`). |
| `CHORD_URL` | none | Routed local-model reasoning for curation/taste. |
| `MUSE_PROWLARR_TICK_INTERVAL_SECS` | `60` | How often the report-pull worker's background loop checks which indexers are due (see below). |
| `MUSE_PROWLARR_MOVIE_CATEGORIES` | `2000` | Comma-separated Newznab parent category ids treated as "movies". |
| `MUSE_PROWLARR_TV_CATEGORIES` | `5000` | Comma-separated Newznab parent category ids treated as "tv". |
| `MUSE_PROWLARR_RESOLVE_MIN_CONFIDENCE` | `0.5` | Minimum release-name parse confidence required before a release is resolved to a title. |
| `MUSE_RELEASE_EXPIRY_DAYS` | `21` | How long a rolling release snapshot row stays before it's pruned. |

```
cargo run
```

Build and tests run on the fleet build host (<host>), not the dev box — see the project's
`moosenet-spec` build pipeline. `GET /health` returns `{status, version, db}` and never 500s even
when the database is down.

## Prowlarr report-pull worker (MUSE-17)

When `PROWLARR_URL`/`PROWLARR_API_KEY` are configured, Muse spawns a background worker that, on
a `MUSE_PROWLARR_TICK_INTERVAL_SECS` cadence, checks every enabled indexer and — only once that
indexer's own `polite_min_interval_secs` has actually elapsed since its last pull (durable,
Postgres-backed scheduling; enforced a second time in-process by the client's own rate limiter as
defense-in-depth) — performs an RSS-mode report-pull for the movie/tv categories the indexer
supports. Each pulled release is run through the MUSE-16 deterministic release-name parser,
upserted into `releases` (idempotent on `(indexer_id, guid)`), and — when the parse is confident
enough and the parsed title+year matches an existing `media_metadata` row — resolved to that
title; unmatched/low-confidence releases are still stored (negative-space discovery, never
dropped). Every resolved title's `availability` rollup is recomputed after the pull, and
expired `releases` rows are pruned at the end of each tick (never silently — a non-zero prune
count is logged). A Prowlarr outage or a single malformed release only skips that one
indexer/release for the current tick; the next tick retries. If Prowlarr isn't configured, the
worker is never spawned at all.
