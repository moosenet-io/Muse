# Muse — Architecture

This document describes where Muse sits in the constellation, how data flows through it, and how
its modules layer. It is grounded in the shipped code (`src/`, `migrations/`), not the aspirational
founding spec. For the data model see [`schema.md`](schema.md); for operations see
[`runbooks.md`](runbooks.md); for behavioral contracts see [`behavior-spec.md`](behavior-spec.md).

## 1. Constellation position

Muse is a **standalone Rust service** and a first-class peer of the other MooseNet services, not a
plugin of any of them:

- **Harmony** — the build orchestrator; it builds Muse through the `moosenet-spec` pipeline.
- **Chord** — the inference proxy/orchestrator. Muse routes its *reasoning* calls (curation
  rationale, taste `model_notes`, optional channel-lineup composition) through Chord's
  OpenAI-compatible `/v1/chat/completions` surface to a local model on <host>. Muse never talks to a
  hosted LLM.
- **Terminus** — the tool hub. Terminus's shipped `media_*` tools are the *voice/tool surface* and
  re-point at Muse as phases land. **Muse does not call Terminus MCP tools in-process** — where it
  needs a fleet capability (SearXNG, news search) it calls the configured HTTP endpoint directly,
  mirroring the shape of a Terminus tool rather than invoking one.
- **Lumina** — the assistant. Lumina's reminders/engagement scheduler polls Muse's
  `GET /proactive/pending` surface and relays nudges. Muse is the brain; Lumina is the voice.

Muse keeps **qBittorrent** (acquisition) and **Plex** (consumption) in place and owns the brain in
between. Postgres + pgvector is Muse's own **system of record** — taste, telemetry, embeddings, and
availability all live there, on-LAN. Only non-personal metadata lookups (TMDb, web sentiment)
egress; embeddings and all behavioral data stay local.

```
Harmony ──builds──►  Muse
Chord    ◄──reasoning (/v1/chat/completions, local model on <host>)──  Muse
Ollama   ◄──embeddings (nomic-embed-text)──  Muse
Terminus (media_* tools) ──re-points at──►  Muse HTTP API
Muse (proactive_items) ──GET /proactive/pending──►  Lumina scheduler ──►  user
```

## 2. Process shape

A single `axum` HTTP server (`muse` binary, default bind `0.0.0.0:8090`) plus background workers,
all over an `sqlx` Postgres pool. Startup (`src/main.rs`):

1. `Config::from_env()` — read every setting from the environment (<secret-manager>-materialized).
2. Build a **lazy** Postgres pool (`connect_lazy`) — a missing/unreachable DB never blocks startup.
3. Construct the optional upstream clients: `PlexClient`, `ProwlarrClient`, the `*arr` fleet,
   `TmdbClient`, `OllamaEmbedClient`, `EnrichmentService`. Each `from_config` returns `None`/empty
   when unconfigured (logged at boot so misconfiguration is visible).
4. Assemble `AppState` (pool + config + those clients), shared behind `Arc`.
5. Best-effort run migrations (`db::migrate`) — logged and continued if the DB is down.
6. `workers::spawn_workers(state)` — spawn the background workers (see §4).
7. Serve the router.

`AppState` is the one shared context every handler receives. Errors flow through the crate `MuseError`
enum (`src/error.rs`), whose `IntoResponse` maps each variant to a status: `Database`/`Config`/
`Internal` → 500, `NotFound` → 404, `BadRequest` → 400, `Conflict` → 409, `NotImplemented` → 501,
`Http`/`Upstream` → 502, `ServiceUnavailable` → 503. Every error body is `{"error": message}`.

## 3. Data flow

### 3.1 Ingest → telemetry → taste → recall → curation → proactive

```
 Radarr/Sonarr ──(arr::ingest, SEAM)──►  media_metadata / media_items / seasons / episodes / media_files
 Plex webhook ──POST /ingest/plex-webhook──►  play_events ──┐
 Plex /status/sessions ──(tracker::poller)──►  play_events ─┤
 Tautulli ──(tautulli::backfill, SEAM)──►  play_sessions ───┤
                                                            ▼
                        tracker::reconstruct  ──►  play_sessions (+ media_info)
                                                            │
                        (watch_stats / ratings / watchlist derived)
                                                            ▼
   embed::embed_stale (SEAM) ──nomic-embed-text──►  embeddings (pgvector, HNSW)
   taste_model::recompute (SEAM) ──►  taste_signals → taste_profile + taste_context_centroids
                                                            │
        recall (/query/*) ◄──embed::nearest──────────────  embeddings
        curation (/recommend*) ◄──taste_profile + watch_stats + availability──
                                                            ▼
   proactive::generators ──(worker, hourly)──►  proactive_items ──GET /proactive/pending──►  Lumina
```

The five telemetry/taste tables (`play_events` → `play_sessions`) are the Tautulli-equivalent core;
everything downstream (embeddings, taste, curation, proactive) reads from them. Note the **SEAM**
markers: `arr::ingest`, `tautulli::backfill`, `embed::embed_stale`, and `taste_model::recompute`
are implemented and tested but have no scheduled caller yet, so in a running deployment those arrows
only move when someone invokes them manually. The *read* side of embeddings (`embed::nearest`),
the tracker (webhook + poller + reconstruct), and curation/proactive are fully live.

### 3.2 Availability report-pull (Prowlarr)

