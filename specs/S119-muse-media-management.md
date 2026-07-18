# Muse Native Media Management — Sonarr/Radarr/Lidarr/<media-service> Replacement (Sprint 1: Acquisition Write-Path Foundation)
plane_project: MUSE
module: Muse
prefix: MUSEM
spec_id: S119-muse-media-management

## Metadata
- **Author:** <operator> (Moose) + Claude (design)
- **Session:** S119
- **Date:** 2026-07-18
- **Module version:** Muse (post-S118: MUSEX experience layer + CAP-SEC merged, Phase-0 read-only foundation live)
- **Estimated total (Sprint 1):** ~34h autonomous agent work
- **Context:** Muse today is a *read-only* observer of the operator's 8-instance *arr fleet + Tautulli
  (verified audit, 2026-07-18): `src/arr` is GET-only ingest, `src/prowlarr` is RSS/report-pull availability
  intelligence, and `src/arr/request.rs` is a tier CLASSIFIER whose only `MediaRequestSink` impl is a mock —
  there is **no write path, no download-client control, no release-decision engine, and no request lifecycle**.
  The operator has committed to the charter's **full-replacement end-state**: Muse natively owns the
  <media-service> (request/approval) + Sonarr/Radarr/Lidarr (monitor → select → grab → import/organize)
  responsibilities, **keeping Prowlarr (indexer) and qBittorrent (download client) as substrate** and
  excluding transcode/HandBrake (a later ingest sprint). This sprint builds the acquisition WRITE-PATH
  FOUNDATION — the smallest increment that makes Muse able to take a title from request → targeted search →
  scored release decision → grab into qBittorrent, and to drive a monitored "wanted" list — grounded in the
  live *arr data model captured in `ARR-BLUEPRINT.md` and the phased plan in `MUSE-charter.md`. Import/rename
  (the guarded "risky 80%"), Lidarr/music, and the *arr retirement cutover are explicit later sprints (see the
  Program Roadmap at the end).

## Pre-flight
- Repository: `moosenet/Muse` on Gitea (exists; default branch `main`).
- Build/test host: **<host>** via the compiler tool (`compiler_build(module="muse", ref=<branch>, mode=test)`)
  — NOT the dev box (OOM); the 2–4 GB deploy CT never builds. <host> <host>-root verified healthy (48% used) 2026-07-18.
- Plane project: `MUSE` (id resolved at ingest via the Terminus Plane tool). Ingest/transitions/comments go
  through the Terminus Plane tool ONLY (single sanctioned path); acting identity per each item's `Agent:` field.
- Prefix: `MUSEM` — confirmed free via `plane_prefix_check` (2026-07-18); register + `plane_prefix_promote`
  (durable baseline entry + Terminus PR) as part of Stage 1.
- Review gate: `review_run` panel through the review-daemon (<host> loopback:8790, active). Default panel
  **`["codex","free","opus"]`** — agy is quota-capped; do NOT include agy this window.
- Secrets (materialized from <secret-manager> into Muse's runtime env, NEVER hardcoded — S7): existing `MUSE_ARR_INSTANCES`,
  `TMDB_API_KEY`, `MUSE_DATABASE_URL`; NEW for this sprint — `MUSE_QBIT_URL`, `MUSE_QBIT_USER`, `MUSE_QBIT_PASS`,
  `MUSE_PROWLARR_URL`, `MUSE_PROWLARR_API_KEY`. If any new secret is not yet in <secret-manager>, surface it as a
  human-action/ops item — do NOT hardcode a stopgap.
- S9 note: qBittorrent and Prowlarr are Muse's OWN acquisition substrate (no dedicated Terminus tool fronts
  them), exactly like `src/arr` already calls Radarr/Sonarr directly. Muse talking to them via its own
  runtime-materialized creds is an application's own data/integration plane, NOT an S9 single-door violation.
- Atlas KG: Muse has no graph yet (`kg_search` → "run scribe_kg_build first"). MUSEM-00 (below) stands up the
  Muse KG so subsequent items + reviews ground against a real call graph; until it lands, grounding is the
  verified module audit + `ARR-BLUEPRINT.md`.
- DB access: Muse owns its own pool via `MUSE_DATABASE_URL` (the app-service data-plane exception to S9-pg) —
  migrations run through Muse's existing `migrations/` + `sqlx` convention, not the `pg_*` fleet tools.

