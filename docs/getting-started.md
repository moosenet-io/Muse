# Getting started

This walks from clone to a verified running Muse instance. Everything here names real
binaries and real env keys from `src/config.rs`; secret **values** are never written into
files — they are materialized into the process environment from the vault at
deploy/runtime.

## Prerequisites

- **Rust 1.97.0** — pinned by `rust-toolchain.toml`; `rustup` will select it
  automatically. Do not run `rustup update` on a shared build host.
- **Postgres with the `pgvector` extension** — Muse's mandatory system of record.
  Migrations (`migrations/`, 47 files) create everything else, including extensions,
  at startup.
- **ffmpeg** (optional) — only needed for channel streaming (`/auto/v{id}`) and
  still-frame matching verification. Path via `MUSE_FFMPEG_PATH` (default: `ffmpeg`
  on `$PATH`); the streaming handler degrades to a clean 501 when it can't be spawned.

## Build

```sh
cargo build --release
```

The workspace produces one binary: `target/release/muse`.

## Minimal configuration

Only the database is load-bearing; everything else degrades gracefully when unset.

| Key | Purpose |
|---|---|
| `MUSE_DATABASE_URL` | Postgres DSN. The pool is built lazily, so an unreachable DB never blocks startup — `/health` reports `db: down` instead. |
| `MUSE_BIND_ADDR` | HTTP bind address (default `0.0.0.0:8090`). |
| `MUSE_LOG_LEVEL` | Tracing filter (default `info`). |
| `MUSE_API_TOKEN` | Bearer token for the protected/mutating route group. **Fail-closed**: unset means protected routes answer 503 — set `MUSE_AUTH_DISABLED=true` only on a dev box, deliberately. |

## First run

```sh
export MUSE_DATABASE_URL=postgres://...   # value from the vault, not a literal
./target/release/muse
```

Startup logs show, for every optional integration, whether it was configured
(`plex_configured`, `prowlarr_configured`, `tmdb_configured`, `embed_configured`,
`qbit_configured`, arr fleet count) — misconfiguration is visible at boot, not at first
use. Migrations run best-effort at startup.

**Verify:**

```sh
curl http://localhost:8090/health
```

## Enabling integrations

Each integration turns on purely by configuration; unset means inert, never an error.

| Feature | Keys |
|---|---|
| Plex playback tracking (webhook + poller) | `PLEX_URL`, `PLEX_TOKEN`, `MUSE_PLEX_POLL_SECS` |
| *arr fleet ingest | `MUSE_ARR_INSTANCES` (JSON array of instances; see `src/arr/config.rs`) |
| Prowlarr availability + search | `PROWLARR_URL`, `PROWLARR_API_KEY`, `MUSE_PROWLARR_*` tunables |
| Metadata | `TMDB_API_KEY`; `MUSE_TVDB_API_KEY` (+ optional `MUSE_TVDB_PIN`, `MUSE_TVDB_BASE_URL`) |
| Embeddings (taste/recall) | `MUSE_OLLAMA_URL` (a local `nomic-embed-text` server) |
| LLM rationale / vision | `CHORD_URL` (OpenAI-compatible chat-completions proxy) |
| qBittorrent grabs | `MUSE_QBIT_URL`, `MUSE_QBIT_USER`, `MUSE_QBIT_PASS` |
| Library filesystem scan | `MUSE_LIBRARY_ROOT` (a **read-only** mount; unset = scanner is a no-op) |
| Channel streaming | `MUSE_FFMPEG_PATH`, `MUSE_MEDIA_ROOT`, `MUSE_PUBLIC_URL` |
| Enrichment (sentiment/news) | `MUSE_SEARXNG_URL`, `MUSE_NEWS_URL`, `MUSE_NEWS_API_KEY` |
| Cultural layer (Trakt) | `TRAKT_CLIENT_ID` (+ optional `TRAKT_API_KEY`) |
| Discord bot | `DISCORD_BOT_TOKEN` |

The full tunable surface (worker cadences, thresholds, budgets) is documented field-by-
field in `src/config.rs`; each reference page lists the keys its subsystem reads.

## Operator subcommands

The `muse` binary carries three CLI subcommands that return before any server bootstrap
and are never invoked by service startup or the test suite:

- `muse snapshot-acquire` — read-only acquisition of source snapshots (Plex/Tautulli/
  *arr SQLite copies, `pg_dump` of the Muse DB) with checksummed provenance.
- `muse shadow-run` — the shadow Tautulli-replacement analytics pass over the guarded
  snapshot DB; computes and reports, never writes back.
- `muse parity-report` — diffs shadow analytics against Tautulli-origin history and
  prints a retirement-readiness report.

All three connect only through the guarded snapshot path
(`MUSE_SNAPSHOT_DATABASE_URL`/`MUSE_TEST_DATABASE_URL`) — see the
[snapshot pipeline guide](guides/snapshot-pipeline.md).

## Running the tests

```sh
cargo test                                   # pure-function units, no DB needed
MUSE_TEST_DATABASE_URL=postgres://... cargo test   # + live-DB round-trip tests
```

Live-DB tests are gated: without `MUSE_TEST_DATABASE_URL` they skip cleanly. The DSN is
guard-checked (`snapshot::guard`) so it can never point at a production database. See
[TESTING.md](TESTING.md).

## Next steps

- [Add Muse as a Plex tuner](guides/plex-tuner.md)
- [The acquisition pipeline end-to-end](guides/acquisition-pipeline.md)
- [Architecture](architecture.md) for how the pieces fit.