```
 Prowlarr /api/v1/indexer ──►  indexers
 Prowlarr /api/v1/search (RSS mode, per indexer) ──►  releases (deterministic name-parse) ──►  availability rollup
```

The report-pull worker (`prowlarr::worker`, spawned only when Prowlarr is configured) wakes every
`MUSE_PROWLARR_TICK_INTERVAL_SECS`, and for each enabled indexer that is due (per its own
`polite_min_interval_secs`, DB-backed) performs an RSS-mode pull, parses each release name
deterministically, upserts into `releases`, resolves confident matches to a `media_metadata` title,
recomputes the per-title `availability` rollup, and prunes expired rows. Read-only: a search is not
a grab. Availability feeds curation ("there's a good release up right now") and the `grab_window`
proactive generator.

### 3.3 Population feed + you-vs-masses radar

```
 TMDb /trending, /popular, /watch/providers ──(trending::snapshot_trending, SEAM)──►
        trending_snapshots + streaming_availability + population_profile
 radar::recompute_divergence (SEAM) ──►  taste_divergence  ──►  proactive::zeitgeist
```

Both the trending ingest and the radar computation are seams (no scheduled caller). The `zeitgeist`
proactive generator reads the latest `taste_divergence` snapshot; because nothing computes one in a
running deployment today, that generator is functionally dormant until wired.

### 3.4 Channels: compose → schedule → tune → stream

```
 (on-demand)  channels::compose_channel_run (SEAM) ──►  channel_runs + channel_programs
 (linear)     tuner::scheduler (worker) ──rolling grid──►  channel_programs
                                                            │
              tuner: /discover.json /lineup.json /muse.m3u /xmltv.xml  ◄── Plex reads this as a custom tuner
                                                            ▼
              streaming: GET /auto/v{id}  ──onnow──►  ffmpeg (stream-copy, join-mid-stream)  ──►  MPEG-TS
              web: / /guide /api/channels /art  ──►  browser EPG (Plex token never leaves the server)
```

Two channel modes share the same `channels`/`interstitials` tables but different composers:
`mode='on_demand'` uses the LLM-optional `channels::compose` director (a seam — no route mounts it);
`mode='linear'` is fed by `tuner::scheduler`, a separate deterministic round-robin grid-filler that
keeps `channel_programs` topped off a rolling `MUSE_CHANNEL_GUIDE_WINDOW_HOURS` ahead. The tuner
routes and the ffmpeg streaming engine are fully live.

## 4. Background workers (`src/workers.rs`)

`spawn_workers` starts exactly four workers, in order:

| Worker | Cadence | Spawn condition | Notes |
|---|---|---|---|
| `tracker::poller` | `MUSE_PLEX_POLL_SECS` (default 10s) | always | no-ops (single log line) if Plex unconfigured |
| `prowlarr::spawn_report_pull_worker` | `MUSE_PROWLARR_TICK_INTERVAL_SECS` (default 60s) | **only if Prowlarr configured** | never runs an idle task on an unconfigured deployment |
| `tuner::scheduler` | `MUSE_CHANNEL_SCHEDULER_TICK_SECS` (default 900s) | always | no-op tick with zero linear channels |
| `proactive::scheduler` | `MUSE_PROACTIVE_TICK_INTERVAL_SECS` (default 3600s) | always | first tick skipped so a restart doesn't hammer every account; no-op with zero accounts |

The embedder, taste-recompute, and taste-divergence recompute workers are **not** among these — see
the [Wiring status](../README.md#wiring-status-what-actually-runs) note. (The `workers.rs` module
doc comment is slightly stale: it says the proactive scheduler "still lands in a later item," but it
is in fact spawned.)

## 5. Module dependency layering

Roughly bottom-up (each layer depends only on those below it):

1. **Foundation** — `config`, `error`, `db`, `models/*` (typed rows + `New*` insert structs),
   `repo/*` (the sqlx query layer, one module per table group).
2. **Upstream clients** — `plex`, `arr::client`, `prowlarr::client`, `trending::client`
   (TMDb), `embed::ollama`, `taste_model::chord_client`, `enrichment::client`,
   `plex_control::client`. All typed, read-only, `from_config`-constructed, graceful-degrading.
3. **Ingest / capture** — `arr::ingest`, `tracker` (webhook/poller/reconstruct), `tautulli::backfill`,
   `prowlarr::worker`, `trending`. Turn upstream data into Postgres rows.
4. **Intelligence** — `embed::pipeline`, `recall`, `taste_model` (signals/profile/recompute),
   `radar::divergence`, `curation` (candidates/recommend), `proactive` (generators/scheduler),
   `enrichment` (cache). Read the captured data, compute derived value.
5. **Presentation / control** — `channels` (compose/presets), `tuner` (hdhr/m3u/xmltv/scheduler),
   `streaming` (ffmpeg/onnow), `web` (guide/artwork), `plex_control` (cast).
6. **HTTP** — `http::router` + `AppState` wire the live handlers; `workers::spawn_workers` wires the
   live workers; `main` assembles everything.

The `repo` layer is the only place raw SQL lives; every other module composes `repo` functions.
Pure computation (name parsing, taste weights, radar formulas, session reconstruction folds, lineup
building) is deliberately separated from I/O so it is unit-testable without a database — the live-DB
tests then cover the persistence round-trips (all gated on `MUSE_TEST_DATABASE_URL`).
