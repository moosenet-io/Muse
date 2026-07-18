## Wiring status: what actually runs

Not every implemented subsystem is triggered in a running deployment. This matters for operators —
some capabilities need a manual invocation or a future worker/route to become live.

**Live HTTP routes:** all 24 listed above.

**Newly wired at MUSEM-05** (previously listed below as unwired seams — each now has a real
caller):
- `download::qbit::QbitClient` — called by `acquisition::fulfill_request` (via `AppState.download`,
  constructed in `main.rs` from `Config::qbit()`) whenever a request's search resolves to a `Grab`.
- `prowlarr::search_releases` — called by `acquisition::search_candidates`, both from
  `POST /requests`' availability check and from `acquisition::fulfill_request`'s search step.
- `decision::decide_release` — called by `acquisition::fulfill_request` against the candidates the
  search above returns.
- `repo::acquisition::*` — `media_requests`/`download_queue`/`history_events` are now written
  end-to-end by `acquisition::fulfill_request` and the `POST /requests*` handlers.

**Newly wired at MUSEL-A2:**
- `metadata::resolve::resolve_and_merge` + `repo::media_metadata::apply_enrichment` — scheduled
  inside the maintenance chain (`maintenance::run_maintenance_pass`, step (a3)) whenever `state.tmdb`
  and/or a config-built `metadata::tvdb::TvdbClient` is configured; see the "Provider-resolution +
  enrichment aggregator" section above.

**Newly wired at MUSEM-06:**
- `monitored_items` (MUSEM-01's "wanted" driver table) — now has a real caller:
  `acquisition::worker::run_wanted_pass`, scheduled inside the maintenance chain (see above).
  `blocklist` still has no caller (no write path adds to it yet).

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
- `repo::acquisition::blocklist` (MUSEM-01) — `media_requests`/`download_queue`/`history_events`/`monitored_items` are now wired (see "Newly wired at MUSEM-05"/"Newly wired at MUSEM-06" above); `blocklist` still has no write-path caller.

Consequence: in a fresh deployment with a fully populated library and Ollama/Chord configured,
`/recommend`'s taste tier, `proactive`'s `friday_evening`, and `zeitgeist` will silently return
empty until the three recompute write-paths are given a scheduled caller. See
[`docs/runbooks.md`](docs/runbooks.md) for how to drive them and
[`docs/behavior-spec.md`](docs/behavior-spec.md) for the full contract.

