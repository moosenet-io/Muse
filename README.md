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
| `MUSE_RECALL_VECTOR_MAX_DISTANCE` | `0.4` | Max pgvector cosine distance a `/query/resolve` vector-tier match may have and still count as confident (see below). |

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

## Vector recall + search API (MUSE-09)

Assistant-speed, private lookup over the library — `/query/resolve` and `/query/similar`, both
`POST`, both under the top-level `/query` group. Every tier degrades gracefully: an unconfigured
or failing dependency never turns into a 500, it just falls through to the next rung.

### `POST /query/resolve`

```json
{"query": "that space linguist movie", "limit": 10, "include_tmdb": false}
```

A resolution ladder, in order, stopping at the first rung with results:

1. **vector** — embeds `query` via the MUSE-08 `OllamaEmbedClient` (`MUSE_OLLAMA_URL`) and runs a
   pgvector cosine nearest-neighbor search over the library's stored embeddings. A match is only
   "confident" if its cosine distance is `<= MUSE_RECALL_VECTOR_MAX_DISTANCE`; anything less
   confident (or no Ollama configured, or the embed/search call fails) falls through.
2. **trigram** — `pg_trgm` fuzzy title search over the library's own `media_metadata`, scoped to
   what's actually in the catalog.
3. **tmdb** — only attempted when the caller passes `include_tmdb: true`. A TMDb
   `/search/multi` lookup beyond the library; every hit is tagged `"source": "tmdb"` with a
   `note` explicitly marking it as not in your library.

The response reports which tier answered (`"tier": "vector" | "trigram" | "tmdb" | "none"`) and
a `results` array whose entries are tagged by `"source"`. An unmatched or empty query returns
`"tier": "none"` with an empty `results` array — never an error.

### `POST /query/similar`

```json
{"media_item_id": 42, "limit": 10}
```

"More like this" for a known `media_item_id`. Prefers the item's own stored MUSE-08 embedding
(cosine nearest-neighbor, excluding the seed from its own result list — `"tier": "vector"`).
When the seed has no embedding yet (not embedded, or Ollama was never configured), falls back to
a shared-genre similarity ranking (`"tier": "genre"`) so the endpoint still returns something
useful instead of erroring. Returns `"tier": "none"` with an empty `results` array when neither
tier finds anything. A `media_item_id` that doesn't exist in the library is a `404`, not a
degraded response — unlike `/query/resolve`'s free-text ladder, a caller-supplied id is expected
to resolve.