---

### MUSEM-00: Stand up the Muse Atlas knowledge graph (KG grounding prerequisite)
- **Priority:** High
- **Labels:** muse, infra, kg
- **Agent:** claude
- **Estimate:** 1h
- **Type:** human-action
- **Description:** Muse has no Atlas KG (`kg_search project_id=muse` → "no knowledge graph for this project").
  The build skill mandates KG grounding before scoping and injects the graph into every `review_run`, so a
  missing Muse graph degrades every downstream item. Build it once via the Terminus tool so all later MUSEM
  items and their reviews ground against Muse's real call graph.
- **Steps:**
  1. Confirm the Muse source is on the daemon's `SCRIBE_ALLOWED_REPO_ROOTS` (the <host> RO parking-lot mount);
     if not, add it (ops) and sync the parking lot from the RW dev box (see the scribe_kg_rebuild_fix runbook).
  2. Call `scribe_kg_build(project_id="muse", repo_path=<muse source on an allowed root>)` (full, non-incremental).
  3. Verify with `scribe_kg_status(project_id="muse")` (nodes > 0) and a spot `kg_search(project_id="muse", "arr ingest")`.
  4. Thereafter Stage 7c refreshes it incrementally on every MUSEM merge.

---

### MUSEM-01: Acquisition-domain DB schema + migrations
- **Priority:** Critical
- **Labels:** muse, db, acquisition
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Add the Postgres schema that the whole acquisition write-path needs — mirroring the
  normalized *arr model captured in `ARR-BLUEPRINT.md` (quality is a compound `{tier, revision}`; custom
  formats are a named scored-matcher registry; history is typed `jsonb`, not a loose bag; provider IDs are a
  keyed map; library sharding is first-class). This item is schema + repository layer ONLY — no workers, no
  endpoints (those are MUSEM-04/05/06). Music/Lidarr columns are provisioned as nullable/extensible but not
  exercised this sprint.

  ## FILES
  - `migrations/{next}_acquisition_domain.sql` — new tables (see APPROACH)
  - `src/repo/acquisition.rs` — new sqlx repository module (CRUD for the new tables)
  - `src/repo/mod.rs` — register the new repository module
  - `src/models/acquisition.rs` — new domain structs/enums (RequestStatus, QueueStatus, HistoryEventType, QualityRevision)
  - `src/models/mod.rs` — register
  - `README.md` — document the new acquisition schema + tables (feature-adding)

  ## APPROACH
  1. Create migration adding, all scoped by the existing `library_id` where instance-level:
     - `quality_definitions(id, tier_id UNIQUE, title UNIQUE, min_size_per_min NUMERIC, max_size_per_min NUMERIC, preferred_size_per_min NUMERIC)` — the global tier registry (SDTV/WEBDL-1080p/Remux…); non-contiguous historical ids per blueprint §2.
     - `quality_profiles(id, name, library_id, cutoff_tier_id, items JSONB, upgrade_allowed BOOL, min_format_score INT, cutoff_format_score INT, min_upgrade_format_score INT)`.
     - `custom_formats(id, name UNIQUE, specifications JSONB)` — named scored matcher rules (regex/source/resolution/language/size/edition/indexer-flag specs).
     - `quality_profile_format_scores(quality_profile_id, custom_format_id, score INT, PRIMARY KEY(qp,cf))`.
     - `monitored_items(id, media_metadata_id, media_item_id NULLABLE, library_id, monitored BOOL, quality_profile_id, min_availability TEXT, last_search_at TIMESTAMPTZ NULLABLE, UNIQUE(media_metadata_id, library_id))` — the "wanted" driver.
     - `media_requests(id, provider_ids JSONB, media_kind TEXT, title TEXT, requested_by TEXT, status TEXT, tier TEXT, quality_profile_id NULLABLE, note TEXT, created_at, updated_at)` — <media-service> lifecycle.
     - `download_queue(id, request_id NULLABLE, monitored_item_id NULLABLE, release_guid TEXT, release_title TEXT, indexer TEXT, download_client TEXT, client_hash TEXT NULLABLE, protocol TEXT, status TEXT, size_bytes BIGINT NULLABLE, added_at, updated_at)`.
     - `history_events(id, event_type TEXT, media_metadata_id NULLABLE, monitored_item_id NULLABLE, download_id TEXT NULLABLE, source_title TEXT, quality JSONB, data JSONB, languages JSONB, created_at)` — typed per blueprint rec #6.
     - `blocklist(id, source_title TEXT, torrent_hash TEXT NULLABLE, media_metadata_id NULLABLE, indexer TEXT, message TEXT, size_bytes BIGINT NULLABLE, created_at)`.
     Add FKs where a real parent exists (quality_profile_id → quality_profiles, etc.) and indexes on the hot lookup columns (monitored+missing scan, request status, queue status, history download_id correlation).
  2. `src/models/acquisition.rs`: `RequestStatus{Requested,Approved,Denied,Searching,Grabbed,Available,Failed}`, `QueueStatus{Queued,Downloading,Completed,Importing,Imported,Failed,Removed}`, `HistoryEventType{Requested,Grabbed,DownloadImported,DownloadFailed,Blocklisted,Deleted}`, `QualityRevision{version,real,is_repack}`, a `QualityStamp{tier_id, revision}` compound (serde ↔ the `{quality,revision}` JSON shape). Derive Debug/Clone/Serialize/Deserialize/PartialEq.
  3. `src/repo/acquisition.rs`: typed sqlx CRUD — insert/get/list/update-status for each table; the hot queries the later items need (`list_wanted(library_id)` = monitored & (no file OR below cutoff), `list_requests_by_status`, `enqueue_download`, `record_history_event`). Use the existing repo error/`MuseResult` conventions; parameterized queries only.
  4. All DB access via Muse's existing pool (`MUSE_DATABASE_URL`); no `pg_*` fleet tools, no hardcoded DSN.

  ## TEST PLAN
  - `compiler_build(module="muse", ref=<branch>, mode=test)` — workspace tests pass, including new ones.
  - New `#[cfg(test)]` (or `tests/` with the existing pg-test harness/fixtures) round-tripping each table: insert → get → list → update status; the compound `QualityStamp` serde matches the `{quality,revision}` shape; `list_wanted` returns only monitored-and-missing/below-cutoff rows (a monitored+has-file-at-cutoff row is excluded).
  - Negative: inserting a `download_queue` row with neither `request_id` nor `monitored_item_id` is rejected (CHECK); an unknown `status` string round-trips as the correct enum or errors.
  - Verify no hardcoded IPs/org names in new/modified files; grep for `std::env::var` of secrets = 0.

  ## EDGE CASES
  - A media_metadata row deleted while a monitored_item references it — ON DELETE behavior explicit (CASCADE the monitor, keep history via NULLABLE ref).
  - Non-contiguous quality tier ids (blueprint: ids are historical, not ordered) — never assume id order == quality order; ordering lives in `quality_profiles.items`.
  - Migration is idempotent/re-runnable in the sqlx migration framework (no bare CREATE without IF-appropriate guards per the repo's existing migration style).
  - Music kind: `media_kind` accepts a future `Music` value without a schema change (TEXT, not an enum-constrained column that would need ALTER).

- **Acceptance criteria:**
  - [ ] Migration creates all nine tables with FKs + indexes and applies cleanly on a fresh DB and on top of current main's schema
  - [ ] `src/repo/acquisition.rs` provides typed CRUD + `list_wanted`/`list_requests_by_status`/`enqueue_download`/`record_history_event`, all parameterized
  - [ ] `QualityStamp`/`QualityRevision` serde round-trips the `{quality,revision}` JSON shape from the blueprint
  - [ ] `list_wanted` excludes monitored items already at/above cutoff (at least one negative test proves it)
  - [ ] No hardcoded infrastructure values in new/modified code; secrets via Muse config/SecretManager, not `std::env::var`
  - [ ] README updated to document the acquisition schema
  - [ ] All existing tests still pass

---

### MUSEM-02: qBittorrent download-client adapter
- **Priority:** Critical
- **Labels:** muse, acquisition, download-client
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Muse has NO download-client control today. Add a qBittorrent WebUI (v2 API) adapter behind a
  `DownloadClient` trait (so SABnzbd/others can slot in later, and so the grab path is mockable). This is
  Muse's first *write* to an acquisition substrate — scoped, trait-isolated, and covered by httpmock parsing
  tests (mirroring `src/arr`/`src/plex` client shape). It does NOT decide what to grab (that's MUSEM-04) — it
  only executes an add/list/delete against qBittorrent.

  ## FILES
  - `src/download/mod.rs` — new module; the `DownloadClient` trait + a `MockDownloadClient` for tests
  - `src/download/qbit.rs` — `QbitClient` (login/cookie, add, list, torrent-info, delete)
  - `src/download/config.rs` — `QbitConfig` loaded from `MUSE_QBIT_URL`/`_USER`/`_PASS` via Muse's config layer
  - `src/lib.rs` / `src/main.rs` — declare the module
  - `README.md` — document the download-client integration + its env/secret config

  ## APPROACH
  1. `DownloadClient` async trait: `add(&self, req: GrabRequest) -> MuseResult<GrabReceipt>` (url/magnet, category, save_path, paused?), `list(&self) -> MuseResult<Vec<TorrentStatus>>`, `info(&self, hash) -> MuseResult<Option<TorrentStatus>>`, `delete(&self, hash, delete_files: bool) -> MuseResult<()>`. `GrabReceipt` carries the resolved hash where qBittorrent returns it (add-by-hash flow) or the parsed infohash.
  2. `QbitClient`: authenticate via `POST /api/v2/auth/login` (form user/pass), capture the `SID` cookie, re-auth on 403; `POST /api/v2/torrents/add` (multipart/form urls=, category=, savepath=, paused=); `GET /api/v2/torrents/info` (+ filter/hashes); `POST /api/v2/torrents/delete`. Reqwest client with a cookie store; timeouts + one re-auth retry.
  3. Credentials strictly via `QbitConfig` (SecretManager / Muse config), never a literal; `MUSE_QBIT_PASS` wrapped so it never logs.
  4. Graceful degradation: an unreachable/erroring qBittorrent returns a typed `MuseError` the callers surface (never panics, never blocks the service) — same posture as `arr::ingest` skipping an offline instance.

  ## TEST PLAN
  - `compiler_build(module="muse", ref=<branch>, mode=test)` — tests pass.
  - httpmock tests: login→cookie set; add torrent parses the response + returns a receipt; info parses a torrent list into `TorrentStatus`; a 403 on a data call triggers exactly one re-auth then retry; delete issues the right form.
  - Negative: login 401/403 → typed auth error (not a panic); add against a 5xx → typed error, batch not aborted.
  - `MockDownloadClient` records adds for MUSEM-05's tests.
  - Verify no hardcoded infra values; `MUSE_QBIT_PASS` never appears in logs (Display/Debug redacted).

  ## EDGE CASES
  - Session cookie expiry mid-run → single transparent re-auth then retry; a second failure surfaces.
  - Magnet vs .torrent-url add both supported (urls= accepts both).
  - Category/save-path unset → omit the field (let qBittorrent defaults apply) rather than send empty.
  - qBittorrent add returns "Ok." with no hash (older builds) → resolve the hash from the submitted magnet/infohash, don't assume the body carries it.

- **Acceptance criteria:**
  - [ ] `DownloadClient` trait + `QbitClient` implement add/list/info/delete against the qBittorrent v2 API
  - [ ] Auth uses the SID cookie with a single transparent re-auth on 403; credentials come only from Muse config/SecretManager
  - [ ] httpmock tests cover login, add, info-parse, re-auth-retry, and delete
  - [ ] Negative test: auth failure and an add 5xx return typed errors, never panic
  - [ ] `MUSE_QBIT_PASS` is never written to logs/Debug output
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README updated to document the download-client integration
  - [ ] All existing tests still pass

---

### MUSEM-03: On-demand targeted Prowlarr search
- **Priority:** High
- **Labels:** muse, acquisition, prowlarr
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Today `src/prowlarr` only does RSS/report-pull availability rollup. Add a *targeted* search —
  "search this specific title now and return parsed candidate releases" — against Prowlarr `/api/v1/search`
  (blueprint §4), reusing the existing release-name parser (`src/prowlarr/parse.rs`). This is the input to the
  decision engine (MUSEM-04); it makes no grab decision itself.

  ## FILES
  - `src/prowlarr/search.rs` — new: `search_releases(query, categories, provider_ids) -> MuseResult<Vec<ParsedRelease>>`
  - `src/prowlarr/mod.rs` — export the new function
  - `src/prowlarr/client.rs` — add the `/api/v1/search` call if not already present (client is otherwise read-only-report today)
  - `README.md` — document the on-demand search capability

  ## APPROACH
  1. `search_releases`: build the Prowlarr search query (`query`, `categories[]`, and any `{imdbId,tmdbId,tvdbId}` hints), call `GET /api/v1/search` with the API key from `MUSE_PROWLARR_*` config, map each result object (guid, title, size, seeders/leechers, indexer, protocol, downloadUrl, indexerFlags, categories, publishDate) into the existing `ParsedRelease`/release model, running each title through `parse.rs` for quality/edition/revision/language/release-group.
  2. Keep the existing RSS report-pull path untouched — this is an additional, on-demand entry point.
  3. Respect the existing Prowlarr rate-limit (`src/prowlarr/rate_limit.rs`) so an on-demand search shares the same budget as the report-pull worker.
  4. Preserve Prowlarr's own extracted `{imdbId,tmdbId,tvdbId}` on each result AND Muse's own parse (blueprint §4: don't rely solely on Prowlarr's guess).

  ## TEST PLAN
  - `compiler_build(module="muse", ref=<branch>, mode=test)` — tests pass.
  - httpmock: a canned `/api/v1/search` response (torrent results incl. a freeleech + a null-seeders private result) parses into N `ParsedRelease`s with correct quality/size/seeders/flags; the `downloadUrl` (Prowlarr-proxied) is preserved verbatim for the grab step.
  - Negative: search error/5xx → typed error, empty vec never silently swallows a failure; a result with an unparseable title → kept with low/unknown parse confidence, not dropped silently.
  - Verify no hardcoded infra values; API key via config, not `std::env::var`.

  ## EDGE CASES
  - Private-tracker results with `null` seeders/infoHash (blueprint §4) — represented as unknown, not 0, so decision scoring can treat them correctly.
  - Rate-limit hit → back off per the existing limiter, not a hard error.
  - Zero results → empty vec + a logged "no releases" (a legitimate outcome, distinct from an error).

- **Acceptance criteria:**
  - [ ] `search_releases` calls Prowlarr `/api/v1/search` and returns parsed candidate releases, reusing `parse.rs`
  - [ ] The existing RSS report-pull path is unchanged; on-demand search shares the existing rate limiter
  - [ ] Prowlarr-proxied `downloadUrl` is preserved verbatim for the grab step
  - [ ] httpmock test covers a mixed result set (freeleech + null-seeders private); negative test covers a search error
  - [ ] No hardcoded infrastructure values; API key via Muse config/SecretManager
  - [ ] README updated to document on-demand search
  - [ ] All existing tests still pass

---

### MUSEM-04: Release-decision / scoring engine
- **Priority:** Critical
- **Labels:** muse, acquisition, decision
- **Agent:** claude
- **Estimate:** 6h
- **Description:** The heart of Sonarr/Radarr's "what to grab": given candidate releases (MUSEM-03) + a quality
  profile with its custom-format scores (MUSEM-01), pick the single best *eligible* release — or none. Pure,
  deterministic, exhaustively testable (mirrors `classify_tier`'s pure-function shape). This is also the charter's
  AI seam: a local-LLM scorer can later be registered alongside the static scorers, so model the scorer as a
  registry from day one — but this item ships the deterministic scorers only.

  ## FILES
  - `src/decision/mod.rs` — new module: `decide_release(candidates, profile, format_scores, policy) -> Decision`
  - `src/decision/scoring.rs` — quality-tier ordering + cutoff, custom-format matcher evaluation, size/seeder/flag gates
  - `src/models/mod.rs` — `Decision{Grab(ReleaseChoice)|Reject{reasons}}`, `ReleaseChoice{release, total_score, quality_tier, reason}`
  - `README.md` — document the decision engine + scoring model

  ## APPROACH
  1. For each candidate: resolve its parsed quality → a `quality_definitions` tier; reject if the tier isn't `allowed` in the profile's `items`, or if size-per-minute is outside the tier bounds (blueprint §2 mis-tag guard).
  2. Evaluate every applicable `custom_formats.specifications` matcher (release-title regex, source, resolution, language, size, indexer-flag) against the candidate → sum the profile's `quality_profile_format_scores`; reject if total < `min_format_score`.
  3. Rank the surviving candidates by (quality-tier order in `profile.items`, then total custom-format score, then seeders, then freeleech, then smaller size as a tiebreak); apply revision/upgrade rules (PROPER/REPACK/REAL bump; never pick a strictly-inferior re-release). `cutoff`/`cutoff_format_score` decide "good enough — stop".
  4. Return `Decision::Grab(best)` with a human-readable reason, or `Decision::Reject{reasons}` enumerating why nothing qualified. No I/O in this module — every signal is passed in (testability).
  5. Model the scorer set as a `trait Scorer`/registry so a future LLM/taste scorer (charter Phase 1) plugs in without a special case.

  ## TEST PLAN
  - `compiler_build(module="muse", ref=<branch>, mode=test)` — tests pass.
  - Exhaustive unit tests: allowed-tier filtering; size-per-min rejection; min-format-score rejection; ranking picks the higher-quality-tier over a higher-seeder-lower-tier; REPACK beats the same non-repack; cutoff stops an upgrade; empty candidates → `Reject`; all-rejected → `Reject{reasons}` (non-empty).
  - Negative: a candidate whose quality can't be resolved to a known tier is rejected with a clear reason, not grabbed by default.
  - Verify no hardcoded infra values (pure module — none expected).

  ## EDGE CASES
  - Unknown/unparseable quality → rejected (fail-closed), never a silent default-grab.
  - Ties across every key → deterministic tiebreak (stable order by guid) so the choice is reproducible.
  - Empty custom-format set / empty profile items → still functions (grab by tier order alone if allowed).
  - `null`-seeder private results (from MUSEM-03) → treated as unknown in ranking, not as 0 (don't unfairly sink a private-tracker release).

- **Acceptance criteria:**
  - [ ] `decide_release` returns the best eligible release or a `Reject` with enumerated reasons, with no I/O
  - [ ] Tier-allowed, size-per-min, and min-format-score gates all enforced; ranking honors profile tier order then format score
  - [ ] Revision rules pick REPACK/PROPER over an equal non-repack and never a strictly-inferior re-release
  - [ ] Scorers are a registry/trait so a future LLM scorer slots in without special-casing
  - [ ] Fail-closed on unknown quality (negative test proves no default-grab)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README updated to document the decision engine
  - [ ] All existing tests still pass

---

### MUSEM-05: Live acquisition sink + request lifecycle endpoints (<media-service>)
- **Priority:** Critical
- **Labels:** muse, acquisition, http, requests
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Wire the pieces into a real request lifecycle: replace the `NoopMediaRequestSink` with an
  `AcquisitionSink` that, on an approved request, runs targeted search (MUSEM-03) → decision (MUSEM-04) →
  qBittorrent grab (MUSEM-02) → persists `download_queue` + a `history_events` row, and add the auth-gated
  request endpoints. This is the <media-service> responsibility, native to Muse. It preserves the existing
  `classify_tier` safety gate — `AutoApprovable` grabs immediately, `NeedsReview` persists a request for
  operator approval, `Blocked` is rejected.

  ## FILES
  - `src/arr/request.rs` — add `AcquisitionSink` (a real `MediaRequestSink` impl) alongside the existing trait/mock
  - `src/acquisition/mod.rs` — new orchestrator: `fulfill_request(req) = search → decide → grab → persist`
  - `src/http/mod.rs` — new PROTECTED routes: `POST /requests`, `GET /requests`, `POST /requests/:id/approve`, `POST /requests/:id/deny`
  - `src/http/requests.rs` — the handlers
  - `README.md` — document the request lifecycle + endpoints

  ## APPROACH
  1. `AcquisitionSink::submit(draft)`: resolve the media kind → categories + a quality profile → `search_releases` → `decide_release` → on `Grab`, `QbitClient::add` → persist `download_queue` (status Queued, client hash from the receipt) + `record_history_event(Grabbed, …)`; on `Reject`, persist the request as `Failed`/`NeedsReview` with the reasons. All via the existing `classify_tier`/`submit_if_appropriate` seam so the tiered safety property is preserved (sink only called for `AutoApprovable`).
  2. Endpoints (all on the auth-gated `protected` router per the CAP-SEC posture — bearer required, side-channel-safe compare already in the codebase):
     - `POST /requests` — body {provider_ids, kind, title, quality_profile_id?}; classify_tier; `AutoApprovable` → fulfill now; else persist `media_requests` (status Requested).
     - `GET /requests` — list by status (auth-gated; never leak per-account data unauthenticated — the CAP-SEC-03 lesson).
     - `POST /requests/:id/approve` — operator approves a `Requested` item → `fulfill_request` → status Approved/Grabbed/Failed.
     - `POST /requests/:id/deny` — status Denied.
  3. A global master "acquisition enabled" settings gate (extend `ExperienceSettings`) — with it off, endpoints persist requests but never grab (fail-safe default OFF).
  4. Never write to *arr here — this sprint's grab path is Prowlarr+qBittorrent-native (the charter's Phase-2 direction), not a *arr `POST`.

  ## TEST PLAN
  - `compiler_build(module="muse", ref=<branch>, mode=test)` — tests pass.
  - Handler tests with a `MockDownloadClient` + mock search: `AutoApprovable` request → grab called once, `download_queue` + `history_events` rows written; `NeedsReview` → request persisted, grab NOT called; approve on a persisted request → grab; deny → status Denied, no grab.
  - Auth tests: all four routes return 401 without a token (tokenless-401 gate tests, matching the CAP-SEC-01/03 pattern); `GET /requests` never returns request data unauthenticated.
  - Master-gate off → no grab even for `AutoApprovable`.
  - Negative: a decision `Reject` → request `Failed` with reasons, no queue row; a qBittorrent error → request `Failed`, surfaced, service not crashed.
  - Verify no hardcoded infra values; no `std::env::var` for secrets.

  ## EDGE CASES
  - Double-approve / approve-then-deny races → idempotent status transitions (approving an already-Grabbed request is a no-op, not a second grab).
  - Missing quality profile for the kind → `NeedsReview` with a clear reason, never a blind grab.
  - `classify_tier` returning `Blocked` (no matching instance/kind capability) → 4xx with reason, no persistence of a fulfillable request.
  - Tiered-safety invariant preserved: the sink is structurally only reachable for `AutoApprovable` (assert via call-graph test, per the existing `submit_if_appropriate` tests).

- **Acceptance criteria:**
  - [ ] `AcquisitionSink` fulfills an approved request via search → decide → qBittorrent grab → persist queue + history
  - [ ] `POST /requests`, `GET /requests`, `POST /requests/:id/approve|deny` exist on the auth-gated protected router
  - [ ] Tokenless requests to all four routes return 401; `GET /requests` never serves request data unauthenticated
  - [ ] The `classify_tier` tiered-safety gate is preserved (sink only reached for `AutoApprovable`); master acquisition gate defaults OFF
  - [ ] Negative tests: decision-reject and download-client error both yield `Failed` with reasons, no crash, no phantom queue row
  - [ ] No hardcoded infrastructure values; secrets via config/SecretManager
  - [ ] README updated to document the request lifecycle + endpoints
  - [ ] All existing tests still pass

---

### MUSEM-06: Monitored "wanted" acquisition worker
- **Priority:** High
- **Labels:** muse, acquisition, worker
- **Agent:** claude
- **Estimate:** 5h
- **Description:** The Sonarr/Radarr background engine: periodically scan `monitored_items` that are missing or
  below cutoff, and for each run search (MUSEM-03) → decide (MUSEM-04) → grab (MUSEM-02) → enqueue, respecting
  the master acquisition gate, per-item auto-tier policy, and rate/concurrency caps. Runs inside the existing
  maintenance worker chain (dependency-ordered after ingest) or as its own capped worker.

  ## FILES
  - `src/acquisition/worker.rs` — the wanted-scan worker loop
  - `src/maintenance/mod.rs` — schedule the wanted pass in the dependency-ordered chain (after arr ingest, gated)
  - `src/acquisition/mod.rs` — shared `fulfill_for_monitored(item)` (reuse the MUSEM-05 orchestrator)
  - `README.md` — document the wanted worker + its gates

  ## APPROACH
  1. `run_wanted_pass`: `repo::acquisition::list_wanted(library)` → for each, if not searched within a cooldown (`last_search_at`), run the fulfill orchestrator; update `last_search_at`; cap total grabs-per-pass and searches-per-minute (reuse the Prowlarr limiter); write `history_events` for every grab.
  2. Gate hard on the master acquisition setting (default OFF) AND the per-item/profile auto-tier — a monitored item only auto-grabs when policy says so; otherwise it produces a `NeedsReview` `media_request` for the operator (never a silent grab), reusing `classify_tier`.
  3. Non-blocking + graceful: an unreachable Prowlarr/qBittorrent logs + continues to the next item (the `arr::ingest` degradation posture); a pass never aborts the maintenance chain.
  4. Idempotent: an item already in `download_queue` (active) is skipped, not re-grabbed.

  ## TEST PLAN
  - `compiler_build(module="muse", ref=<branch>, mode=test)` — tests pass.
  - Worker tests with mocks: a wanted item under an auto-tier profile → one grab + queue row; a wanted item without auto-tier → a `NeedsReview` request, no grab; an item already queued → skipped; master gate off → no grabs at all.
  - Cooldown: an item searched within the cooldown window is not re-searched; caps limit grabs-per-pass.
  - Negative: Prowlarr down → that item logged + skipped, pass completes for the rest.
  - Verify no hardcoded infra values.

  ## EDGE CASES
  - Concurrency: two passes never double-grab the same item (queue-membership check + `last_search_at` guard).
  - A monitored item whose metadata was deleted → skipped cleanly (NULLABLE ref), not a crash.
  - Empty wanted list → pass is a clean no-op.
  - Master gate toggled mid-pass → honored on the next item (read once per item, fail-safe OFF).

- **Acceptance criteria:**
  - [ ] The wanted worker scans monitored-and-missing/below-cutoff items and fulfills them via the shared orchestrator, capped + cooldown-guarded
  - [ ] Auto-tier policy honored: no silent grab; non-auto items become `NeedsReview` requests
  - [ ] Master acquisition gate (default OFF) hard-gates all grabbing; already-queued items are skipped (idempotent)
  - [ ] Non-blocking: an unreachable substrate skips the item and the pass completes; never aborts the maintenance chain
  - [ ] Negative test proves master-gate-off and no-auto-tier both prevent grabs
  - [ ] No hardcoded infrastructure values; secrets via config/SecretManager
  - [ ] README updated to document the wanted worker + gates
  - [ ] All existing tests still pass

---

## Program Roadmap (later sprints — NOT ingested until enriched)

Sprint 1 (above) delivers the acquisition WRITE-PATH: request → search → decide → grab (qBittorrent) →
monitor a wanted list. It deliberately stops BEFORE touching the library on disk. The remaining sprints,
in strangler order (each independently shippable, import last + hardest-gated per the charter):

- **Sprint 2 — Import / organize pipeline (the guarded "risky 80%").** Watch `download_queue` for completed
  downloads → parse/match → **dry-run first**, atomic hardlink/rename into the Plex library, permission-safe,
  hard-typed confirmation, full `history_events` audit, rollback. `src/plex_control` (currently an all-
  `NotImplemented` stub) gets its real library-refresh/scan-trigger. This is where Muse first mutates the
  library on disk — strongest gates, dry-run default, *arr kept as fallback until proven.
- **Sprint 3 — Lidarr / music parity.** Add the `Music` media kind end-to-end (artist/album/track model,
  Lidarr-shaped quality/format profiles, music indexer categories, the music grab+import path) — the schema
  provisioned nullable-for-music in MUSEM-01 gets exercised.
- **Sprint 4 — *arr retirement cutover + quality-profile/custom-format import.** Import the operator's live
  quality profiles + (recyclarr/TRaSH) custom formats from the 8 *arr instances into Muse's tables; run Muse
  acquisition in parallel/shadow against *arr; parity-diff (extend `src/parity`) until Muse is proven; then
  retire each *arr instance function-by-function. Only here does *arr fully retire.
- **Follow-on (operator's stated NEXT step, separate spec) — Ingest + HandBrake standardization.** Normalize
  acquired media into super-streamable formats to minimize on-the-fly transcoding — explicitly OUT of this
  program's scope, sequenced after library management is native.
