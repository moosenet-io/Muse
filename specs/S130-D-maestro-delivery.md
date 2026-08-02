# S130-D — Maestro: delivery (direct play, remux, sessions)

plane_project: MUSE
module: Maestro
prefix: MDLV
spec_id: S130-D-maestro-delivery

## Metadata
- **Author:** Moose
- **Session:** S130
- **Date:** 2026-08-01
- **Module version:** Maestro v0.1 (child spec D of `S130-maestro-epic.md`)
- **Repo / binary:** `moosenet/Muse`, second `[[bin]]` target `maestro` (epic §2). **Not a new repo.**
- **Estimated total:** ~42h autonomous agent work (11 items)
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 2–7. **Clause 1 is deliberately carved out for the media
  plane only** (epic §8.6): control goes through the Terminus gateway, bytes do not. See §0.2 — this
  is a documented decision with a stated cost, not an oversight.
- **Depends on:** `S130-B-maestro-backends.md` (`PlaybackBackend`, `BackendCapabilities`,
  `BackendMediaRef`, the `plex` adapter, sidecar skeleton, `proxy_maestro`),
  `S130-C-maestro-decision.md` (`DeviceProfile`, `PlaybackPlan`, `plan()`),
  `S130-A-maestro-probe.md` (real `MediaInfo` — MDLV-05 derives `Content-Type` from the probe, not
  the file extension).
- **Blocks:** `S130-E-maestro-transcode.md`, `S130-G-maestro-player-gui.md` (full player).
- **Context:** This is the milestone that makes the epic real. Per epic §6, most playback needs no
  transcoding at all — the right answer for an H.264/AAC file on a client that plays H.264/AAC is
  *serve the bytes and get out of the way*. Spec D delivers the **native** data plane: the first two
  tiers (DirectPlay, Remux), signed stream URLs, the `PlaybackSession` model every later tier reuses,
  and the durable event path that feeds Muse's taste model. It does **not** proxy Plex bytes — see
  §0.3, a withdrawn decision that must not be re-proposed. When D lands, the epic's proof point is
  reachable: **cast a movie from constellation-web to a TV and have the progress land in Muse's taste
  model — with no transcoder in existence.**

---

## 0. Ground rules this spec inherits and makes concrete

### 0.1 One repo, two binaries, one database — and a hard ownership asymmetry

Per epic §2, **Maestro is a second binary inside `moosenet/Muse`.** One crate, two `[[bin]]` targets
(`muse`, `maestro`), Maestro's modules under `src/maestro/`, shared `models/`, `config.rs`, `repo/`,
`error.rs`, one `Cargo.lock`, one OCI image with two bins. Crash isolation comes from two processes
and two systemd units. Every `## FILES` path below is in the Muse tree.

That makes several things free — `src/streaming/ffmpeg.rs` is imported rather than ported,
`src/foundry/paths.rs`'s `PathGuard` is generalised rather than reimplemented, and the "Muse
resolution API" of epic §8.6 is an in-crate function over the read-only pool rather than an HTTP hop
on the hot path of every playback. It also creates exactly one hazard, and it is the one to keep
watching for: **sharing a repo must not become sharing ownership.**

| Direction | Mechanism | Why |
|---|---|---|
| Library → Maestro (item → `BackendMediaRef`, probe `MediaInfo`, account mapping) | **Direct `maestro_ro` query, no HTTP on the playback hot path** | Hot path; shared types; nothing is decided, only looked up |
| Maestro → watch state (start/progress/stop) | **HTTP to Muse's ingest, via a durable outbox**, authenticated by `MAESTRO_MUSE_TOKEN` | Muse must remain the **single writer** of watch state (epic §2) |

`MAESTRO_MUSE_TOKEN` authenticates **only** that reverse direction. There is no forward HTTP call:
item resolution is a database read, `BackendMediaRef` is the type contract, and no playback request
ever waits on an HTTP round-trip to Muse.

**Two roles, two DSNs, two pools — selected by operation** (epic, post-review). A single role with
mixed grants was the earlier design and it blurred exactly the line this split exists to draw:

| Role | DSN | Grants |
|---|---|---|
| `maestro_ro` | `MAESTRO_DATABASE_URL_RO` | `SELECT` on `media_items`, `media_files`, `accounts`, `browser_account_map`. **Nothing at all** on taste, embedding, or play-event tables |
| `maestro_rw` | `MAESTRO_DATABASE_URL_RW` | `SELECT`, `INSERT`, `UPDATE`, `DELETE` on `playback_sessions` and `maestro_event_outbox` **only**. No grant on any library table |

Two properties fall out of that, and both are enforced by Postgres rather than by discipline:

