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

```
cargo run
```

Build and tests run on the fleet build host (<host>), not the dev box — see the project's
`moosenet-spec` build pipeline. `GET /health` returns `{status, version, db}` and never 500s even
when the database is down.
