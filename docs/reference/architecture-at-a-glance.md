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
| `prowlarr/` | Availability report-pull worker (indexer sync, RSS pull, release parse, rollup) + on-demand targeted search (`search_releases`) |
| `decision/` | Pure release-decision/scoring engine ("what to grab") — see [Release-decision engine](#release-decision-engine) below |
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
| `matching/` | Matching-verification support: MUSEL-C1's ffmpeg sample-still extraction primitive + MUSEL-C2's `verify_match` verdict (VLM-via-Chord, still-liveness, metadata-consistency) |
| `web/` | Channel-guide web page + JSON API + artwork proxy |
| `plex_control/` | Plex Companion cast/play-queue client (library-only, unwired — see below) |