1. **Maestro cannot become taste-aware.** Not "must not" — *cannot*. `maestro_ro` has no `SELECT`
   on watch state or embeddings, so the tempting shortcut ("just read what's watched, it's right
   there") fails at the query rather than passing review. This is what makes epic §2's one-direction
   rule structural instead of aspirational, and it is why the events client in MDLV-09 has no read
   method to pair with.
2. **Maestro cannot touch the library.** `maestro_rw` has no grant on `media_items` and friends, so
   the session/outbox pool physically cannot write a library row even if a bug tried.

**`maestro_rw` must have `SELECT` on its own two tables — this is a deliberate correction, not an
oversight to tighten later.** An earlier draft granted only the write verbs. Because `maestro_ro`
cannot read those tables either, that combination has **no role capable of reading a session or an
outbox row**: session retrieval, `list_active`, the reaper's idle scan, and the outbox drain would
each have failed at their first query — a runtime-fatal bug that no amount of code review of the
Rust would have caught, because the defect was in the grant. A role that owns a table must be able
to read it. If a future reviewer proposes stripping `SELECT` from `maestro_rw` in the name of least
privilege, the answer is no, and this paragraph is why.

If a future item needs Maestro to change something in Muse's world, the answer is an HTTP call,
never a widened grant.

### 0.2 Control plane vs media plane — the documented Module Contract carve-out

Epic §8.6's corollary, stated here because this spec is where it becomes code:

- **Control plane** — session start/stop, transport, status, `GET /backends` — goes through the
  Terminus gateway (`proxy_maestro`, `CONSTELLATION_MAESTRO_TOKEN`). Module Contract clause 1 holds
  in full.
- **Media plane** — the actual video bytes — is served **direct from Maestro**, never through
  Terminus. Routing sustained video through the tool-hub process would couple playback uptime to
  Terminus restarts, put a film's worth of throughput through the process that arbitrates the whole
  fleet's tools, and trade away exactly the crash isolation this epic exists to buy. A Terminus
  redeploy must not stutter a movie.

That carve-out costs the media plane its cookie/bearer path, which is why signed URLs (MDLV-04) are
not a convenience but the mechanism that makes the split safe. **State the carve-out explicitly in
the module docs and the README** — an undocumented deviation from clause 1 reads as a bug to the
next reviewer; a documented one is a decision.

### 0.3 There is no Plex byte-proxy — a withdrawn decision, recorded so it is not re-proposed

An earlier draft of epic §8.6 had Maestro be the data plane for **every** backend, reverse-proxying
Plex's stream so `start_session` returned a Maestro URL regardless of backend. This spec was written
against that text and carried an item for it. **That decision has been withdrawn and the item is
cut.** §8.6 now reads: **`plex` mode = control + observe, no bytes through Maestro; `native` mode =
bytes.** Spec B cut its equivalent (MBAK-09) for the same reason, exactly as §8.5 cut the
jellyfin/emby adapters.

**Why, so nobody rediscovers the idea and re-proposes it.** Proxying Plex means re-streaming its HLS
output against the `transcode/universal/*` endpoints — undocumented, token-lifecycle-bound,
keepalive-sensitive, and changed without notice. That is weeks of brittle reverse-engineering spent
polishing the precise component the strangler fig exists to *replace*, and it would need re-doing
every time Plex ships. The benefits it promised were real but small by comparison: keeping
`X-Plex-Token` off the browser (which control-plane-only mode also achieves, since no client-side
Plex stream URL is minted by Maestro at all) and making the eventual native swap invisible (which
matters for a path we intend to retire).

**Three concrete consequences for this spec:**

1. **`SessionResponse.stream_url` is `Option<String>`**, accompanied by
   `playback_mode: "maestro_stream" | "backend_controlled"`. Native tiers carry a signed URL; `plex`
   sessions carry none and the client plays via the backend's own control surface (spec B/G). The
   response says which, honestly, rather than fabricating a URL that would 404.
2. **The media plane is native-only.** Every handler in MDLV-05/06 serves a local file or an ffmpeg
   pipe. No item ships code whose purpose is proxying another server's bytes.
3. **`ByteSource` (MDLV-03) survives, deliberately, with one real implementation.** A trait with a
   single impl is speculative generality *only* when nothing else consumes the seam. Here two things
   do: the mock source that lets the range-conformance suite run with no filesystem, and spec E's
   segment source, which is the genuine second implementation and is next. If E is ever cut, collapse
   the trait then — do not keep it on principle.

### 0.4 Five inherited requirements, made concrete in items

| Epic requirement | Where it lands |
|---|---|
| Path safety = Foundry's allowlist **verbatim**; outside a root is `403` **even if Muse asked** | MDLV-02 |
| `account_id` is **Muse's account id** via `browser_account_map`, never the cookie session (§8.1) | MDLV-01, MDLV-07 |
| Resume position originates in Muse and is never authoritative in Maestro | MDLV-07 (see §0.5) |
| Durable outbox, retry, dedupe keys, `"v":1` payload from the first commit (§10b) | MDLV-09 |
| TTFF budgets (direct play < 1s), `FakeBackend`, chaos-test the isolation claim (§10b) | MDLV-11 |

### 0.5 Resume moves to the caller — a collision the two-role grant model forces

Two inherited requirements now conflict, so this spec resolves it explicitly rather than letting an
implementer discover it at the first failed query:

- "Resume position read from Muse at session start" implies Maestro reads watch state.
- `maestro_ro` has **no `SELECT` on play-event or watch tables** — deliberately, so Maestro cannot
  become taste-aware (§0.1).

An earlier draft of MDLV-07 read the last `play_sessions` position through the read-only pool. **That
is now impossible, and the grant model is right.** Resume position *is* watch state; a component
walled off from watch state cannot read it, and carving a per-column exception would reopen the exact
door §0.1 closes — "just this one field" is how a wall becomes a suggestion.

**Resolution: the caller resolves resume and passes it.** constellation-web is already talking to
Muse through `proxy_muse` to render the item it is about to play; it fetches the resume position on
that same control-plane call and sends it as `start_position_ms` when opening the session. Maestro's
fallback is 0.

This satisfies the requirement as written — the position originates in Muse, at session start, and is
never authoritative in Maestro — while removing the last reason Maestro would need any grant on watch
state. It is also simpler: one fewer query on the open path, one fewer coupling, and the component
that knows what the user was looking at is the one that supplies where they left off. The trade is
that a caller which forgets to send it starts from the beginning; MDLV-07 records that as a
deliberate, benign default rather than a failure.

---

## 1. The payoff: seeking is a range request

The single most important consequence of building this tier first, and the thing to keep in front of
every reviewer:

**A client seeking in a direct-play file needs no special handling whatsoever.** The player issues a
new `GET` with a different `Range:` header, Maestro answers `206 Partial Content` from a fresh seek
on the byte source, and playback resumes. There is no seek endpoint, no segment realignment, no
"restart the transcode at offset T", no keyframe search, no in-flight ffmpeg to kill. Scrubbing,
resume-from-position, and skip-intro are all the same one mechanism, and it is a mechanism HTTP
already specifies.

Every genuinely hard problem in playback engineering — seek-during-transcode, segment alignment,
throttling a producer that outruns its consumer — belongs to spec E and **exists only for the
fraction of the library that cannot direct-play**. Spec A's backfill sizes that fraction; MDLV-11's
tier-distribution metric measures what actually happens. If direct play dominates in real household
use, spec E is an edge case rather than the centrepiece, and that is a scheduling decision worth
making from data rather than from assumption.

This is also why the remux tier (MDLV-06) is honestly labelled non-seekable: a `-c copy` pipe is a
live producer, `Accept-Ranges: none`, and seeking means tearing down the child and starting a new
one at a new `-ss`. That asymmetry is not a defect to paper over — it is a standing argument for the
decision engine preferring DirectPlay whenever it legitimately can.

---

## 2. The delivery surface

```
CONTROL PLANE  (through Terminus proxy_maestro, bearer-authenticated)
  POST /playback/sessions          open a session → id, tier, SIGNED stream URL, resume position
  GET  /playback/sessions          active sessions (spec H's Activity feed)
  GET  /playback/sessions/{id}     one session's state
  POST /playback/{id}/heartbeat    liveness + position + state → ack, refreshed URL if near expiry
  POST /playback/{id}/stop         explicit close
  GET  /backends                   capability descriptors (epic §8.6)

MEDIA PLANE    (native backend only — direct from Maestro, signed URL only, never through Terminus)
  GET/HEAD /playback/{id}/stream?exp=…&sig=…   bytes: local file or remux pipe
```

A `plex` session has **no media-plane route at all** (§0.3): its `SessionResponse` carries
`playback_mode: "backend_controlled"` and no `stream_url`, and the control plane above is the whole
of its surface.

**The byte path names a session and carries a signature. It never names a file, an item, or a
path.** By the time `/playback/{id}/stream` runs, the on-disk path was resolved,
symlink-canonicalised, and root-confined at session-open time and is held inside an opaque handle.
A directory-traversal payload has nowhere to go because there is no parameter to put it in.

**Error mapping (uniform across every tier and backend):**

| Condition | Status | Notes |
|---|---|---|
| Missing / malformed / expired / bad signature | `403` | Never `401` — there is no credential to re-present on this plane |
| Unknown session id | `404` | Never distinguishes "never existed" from "not yours" |
| Session reaped or stopped | `410 Gone` | Distinct from 404 so the client re-opens instead of retrying |
| Concurrent-session cap reached at open | `429` + `Retry-After` | MDLV-08 |
| Resolved path outside the allowlist | `403` | **Even if Muse's own row asked for it** (§10b) |
| Plan requires transcode | `501` | `Unsupported` from the backend until spec E |
| Range unsatisfiable | `416` + `Content-Range: bytes */{len}` | MDLV-03 |
| File vanished since open | `410 Gone` | |
| Database unreachable at open | `503` | Cannot resolve item → media ref |
| Stream requested on a `backend_controlled` session | `404` | A `plex` session has no media plane (§0.3) |
| ffmpeg binary absent | `501` | `classify_spawn_error` → `BinaryMissing` |
| ffmpeg spawn failed (perms, rlimit) | `503` | `SpawnError` — transient |

---

## 3. Items

### MDLV-01: `PlaybackSession` model, migration, and repo
- **Priority:** Critical
- **Labels:** maestro, session, db
- **Agent:** claude
- **Estimate:** 4h
- **Description:** The durable session record every later item reads and every later spec extends.
  Persisted rather than in-memory because it must survive a Maestro restart long enough for MDLV-08's
  orphan sweep to close what the restart abandoned, and because spec H's Activity panel and the
  assistant's "what's playing" tool both query it.

  **`account_id` is Muse's account id** (epic §8.1, corrected) — the same id-space the taste model
  already joins on, and the same one `play_sessions.account_id` uses today. It is emphatically **not**
  the constellation-web cookie session: that session carries roles (`operator|viewer`), not household
  members, and resolving an account from it would mint a third id-space matching neither Plex accounts
  nor Muse accounts. Taste attribution — the entire reason the field exists — would then silently fail
  to join, producing a system that looks like it is learning and is not. The cookie subject is mapped
  to a Muse account through `browser_account_map` (MDLV-07); a real identity service unifies them
  later as its own spec. Same field, day one, in the id-space taste actually uses.

  **Where the table lives, and why.** A Maestro-owned table in the **same** Postgres database, created
  by the shared `migrations/` sequence (epic §10b: "sessions: ephemeral, Maestro-owned tables in the
  shared Postgres"). Not a separate database — the row references `media_items.id`/`episodes.id`/
  `accounts.id`, and putting those across a database boundary costs referential integrity, a second
  pool, a second migration path, and a second backup for no benefit. Not a Muse-owned table either:
  **Maestro is the only writer**, and Muse's read of it (spec H) is a plain `SELECT`.

  **The owning role is `maestro_rw`, and it holds `SELECT` as well as the write verbs** (§0.1). That
  is load-bearing rather than lax: `maestro_ro` has no grant on this table, so if `maestro_rw` were
  write-only there would be **no role able to read a session at all** — `get`, `list_active`, and the
  reaper's idle scan would each fail at their first query. The migration's `GRANT` statement carries a
  comment saying so, because the next person to audit privileges will otherwise read `SELECT` here as
  something to trim.

  ## FILES
  - `migrations/{next}_playback_sessions.sql` — table, enum types, indexes
  - `src/models/playback_session.rs` — `PlaybackSession`, `NewPlaybackSession`, `SessionState`,
    `PlaybackTier`
  - `src/repo/playback_session.rs` — create / get / list_active / update_position / update_state /
    close / reap / prune
  - `src/models/mod.rs`, `src/repo/mod.rs` — registration

  ## APPROACH
  1. **Do not confuse this with `src/models/play_session.rs`.** That is Muse's *reconstructed watch
     history* (Tautulli parity, written by `tracker::reconstruct`). `PlaybackSession` is Maestro's
     *live playback session*. Different lifetimes, different owners, different tables; MDLV-09/11 are
     the bridge. Say so in both modules' doc comments so a future reader does not merge them.
  2. Schema (`playback_sessions`):
     - `id UUID PRIMARY KEY` — client-opaque, non-enumerable. Never a serial: the id appears in a URL
       a browser and a Cast receiver both hold, and a guessable id is a byte-serving capability.
     - `account_id BIGINT NULL REFERENCES accounts(id) ON DELETE SET NULL` — Muse's account id
       (above). Nullable = household attribution.
     - `media_item_id BIGINT NULL`, `episode_id BIGINT NULL` — FKs; exactly one non-null, CHECK-enforced.
     - `backend TEXT NOT NULL` (`native` | `plex` | …), `media_ref JSONB NOT NULL` — the resolved
       `BackendMediaRef` (epic §8.6): `FilePath{..}` for native, `PlexRatingKey(..)` for a
       control-only `plex` session. Stored so the byte path never re-resolves and never sees a
       client-supplied value.
     - `device_id TEXT NULL`, `device_profile JSONB NOT NULL` — the spec-C profile used to plan.
       Stored, not recomputed: a plan must stay explicable, and a profile that changed underneath
       would make the recorded tier a lie.
     - `plan JSONB NOT NULL`, `tier playback_tier NOT NULL` — the chosen `PlaybackPlan` plus its tier
       lifted out as a queryable enum (`direct_play` | `remux` | `partial_transcode` |
       `full_transcode`) so MDLV-11's tier distribution is a `GROUP BY`, not a JSON scan.
     - `position_ms BIGINT NOT NULL DEFAULT 0`, `duration_ms BIGINT NULL`,
       `start_position_ms BIGINT NOT NULL DEFAULT 0` (the resume point Muse supplied — recorded for
       explicability, never treated as authoritative; see MDLV-07).
     - `state playback_state NOT NULL` (`playing` | `paused` | `buffering` | `stopped`).
     - `started_at`, `last_heartbeat_at`, `stopped_at NULL`, `stop_reason TEXT NULL`
       (`client` | `idle_timeout` | `orphan_sweep` | `error`).
     - `bytes_served BIGINT NOT NULL DEFAULT 0` — persisted so a restart does not zero accounting.
     - Indexes: partial on `state` (hot `list_active`), on `last_heartbeat_at` (the reaper),
       `(account_id, started_at DESC)`.
  3. Enums as `sqlx::Type` with `#[serde(rename_all="snake_case")]`, matching the `DecisionKind`
     convention already in `src/models/play_session.rs`.
  4. Repo functions take `&PgPool` (the caller passes `pool_rw` — these tables are unreachable from
     `pool_ro`) and return `MuseResult<_>`. `close(id, reason)` is **idempotent** —
     closing an already-closed session is `Ok`, never an error, because MDLV-08's reaper and an
     explicit client stop legitimately race.
  5. `list_active()` returns `playing | paused | buffering`. A stopped session is history, and history
     is Muse's (epic §2) — Maestro keeps closed rows only for `MAESTRO_SESSION_RETENTION_DAYS`
     (default 7) and prunes beyond it.

  ## TEST PLAN
  - `cargo test` — serde round-trips for both enums (mirrors `decision_kind_serde_round_trip`)
  - Live-DB test gated on the existing `MUSE_TEST_DATABASE_URL`, skipping cleanly when unset — one
    gate variable for one crate, not a second Maestro-specific one
  - Round trip: create → get → update_position → close; assert `close` is idempotent
  - CHECK constraint rejects both-set and both-null item references
  - `media_ref` round-trips both `FilePath` and `PlexRatingKey` variants

  ## EDGE CASES
  - Two `close` calls racing (client stop + reaper) — last-write-wins on `stop_reason`, no error
  - `account_id` NULL — every downstream query treats it as household, never panics or drops the row
  - Clock skew — `last_heartbeat_at` is always server `now()`, never a client-supplied timestamp
  - An account deleted mid-session — `ON DELETE SET NULL` degrades the session to household rather
    than cascading a live playback row out of existence

- **Acceptance criteria:**
  - [ ] Migration applies cleanly and is idempotent on re-run
  - [ ] The migration grants `maestro_rw` **`SELECT`, `INSERT`, `UPDATE`, `DELETE`** on
        `playback_sessions` — `SELECT` included, with the in-migration comment explaining why
        (§0.1); a live `get`/`list_active` under the `maestro_rw` DSN proves it
  - [ ] `account_id` is Muse's account id (FK to `accounts`), never a cookie-session identifier
  - [ ] `media_ref` stores the resolved `BackendMediaRef` for both native and control-only backends
  - [ ] `close()` is idempotent and safe under a stop/reap race
  - [ ] `PlaybackSession` is distinct from `PlaySession`; neither is modified to accommodate the other
  - [ ] Crate test suite passes with no database configured
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDLV-02: Path safety — generalise Foundry's `PathGuard`, do not reimplement it
- **Priority:** Critical
- **Labels:** maestro, security, library
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Make a path outside the configured library roots structurally unable to be served
  — **including one Muse's own database row asks for**. Epic §10b is explicit: a resolved path
  outside the allowlist is a `403` even if Muse asked. Muse being compromised, or simply carrying a
  bad row from a botched import, must not make Maestro an arbitrary-file-read oracle.

  **This already exists in this crate and must be reused verbatim, not rebuilt.**
  `src/foundry/paths.rs` (MUSEF-01) is exactly the right primitive and was already reviewed as such:
  `ResolvedPath` has a private field and no public constructor, `PathGuard::resolve` is the only way
  to obtain one, and resolution is symlink-canonicalise-**then**-check (not check-then-use), which is
  the load-bearing ordering — a `..` component or a symlink pointing out of the library passes a
  textual prefix test and then escapes at `open(2)`. A second implementation of this under
  `src/maestro/` would be the single worst outcome of this spec: two allowlists that drift, one of
  them less reviewed.

  ## FILES
  - `src/paths.rs` (new) — `PathGuard`, `ResolvedPath`, `PathError` **moved** from
    `src/foundry/paths.rs` with their tests, unchanged in behaviour
  - `src/foundry/paths.rs` — becomes a re-export so every existing Foundry call site is untouched
  - `src/foundry/config.rs` — construct its guard from the moved type (mechanical)
  - `src/maestro/library/handle.rs` — `MediaHandle` wrapping a `ResolvedPath`
  - `src/maestro/library/resolve.rs` — read-only lookup → guard → handle
  - `src/config.rs` — `MAESTRO_MEDIA_ROOTS` (defaulting to `MUSE_FOUNDRY_ALLOWED_ROOTS` when unset),
    `MAESTRO_DATABASE_URL_RO`, `MAESTRO_DATABASE_URL_RW`
  - `src/maestro/db.rs` — the two pools and the startup grant probe

  ## APPROACH
  1. Move, do not copy. `PathGuard::new` is currently `pub(in crate::foundry)` — deliberately, because
     "code that can mint its own guard with arbitrary roots has bypassed configuration entirely."
     Preserve that property while widening the module: keep construction private to `crate::paths` and
     expose exactly two blessed constructors — `PathGuard::for_foundry(&Config)` (existing behaviour,
     mutation-gated) and `PathGuard::for_maestro(&Config)` (**`enable_mutation: false`, always**).
     Maestro cannot construct a mutating guard; the capability is not reachable from `src/maestro/`.
  2. `MediaHandle { path: ResolvedPath, size: u64, mtime: SystemTime, container: String,
     media_info: Option<MediaInfo> }` — all fields private, **no public constructor**. The sole
     constructor is `library::resolve::resolve_item(&PgPool /* pool_ro */, &PathGuard, item_ref) ->
     MuseResult<MediaHandle>`, and `item_ref` is `MediaItemRef { media_item_id: Option<i64>,
     episode_id: Option<i64> }` — integers only. There is no string-path input anywhere in this
     module's public API.
  3. Resolution, all inside `resolve_item`:
     a. Read the attached `media_files` row through Muse's existing `repo::media_file::list_for_episode`
        / `list_by_media_item` — the same calls `streaming::resolve_file_path` already makes. A
        `SELECT`, nothing more. Never scan, never cache metadata, never add a provider (epic §2).
     b. Join via `streaming::ffmpeg::join_media_path` — the function, called directly, not a copy.
     c. `PathGuard::resolve()` — symlink resolution + default-deny root confinement. **Its `Err` is a
        `403`, and the log line says the path came from the database**, so an operator can tell a
        misconfigured root from a bad library row.
     d. Reject a non-regular file (directory, fifo, device, socket).
     e. Carry the probe's `MediaInfo` (spec A) onto the handle — MDLV-05 needs it for `Content-Type`.
  4. **Two pools, selected by operation, and a startup probe that asserts both** (§0.1). The `maestro`
     binary builds `pool_ro` from `MAESTRO_DATABASE_URL_RO` and `pool_rw` from
     `MAESTRO_DATABASE_URL_RW`, both via `SecretManager::get()` (S7). Library resolution takes
     `&pool_ro` — it is a compile-time-visible parameter, so a handler cannot accidentally resolve an
     item through the writable pool. Session and outbox work takes `&pool_rw`.

     The probe runs before the listener binds and checks **four** things, each inside a transaction it
     rolls back:
     - `pool_ro` **can** `SELECT` from `media_files` → else fatal; nothing works without it.
     - `pool_ro` **cannot** write `media_files` → a successful write is a loud over-privilege warning.
     - `pool_rw` **can** `SELECT` from `playback_sessions` → **fatal if it cannot.** This is the exact
       misconfiguration the review caught (§0.1): with a write-only `maestro_rw` and a `maestro_ro`
       that has no grant here, every session read fails. Failing at startup with a message naming the
       missing grant turns a baffling runtime symptom — "sessions open fine and then vanish" — into a
       one-line fix.
     - `pool_rw` **cannot** `SELECT` from `media_files` → a successful read means the roles were
       provisioned as one, and the taste-awareness wall (§0.1) is not actually there. Loud warning.

     Two of these are positive assertions and two negative, which is the point: a privilege model is
     only real if both what it permits and what it forbids are checked. A guarantee nobody checks is a
     guarantee that quietly stops being true.
  5. Fail closed: no usable root ⇒ the guard is inert and resolves nothing, and the media plane
     registers as unavailable rather than serving from an empty allowlist (`PathGuard::is_inert`
     already models this exactly).

  ## TEST PLAN
  - `cargo test` — the moved `paths.rs` tests pass **unchanged** (proof of a move, not a rewrite)
  - New tests over a `tempfile` root, with the path supplied as if from the database:
    - `..` escaping to a parent → `403`
    - Absolute stored path (`/etc/passwd`) → `403`
    - Symlink inside the root pointing outside → `403` after canonicalisation
    - Sibling-prefix root (`…/media-backup` vs `…/media`) → `403` (component compare, not string prefix)
    - Empty root list → everything rejected, guard inert
    - Legitimate file → resolves with correct size/container
  - `PathGuard::for_maestro` always yields `mutation_enabled() == false` (negative test)
  - Grep-style test: `src/maestro/` contains no `INSERT`/`UPDATE`/`DELETE` against a library or taste
    table, no `SELECT` against a taste/embedding/play-event table, and no second path-confinement
    implementation
  - Grant-probe tests against a live DB with both DSNs configured (gated, skipping when unset):
    all four assertions fire correctly, and a deliberately write-only `maestro_rw` fixture makes
    startup **fail with a message naming the missing `SELECT`** rather than starting and breaking later
  - Live-DB test gated on `MUSE_TEST_DATABASE_URL`: seed item + media_file (the pattern already in
    `src/streaming/mod.rs`'s live-DB test), resolve, assert path and container

  ## EDGE CASES
  - Item with no attached file — `Ok(None)`-shaped unresolvable → `404`, the same posture
    `streaming::resolve_file_path` already takes
  - Library mount temporarily absent — `PathGuard::new` already drops unreachable roots with a warning
    rather than failing startup; resolution then `403`s and the health probe reports degraded
  - File replaced between resolve and open (an upgrade/repack) — MDLV-05's ETag revalidation catches it
  - TOCTOU: documented as out of scope, exactly as `foundry/paths.rs` already documents it — the
    threat model is our own bugs and operator misconfiguration on a single-tenant fleet

- **Acceptance criteria:**
  - [ ] `PathGuard`/`ResolvedPath` are **moved and shared**, not reimplemented; Foundry's call sites
        and tests are unchanged
  - [ ] Maestro's guard is non-mutating by construction (negative test)
  - [ ] A path outside the allowlist is `403` **even when it came from Muse's database** (test)
  - [ ] `MediaHandle` cannot be constructed outside `library::resolve`; no public API takes a path
  - [ ] Library resolution runs on `pool_ro`; sessions and outbox on `pool_rw` — the pool is an
        explicit parameter, not ambient state
  - [ ] The startup probe asserts all four grant properties (§0.1), is **fatal** when `maestro_rw`
        cannot `SELECT` its own tables, and warns loudly on either over-privilege
  - [ ] `maestro_ro` has no `SELECT` on taste, embedding, or play-event tables — Maestro *cannot*
        become taste-aware, not merely must not (grep-style + probe)
  - [ ] Unconfigured roots ⇒ inert guard, media plane unavailable, never an open allowlist
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDLV-03: Pure range arithmetic over an abstract byte source
- **Priority:** Critical
- **Labels:** maestro, http, pure
- **Agent:** codex
- **Estimate:** 4h
- **Description:** Parse a `Range` header and resolve it against a known content length — pure, no
  I/O, no axum types, exhaustively unit-tested with golden cases. Epic §7.3's discipline applied to
  the place it is cheapest to get right, and for the usual reason: a byte-offset bug *presents* as
  "the video is corrupt at 40 minutes" and *is* an off-by-one in a suffix-range calculation.

  This item also defines the `ByteSource` seam MDLV-05 implements. **It has one real implementation
  today and that is deliberate** (§0.3): the second consumer is the in-memory mock that lets the
  range-conformance suite run with no filesystem, and the genuine second implementation is spec E's
  segment source, which is next. If E is ever cut, collapse the trait then rather than keeping it on
  principle.

  ```rust
  #[async_trait]
  pub trait ByteSource: Send + Sync {
      fn total_len(&self) -> Option<u64>;         // None = unknown/live (remux)
      fn validator(&self) -> Option<Validator>;   // strong ETag + Last-Modified
      async fn open_range(&self, r: Option<ResolvedRange>) -> MuseResult<BoxBody>;
  }
  ```

  Range arithmetic is where naive implementations get subtly wrong things that only surface on real
  TV firmware: inclusive-vs-exclusive end bytes, `bytes=-500` meaning *the last 500 bytes* rather
  than *from 500 on*, an end past EOF being clamped rather than rejected, and a zero-length file
  having no satisfiable range at all.

  ## FILES
  - `src/maestro/http/range.rs` — `parse_range`, `RangeSpec`, `ResolvedRange`, `RangeOutcome`
  - `src/maestro/http/source.rs` — the `ByteSource` trait + `Validator`

  ## APPROACH
  1. `parse_range(&str) -> Result<Vec<RangeSpec>, RangeError>` per RFC 9110 §14.1: unit must be
     `bytes` case-insensitively (any other → `Unsupported`); `bytes=0-499` → `FromTo(0,499)`
     (**end inclusive**); `bytes=500-` → `From(500)`; `bytes=-500` → `Suffix(500)` (the LAST 500
     bytes); ABNF whitespace tolerance; malformed → `Malformed`.
  2. `resolve(specs, content_length) -> RangeOutcome` — `Full` | `Partial(ResolvedRange)` |
     `Unsatisfiable`. `From(s)` with `s >= len` → unsatisfiable. `FromTo(s,e)` with `e >= len` →
     clamp `e` to `len-1` (RFC: clamp, do not reject). `Suffix(n)` with `n >= len` → whole file.
     `Suffix(0)` → unsatisfiable. `content_length == 0` → nothing satisfiable.
  3. **Documented single-range-only policy.** `multipart/byteranges` is not implemented. A
     multi-range request resolves to `Full` and the caller answers `200` with the entire body — which
     RFC 9110 §14.2 explicitly permits ("a server MAY ignore the Range header"), which every player
     handles, and which is strictly more honest than the common alternative of serving only the first
     range under a `206` claiming to satisfy all of them. No client in the closed device matrix
     (epic §6) issues multi-range for video. Asserted by a test, stated in the module doc, documented
     in the README.
  4. `ResolvedRange::content_range_header(total)` → `bytes {start}-{end}/{total}`; the unsatisfiable
     form is `bytes */{total}`.
  5. `Validator { etag: String, last_modified: SystemTime }` with `matches(&str) -> bool` for
     `If-Range`/`If-None-Match`, **strong comparison only** — a weak validator is not usable with
     `If-Range`, and pretending otherwise produces a corrupt splice.

  ## TEST PLAN
  - `cargo test` — golden table: `0-`, `0-0`, `0-499`, `500-999`, `500-`, `-500`, `-0`, `-99999`,
    `999-500` (inverted → malformed), `0-499,600-999` (multi → Full), `items=0-499` (bad unit → Full),
    `abc` (malformed), empty, `bytes=` (malformed) — each against content lengths 0, 1, and 1000
  - Property check: for any satisfiable resolution,
    `start <= end_inclusive < len` and `len == end_inclusive - start + 1`
  - No panic on any malformed or overflowing input (`bytes=99999999999999999999-` saturates)
  - The conformance suite runs against **both** a real file source and an in-memory mock source, so
    the range semantics are proven independently of the filesystem — and so spec E inherits a suite
    its segment source can be dropped straight into

  ## EDGE CASES
  - Zero-length file — nothing satisfiable; `416` with `bytes */0`
  - `bytes=0-0` — a legal 1-byte range and a real pattern (players probe with it)
  - `total_len() == None` (a live remux pipe) — any `Range` resolves to `Full`; the tier advertises
    `Accept-Ranges: none` so a conformant client will not ask
  - `Range` on a `HEAD` — parsed identically; MDLV-05 answers `206` headers with no body

- **Acceptance criteria:**
  - [ ] Every golden case resolves to the documented outcome at all three content lengths
  - [ ] Multi-range resolves to `Full`; the policy is in the module doc and the README
  - [ ] No panic on malformed or overflowing input (negative test)
  - [ ] The conformance suite runs against both the file source and an in-memory mock source
  - [ ] Module is pure — no I/O, no axum types
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDLV-04: Signed, session-scoped, expiring stream URLs
- **Priority:** Critical
- **Labels:** maestro, security, auth
- **Agent:** claude
- **Estimate:** 4h
- **Description:** The mechanism that makes §0.2's media-plane carve-out safe, and — per epic §8.7 —
  a thing that must be decided **now**, not when the Cast receiver breaks.

  The constraint is not negotiable and is not ours: **`<video>` and native Safari cannot set an
  `Authorization` header on media fetches, and a Cast receiver holds no Terminus cookie.** A
  Chromecast is handed a URL and nothing else. So the credential has to be *in the URL*.
  Cookie-through-proxy remains fine for the browser; signed URLs are the only thing that works for
  Cast, and retrofitting them later is a full delivery-path rewrite touching every handler, the
  session model, the GUI, and the receiver.

  ## FILES
  - `src/maestro/auth/signing.rs` — mint + verify (pure over an injected clock and key)
  - `src/maestro/http/mod.rs` — the media-plane verification layer
  - `src/config.rs` — `MAESTRO_STREAM_URL_TTL_SECS`, `MAESTRO_PUBLIC_BASE_URL`,
    `MAESTRO_CLOCK_SKEW_SECS`
  - Secrets: `MAESTRO_STREAM_SIGNING_KEY` (+ `_PREVIOUS` for rotation), via `SecretManager::get()`

  ## APPROACH
  1. Token = base64url(HMAC-SHA256(key, canonical)) where `canonical = "v1|{session_id}|{exp_unix}"`.
     URL shape: `{MAESTRO_PUBLIC_BASE_URL}/playback/{session_id}/stream?exp={exp}&sig={sig}`.
     Query parameters rather than a path segment: Cast senders, `<video src>`, and every HTTP client
     handle a query cleanly, and it keeps the route table identical to the unsigned shape. Version
     the canonical string (`v1|`) from the first commit so the scheme can change without a flag day.
  2. **Scope is the session, and that is the entire containment argument.** A leaked URL grants
     exactly one item, to exactly one session, until `exp` — and it dies the moment the session
     closes (`410`), which MDLV-08's idle reaper guarantees happens. It is not a library-wide
     capability and cannot be extended into one, because there is no item or path in the signed
     material to alter.
  3. TTL default 6h (`MAESTRO_STREAM_URL_TTL_SECS = 21600`). A token that expires mid-film is a bug,
     not security; the session lifecycle is the real bound and it is tighter. MDLV-08's heartbeat ack
     returns a **refreshed URL** when under 25% of TTL remains, so a long session renews without the
     player ever seeing a failure.
  4. Verification, in a `route_layer` on the media plane only: parse `exp` → reject if past (with
     `MAESTRO_CLOCK_SKEW_SECS`, default 30, of tolerance) → recompute the HMAC → **constant-time
     compare** (`subtle::ConstantTimeEq`; a `==` on a MAC is a timing oracle and a reviewer must
     reject it) → then load the session. Any failure is `403` with no detail: never say whether the
     signature was wrong or the session unknown, and never log the presented signature.
  5. **No client-IP binding.** It is the obvious hardening and it breaks the primary use case: the
     Cast device fetches from a different IP than the browser that opened the session. Documented as
     considered-and-rejected so it is not "fixed" later by someone who has not read this.
  6. Key from `SecretManager::get("MAESTRO_STREAM_SIGNING_KEY")` (S7). Unset ⇒ **the media plane
     refuses to start** — fail-closed. Serving unsigned bytes because a secret is missing is the one
     degradation this spec does not permit; every other unconfigured capability degrades to inert,
     and this one degrades to *off*.

  ## TEST PLAN
  - `cargo test` (pure, injected clock and fixed key):
    - Mint → verify round-trip succeeds
    - Expired token → `403`; within-skew token → accepted
    - Tampered `session_id`, tampered `exp`, truncated sig, missing `sig`, missing `exp` → `403`
    - A token minted for session A rejected on session B's stream path
    - Comparison is constant-time (assert `subtle` is used; a `==` is a review reject)
    - Valid token for a closed session → `410` (verification passes, the session does not)
    - Refresh: an ack under 25% TTL returns a new URL with a later `exp` and a different sig
    - Unset signing key → media plane refuses to start (negative test)
    - Rotation: a token signed with `_PREVIOUS` verifies during the grace window
  - Assert no log line, error body, or metric label ever contains the signature or the key

  ## EDGE CASES
  - Clock skew between Maestro and a Cast device — only `exp` matters and only Maestro reads it; the
    device never validates
  - URL logged by an intermediary — step 2's containment argument plus a short TTL is the mitigation;
    Maestro's own access log must **redact `sig`**
  - Key rotation mid-playback — the `_PREVIOUS` grace window keeps in-flight sessions alive
  - A player that strips query parameters on redirect — Maestro never redirects the media plane

- **Acceptance criteria:**
  - [ ] Stream URLs are HMAC-signed, session-scoped, and expiring; minted at session open
  - [ ] Verification is constant-time and fails closed with an undetailed `403`
  - [ ] Tampered session id, tampered expiry, and cross-session reuse are all rejected (negative tests)
  - [ ] Heartbeat refreshes a near-expiry URL without interrupting playback
  - [ ] Missing signing key prevents the media plane from starting — never serves unsigned bytes
  - [ ] Signature and key never appear in logs, error bodies, or metric labels
  - [ ] README documents the scheme, the TTL, and why client-IP binding was rejected
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDLV-05: DirectPlay — the ranged file-serving handler
- **Priority:** Critical
- **Labels:** maestro, http, streaming
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Serve a `MediaHandle`'s bytes with correct `206`/`200`/`304`/`416` semantics,
  `HEAD` support, and conditional requests — streaming from the file, never buffering it. Tier 1 of
  epic §6, and per §1 the entirety of seek support.

  **Never buffer.** A `tokio::fs::File` seeked to `start` and wrapped in a length-limited
  `ReaderStream` is constant memory regardless of file size; reading a 40 GB remux into a `Vec` to
  slice it would OOM the sidecar. Memory must not scale with file size or with concurrent sessions.

  ## FILES
  - `src/maestro/http/stream.rs` — the tier-dispatching handler
  - `src/maestro/http/file_source.rs` — `ByteSource` for a local file
  - `src/maestro/http/content_type.rs` — probe-first MIME derivation
  - `src/maestro/http/mod.rs` — the `maestro` binary's router

  ## APPROACH
  1. Signature:
     ```rust
     pub async fn stream_session(
         State(state): State<Arc<MaestroState>>,
         Path(session_id): Path<Uuid>,
         method: Method,
         headers: HeaderMap,
     ) -> Response
     ```
     Registered `.route("/playback/:id/stream", get(stream_session).head(stream_session))` behind
     MDLV-04's verification layer — one handler, `method` discriminating body-vs-no-body, so the two
     can never disagree on headers. Errors map through the shared `MuseError: IntoResponse`
     (extended with the §2 variants — extended, not forked; `error.rs` is shared), and the handler
     returns `Response`, mirroring the `stream_channel`/`build_stream_response` split already proven
     in `src/streaming/mod.rs`. `MaestroState` is the `maestro` binary's own state (`pool_ro`,
     `pool_rw`, guard, config, child registry) — **not** Muse's `AppState`, which carries *arr clients, the Plex
     client, and the proactive outbox Maestro has no business holding.
  2. Load the session (`404`/`410`), then dispatch on tier: `DirectPlay` → this path, `Remux` →
     MDLV-06. A session whose `playback_mode` is `backend_controlled` has no media plane and answers
     `404` here (§0.3) — it never had a signed URL to reach this route with. The route stays
     tier-agnostic so a client never learns whether it got direct play or remux.
  3. **`Content-Type` comes from the probe, not the extension** (epic requirement). Order: spec A's
     `MediaInfo.container` on the handle → extension → `application/octet-stream`, logging once at
     `debug` when it falls back. An extension is a claim; the probe is an observation, and the two
     disagree exactly often enough to matter — an `.mkv` that is really MP4 will fail on a TV told it
     is Matroska. Map probe containers: `mov,mp4,m4a → video/mp4`, `matroska → video/x-matroska`,
     `webm → video/webm`, `mpegts → video/mp2t`, `avi → video/x-msvideo`, audio containers to their
     audio types.
  4. Validator: strong ETag `"{file_id}-{size:x}-{mtime_nanos:x}"` + `Last-Modified`. Strong, because
     `If-Range` requires it.
  5. Conditional order (RFC 9110 §13.2): `If-None-Match` match → `304`; `If-Match` mismatch → `412`;
     `If-Range` mismatch → ignore `Range`, serve `200` full (the mechanism that turns a mid-session
     file replacement into a clean restart instead of a corrupt splice).
  6. Range via MDLV-03: `Full` → `200` + `Content-Length`; `Partial` → `206` + `Content-Range` +
     `Content-Length`; `Unsatisfiable` → `416` + `Content-Range: bytes */{size}`, empty body.
  7. Headers on every response: `Accept-Ranges: bytes` (**including on `416` and `304`** — that is
     how a client learns to retry sanely), `Content-Type`, `ETag`, `Last-Modified`,
     `Cache-Control: private, max-age=0, must-revalidate`.
  8. Body: `File::open` → `seek(Start(start))` → `.take(len)` → `ReaderStream` → `Body::from_stream`.
     `HEAD` builds the identical builder and finishes `Body::empty()`.
  9. Per-response: add the served length to `bytes_served` and touch `last_heartbeat_at` — a client
     actively pulling bytes is alive by definition, which is what makes a dumb `<video src>` client
     or a Cast receiver workable without JS heartbeats.
  10. Emit the per-session structured log line epic §10b requires (session id, item, plan, backend,
      client) once per session, not per range request.

  ## TEST PLAN
  - `cargo test` with `tempfile` fixtures and axum's `oneshot` — the **range-conformance suite** epic
    §8.7/§10b calls for, since every TV firmware exercises a different corner:
    - No range → `200`, full length, `Accept-Ranges: bytes`
    - `bytes=0-99` → `206`, `Content-Range: bytes 0-99/{n}`, `Content-Length: 100`, 100 bytes
    - Open-ended `bytes=500-` → `206` to EOF
    - Suffix `bytes=-100` → last 100 bytes
    - `bytes=0-0` → 1 byte
    - Past-EOF start → `416` with `bytes */{n}`
    - Multi-range → `200` full body (the documented rejection)
    - `If-Range` fresh → `206`; `If-Range` stale → `200` full
    - `If-None-Match` → `304`; `If-Match` mismatch → `412`
    - `HEAD` → header-identical to `GET`, empty body
    - `Content-Type` from probe `MediaInfo`; with probe data absent, from extension, fallback logged;
      an `.mkv` whose probe says `mp4` serves `video/mp4` (explicit disagreement test)
    - Unknown session `404`; stopped `410`; unsigned/expired URL `403`
  - Memory: serve a range from a sparse fixture larger than any plausible buffer; assert completion,
    and assert in review that no `read_to_end` exists on this path

  ## EDGE CASES
  - Client disconnects mid-stream — the stream drops, the handle closes, the session waits for the
    idle reaper; nothing leaks
  - File truncated/replaced after resolution — `seek` past EOF yields zero bytes; the `If-Range` path
    prevents a corrupt splice on the next request
  - Concurrent ranged requests on one session (players routinely open two connections) — each opens
    its own file handle; nothing shared, nothing locked
  - Zero-byte file — `200` with `Content-Length: 0`; any range `416`

- **Acceptance criteria:**
  - [ ] Correct `Content-Range`/`Content-Length` for start, middle, end, suffix, and open-ended ranges
  - [ ] `HEAD` returns byte-identical headers to `GET` with no body
  - [ ] `416` carries `Content-Range: bytes */{len}` and `Accept-Ranges: bytes`
  - [ ] `If-None-Match` → `304`; stale `If-Range` → full `200`; multi-range → full `200`
  - [ ] `Content-Type` is derived from the probe, falling back to extension only when probe data is
        absent (explicit test where the two disagree)
  - [ ] Memory does not scale with file size (streamed, never buffered)
  - [ ] Per-session structured log line emitted once per session
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDLV-06: Remux tier — `-c copy` into fMP4, MPEG-TS fallback
- **Priority:** High
- **Labels:** maestro, ffmpeg, streaming
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Tier 2 of epic §6: right codecs, wrong container. Stream-copy the elementary
  streams into a container the client accepts, spending no CPU on encoding.

  **Extend `src/streaming/ffmpeg.rs` in place; do not fork or port it.** Because Maestro is a binary
  in this crate (epic §2), that module is directly importable — and it is already the right shape:
  pure argument builders, no spawning, `classify_spawn_error` separating "binary missing" (a
  deployment gap, `501`) from "spawn failed" (transient, `503`), and every argument choice justified
  in a doc comment. `build_remux_args` is a **new function in that existing file**, next to
  `build_args` and `build_still_args`, sharing their common-prefix and input-seek conventions;
  `join_media_path`, `classify_spawn_error`, and `StreamAvailability` are reused unchanged.

  ffmpeg stays a **subprocess, never a linked library** (epic §7.1) — argv vector, never a shell
  string, which is also why a filename with a space or a quote is a non-event.

  ## FILES
  - `src/streaming/ffmpeg.rs` — **extend**: `build_remux_args`, `RemuxContainer`; existing builders
    and their tests untouched
  - `src/maestro/ffmpeg/spawn.rs` — the one impure caller (spawn, stderr ring buffer, child registry)
  - `src/maestro/http/remux.rs` — the chunked-response handler
  - `src/config.rs` — `MAESTRO_FFMPEG_BIN` (defaulting to the existing `ffmpeg_path`),
    `MAESTRO_REMUX_CONTAINER`

  ## APPROACH
  1. `build_remux_args(path, seek_ms, container) -> Vec<String>` — pure. Common prefix
     `-hide_banner -loglevel error`, `-ss {secs:.3}` **before** `-i` when `seek_ms > 0` (input seek:
     keyframe-nearest demuxer seek, no decode — the reasoning already in the file's doc comment, and
     doubly right for a copy pipeline), `-i {path}`, `-c copy`, `-map 0:v:0 -map 0:a:0?`, then:
     - `Fmp4` → `-f mp4 -movflags +frag_keyframe+empty_moov+default_base_moof pipe:1`.
       `empty_moov` + `frag_keyframe` is what makes MP4 **streamable over a pipe at all**: without
       them the muxer must seek back to write the `moov` atom, which a pipe cannot do, and the output
       is unplayable until the process exits. This is the single most common way a home-built remux
       tier fails.
     - `MpegTs` → `-f mpegts pipe:1` — the fallback, and the container that tolerates streams MP4
       will not carry.
     - Never `-ss` after `-i` (forces a decode pass, contradicting `-c copy`).
  2. `RemuxContainer::for_plan(&PlaybackPlan)` — pure, unit-tested; the plan names the target.
  3. Handler `stream_remux(&MaestroState, &PlaybackSession, &MediaHandle) -> MuseResult<Response>`.
     Spawn **before** committing to a response so a failure is a clean `501`/`503` rather than a `200`
     that dies three bytes in — the ordering `build_stream_response` already establishes.
     `Stdio::null()` stdin, `piped()` stdout, stderr into a bounded ring buffer (**not** `null()` —
     this is the tier that will need logs), `kill_on_drop(true)` so a disconnect reaps the child.
  4. Body: `async_stream::stream!` reading stdout in 64 KiB chunks and yielding `Bytes`, the shape
     already proven in `src/streaming/mod.rs`. Awaiting on `yield` **is** the backpressure: a slow
     client blocks the read, the pipe fills, ffmpeg blocks on write. Free, and correct.
     `child.wait()` at the end so no zombie remains.
  5. Headers: `Content-Type` per container, **`Accept-Ranges: none`**, no `Content-Length`,
     `Cache-Control: no-store`. A remux stream is a live producer; advertising byte ranges would
     invite a request the tier cannot honour. Seeking means the client re-opens and Maestro spawns a
     fresh child at a new `-ss` — documented as the tier's honest limitation and a standing argument
     for preferring DirectPlay.
  6. One live child per session, tracked in `MaestroState`; opening a second stream kills the first,
     so a reconnect after a network blip never leaves an orphan holding CPU.

  ## TEST PLAN
  - `cargo test` — golden argument tests in the existing file's style:
    - fMP4 args contain `-c copy` and all three `-movflags`, never an encoder
    - MPEG-TS args match the expected vector exactly
    - `-ss` precedes `-i` when seeking; absent at `seek_ms <= 0`
    - `RemuxContainer::for_plan` mapping table
  - `classify_spawn_error` distinguishes `NotFound` → `BinaryMissing`
  - The pre-existing `build_args`/`build_still_args` tests pass unchanged (proof of extension)
  - **ffmpeg is never invoked in tests** — the standing Muse rule, and now a verified constraint:
    **`ffmpeg`/`ffprobe` are not installed on the dev box (<host>)**. The suite must stay green there.
  - Manual verification (recorded in the PR, not CI) runs **on a host that has ffmpeg** — the Muse
    deploy host or <host> via the compiler tool, never the dev box: remux an MKV to fMP4 and confirm a
    browser `<video>` plays it before the process exits

  ## EDGE CASES
  - ffmpeg absent → `501`; spawn denied → `503`
  - Client disconnect → `kill_on_drop` reaps; negative test asserts no orphan
  - ffmpeg exits non-zero mid-stream (codec the container cannot carry) → stream ends, stderr tail
    logged at `warn`, session closed `stop_reason = error`
  - A file whose audio MP4 cannot carry — the plan should have chosen MPEG-TS; if not, ffmpeg fails
    fast and the log names the codec
  - Filenames with spaces/quotes/newlines — argv vector; covered by a builder test

- **Acceptance criteria:**
  - [ ] fMP4 args include `+frag_keyframe+empty_moov+default_base_moof`
  - [ ] MPEG-TS fallback args match the golden vector
  - [ ] `-ss` is an input seek and is omitted at `seek_ms <= 0`
  - [ ] Remux responses set `Accept-Ranges: none` and no `Content-Length`
  - [ ] Client disconnect kills the child (negative test: no orphan process)
  - [ ] `build_remux_args` lives in the existing `src/streaming/ffmpeg.rs`; prior builders and tests
        unmodified
  - [ ] No test invokes ffmpeg; the suite passes on a host with no ffmpeg installed
  - [ ] README documents the tier's non-seekability and the re-open-to-seek behaviour
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDLV-07: Session open, account mapping, backend policy, and the active-sessions query
- **Priority:** High
- **Labels:** maestro, http, session, identity
- **Agent:** claude
- **Estimate:** 5h
- **Description:** The open path that ties spec C's decision engine, MDLV-02's resolver, MDLV-04's
  signer, and MDLV-01's model together — plus the read endpoints spec H consumes, the
  `browser_account_map` that makes `account_id` mean something, and the per-request backend policy
  epic §10b requires.

  **`browser_account_map` is the item that turns `account_id` from a well-modelled column into actual
  attribution.** Without it, every row carries the same configured default, which is *worse* than a
  null: a column that looks like per-account attribution and silently isn't will be trusted by the
  taste model, by spec H's Activity panel, and by whoever later builds per-account resume. A single
  shared value is a plausible-looking lie; a null is at least honest about being unknown.

  ## FILES
  - `migrations/{next}_browser_account_map.sql` — `(cookie_subject, muse_account_id)`, operator-managed
  - `src/models/browser_account_map.rs`, `src/repo/browser_account_map.rs` — read path only
  - `src/maestro/http/sessions.rs` — open / get / list / stop handlers
  - `src/maestro/session/open.rs` — resolve → plan → sign → persist
  - `src/maestro/session/identity.rs` — cookie subject → Muse `account_id`
  - `src/maestro/backends/policy.rs` — per-request backend selection
  - `src/config.rs` — `MAESTRO_DEFAULT_BACKEND`, `MAESTRO_DEFAULT_ACCOUNT_ID`,
    `MAESTRO_BACKEND_DEVICE_OVERRIDES`

  ## APPROACH
  1. `POST /playback/sessions`:
     ```rust
     pub async fn open_session(
         State(state): State<Arc<MaestroState>>,
         Extension(caller): Extension<ProxyCaller>,
         Json(req): Json<OpenSessionRequest>,
     ) -> Result<(StatusCode, Json<SessionResponse>), MuseError>
     ```
     `OpenSessionRequest { media_item_id, episode_id, device_id, profile, start_position_ms: Option<i64>,
     backend: Option<String> }`. Note what is **absent**: no path, no URL, no container, no
     `account_id`. Sequence: cap check (MDLV-08) → account mapping → backend policy → resolve
     `BackendMediaRef` on `pool_ro` (MDLV-02) → `plan()` (spec C, pure) → mint the signed URL **when
     the backend serves bytes** (MDLV-04) → persist on `pool_rw` → `201` with
     `{ session_id, tier, backend, playback_mode, stream_url, position_ms, duration_ms, expires_at }`.
     **`stream_url` is `Option<String>` and `playback_mode` is
     `maestro_stream | backend_controlled`** (§0.3): a `native` session carries a signed URL, a `plex`
     session carries `None` and is played through the backend's own control surface (spec B/G).
     Returning a URL that would 404 in order to keep the response shape uniform would be a lie the
     client discovers at the worst possible moment; the discriminant makes the asymmetry legible
     instead — the same honesty `BackendCapabilities` applies to feature differences.
  2. **`browser_account_map` (epic §8.1).** Schema: `cookie_subject TEXT PRIMARY KEY`,
     `muse_account_id BIGINT NOT NULL REFERENCES accounts(id)`, plus `note TEXT` and timestamps so an
     operator can see what they configured. **Operator-managed: `SELECT` to `maestro_ro`, and no role
     gets write** — not even `maestro_rw`. A component that could mint its own identity mappings would
     be an identity service, which epic §8.1 explicitly defers; rows are added through the `pg_*`
     sanctioned door.
  3. **`account_id` resolution.** The proxy forwards its authenticated caller; `identity.rs` resolves
     in strict order: `browser_account_map[cookie_subject]` → per-device override
     (`MAESTRO_BACKEND_DEVICE_OVERRIDES`' account sibling) → `MAESTRO_DEFAULT_ACCOUNT_ID` → `NULL`
     (household). It is **never** derived from the cookie session's `operator|viewer` role — that is a
     permission tier, not a person — and **never** read from the request body, because a
     client-declared account is a client-declared watch history and that flows straight into taste.
     Log at `info`, once per distinct unmapped subject, when resolution falls through to the default:
     an unmapped household member is the failure mode that quietly attributes one person's viewing to
     another, and it is invisible unless something says so.
  4. **Resume is supplied by the caller, not read by Maestro (§0.5).** `start_position_ms` on the
     request is the resume point; constellation-web fetches it from Muse through `proxy_muse` on the
     same control-plane turn that renders the item. Absent → 0. Maestro performs **no watch-state
     read** — `maestro_ro` has no grant to perform one (§0.1), which is the enforcement. The value is
     copied to the session row's `start_position_ms` **for explicability only**; Maestro never writes
     it back and never treats its own copy as truth. Muse remains authoritative for resume exactly as
     it is for watch history.
  4. **Backend selection is per-request policy, not a compile-time switch** (epic §10b): explicit
     `backend` in the request → per-device override → `MAESTRO_DEFAULT_BACKEND` (default `plex`).
     That is the one-line kill switch and the A/B mechanism — route one named device through `native`
     while the household stays on `plex`, and revert by editing config.
  5. Read endpoints: `GET /playback/sessions/{id}`; `GET /playback/sessions` (spec H's feed — item,
     account, device, backend, tier, position, state, elapsed, plus the backend's
     `can_report_transcode_detail` capability so the panel can say "Plex cannot report this" rather
     than display zeros as facts). `POST /playback/{id}/stop` → close `stop_reason = client`, emit the
     stop event (MDLV-09), `204`.
  6. A transcode-tier plan against a backend reporting `Unsupported` → `501` naming the tier and the
     reason, so the Player panel renders "this file needs transcoding, which isn't available yet"
     instead of a spinner.

  ## TEST PLAN
  - `cargo test` via `oneshot`, using `FakeBackend` (MDLV-11) so these need no Plex, ffmpeg, or library:
    - Valid item on `native` → `201`, `playback_mode: maestro_stream`, a **signed** stream URL,
      and `expires_at`
    - Same item on `plex` → `201`, `playback_mode: backend_controlled`, `stream_url: null`, and no
      signed URL minted (negative test — no media-plane capability is handed out for a backend that
      does not serve bytes)
    - Both id fields null → `400`
    - `account_id` in the request body → the field does not exist; rejected as malformed (negative
      test that it cannot be spoofed)
    - Identity resolution yields a Muse account id, never a role string (explicit assertion)
    - `browser_account_map` precedence: a mapped subject → its account; unmapped subject → device
      override; no override → `MAESTRO_DEFAULT_ACCOUNT_ID`; none configured → `NULL`
    - Two different mapped subjects opening the same item → two different `account_id`s (the test that
      would have caught "every row holds one value")
    - An unmapped subject logs the fall-through once, not per request
    - No role can write `browser_account_map` (negative test against both DSNs)
    - Resume: `start_position_ms` supplied → session opens there; omitted → 0; **and no query touches
      `play_sessions`** (assert by grant — a watch-state read fails under `maestro_ro`)
    - Backend policy: explicit > device override > default; `MAESTRO_DEFAULT_BACKEND=plex` yields a
      `backend_controlled` session, `=native` a `maestro_stream` one
    - Transcode plan → `501` naming the tier
    - `GET /playback/sessions` lists only active sessions with the fields spec H needs
  - Live-DB round trip gated on `MUSE_TEST_DATABASE_URL`

  ## EDGE CASES
  - Database unreachable → `503`
  - Item resolves but the file is gone → `404` at open, not a broken stream later
  - `start_position_ms` beyond duration — clamp to 0 and log
  - Two opens for the same item on the same device — both allowed (a legitimate second player); the
    MDLV-08 cap is the only limit
  - Caller omits `start_position_ms` — open at 0. A deliberate, benign default (§0.5): starting a film
    from the beginning is a small annoyance; refusing to play would not be
  - `browser_account_map` references an account that was since deleted — the FK prevents the dangling
    row; if the mapping is simply absent, fall through to the default and log
  - `MAESTRO_DEFAULT_ACCOUNT_ID` set to an id that does not exist — validate at startup and refuse to
    boot, rather than attributing every session to a phantom account

- **Acceptance criteria:**
  - [ ] `POST /playback/sessions` returns `201` with session id, tier, backend, `playback_mode`,
        resume position, and — for a byte-serving backend — a signed URL and its expiry
  - [ ] A `backend_controlled` session returns `stream_url: null` and mints no signed URL (negative test)
  - [ ] `browser_account_map` ships, is `SELECT`-only to `maestro_ro`, and is writable by no role
  - [ ] `account_id` is resolved map → device override → default → NULL; two mapped subjects yield two
        different account ids (the test that catches a column holding one value for every row)
  - [ ] `account_id` is never taken from the request body and never from the cookie role (negative test)
  - [ ] Resume arrives as `start_position_ms` from the caller; Maestro issues **no watch-state query**
        and could not (§0.5)
  - [ ] Backend selection is per-request policy with a `MAESTRO_DEFAULT_BACKEND` kill switch
  - [ ] A transcode-tier plan fails `501` naming the tier
  - [ ] The request model exposes no path, URL, container, or account field
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDLV-08: Session lifecycle — heartbeat, idle timeout, orphan sweep, concurrency cap
- **Priority:** High
- **Labels:** maestro, session, lifecycle
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Clients vanish. A TV loses power, a tab closes, a phone walks out of Wi-Fi range —
  and none of them say goodbye. An engine that only closes sessions when told to accumulates phantom
  "now playing" rows forever, which corrupts the Activity panel, inflates watch history, and (once
  spec E exists) leaks ffmpeg processes holding CPU. Four mechanisms, all necessary:

  1. **Heartbeat** — the client says "alive, at position P, in state S".
  2. **Idle timeout** — no heartbeat and no bytes for N seconds means gone.
  3. **Orphan sweep on startup** — a restart abandons every in-flight session; they sit in the
     database claiming to play, and nothing will ever heartbeat them again.
  4. **Concurrency cap** — a bug or a hostile client must not open unbounded sessions.

  ## FILES
  - `src/maestro/http/heartbeat.rs`
  - `src/maestro/session/reaper.rs` — background reaper + startup orphan sweep
  - `src/config.rs` — the knobs
  - `src/bin/maestro/main.rs` — sweep before binding, then spawn the reaper

  ## APPROACH
  1. `POST /playback/{id}/heartbeat`, body `{ position_ms, state, buffering_ms? }`:
     ```rust
     pub async fn heartbeat(
         State(state): State<Arc<MaestroState>>,
         Path(session_id): Path<Uuid>,
         Json(req): Json<HeartbeatRequest>,
     ) -> Result<Json<HeartbeatAck>, MuseError>
     ```
     Updates `position_ms`, `state`, `last_heartbeat_at = now()` (server clock, never the client's).
     Returns `HeartbeatAck { interval_secs, stream_url: Option<String>, expires_at }` — the cadence is
     **server-directed** so Maestro can lengthen it under load without a client change, and the
     optional refreshed URL is MDLV-04's renewal path. `410 Gone` if already reaped, telling the
     client to re-open rather than retry forever. Emits a progress event (MDLV-09) at most every
     `MAESTRO_EVENT_PROGRESS_SECS` (default 30) — heartbeats are frequent, taste events should not be.
  2. Reaper: a `tokio::spawn` loop on `MAESTRO_REAPER_INTERVAL_SECS` (default 15), following the
     never-die-on-a-bad-tick pattern already proven in `src/tracker/poller.rs` — a failing tick logs
     and retries, it never kills the loop. Each tick closes sessions idle beyond
     `MAESTRO_SESSION_IDLE_TIMEOUT_SECS` (default 90) with `stop_reason = idle_timeout`, kills any
     ffmpeg child, and emits a stop event at the last known position. 90s is deliberately several
     heartbeats: a brief network stall must not end a movie.
  3. Startup orphan sweep runs **before** the listener binds: every non-stopped session is closed
     `stop_reason = orphan_sweep` at its last known position, with a stop event each. Order matters —
     sweeping after binding races a client reconnecting to a session the sweep is about to close.
  4. Cap `MAESTRO_MAX_CONCURRENT_SESSIONS` (default 8), checked at open; over-cap → `429` +
     `Retry-After: 5` + a body naming the limit. Counted from the database, not an in-memory counter,
     so a restart cannot lose count. Emits `maestro_sessions_rejected_total{reason="cap"}` — a cap hit
     routinely is a signal, not just a refusal.
  5. Byte-serving also touches `last_heartbeat_at` (MDLV-05 step 9), so a dumb `<video src>` client or
     a Cast receiver with no JS stays alive purely by pulling bytes.

  ## TEST PLAN
  - `cargo test`:
    - Heartbeat updates position/state and returns the configured interval
    - Heartbeat near URL expiry returns a refreshed signed URL; well before expiry, it does not
    - Heartbeat on a stopped session → `410`
    - Reaper (injected clock, no `sleep`) closes a stale session and leaves a fresh one alone
    - Progress throttle: 10 heartbeats in one window → exactly 1 progress event
    - Startup sweep closes every non-stopped session with `stop_reason = orphan_sweep` + a stop event
    - Open at the cap → `429` with `Retry-After`; after one closes → `201`

  ## EDGE CASES
  - Heartbeat racing the reaper — close is idempotent (MDLV-01); the heartbeat gets `410`
  - Position going backwards (a seek) — legal, recorded as-is; interpretation is Muse's business
  - Client timestamps never trusted or stored
  - Idle timeout shorter than the heartbeat interval — would reap every live session; validate at
    startup and refuse to boot with a clear message
  - Cap of 0 — treated as unlimited and logged once, rather than bricking playback

- **Acceptance criteria:**
  - [ ] Heartbeat updates position/state with a server-side timestamp and returns the interval
  - [ ] A session idle past the timeout is reaped with `stop_reason = idle_timeout` and a stop event
  - [ ] Startup sweep closes every orphan before the listener binds
  - [ ] Over-cap open returns `429` + `Retry-After` and increments the rejection metric
  - [ ] Idle timeout shorter than the heartbeat interval is rejected at startup (negative test)
  - [ ] Active byte-serving keeps a session alive with no explicit heartbeat
  - [ ] README documents the four mechanisms and their knobs
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDLV-09: Durable play-event outbox — one direction, at-least-once, versioned
- **Priority:** Critical
- **Labels:** maestro, muse, taste, strangler-fig
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Maestro emits `start` / `progress` / `stop` to Muse so watch history and the taste
  model are fed by native playback. The strangler-fig step that begins replacing the Plex/Tautulli
  path — same destination, new source.

  Epic §10b names the stakes precisely: **a lost stop-event is a corrupted watch duration, which
  corrupts taste — the one failure that silently damages the product rather than visibly breaking
  it.** A dropped frame is obvious. A film recorded as 12 minutes long because the stop never landed
  is invisible, permanent, and teaches the recommender something false. So delivery is durable, not
  best-effort.

  **One direction only, and this is where that discipline is enforced or lost.** Maestro emits; Muse
  consumes and remains authoritative (epic §2). Maestro never reads taste data back — no "is this
  watched", no resume lookup, no recommendation call. As of the two-role grant model it *cannot*:
  `maestro_ro` has no `SELECT` on watch, event, or embedding tables, and there is no second door
  (§0.1, §0.5). Concretely: **this client exposes only `POST`, has no `GET` at all, and a PR adding
  one is rejected citing epic §2.**

  **Why HTTP when both processes share a database.** Maestro *could* insert `play_events` directly.
  It must not: one process owning the fold from raw events → `play_sessions` → taste is exactly what
  keeps the two from drifting. The hop is the boundary, and it is free because the outbox drain is
  never awaited by a streaming request. Maestro's DB role does not grant write on `play_events`, so
  this is enforced rather than intended.

  ## FILES
  - `src/maestro/events/mod.rs` — `MaestroPlayEvent`, `EventKind`
  - `src/maestro/events/emitter.rs` — transactional outbox writer + drain loop
  - `src/maestro/events/client.rs` — `post_play_event` (POST only)
  - `migrations/{next}_maestro_event_outbox.sql`
  - `src/config.rs` — `MAESTRO_MUSE_BASE_URL`, `MAESTRO_EVENT_*`

  ## APPROACH
  1. **Versioned payload from the first commit** (epic §10b):
     ```json
     { "v": 1, "event_id": "<uuid>", "session_id": "<uuid>", "kind": "start|progress|stop",
       "occurred_at": "<rfc3339>", "account_id": 12, "media_item_id": 34, "episode_id": null,
       "position_ms": 1234000, "duration_ms": 5400000, "state": "playing",
       "device": "living-room-tv", "player": "constellation-web", "backend": "native",
       "tier": "direct_play", "container": "mp4", "video_codec": "h264", "audio_codec": "aac",
       "stop_reason": null }
     ```
     `event_id` is the **dedupe key**: at-least-once delivery + server-side dedupe = exactly-once
     folding. `"v"` is checked by the receiver and lets the shape evolve without a flag day.
  2. **Transactional outbox, not fire-and-forget.** Every event is written to `maestro_event_outbox`
     **in the same transaction as the session state change** — an event cannot exist without its state
     change, or vice versa. This is possible precisely because both tables sit under `maestro_rw`
     (§0.1): one role, one pool, one transaction. A background task drains with exponential backoff
     (1s → 60s, jittered), and its polling `SELECT` is the second reason `maestro_rw` needs read
     access on its own tables.
     Muse restarting (routine now that they are two units) must not lose a film's watch history, and
     must not block the byte path — the emitter is never awaited on a streaming request.
  3. Bounded (`MAESTRO_OUTBOX_MAX_ROWS`, default 10 000). At the bound, drop the **oldest progress**
     events first and **never** a `start` or `stop`: progress is resumable detail, start/stop are the
     skeleton of a watch record, and losing one is the failure this item exists to prevent.
  4. Emission points: `start` at open; `progress` on heartbeat (throttled); `stop` on **every**
     terminal path — client stop, idle timeout, orphan sweep, stream error — each with its
     `stop_reason`. A session producing a `start` and no `stop` is a bug in this item, not an
     acceptable outcome, and MDLV-11 has a metric for exactly that.
  5. `MuseEventClient::post_play_event` — `reqwest` POST with
     `Authorization: Bearer {SecretManager::get("MAESTRO_MUSE_TOKEN")}`. Epic §10b: **there are two
     credentials, not one** — `CONSTELLATION_MAESTRO_TOKEN` (Terminus → Maestro control) and this one
     (Maestro → Muse). TERM #549 taught the cost of an unprovisioned token once already, so it is a
     pre-flight item and the client logs a `401` **at error level** rather than retrying quietly into
     silence. Retry on 5xx/timeout only; a 4xx is a contract bug.
  6. Degrade gracefully (epic §7.4): `MAESTRO_MUSE_BASE_URL` unset → log once at startup, run inert.
     Playback works; only the taste feed is absent. Never a hard dependency.

  ## TEST PLAN
  - `cargo test`:
    - Outbox row written in the same transaction as the state change (rollback → no orphan event)
    - Drain retries on 5xx with backoff; logs-and-gives-up on 4xx (mock HTTP server)
    - A `401` is logged at `error`, not silently retried
    - Progress throttle: 10 heartbeats in a window → 1 outbox row
    - Every terminal path produces exactly one `stop` with the right `stop_reason` (four cases)
    - Bound enforcement drops oldest `progress`, never `start`/`stop`
    - Payload carries `"v": 1` and a unique `event_id`
    - Unconfigured base URL → inert, playback unaffected
  - **Negative test (the epic §2 guard):** `src/maestro/events/client.rs` has no `GET`/read method and
    no taste/recommendation call; `src/maestro/` contains no write to `play_events`/`play_sessions`
    despite holding a pool that reaches them

  ## EDGE CASES
  - `muse.service` restarts while `maestro.service` keeps streaming (the scenario the two-process
    split creates) — the outbox absorbs it; start/stop survive
  - Duplicate delivery after a retry — the receiver dedupes on `event_id` (MDLV-10)
  - `account_id` NULL — emitted as household; the receiver must accept it
  - Secret unavailable at startup — log the blocker and run inert; **never** fall back to an unauthed
    POST, never hardcode a stopgap (S7)
  - Maestro killed with a full outbox — rows are durable; the drain resumes on restart, after the
    orphan sweep has added its own stop events

- **Acceptance criteria:**
  - [ ] `start`/`progress`/`stop` emitted at every documented point, with `"v": 1` and a dedupe key
  - [ ] Events go through a transactional outbox with bounded retry; the byte path never awaits it
  - [ ] Every terminal path emits exactly one `stop` with a `stop_reason` (four-case test)
  - [ ] Shedding never drops a `start` or `stop`
  - [ ] The Muse client exposes POST only; no read path exists (epic §2 negative test)
  - [ ] `MAESTRO_MUSE_TOKEN` via `SecretManager::get()`; a `401` is logged loudly, never retried quietly
  - [ ] Unconfigured Muse URL degrades to a no-op without breaking playback
  - [ ] README documents the one-directional contract and why
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDLV-10: Muse-side ingest — `POST /ingest/maestro`
- **Priority:** Critical
- **Labels:** muse, tracker, taste, strangler-fig
- **Agent:** claude
- **Estimate:** 3h
- **Description:** The receiver. Accept Maestro's events and fold them through the **existing**
  reconstruction path so native playback lands in the same `play_sessions` rows, the same watch
  history, and the same taste model as Plex playback does today.

  `src/tracker/mod.rs` is explicit: "both ingest paths funnel through the same `play_events` table and
  the same `reconstruct_and_persist` — there is exactly one reconstruction algorithm, not one per
  source." Maestro is the third ingest path and obeys that rule exactly. Epic §10b names
  `tracker/reconstruct.rs`'s idempotent reconstruction as the consumer contract, which is precisely
  what makes at-least-once delivery safe. A parallel Maestro-specific fold would be a second
  watch-state store wearing a different hat, drifting within a sprint.

  ## FILES
  - `src/tracker/maestro.rs` — the handler
  - `src/tracker/mod.rs` — registration
  - `src/http/mod.rs` — mount inside the existing `ingest_routes()`
  - `migrations/{next}_play_events_event_id.sql` — nullable unique `event_id`

  ## APPROACH
  1. `pub async fn maestro_event(State(state): State<Arc<AppState>>, Json(ev): Json<MaestroEvent>) -> StatusCode`
     at `/ingest/maestro`, behind the existing `MUSE_API_TOKEN` bearer middleware (`src/http/auth.rs`).
     Unlike the Plex webhook — which must answer `200` to everything because Plex retries aggressively
     on failure — this endpoint has a cooperating client with an outbox, so it answers honestly:
     `202` accepted, `400` malformed or unknown `"v"`, `401` unauthorised, `409` duplicate `event_id`
     (the emitter treats it as success), `500` on a genuine persistence failure so the outbox retries.
     **Document that divergence in the handler** — the next reader will otherwise "fix" it to match
     the webhook.
  2. Persist a `play_events` row with `source = "maestro"`, reusing `NewPlayEvent` unchanged, full
     payload in `raw`. Add nullable `event_id UUID UNIQUE`; a conflict on it **is** the dedupe,
     returning `409` without a second fold.
  3. **Bypass rating-key resolution.** Maestro sends Muse's own ids — it read them from Muse in the
     first place. Where the Plex path must map an opaque `rating_key` back to a library row, this path
     already has the ids. Populate `rating_key` with the native id for schema compatibility, set
     `account_ref` from `account_id`, and short-circuit resolution.
  4. Call `reconstruct::reconstruct_and_persist` exactly as the webhook and poller do. Map tier onto
     the existing `DecisionKind` for `play_session_media_info`: `direct_play → DirectPlay`,
     `remux → Copy`, `partial_transcode|full_transcode → Transcode`, `transcode_reason` from the plan.
     That is what makes native sessions comparable to Plex sessions in every existing query and
     dashboard, with no new reporting surface.
  5. Migration is **additive and idempotent** (nullable column + unique index) and, per the v4.6 DEPLOY
     rule, must be applied to the live database with or before the image swap — migrations are not
     auto-applied at startup.

  ## TEST PLAN
  - `cargo test`:
    - A `start` persists a `play_events` row with `source = "maestro"`
    - Duplicate `event_id` → `409`, no second row, no double-counted watch time
    - Malformed body → `400`; unknown `"v": 2` → `400` (assert the deliberate divergence from the
      webhook's always-`200` posture)
    - Missing/invalid bearer → `401`
    - start → progress → stop reconstructs one `play_sessions` row with correct `watched_ms`,
      `percent_complete`, `is_finished`
    - Out-of-order (progress after stop) stays idempotent and late-tolerant
    - Tier → `DecisionKind` mapping table
  - Live-DB round trip gated on `MUSE_TEST_DATABASE_URL`

  ## EDGE CASES
  - Unknown `media_item_id` (item deleted mid-session) — persist the raw event, log, skip the fold;
    never `500` on a race the client cannot fix
  - `account_id` null — household attribution, as an unmatched Plex account already is
  - A Plex session and a Maestro session for the same title concurrently — distinct session keys, two
    rows; content-level dedup is not this endpoint's job
  - Very late stop after the session was already reconstructed — the existing late-event tolerance
    handles it; assert it still does with a maestro-sourced stream

- **Acceptance criteria:**
  - [ ] `POST /ingest/maestro` persists to `play_events` with `source = "maestro"`
  - [ ] Events fold through the existing `reconstruct_and_persist` — no second reconstruction path
  - [ ] Duplicate `event_id` → `409`, no double-counted watch time
  - [ ] Unknown payload version → `400`
  - [ ] Behind the existing `MUSE_API_TOKEN` middleware
  - [ ] Tier → `DecisionKind` mapping complete and unit-tested
  - [ ] Migration additive, idempotent, and flagged as a DEPLOY prerequisite
  - [ ] README updated to document the new ingest source
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDLV-11: `native` backend, `FakeBackend`, metrics, budgets, and the isolation chaos test
- **Priority:** High
- **Labels:** maestro, backend, metrics, testing
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Implement spec B's `PlaybackBackend` for the native engine over the two tiers this
  spec delivers, returning `Unsupported` for anything needing transcode — so it composes with the
  plex adapter and **degrades honestly**. A backend that silently falls back to a lower-quality path,
  or stalls on a plan it cannot serve, teaches the decision engine nothing and the operator less.
  `Unsupported { tier, reason }` is a fact spec E can be sized against.

  Plus the three things epic §10b asks every child spec to actually do rather than assume:
  **measure the budgets, fake the backend so tests are cheap, and prove the isolation claim.**

  ## FILES
  - `src/maestro/backends/native.rs` — the `PlaybackBackend` impl
  - `src/maestro/backends/fake.rs` — `FakeBackend` (tests + GUI development)
  - `src/maestro/metrics.rs`
  - `src/maestro/http/mod.rs` — `/metrics`, `/health`, `GET /backends`
  - `tests/maestro_isolation_chaos.rs` — the SIGKILL test

  ## APPROACH
  1. `impl PlaybackBackend for NativeBackend`:
     - `capabilities()` → the epic §8.6 descriptor: `in_browser_stream: true`, `device_cast: true`,
       `server_side_transcode_decision: true`, `seek_during_transcode: false`, `syncplay: false`,
       `can_report_transcode_detail: true`. Spec E flips `seek_during_transcode`; spec F adds
       hardware. Capabilities are **data**, so the Player and Activity panels render what is actually
       possible now instead of discovering asymmetry at integration time.
     - `open(req, plan)` → `DirectPlay | Remux` → a session + signed URL;
       `PartialTranscode | FullTranscode` → `Err(Unsupported { tier, reason: "native transcode lands
       in S130-E" })` → `501`.
     - `health()` → `Ok` when the guard has usable roots and (for remux) ffmpeg is present; otherwise
       degraded, **naming the missing piece**. Config-gated degradation (epic §7.4).
  2. **`FakeBackend`** (epic §10b) — an in-memory `PlaybackBackend` serving a small generated fixture,
     with switchable capabilities and injectable failures. It is what lets MDLV-07/09's handler tests,
     spec G's GUI work, and spec H's Activity panel run with **no Plex, no ffmpeg, and no library** —
     which matters concretely because ffmpeg is not on the dev box.
  3. Metrics (Prometheus text, matching Muse's existing `/metrics` convention):
     - `maestro_active_sessions{backend,tier}` — gauge
     - `maestro_playback_tier_total{tier,backend}` — counter, **incremented once per session open**;
       the tier-distribution measurement that sizes spec E. It must count *opens*, not bytes, or one
       long movie would swamp a hundred short direct plays.
     - `maestro_bytes_served_total{tier,backend}` — counter
     - `maestro_time_to_first_byte_seconds{tier,backend}` — histogram; **the direct-play budget is
       < 1s** (epic §10b) and this is the regression baseline
     - `maestro_session_duration_seconds` — histogram at close
     - `maestro_stop_reason_total{reason}` — how often clients vanish vs say goodbye
     - `maestro_sessions_without_stop_total` — the MDLV-09 correctness canary
     - `maestro_event_delivery_failures_total{kind}`, `maestro_outbox_depth`
     - `maestro_sessions_rejected_total{reason}`, `maestro_remux_children` (the leak canary)
  4. **No PII in labels** (epic §7.6 / S1): no account ids, no paths, no device names, no addresses,
     no signatures. Bounded enumerations only — tier, backend, reason. A cardinality bomb is also an
     operational problem, so the constraint pays twice.
  5. **Chaos test — prove the crash isolation** (epic §10b: "an isolation claim that is never tested
     is a hope, not a property"). An integration test, gated like the other live tests so it skips
     cleanly where it cannot run (the dev box has no ffmpeg): start a remux session against a fixture,
     `SIGKILL` the ffmpeg child mid-stream, and assert (a) the session closes `stop_reason = error`,
     (b) a `stop` event is emitted, (c) no orphan process survives, and (d) the Muse-side surface is
     entirely unaffected.
  6. Recover gauges on startup after the orphan sweep so a restart shows no phantom load.

  ## TEST PLAN
  - `cargo test`:
    - `capabilities()` reports transcode-related capability honestly (`seek_during_transcode: false`)
    - A transcode plan → `Unsupported` naming the tier — never a silent fallback (negative test)
    - A direct-play plan → session + signed URL
    - `health()` degrades (not errors) with ffmpeg absent and names it
    - `GET /backends` returns the descriptor set the GUI consumes
    - `FakeBackend` satisfies the same contract tests as `NativeBackend` (a shared test suite over the
      trait — the contract tests epic §10b asks for)
    - Metrics render valid Prometheus text; `maestro_playback_tier_total` increments once per open and
      not per byte
    - Label-set test: no metric label contains an account id, path, device name, address, or signature
  - Chaos test as described, skipping cleanly where ffmpeg is unavailable
  - TTFF measured on the chosen deploy host and **recorded in the PR** as the baseline for the < 1s
    direct-play budget

  ## EDGE CASES
  - Backend registered but roots unconfigured — `health()` degraded, `open()` clean-fails; the panel
    renders inert, never broken (Module Contract clause 2)
  - Metrics scrape during a reaper tick — snapshot-consistent reads; never lock the byte path
  - Tier counter incremented **after** the session persists, so the distribution counts real playback
    rather than attempts
  - `maestro_sessions_without_stop_total` non-zero — a defect signal, not a curiosity; it means taste
    is being fed corrupt durations

- **Acceptance criteria:**
  - [ ] `NativeBackend` implements `PlaybackBackend` for DirectPlay and Remux
  - [ ] Transcode tiers return `Unsupported` naming the tier — never a silent fallback (negative test)
  - [ ] `capabilities()` / `GET /backends` report honestly; `health()` degrades gracefully
  - [ ] `FakeBackend` exists and passes the same trait contract tests, enabling GUI/session tests with
        no Plex, ffmpeg, or library
  - [ ] `maestro_playback_tier_total` counts session opens per tier (the spec-E sizing metric)
  - [ ] TTFF histogram exists and a direct-play baseline under 1s is recorded in the PR
  - [ ] Chaos test SIGKILLs ffmpeg mid-session and asserts Muse is unaffected and no orphan survives
  - [ ] No metric label contains PII or an unbounded value (negative test)
  - [ ] README documents every metric and what decision it informs
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

## 4. Phases

| Phase | Items | Why this order |
|---|---|---|
| D1 — foundation | MDLV-01, MDLV-02, MDLV-03, MDLV-04 | Model, shared path guard, pure range/`ByteSource`, signing. All independent; run in parallel. Three of the four are pure or near-pure — correctness is cheapest here. |
| D2 — the tiers | MDLV-05, MDLV-06 | Direct play first (epic §6), then remux. Both consume `ByteSource`. |
| D3 — sessions | MDLV-07, MDLV-08 | Open/resume/policy, then the four lifecycle mechanisms. |
| D4 — the taste feed | MDLV-09, MDLV-10 | Same-crate pair; land the receiver (MDLV-10) **before or with** the emitter (MDLV-09) so the first event has somewhere to go. |
| D5 — composition | MDLV-11 | Backend impl, `FakeBackend`, metrics, budgets, chaos test. |

**The proof point is testable at the end of D4** and is the milestone to demo: open a `native`
session from constellation-web against a direct-play file, cast it via a signed URL, scrub it (a
range request, nothing more), let it finish, and watch the play session appear in Muse's watch
history and feed the taste model — with no transcoder anywhere in the system.

The equivalent demo against Plex runs through the **control plane only** and is spec B/G's to show
(§0.3): Maestro starts and observes the session and emits the same events, but the bytes never touch
it.

---

## 5. Out of scope (deliberately)

- **HLS, segmenting, any re-encode** — spec E. A `-c copy` pipe is the ceiling here.
- **Seek within a remux or transcode session as a first-class operation** — the client re-opens
  (MDLV-06); properly solved in E.
- **Subtitles, audio track selection, downmixing** — spec E.
- **Hardware acceleration and GPU arbitration** — spec F.
- **HDR tone-mapping** — epic §8.3, out of scope through spec E.
- **A real identity service** — epic §8.1. `account_id` is Muse's id, resolved server-side through
  the operator-managed `browser_account_map`; unifying identity is its own spec. Maestro can read that
  map and never write it.
- **Maestro reading resume, watch state, or taste** — §0.5. The caller supplies `start_position_ms`;
  `maestro_ro` holds no grant that would permit anything more.
- **Proxying Plex (or any remote backend's) bytes** — §0.3. `plex` is control + observe; only
  `native` serves bytes. Cut for the reasons recorded there; do not re-propose it.
- **Jellyfin/Emby anything** — epic §8.5 cut them from spec B; no server exists to test against.
- **Plex session observation** — epic §8.8 assigns that to **spec J**, which must land before or with
  B. This spec consumes whatever session ownership J establishes; it does not add a second observer.
- **Any library scan or metadata provider in Maestro** — epic §2, permanently.
- **Reading taste data back from Muse** — epic §2. One direction, enforced in MDLV-09.
- **`multipart/byteranges`** — MDLV-03's documented single-range-only policy.

---

## 6. Risks

1. **Signed URLs designed too narrowly.** If the token is scoped to anything other than the session,
   or the TTL is shorter than a film, the Cast path breaks in a way that looks like a player bug.
   MDLV-04's renewal-on-heartbeat and 6h default are the mitigations; the containment argument lives
   in the item so a future reviewer does not "harden" it into uselessness (see the IP-binding note).
2. **Range arithmetic bugs present as "corrupt video".** MDLV-03 is pure and golden-tested before any
   handler exists. If a playback bug appears after this spec, suspect the range math or the resolver
   before ffmpeg.
3. **A second watch-state store creeps in.** It is genuinely convenient for Maestro to cache "what's
   watched", and it shares a database with the answer. MDLV-09's POST-only client, the read-only DB
   role, and MDLV-10's reuse of `reconstruct_and_persist` are the structural guards; epic §2 is the
   review guard.
4. **Path safety reimplemented instead of reused.** The worst outcome available to this spec: two
   allowlists, one less reviewed. MDLV-02 moves the existing type and its tests; a reviewer seeing a
   new confinement check under `src/maestro/` should reject it.
5. **`MAESTRO_MUSE_TOKEN` or `CONSTELLATION_MAESTRO_TOKEN` unprovisioned**, repeating TERM #549 —
   events `401` silently and the taste feed looks fine while feeding nothing. Both are pre-flight
   items; MDLV-09 logs a `401` at error level precisely so this is loud.
6. **The tier metric measuring the wrong thing.** Counting bytes rather than opens would make one
   long remux look like a mandate for spec E. MDLV-11 counts opens; a test asserts it.
7. **The byte-proxy gets re-proposed.** It is a genuinely appealing idea — one URL shape for every
   backend, a truly invisible native swap — and the reasons it was withdrawn (undocumented,
   token-lifecycle-bound, keepalive-sensitive Plex internals that change without notice) are not
   visible from the outside. §0.3 records them, and `playback_mode` makes the asymmetry explicit in
   the API rather than something a future reader might try to "clean up".

---

## 7. Pre-flight

- [ ] Confirm `MDLV` is registered (epic §11 records `plane_prefix_register` done 2026-08-01); run
      `plane_prefix_promote` for the durable baseline entry
- [ ] Confirm specs A, B, C are merged — this spec needs `MediaInfo` (probe-derived `Content-Type`),
      `PlaybackBackend`/`BackendCapabilities`/`BackendMediaRef`, and `plan()`
- [ ] Confirm epic §8.8's **spec J** (Plex session ownership) is landed or scheduled before B, so
      MDLV-09's events do not become a second observer of Plex sessions
- [ ] Decide and record the **Maestro host** (epic §10b) — MDLV-11's budgets are host-specific.
      Recommendation: alongside Muse. It must have the read-only library mount and `ffmpeg`/`ffprobe`
      present; **the dev box (<host>) has neither, so any gate needing them runs via the compiler tool
      on a host that does**
- [ ] Provision **both** credentials in <secret-manager> (operator action): `CONSTELLATION_MAESTRO_TOKEN`
      (Terminus → Maestro control plane) and `MAESTRO_MUSE_TOKEN` (Maestro → Muse events)
- [ ] Provision `MAESTRO_STREAM_SIGNING_KEY` in <secret-manager> — without it the media plane refuses to start
- [ ] Provision **two** Postgres roles and **two** DSNs via the `pg_*` sanctioned door (S9-pg), per
      §0.1 — a single mixed-grant role is the superseded design:
      - `maestro_ro` → `MAESTRO_DATABASE_URL_RO`: `SELECT` on `media_items`, `media_files`,
        `accounts`, `browser_account_map`. **No grant of any kind** on taste, embedding, or
        play-event tables
      - `maestro_rw` → `MAESTRO_DATABASE_URL_RW`: `SELECT, INSERT, UPDATE, DELETE` on
        `playback_sessions` and `maestro_event_outbox` only. **`SELECT` is required, not optional** —
        without it every session read and outbox poll fails at the first query (§0.1). No grant on
        any library table
- [ ] Confirm `MAESTRO_MEDIA_ROOTS` (or the inherited `MUSE_FOUNDRY_ALLOWED_ROOTS`) matches the
      read-only library mount on the chosen host
- [ ] Seed `browser_account_map` with a row per household member (operator action, through `pg_*`) and
      set `MAESTRO_DEFAULT_ACCOUNT_ID` — an empty map means every session lands on the default, which
      is attribution theatre rather than attribution (MDLV-07)
- [ ] Apply the session, outbox, `browser_account_map`, and `event_id` migrations to the live database
      with/before the image swap — migrations are not auto-applied at startup
- [ ] Add the `maestro` systemd unit and extend `OCI_INSTALL` in the muse module conf (operator ops
      action, epic §11)
- [ ] Baseline: `cargo test` green on Muse main; record the count
