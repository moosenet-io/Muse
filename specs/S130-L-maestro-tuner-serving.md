# S130-L — Maestro: linear-channel serving (the tuner transport moves out of the brain)

plane_project: MUSE
module: Maestro
prefix: MTUN
spec_id: S130-L-maestro-tuner-serving

## Metadata
- **Author:** Moose
- **Session:** S130
- **Date:** 2026-08-01
- **Repo / binary:** `moosenet/Muse`, the **`maestro`** `[[bin]]` target (epic §2 — second binary in
  the existing crate, **not** a new repo). New modules under `src/maestro/tuner/`; the Muse-side
  changes are in `src/tuner/`, `src/streaming/`, `src/media/`, `src/config.rs`.
- **Module version:** Muse v0.2 → v0.3 (linear-channel serving relocates to the `maestro` binary)
- **Estimated total:** ~34h autonomous agent work across 11 items
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7 as established by epic §9. This spec adds no GUI
  surface, no new credential beyond the tuner stream-signing key, and no new egress. Clause 1's
  documented media-plane carve-out (epic §8.6) applies here with unusual force: an HDHomeRun client
  is a **persisted device configuration**, not a browser session, and it cannot traverse the
  Terminus gateway at all — see §4.
- **Depends on:** `S130-D-maestro-delivery.md` (`PlaybackSession` model, `MediaHandle` resolver and
  its root confinement, session lifecycle/reaper/orphan sweep, concurrency cap, play-event outbox),
  `S130-E-maestro-transcode.md` (process supervision, kill semantics, orphan reaping — this spec
  reuses E's supervisor for its ffmpeg children rather than growing a second one).
  Spec B supplies the `maestro` binary skeleton, config, and bearer auth.
- **Blocks:** nothing. This is the last item in the epic's dependency graph by design (§1c).
- **Context:** Epic §4b. The epic's central architectural claim is that "a wedged or OOM-killed
  ffmpeg must not take down the taste brain." `src/streaming/mod.rs` spawns ffmpeg **inside the
  `muse` process today** to serve linear channels — the clearest existing instance of exactly the
  workload §2 forbids sharing the brain's failure domain. This spec resolves that inconsistency.
  **Channel composition stays in Muse; channel serving moves to Maestro.**

---

## 1. The honest framing: the epic's isolation claim is incomplete until this lands

State this plainly, because it is the reason the spec exists and the reason it is scheduled last.

**Until MTUN merges, epic §2's crash-isolation property is true of on-demand playback and false of
linear channels.** Every claim the epic makes — "a wedged or OOM-killed ffmpeg takes down
`maestro.service` and nothing else; the taste brain, acquisition pipeline, proactive outbox, and
linear-channel tuner keep running" — is, as written, self-contradicting on its last clause. The
linear-channel tuner *is* an ffmpeg host. Spec E's chaos test (MTRX-16) will pass, and it will be
proving isolation for a code path that is not the one currently spawning long-lived ffmpeg
processes in production.

So the isolation claim carries an asterisk, and the asterisk should be written down in the epic
rather than lived with silently:

> Crash isolation is complete for on-demand playback as of spec E, and complete for **all**
> playback as of spec L. Between E and L, an ffmpeg failure while serving a linear channel can
> still take `muse` down.

**Why "last" is nonetheless correct, not negligent.** The linear pipe is `-c copy` — no decode, no
encode, ~1% of one core per viewer, memory bounded by a 64 KiB read buffer per program. It has run
in production without incident. The realistic failure is not "ffmpeg wedges under load" but
"ffmpeg hits a malformed file and dies", which today already degrades gracefully (the generator
logs and skips to the next program). The risk is real but small, whereas the migration touches a
**working, in-household-use feature** whose breakage is immediately visible to people who did not
consent to being test subjects. Doing it after D and E means the session model, the supervisor, the
reaper, the caps, and the chaos harness all already exist and are proven — this spec spends its
budget on the cutover, which is where the actual risk is.

**What it is not.** It is not a rewrite. `onnow.rs` is well-factored pure math and moves unchanged;
`ffmpeg.rs` is already a pure argument builder and is reused as-is; the stdout-chaining generator in
`streaming/mod.rs` is a good shape and is ported, not redesigned. If this spec produces a new
scheduling algorithm, a new EPG renderer, or a second copy of the guide grid, it has failed.

---

## 2. The split, stated precisely

| Concern | Owner | Why |
|---|---|---|
| The `channel_programs` grid, the rolling-window scheduler, `ensure_rolling_window` | **Muse** | Composition. It is a *write* to the grid, and Muse is the single writer. |
| The director, the composer, presets, serendipity, persona-aware programming | **Muse** | Taste-adjacent brain work; nothing about it is transport. |
| "What is on channel N right now, and how far in?" | **Muse decides, Maestro consumes** | Requires topping off the window (a write). Maestro asks; it never computes it from a raw grid read. |
| EPG / XMLTV generation | **Muse** | A rendering of the grid. Pure composition output. |
| `/discover.json`, `/lineup.json`, `/lineup_status.json`, `/muse.m3u`, `/xmltv.xml` | **Muse** | See §4 — this is the decision, and it is not the obvious one. |
| The ffmpeg spawn, stdout chaining, MPEG-TS transport, join-mid-stream seek | **Maestro** | Muscle. The thing this spec moves. |
| The advertised per-channel stream URL | **Muse renders it, Maestro serves it** | §5 — this one field is the entire cutover seam. |

**The one new data flow, and its direction.** Maestro asks Muse "what is on channel N now?" over
HTTP and receives a bounded playlist of item references plus a seek offset. Maestro resolves those
references to on-disk files through spec D's `MediaHandle` resolver (read-only DB, root-confined),
exactly as it does for on-demand playback. **Maestro never reads `channel_programs` and never calls
the scheduler.** Two reasons, and the second is the load-bearing one:

1. It keeps composition a Muse concern in the code, not merely in this document.
2. **Resolving on-now requires a write.** `build_stream_response` calls
   `tuner::scheduler::ensure_rolling_window` before resolving, precisely so a channel that has
   fallen behind tops itself off instead of 503-ing. Maestro connects under a role that cannot
   write. A Maestro that read the grid directly would either need a widened grant — the exact
   widening epic §2 forbids — or would serve stale/absent grids. Asking Muse is not a compromise;
   it is the only shape that preserves the single-writer invariant.

---

## 3. What already exists, verified by inspection 2026-08-01 (`e8499aa`)

Do not re-derive this.

- **`src/streaming/mod.rs` (449 lines)** — `stream_channel` / `build_stream_response`. Tops off the
  window, resolves on-now, spawns ffmpeg for the *first* program **before** committing to a response
  (so a spawn failure is a clean 501/503 rather than a stream that dies after the headers), then
  yields an `async_stream` generator that spawns each subsequent program lazily and chains 64 KiB
  reads from each child's stdout into one `video/mp2t` body. Unresolvable rows are logged and
  skipped, never fatal. `kill_on_drop(true)` on every child.
  **This shape is correct and is ported, not redesigned.**
- **`src/streaming/onnow.rs` (188 lines)** — `resolve_on_now(&[ChannelProgram], now) -> Option<OnNow>`
  with `{ current, seek_ms, upcoming }`. Pure, no I/O, 7 unit tests covering mid-program seek, exact
  start, one-tick-before-end, gaps, empty grids, unsorted input, and overlap tie-breaking. `seek_ms`
  is clamped to `[0, duration_ms]`. **Well-factored; it moves, it is not rewritten.**
- **`src/streaming/ffmpeg.rs` (285 lines)** — `build_args(file_path, seek_ms)` emits
  `-hide_banner -loglevel error -y [-ss S] -i FILE -c copy -f mpegts pipe:1`, with `-ss` deliberately
  **before** `-i` (fast demuxer input seek, correct for a copy pipeline). Also `join_media_path`,
  `build_still_args`, and `classify_spawn_error` → `BinaryMissing` (501) vs `SpawnError` (503).
  Pure; already shared with `crate::matching`. **Reused as-is by both binaries.**
- **`src/tuner/hdhr.rs`** — `lineup_entries(state)` is the single place the per-channel stream URL is
  constructed: `format!("{base}/auto/v{}", c.id)`. `/lineup.json`, `/muse.m3u` and (via `channel_ref`)
  `/xmltv.xml` all derive from it, "so all three stay in agreement" (its own doc comment).
  `discover.json` advertises `DeviceID` from `MUSE_HDHR_DEVICE_ID`, `BaseURL`, `LineupURL`, and
  `TunerCount: 4`.
- **`src/tuner/mod.rs`** — `base_url(state)` = `MUSE_PUBLIC_URL` or `http://{bind_addr}`.
- **`src/http/mod.rs:246-254`** — `tuner_routes()` is merged at the router **root and outside the
  `protected` router**. Every tuner path, `/auto/v:channel_id` included, is **unauthenticated
  today.** This is not an oversight to fix in passing; it is a constraint (§4b).
- **`src/tuner/scheduler.rs` (834 lines)** and **`src/channels/`** — composition. Untouched by this
  spec except that `scheduler::ensure_rolling_window` gains an HTTP caller.

---

## 4. Decision: the tuner discovery surface **stays on Muse**. Only `/auto/vN` moves.

This is the spec's central design question and the answer is not symmetric with the rest of the
epic. Three independent arguments, any one of which would be sufficient.

### 4a. The tuner is one device with one persistent identity, and Plex remembers it

An HDHomeRun client does not rediscover a tuner per playback. Plex pairs the device **once** — by
`DeviceID` from `/discover.json` — and stores that pairing, the DVR configuration, the channel
mapping, the EPG source binding, and any scheduled recordings against it. Re-pointing
`/discover.json` at a different host and port produces, from Plex's point of view, a **different
tuner device**. The household consequence is a re-pair: reconfigure the DVR, re-map channels,
re-bind the guide, and lose scheduled recordings. That is a real, user-visible cost paid for
nothing, since the discovery documents are static JSON derived from a grid Muse owns anyway.

Contrast with the stream URL, which Plex re-reads from `/lineup.json` on every channel scan and
does not persist as identity. **The lineup URL is a cheap, re-read field; the device identity is an
expensive, persisted one.** Move the cheap one.

### 4b. Discovery is composition output; only the transport is muscle

`/lineup.json` is the channel list. `/xmltv.xml` is the programme grid. `/muse.m3u` is the channel
list in a second syntax. All three are pure renderings of `channels` + `channel_programs` — the
brain's output, requiring the guide window, the scheduler, channel names, artwork URLs, and
season/episode parsing. Moving them to Maestro would mean Maestro reading (or being handed) the
whole grid and growing an EPG renderer: precisely the "Maestro grows a second library model"
failure epic §10.2 names as a standing risk, arriving through the least suspicious door.

`/auto/vN` is the only endpoint in the tuner surface that spawns a process, holds a file handle,
and runs for hours. It is the only one that is muscle.

### 4c. A split surface is what clients already tolerate here

The stated worry — "clients treat the tuner as one device, so a split surface has real
consequences" — is true of *identity* and false of *byte delivery*. The HDHomeRun protocol is
explicitly a discovery document that hands out arbitrary per-channel stream URLs; real HDHomeRun
hardware and every emulator (including the M3U path, where `tvg` attributes and stream URLs are
routinely different hosts) already assume the stream URL is an independent address. The protocol
was designed for exactly this indirection. We are using a seam that exists, not inventing one.

**The M3U/XMLTV pairing reinforces the same answer.** A player consuming `/muse.m3u` + `/xmltv.xml`
matches programmes to channels by `tvg-id` (`muse-{channel_id}`, from `hdhr::channel_ref`). Split
the two documents across hosts and that correlation acquires a cross-origin failure mode for no
benefit. Keep them together, on Muse, where the grid is.

### 4d. The consequence that must be handled: the stream endpoint is unauthenticated

Discovery staying on Muse is easy. The stream moving is not, because of §3's last bullet:
**`/auto/vN` is unauthenticated today**, and it has to be — Plex's tuner client cannot set an
`Authorization` header, does not hold a Terminus cookie, and never traverses `proxy_maestro`.
Maestro's `/playback/*` surface (spec B/MBAK-02) is bearer-gated. So the migration cannot simply
relocate the handler onto an authenticated router.

**Decision:** Maestro serves linear channels on a **separate, deliberately non-bearer route**
(`/tuner/v{channel_id}`) protected by a **stable per-channel HMAC signature** minted by Muse into
the advertised lineup URL, plus D's root confinement, plus an optional source-CIDR allowlist.

The signature is **stable, not expiring**, and that is a considered deviation from epic §8.7's
session-scoped expiring URLs — recorded here rather than discovered later:

- §8.7's expiring URLs are correct for a **session**: minted at session open, handed to a player,
  used for minutes. A tuner URL is the opposite — it is written into a persisted device config and
  replayed for months. An expiring token in `/lineup.json` guarantees a channel that plays the day
  it was scanned and 401s a week later, and the failure would present as "live TV is broken" with
  no obvious cause.
- What the signature buys is that the URL is **unguessable and channel-scoped**: it is not a
  bearer for anything else, it grants exactly one channel's transport, and it is revocable by
  rotating `MAESTRO_TUNER_SIGNING_KEY` (after which the next `/lineup.json` fetch re-mints every
  URL — the same re-read seam §4a relies on).
- The threat model this actually addresses is *unauthenticated LAN enumeration of channel streams*,
  which is what exists today with zero protection. This is strictly better than the status quo,
  and it is honest about not being an identity mechanism.

---

## 5. The cutover seam, and why it is one line

`hdhr::lineup_entries` is the only construction site of the per-channel stream URL, and
`/lineup.json`, `/muse.m3u` and `/xmltv.xml` all derive from it. So the entire cutover is:

```rust
// today
url: format!("{base}/auto/v{}", c.id)

// after MTUN-08
url: tuner_stream_url(&state.config, &c)   // muse: {base}/auto/vN · maestro: {maestro_base}/tuner/vN?sig=…
```

driven by one config value, `MUSE_TUNER_SERVING` ∈ `{muse, maestro}`, default `muse`.

**Both paths stay live for the whole transition.** Muse's `/auto/vN` handler is not touched by the
flip; it keeps working, and pointing the config back at `muse` is a complete rollback that requires
no redeploy of Maestro and no client reconfiguration — only a `/lineup.json` re-read, which Plex
performs on a channel-scan or a service restart. Removal of the old path is a **separate item
(MTUN-10) gated on the verification in MTUN-09**, not a step in the same change.

**Verification before removal is mandatory and is written as an item, not a hope.** MTUN-09 defines
the checklist: the device pairing is unchanged (`DeviceID` identical, no re-pair prompt), every
channel in `/lineup.json` tunes, join-mid-stream lands at the right offset, a channel plays across
a program boundary, the EPG still correlates, and a rollback flip returns to the old path within
one lineup re-read. Only after that passes does MTUN-10 delete Muse's spawn.

---

## 6. Reusing Maestro's session machinery, not reimplementing it

A linear channel is a long-lived stream. That is precisely the thing spec D's session model and
spec E's supervisor were built for, and this spec must consume them rather than grow a parallel set.

| Need | Reused from | Note |
|---|---|---|
| Session identity, state, position, `account_id` | MDLV-01 `PlaybackSession` | Extended with `SessionKind::LinearChannel { channel_id }`, not a new table |
| Idle timeout, orphan sweep on restart, concurrency cap | MDLV-07 | A tuner that walks away without a goodbye is the same problem as a TV that loses power |
| Liveness without heartbeats | MDLV-07 step 5 | A tuner client **never** heartbeats; byte-serving touching `last_heartbeat_at` is exactly what makes this work, unchanged |
| ffmpeg spawn, supervision, kill semantics, zombie reaping | MTRX-05 / MTRX-07 | Including `SIGCONT`-before-`SIGKILL` ordering |
| Path resolution + root confinement | MDLV-02 `MediaHandle` | The tuner path resolves items through the *same* resolver; there is no second path-handling code |
| Play events → Muse | MDLV-08 outbox | Linear viewing becomes visible to watch history for the first time |
| Metrics | MDLV-10 / MTRX-15 | Linear sessions appear in the same tier distribution and session gauges |

**Session open is implicit.** A tuner client issues a bare `GET`; it cannot perform D's
`POST /playback/sessions` handshake. `/tuner/v{id}` therefore opens a session as a side effect of
the GET and closes it when the response body ends or the client disconnects. This is the *only*
structural difference from an on-demand session, and it is confined to one handler.

---

## 7. Two costs this migration creates, named rather than discovered

1. **A Maestro restart now kills live TV.** Today a `muse` restart does that; after this spec a
   `maestro` restart does. Epic §2c's mitigations are what make it payable and they must actually be
   wired for the linear case: the per-bin restart guard (a Muse-only hotfix must not restart
   `maestro.service`), `TimeoutStopSec=90` graceful drain (stop admitting new tunes, keep serving
   live ones), and — the linear-specific part — **a tuner client's own reconnect is the resume
   story**. Plex retries a dropped tuner stream; because on-now is recomputed on every tune, a
   reconnect lands at the correct live position automatically. Linear actually recovers *better*
   than on-demand here, and the spec should say so rather than assume it.
2. **A new inter-process dependency on the hot path of a tune.** `/tuner/vN` cannot serve without
   Muse answering the on-now query. If Muse is down, live TV is down — which was already true (Muse
   served the bytes), so this is not a regression, but it is now a *network* dependency rather than a
   function call. MTUN-02 bounds it with a short timeout and a clear 503, and MTUN-06's playlist
   look-ahead means a Muse blip mid-program does not interrupt an in-flight stream.

---

## Pre-flight

- Repository: `moosenet/Muse` on Gitea. Working branch off `origin/main` (`e8499aa` or later).
- **Specs D and E must be merged and deployed first.** This spec consumes `PlaybackSession`,
  `MediaHandle`, the reaper, the concurrency cap, the event outbox, and E's process supervisor. Do
  not start MTUN-03 onward against an unmerged D.
  *(Note: `S130-D-maestro-delivery.md` was being revised concurrently on 2026-08-01; re-read it
  before implementing and reconcile any renamed type against this spec's references.)*
- Dependencies on the build/test host: `cargo`, `rustc` (pinned toolchain), `ffmpeg` **and**
  `ffprobe`. **Verified 2026-07-31: neither is present on the dev box (<host>)** — the MTUN-11 chaos
  harness and any ffmpeg-touching test must run on the Muse deploy host or <host> via the compiler
  tool. Never install ffmpeg on the dev box to make a gate pass locally.
- Vault secrets required (<secret-manager>, materialized at runtime — never authored into `.env` by hand):
  `MAESTRO_TUNER_SIGNING_KEY` (new, this spec), plus the existing `MAESTRO_API_TOKEN`,
  `MAESTRO_DATABASE_URL_RO`, `MUSE_API_TOKEN`, `MAESTRO_MUSE_TOKEN`.
- Operator ops prerequisites (no code, no Plane item — the sanctioned ops-action exception):
  - Provision `MAESTRO_TUNER_SIGNING_KEY` in <secret-manager>.
  - Confirm `maestro.service` is reachable from the household LAN on `MAESTRO_BIND_ADDR` **without**
    traversing the Terminus gateway (§4d — a tuner client cannot use the proxy).
  - Confirm `maestro.service` has `TimeoutStopSec=90` and that the updater's per-bin restart guard
    (epic §2c) is in place before the flip, not after.
- Infrastructure: Gitea reachable, Plane reachable via the Terminus Plane tool, the read-only
  library mount present on the Maestro host.
- Baseline: `cargo test` green on Muse `main`; record the count. Record the current
  `/lineup.json` output and `discover.json` `DeviceID` verbatim — MTUN-09 diffs against them.
- Prefix: `plane_prefix_check MTUN` → `plane_prefix_register` → `plane_prefix_promote`.

---

## 8. Items

### MTUN-01: Promote `streaming::onnow` to shared `src/media/onnow.rs`
- **Priority:** High
- **Labels:** maestro, muse, refactor, pure
- **Agent:** claude
- **Estimate:** 2h
- **Description:** Move the pure on-now/seek-offset math out of Muse's streaming module into the
  shared `src/media/` tree, so both binaries reason about "what is airing and how far in" with one
  implementation. This is a **mechanical move with zero behaviour change**, done first and on its
  own, following the precedent epic §2b set when it promoted `foundry::probe` to `src/media/`:
  move the code before extending anything that depends on it, with the existing tests green.

  `onnow.rs` is already the right shape — no I/O, deterministic tie-breaking, clamped `seek_ms`,
  seven unit tests. Nothing about it is streaming-specific; it is arithmetic over grid rows. The
  reason to move rather than share-by-reaching-across is that after MTUN-05 the consumers are in two
  different binaries, and a `crate::streaming::onnow` path in Maestro code would read as Maestro
  depending on Muse's transport layer, which is exactly the coupling this epic is untangling.

  ## FILES
  - `src/media/mod.rs` — add `pub mod onnow;` (create the module if spec A/C have not yet landed it)
  - `src/media/onnow.rs` — moved verbatim from `src/streaming/onnow.rs`, tests included
  - `src/streaming/mod.rs` — drop `pub mod onnow;`, update the two call sites to `crate::media::onnow`
  - `src/streaming/onnow.rs` — deleted

  ## APPROACH
  1. `git mv src/streaming/onnow.rs src/media/onnow.rs`. Do not edit the body in this item.
  2. Fix the `use crate::models::channel::ChannelProgram` import path if `src/media/` sits at a
     different depth; nothing else in the file is location-dependent.
  3. Update the module doc comment's first line to say it is shared between the `muse` and `maestro`
     binaries and that it is the single authority for join-mid-stream offset arithmetic. Keep every
     existing paragraph — the MUSE-28 scheduler-invariant note and the tie-breaking rationale are
     still exactly right and re-deriving them later would be waste.
  4. Update `src/streaming/mod.rs`'s two references (`onnow::resolve_on_now`, the `OnNow` binding)
     and its module doc's bullet list.
  5. `grep -rn "streaming::onnow" src/` must return nothing afterwards.

  ## TEST PLAN
  - `cargo test` — all seven existing `onnow` tests pass unmoved at their new path.
  - `cargo test streaming` — the `MUSE_TEST_DATABASE_URL`-gated live test in `src/streaming/mod.rs`
    still compiles and still skips cleanly when the env var is unset.
  - `cargo build --workspace` clean; `cargo clippy` no new warnings.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - `src/media/` may not exist yet if specs A and C landed in a different order — create it with a
    module doc that names it as the shared media core per epic §2b, rather than inventing a new home.
  - A concurrent spec A/C branch also touching `src/media/mod.rs` — this is a one-line addition;
    resolve by re-adding the line, never by relocating the other spec's modules.

- **Acceptance criteria:**
  - [ ] `src/streaming/onnow.rs` no longer exists and `grep -rn "streaming::onnow" src/` is empty
  - [ ] All seven `onnow` unit tests pass at `src/media/onnow.rs` with no assertion changed
  - [ ] `resolve_on_now`'s signature, clamping, and tie-breaking behaviour are byte-identical
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTUN-02: Muse-side channel resolution API — `GET /channels/{id}/onnow`
- **Priority:** Critical
- **Labels:** muse, channels, http, composition
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MTUN-01
- **Description:** The one door through which Maestro learns what to play. Muse tops off the rolling
  guide window (a **write**, and the reason this cannot be a Maestro-side DB read — §2), resolves
  on-now, and returns a bounded, ordered playlist of item references with the join offset.

  The response deliberately carries **item references, not file paths**. Path resolution belongs to
  Maestro's `MediaHandle` resolver (MDLV-02), where it is canonicalised and root-confined; a Muse
  endpoint that handed out absolute paths over HTTP would make Muse an arbitrary-path oracle for
  Maestro and would bypass the confinement the epic §10b requires *even when Muse asks*.

  ## FILES
  - `src/tuner/onnow_route.rs` — new; the handler
  - `src/tuner/mod.rs` — `pub mod onnow_route;`
  - `src/http/mod.rs` — register on the **`protected`** router (Maestro authenticates with a bearer;
    this is a service-to-service call, not a tuner-client call)
  - `src/models/channel.rs` — the response DTOs
  - `README.md` — document the endpoint and its contract

  ## APPROACH
  1. `GET /channels/{channel_id}/onnow?limit=N` (default 8, cap 32) →
     ```rust
     struct OnNowResponse {
         channel_id: i64,
         resolved_at: DateTime<Utc>,          // server clock, always
         current: OnNowEntry,
         upcoming: Vec<OnNowEntry>,           // ordered by start_at, at most `limit`
     }
     struct OnNowEntry {
         program_id: i64,
         item: ProgramItemRef,                // Episode{episode_id} | Movie{media_item_id} | Interstitial{interstitial_id}
         seek_ms: i64,                        // 0 for everything but `current`
         start_at: DateTime<Utc>,
         end_at: DateTime<Utc>,
         duration_ms: i64,
         title: String,                       // for the session/log line only, never for matching
     }
     ```
     `ProgramItemRef` mirrors `ChannelProgramItemType`'s three arms exactly, so an unhandled variant
     is a compile error rather than a silently-skipped program.
  2. Handler sequence, mirroring `build_stream_response`'s existing and correct ordering:
     a. `repo::channel::get_channel` → 404 if absent, 400 if `mode != linear`.
     b. `tuner::scheduler::ensure_rolling_window` — best-effort, **warn-and-continue on error**,
        preserving the existing rationale verbatim (a freshly-created channel must not 503 merely
        because the background tick has not run).
     c. `repo::channel::list_programs_in_window(now, now + channel_guide_window_hours)`.
     d. `media::onnow::resolve_on_now` → `None` ⇒ `503` with a body naming the channel, matching
        today's `ServiceUnavailable` semantics exactly.
     e. Truncate `upcoming` to `limit`.
  3. **No file resolution here.** The endpoint does not call `repo::media_file::*` and does not
     touch `media_root`. Its unresolvability semantics are Maestro's problem, deliberately: a
     program with no attached file is returned normally and Maestro skips it, preserving today's
     "log and skip, never fail the stream" behaviour at the layer that can actually observe it.
  4. Authenticated via the existing `auth::require_api_token` on `protected` — Maestro presents
     `MAESTRO_MUSE_TOKEN` (already provisioned for the MDLV-08 event path; **reuse it, do not mint a
     third credential** — epic §10b's "there are TWO, not one" lesson applies in both directions).
  5. The endpoint is Muse's *only* new obligation in this spec. It must not grow a stream, a path, a
     signed URL, or a transcode hint — those are all §2's other column.

  ## TEST PLAN
  - `cargo test` — handler tests via `oneshot` against a `FakeBackend`-style state:
    - Linear channel with a live grid → `200`, `current.seek_ms` matches `resolve_on_now`
    - `mode != linear` → `400`
    - Unknown channel → `404`
    - Empty/exhausted grid → `503` naming the channel
    - `limit` clamping: `limit=0` → 1 entry (`current` only); `limit=999` → capped at 32
    - `ensure_rolling_window` failing → still `200` from the already-scheduled rows (warn path)
    - Unauthenticated request → `401`
  - Live-DB test gated on `MUSE_TEST_DATABASE_URL`, reusing the seeding pattern already in
    `src/streaming/mod.rs`'s existing test: seed a channel + program starting 10 minutes ago, assert
    `seek_ms` ≈ 10 minutes and `current.program_id` matches.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - Program boundary crossed between `ensure_rolling_window` and `list_programs_in_window` — the
    later `now` is used consistently for both resolution and `resolved_at`; a one-tick race yields a
    slightly-stale `seek_ms`, which is bounded by request latency and immaterial for a live channel.
  - A grid with overlapping rows (not a scheduler output, but possible from a fixture) — inherits
    `resolve_on_now`'s deterministic latest-`start_at` tie-break; do not add a second rule here.
  - Channel deleted mid-request → `404`, never a panic on a missing row.
  - `channel_guide_window_hours` misconfigured to 0 — clamp the query window to at least one hour and
    log once; a zero window would make every channel permanently 503.

- **Acceptance criteria:**
  - [ ] `GET /channels/{id}/onnow` returns current + bounded upcoming with a correct `seek_ms`
  - [ ] The endpoint returns **item references only** — no file path, no `media_root`, no absolute
        path appears anywhere in the response type (negative test on the DTO)
  - [ ] `ensure_rolling_window` failure warns and continues, never 503s a channel with a usable grid
  - [ ] The endpoint is bearer-protected and authenticates with the existing Maestro→Muse token
  - [ ] `limit` is clamped at both ends
  - [ ] README documents the endpoint, its auth, and that it is the single composition door
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTUN-03: `SessionKind::LinearChannel` — extend the session model, do not fork it
- **Priority:** High
- **Labels:** maestro, session, model
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** MTUN-02
- **Description:** Teach spec D's `PlaybackSession` about linear channels so the reaper, the
  concurrency cap, the orphan sweep, the Activity panel, and the metrics all cover tuner streams for
  free. The failure this item exists to prevent is a second, parallel session concept living inside
  the tuner handler — which is how a codebase ends up with two now-playing sources that disagree,
  the exact drift epic §4b warns about for the Activity panel.

  ## FILES
  - `migrations/XXXX_playback_sessions_linear.sql` — additive columns
  - `src/models/playback_session.rs` — `SessionKind`, `channel_id`, `program_id`
  - `src/maestro/session/mod.rs` — construction helper for a linear session
  - `src/repo/playback_session.rs` — the active-sessions query gains the new fields

  ## APPROACH
  1. Additive migration only: `session_kind TEXT NOT NULL DEFAULT 'on_demand'`,
     `channel_id BIGINT NULL`, `current_program_id BIGINT NULL`. **No `NOT NULL` without a default**
     and no column rename — a migration that breaks the running Maestro's read path is the S127
     lesson (v4.6) and it is cheap to avoid here.
  2. `enum SessionKind { OnDemand, LinearChannel }` with a `CHECK`-equivalent invariant enforced in
     Rust: `LinearChannel` requires `channel_id.is_some()`; `OnDemand` requires it to be `None`.
     Encode it as a constructor that cannot produce an invalid pair, not as a validator.
  3. `current_program_id` is updated as the stream advances (MTUN-06) so the Activity panel can show
     *what* is airing, not merely *that* a channel is being served.
  4. **No new table.** A linear session's `item` reference is the currently-airing program's item,
     which reuses the existing item columns unchanged; `channel_id` is the only added dimension.
  5. Metrics: linear sessions carry `tier = "remux"` (they are `-c copy`) with a
     `kind="linear_channel"` label so MDLV-10's tier distribution stays honest and E's telemetry can
     tell a 24/7 pipe apart from a two-hour film — a distinction spec F's GPU sizing will need.

  ## TEST PLAN
  - `cargo test`:
    - Constructing a `LinearChannel` session without a `channel_id` is not expressible (type-level)
    - `OnDemand` sessions round-trip with `session_kind = 'on_demand'` and null channel columns
    - The active-sessions query returns both kinds with the correct discriminator
    - Migration applies to a database already containing `on_demand` rows and leaves them valid
  - Live-DB gated round trip on `MUSE_TEST_DATABASE_URL`.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - A session whose channel is deleted mid-stream — `channel_id` is not a hard FK with cascade
    delete; the session closes normally on the next resolution failure rather than vanishing
  - Rolling back to a Maestro build without `SessionKind` — the defaulted column makes the old
    binary's reads still valid (this is why the migration is additive)
  - The reaper closing a linear session — identical path to on-demand; no special case

- **Acceptance criteria:**
  - [ ] Migration is additive, defaulted, and applies cleanly over existing rows
  - [ ] A `LinearChannel` session without a `channel_id` is unconstructable
  - [ ] Reaper, orphan sweep, and concurrency cap operate on linear sessions with no new code paths
  - [ ] Linear sessions are distinguishable in metrics by a `kind` label
  - [ ] The migration is applied to the live DB as part of DEPLOY (migrations are not auto-applied)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTUN-04: Stable channel-scoped stream URL signing
- **Priority:** Critical
- **Labels:** maestro, muse, security, pure
- **Agent:** claude
- **Estimate:** 3h
- **Description:** The pure signing/verification pair that makes an unauthenticated tuner route
  acceptable (§4d). Muse mints; Maestro verifies; the shared key comes from <secret-manager>.

  Read §4d before implementing: this signature is **deliberately non-expiring**, because a tuner URL
  is a persisted device configuration replayed for months, not a session handle used for minutes.
  Document that reasoning **in the code**, not only here — a future reviewer will otherwise see an
  unexpiring token and "fix" it, breaking live TV in a way that surfaces days later.

  ## FILES
  - `src/media/tuner_sig.rs` — new; pure `sign_channel` / `verify_channel`
  - `src/config.rs` — `MAESTRO_TUNER_SIGNING_KEY` (via `SecretManager::get()`, never `std::env::var`)

  ## APPROACH
  1. `sign_channel(key: &SecretString, channel_id: i64) -> String` — HMAC-SHA256 over the exact byte
     string `format!("muse-tuner:v1:{channel_id}")`, URL-safe base64, no padding. The `v1` prefix is
     a version anchor so a future scheme change is distinguishable rather than ambiguous.
  2. `verify_channel(key, channel_id, sig) -> bool` — recompute and compare in **constant time**
     (`subtle::ConstantTimeEq` or equivalent). A naive `==` here is a timing oracle on a LAN-reachable
     endpoint; it costs one line to not have one.
  3. Fail **closed**: an unset or empty key makes `verify_channel` return `false` for every input and
     logs once at startup. An unconfigured signing key must never mean "allow everything" (the
     `dsn_guard_fail_closed_lesson` pattern).
  4. The key is read through `SecretManager::get("MAESTRO_TUNER_SIGNING_KEY")` (S7) and held as a
     `SecretString`; it is never logged, never included in an error body, and never appears in a
     `Debug` impl.
  5. Rotation semantics, documented on the module: rotating the key invalidates every advertised URL;
     the next `/lineup.json` fetch re-mints them. Note in the doc that this is the intended
     revocation mechanism and that it costs one channel-scan, not a re-pair (§4a).

  ## TEST PLAN
  - `cargo test`, all pure, no I/O:
    - Round trip: `verify_channel(k, id, sign_channel(k, id))` is true
    - Cross-channel replay: a signature for channel 5 fails for channel 6
    - Wrong key fails
    - Empty/unset key fails **every** verification including a correctly-computed one (fail-closed)
    - Signature is URL-safe (no `+`, `/`, or `=` in the output) across 1,000 generated ids
    - Tampered signature (single character flipped) fails
  - Verify the key never appears in `Debug`/`Display` output (assert on a formatted struct).
  - Verify no hardcoded infrastructure values or secret literals.

  ## EDGE CASES
  - Negative or zero `channel_id` — signs and verifies consistently; the route's own parsing rejects
    non-positive ids before this is reached
  - Key rotated while a stream is in flight — an in-flight stream is not re-verified mid-body; it
    finishes and the client's reconnect uses the re-minted URL
  - Muse and Maestro holding different keys (a partial secret rollout) — every tune 403s with a log
    line naming a signature mismatch, which is the correct, diagnosable failure

- **Acceptance criteria:**
  - [ ] Signing and verification are pure, deterministic, and constant-time on comparison
  - [ ] An unset key fails closed for every verification and logs once
  - [ ] A signature is channel-scoped and cannot be replayed across channels
  - [ ] The key is read via `SecretManager::get()`, never `std::env::var`, and never logged
  - [ ] The non-expiring design decision and its rationale are documented in the module doc comment
  - [ ] No hardcoded infrastructure values or secrets in new/modified code
  - [ ] All existing tests still pass

---

### MTUN-05: Maestro `/tuner/v{channel_id}` — the ported streaming handler
- **Priority:** Critical
- **Labels:** maestro, streaming, http
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MTUN-02, MTUN-03, MTUN-04
- **Description:** The migration itself. Port `build_stream_response`'s shape into Maestro: resolve
  the playlist from Muse (MTUN-02), resolve each entry to a `MediaHandle` (MDLV-02), spawn ffmpeg
  per program with `streaming::ffmpeg::build_args`, and chain their stdout into one `video/mp2t`
  body.

  **Port, do not redesign.** The existing handler's decisions are correct and each one must survive
  the move, with the reasoning carried across in comments:
  - The **first** program is resolved and spawned *before* the response is committed, so a failure on
    "now" is a clean `501`/`503` rather than a response that starts and then dies. This is the single
    most important behaviour in the file.
  - Subsequent programs are resolved and spawned **lazily**, one at a time, inside the generator — a
    bad row further down the grid never blocks or fails the ones before it.
  - An unresolvable or unspawnable program is **logged and skipped**, never fatal.
  - `classify_spawn_error` maps `BinaryMissing` → `501` and everything else → `503`.
  - `kill_on_drop(true)`, `stdin(null)`, `stderr(null)`, 64 KiB reads.

  ## FILES
  - `src/maestro/tuner/mod.rs` — new module
  - `src/maestro/tuner/stream.rs` — the handler + generator
  - `src/maestro/tuner/resolve.rs` — Muse on-now client (typed, timeout-bounded)
  - `src/maestro/http/mod.rs` — register `/tuner/v:channel_id` on the **non-bearer** router
  - `src/config.rs` — `MAESTRO_MUSE_URL`, `MAESTRO_TUNER_ENABLED`, `MAESTRO_TUNER_ALLOWED_CIDRS`
  - `README.md` — the tuner serving surface

  ## APPROACH
  1. Route registration: `/tuner/v:channel_id` goes on a router **outside** `require_bearer`, with a
     module doc comment stating why (§4d — an HDHomeRun client cannot set a header) and what replaces
     it (signature + confinement + optional CIDR). This is the one non-bearer route in Maestro and it
     must be visibly, deliberately so.
  2. Gate order, cheapest and most-certain first:
     a. `MAESTRO_TUNER_ENABLED` (default false) — off ⇒ `404`, not `403`; a disabled feature should
        not advertise its own existence.
     b. `MAESTRO_TUNER_ALLOWED_CIDRS` if set — source not in an allowed range ⇒ `403`. Unset means
        "no CIDR restriction", which is honest for a LAN deployment; **empty-string does not mean
        allow-all** (fail-closed on a misconfigured-but-present value).
     c. `verify_channel` on the `sig` query parameter ⇒ `403` on mismatch, with a log line but a
        body that does not distinguish "bad signature" from "unknown channel".
     d. Concurrency cap (MTUN-07).
  3. Resolve: `GET {MAESTRO_MUSE_URL}/channels/{id}/onnow?limit=8` with a **short timeout**
     (`MAESTRO_MUSE_TIMEOUT_MS`, default 3000) and the Maestro→Muse bearer. Map Muse's `503` through
     as `503`, its `404` as `404`; a timeout or connection failure is `503` with a distinct log line
     (§7.2 — this is the new cross-process dependency and it must be diagnosable, not a generic 500).
  4. Open a `SessionKind::LinearChannel` session (MTUN-03) before the first spawn, so the cap, the
     reaper, and the Activity panel see the tune immediately rather than after the first byte.
  5. Resolve the first entry via `library::resolve_item` (MDLV-02) → `MediaHandle`. Note explicitly
     in code that **the path never comes from Muse** — Muse supplied an item id; the path is derived
     and root-confined locally. Unresolvable first entry ⇒ `503` naming the channel, matching today.
  6. Spawn ffmpeg via E's supervisor (MTUN-07) with
     `streaming::ffmpeg::build_args(handle.path(), seek_ms)` — the **same function**, called
     directly, not copied. `seek_ms` comes from MTUN-02's response and is applied only to `current`.
  7. Generator: same `async_stream` structure as today. On each iteration, update the session's
     `current_program_id`, touch `last_heartbeat_at` (MDLV-07 step 5 — this is what keeps a
     heartbeat-less tuner client alive), and stream 64 KiB chunks. A read error ends the stream,
     logged, as today.
  8. On body completion or client disconnect, close the session with the appropriate `stop_reason`
     and emit the MDLV-08 stop event.
  9. `Content-Type: video/mp2t`, and — new, because a tuner client benefits from it —
     `Cache-Control: no-store` and `Accept-Ranges: none` (a live pipe is not seekable; saying so
     explicitly stops a client from trying).

  ## TEST PLAN
  - `cargo test`, none of which invoke ffmpeg (the standing gate rule — ffmpeg is absent on the dev
    box):
    - Disabled feature → `404`
    - Missing/invalid/cross-channel signature → `403`
    - Source outside a configured CIDR → `403`; inside → proceeds
    - Muse on-now returning `503` → `503`; timing out → `503` with the distinct reason
    - First entry unresolvable → `503`, and **no session row is left open** (negative test)
    - `BinaryMissing` from a stubbed spawner → `501`; other spawn errors → `503`
    - A successful open (stubbed spawner yielding canned bytes) creates exactly one
      `LinearChannel` session and closes it when the body ends
    - Skip semantics: a stubbed playlist whose second entry is unresolvable streams the first and
      third without erroring
  - `MAESTRO_TEST_FFMPEG=1` on a host with ffmpeg: tune a seeded channel, assert the response is a
    well-formed MPEG-TS (ffprobe reports a video stream) — skips cleanly without ffmpeg.
  - Verify no hardcoded IPs, hostnames, or library paths in new/modified files.

  ## EDGE CASES
  - Client disconnects mid-program — `kill_on_drop` reaps the child; the session closes with
    `stop_reason = client_disconnect`; **assert no zombie** (this is the leak that accumulates on a
    24/7 feature)
  - A program shorter than its scheduled slot (bad `runtime_minutes`) — the child exits early and the
    generator advances; the stream runs ahead of the grid until the next re-poll corrects it (MTUN-06)
  - A channel with exactly one program and no upcoming — streams to the end, then MTUN-06 re-polls
  - Muse returning an `Interstitial` with no `file_path` — skipped, logged, exactly as today
  - Two tuners on the same channel — two independent sessions and two ffmpeg children; correct, and
    bounded by the cap. Do **not** attempt to share one child between clients: the join offsets differ
  - `sig` present but the channel is not `mode=linear` — Muse's `400` maps to `404` at this boundary
    (do not leak channel-mode information to an unauthenticated caller)

- **Acceptance criteria:**
  - [ ] `/tuner/v{id}` serves a continuous `video/mp2t` stream joined at the correct live offset
  - [ ] The first program is resolved and spawned before the response is committed; a failure on
        "now" is a clean `501`/`503`, never a truncated stream
  - [ ] Unresolvable/unspawnable later programs are logged and skipped, never fatal
  - [ ] File paths are resolved **locally** through `MediaHandle`; Muse supplies item ids only
  - [ ] `streaming::ffmpeg::build_args` is reused directly — no second argument builder exists
  - [ ] The route is the only non-bearer route in Maestro, and its doc comment says why
  - [ ] Signature, CIDR, and enable-flag gates all fail closed
  - [ ] A client disconnect leaves no zombie process and no open session
  - [ ] README documents the endpoint, its gates, and its non-seekability
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTUN-06: Playlist continuation — a 24/7 channel outlives any one resolution
- **Priority:** High
- **Labels:** maestro, streaming, lifecycle
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** MTUN-05
- **Description:** Today's handler resolves the guide window **once**, at tune time, and streams
  whatever it found. For a 48-hour window that is usually enough, but a linear channel is by
  definition unbounded: a tuner left on a channel for three days exhausts the playlist and the
  stream simply ends. Worse, a channel re-composed mid-stream (the director runs, a preset changes)
  is invisible to an in-flight viewer, who then sees content that no longer matches the EPG the
  client is displaying — a divergence that reads to a household member as "the guide is wrong."

  Re-poll instead: when the local playlist drops below a look-ahead threshold, ask MTUN-02 again.

  ## FILES
  - `src/maestro/tuner/stream.rs` — the continuation logic in the generator
  - `src/config.rs` — `MAESTRO_TUNER_LOOKAHEAD_MIN` (default 2 remaining entries)

  ## APPROACH
  1. Track the remaining queue length in the generator. When it drops to
     `MAESTRO_TUNER_LOOKAHEAD_MIN`, issue a background re-poll of MTUN-02 for the same channel.
  2. **Merge, do not replace.** Re-poll returns a fresh `current` + `upcoming`; append only entries
     whose `program_id` is not already queued or already played in this session. Replacing the queue
     wholesale would restart the currently-streaming program.
  3. **Ignore the re-poll's `seek_ms`.** It describes a viewer tuning in *now*; a continuing stream is
     already positioned. Only a fresh tune uses `seek_ms`. Getting this wrong produces the subtle
     failure where a long-running channel jumps forward every look-ahead — state it in a comment.
  4. A failed re-poll is **non-fatal**: log, keep streaming the queue in hand, retry on the next
     threshold crossing with a bounded backoff. A Muse blip must not interrupt live TV (§7.2).
  5. When the queue genuinely empties and a re-poll yields nothing, end the stream cleanly (the
     client reconnects and gets a fresh `503`-or-tune, which is the honest outcome for a channel with
     no programming).
  6. Update the session's `current_program_id` on each advance so the Activity panel tracks the
     actual airing item.

  ## TEST PLAN
  - `cargo test` with a stubbed resolver and a stubbed spawner:
    - Queue draining to the threshold triggers exactly one re-poll, not one per iteration
    - Re-polled entries already in the queue are not duplicated
    - The re-poll's `seek_ms` is not applied to a continuing stream (assert the spawn args carry no
      `-ss` for a continuation entry)
    - A failing re-poll leaves the in-flight stream running and retries with backoff
    - An empty re-poll on an empty queue ends the stream cleanly, closing the session
    - `current_program_id` advances with each program
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - A channel re-composed mid-stream so that the queued upcoming programs no longer exist — dedupe by
    `program_id` naturally drops them; the viewer finishes the current program and continues on the
    new grid
  - Re-poll returning a `current` that is *behind* the streaming position (a clock skew or a
    scheduler catch-up) — entries with `end_at` in the past are discarded rather than queued
  - A re-poll storm from a very short program run (many 15-second interstitials) — the threshold is
    in *entries*, and the backoff bounds the rate; assert no more than one in-flight re-poll at a time
  - Session closed by the reaper while a re-poll is in flight — the response is discarded; the
    generator has already terminated

- **Acceptance criteria:**
  - [ ] A stream continues past its initially-resolved playlist via re-poll
  - [ ] Re-polled entries are merged and deduped, never wholesale-replaced
  - [ ] `seek_ms` is applied only on a fresh tune, never on a continuation
  - [ ] A failed re-poll never interrupts an in-flight stream
  - [ ] At most one re-poll is in flight per session
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTUN-07: Supervision, caps, and orphan reaping for linear children
- **Priority:** High
- **Labels:** maestro, process, lifecycle
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** MTUN-05
- **Description:** Put the tuner's ffmpeg children under spec E's existing supervisor rather than
  letting MTUN-05 own a second, subtly-different process-management path. Long-lived children,
  orphan reaping after a restart, kill ordering, and a concurrency cap are all solved problems in
  E — a linear channel is simply another supervised child with an unusually long lifetime.

  ## FILES
  - `src/maestro/tuner/stream.rs` — spawn through the supervisor
  - `src/maestro/session/reaper.rs` — include linear sessions in the sweep (should require no change
    if MTUN-03 is right; verify and document if so)
  - `src/config.rs` — `MAESTRO_TUNER_MAX_CONCURRENT` (default 4)

  ## APPROACH
  1. Spawn through E's supervisor (MTRX-05) so every child is registered, has its exit observed, and
     is reaped on session close by the same code that reaps transcode children. Adopt E's
     `SIGCONT`-before-`SIGKILL` ordering unchanged — a supervisor that special-cases one child kind
     is a supervisor with two behaviours.
  2. **Cap default 4, matching `discover.json`'s advertised `TunerCount: 4`.** These two numbers must
     agree or the tuner lies to its client: Plex will happily open a fifth stream it was told it
     could have. Assert the relationship in a test and note it in both files. Over-cap ⇒ `503`
     (a tuner client understands "no tuner available"; a `429` is an HTTP idiom it does not act on).
  3. The linear cap is **separate from** and **additional to** MDLV-07's global
     `MAESTRO_MAX_CONCURRENT_SESSIONS`: a household saturating its tuners must not thereby block
     on-demand playback, and vice versa. Both are checked; the more restrictive wins.
  4. Startup orphan sweep (MDLV-07 step 3) already closes non-stopped sessions before the listener
     binds. Confirm linear sessions are included and that closing one kills no process (the process
     died with the old Maestro) — add a test asserting the sweep is safe when the child is already
     gone, since that is the *normal* case for a restart.
  5. Emit `maestro_tuner_sessions_active` and `maestro_tuner_sessions_rejected_total{reason="cap"}`.
     A cap hit routinely is a signal to raise `TunerCount` and the cap together, not to silently drop
     tunes.

  ## TEST PLAN
  - `cargo test`:
    - Opening at the cap → `503`; after one closes → succeeds
    - The linear cap and the global session cap are both enforced; the tighter one wins
    - `discover.json`'s `TunerCount` equals `MAESTRO_TUNER_MAX_CONCURRENT` (assert the config
      relationship, so a future change to one fails the test rather than the household)
    - The startup sweep closes a linear session whose child is already gone, without error
    - Session close kills the child exactly once and is idempotent on a second call
  - Process-leak check on a host with ffmpeg (`MAESTRO_TEST_FFMPEG=1`): open and abandon 10 tunes,
    assert the child count returns to zero within the reaper interval.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - Cap set to 0 — treated as unlimited and logged once, matching MDLV-07's convention rather than
    inventing a second meaning for zero
  - `TunerCount` and the cap diverging through config drift — the test above is the guard; make its
    failure message name both env vars
  - A child that ignores SIGTERM (a wedged ffmpeg on a stuck network mount) — E's escalation to
    SIGKILL after its grace period applies unchanged; this is exactly the case MTUN-11 kills for real

- **Acceptance criteria:**
  - [ ] Linear ffmpeg children are spawned and reaped by E's supervisor, with no second process path
  - [ ] The tuner cap defaults to 4 and is asserted equal to `discover.json`'s `TunerCount`
  - [ ] Linear and global session caps are independent and both enforced
  - [ ] The startup orphan sweep covers linear sessions and tolerates an already-dead child
  - [ ] Abandoned tunes leave no zombie processes
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTUN-08: The cutover switch — one config value, both paths live
- **Priority:** Critical
- **Labels:** muse, tuner, config, rollout
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** MTUN-05
- **Description:** Route the advertised per-channel stream URL through a single function gated on
  one config value, so the cutover is a config flip and the rollback is the same flip in reverse —
  with **no client reconfiguration and no re-pair** (§4a, §5).

  Muse's `/auto/vN` handler is **not touched** by this item. Both serving paths are simultaneously
  live and functional for the whole transition; only the *advertised* URL changes.

  ## FILES
  - `src/tuner/hdhr.rs` — `lineup_entries` calls a new `tuner_stream_url(&Config, &Channel)`
  - `src/tuner/mod.rs` — `tuner_stream_url` + `stream_base_url`
  - `src/config.rs` — `MUSE_TUNER_SERVING` (`muse`|`maestro`, default `muse`),
    `MUSE_TUNER_MAESTRO_BASE_URL`, and read access to `MAESTRO_TUNER_SIGNING_KEY` for minting
  - `README.md` — the cutover procedure and the rollback

  ## APPROACH
  1. ```rust
     pub fn tuner_stream_url(config: &Config, channel: &Channel) -> String {
         match config.tuner_serving {
             TunerServing::Muse => format!("{}/auto/v{}", base_url_from(config), channel.id),
             TunerServing::Maestro => {
                 let sig = media::tuner_sig::sign_channel(&key, channel.id);
                 format!("{}/tuner/v{}?sig={}", maestro_base, channel.id, sig)
             }
         }
     }
     ```
     One function, one call site. `/lineup.json`, `/muse.m3u`, and `/xmltv.xml` inherit it
     automatically — which is the property `lineup_entries`' existing doc comment already promises
     ("so all three stay in agreement") and which this item must not break.
  2. **Fail back, do not fail closed, on a misconfigured `maestro` mode.** If
     `MUSE_TUNER_SERVING=maestro` but `MUSE_TUNER_MAESTRO_BASE_URL` is unset or the signing key is
     missing, log a loud error **once per lineup fetch** and emit the `muse` URL. This is the one
     place in this spec where failing closed is wrong: a fail-closed lineup is a household with no
     television and no obvious cause, whereas falling back means live TV keeps working on the old,
     still-present path while the log names the misconfiguration. Assert this in a test so it is a
     property, not an accident.
  3. `discover.json`, `lineup_status.json`, `DeviceID`, and every XMLTV `channel_ref` are
     **byte-identical** across the flip. Add a test that renders both modes and diffs everything
     except the `URL` field, so a future change that perturbs device identity fails loudly (§4a).
  4. Document the procedure in the README: flip the config → restart/reload Muse → trigger a Plex
     channel scan (or wait for its periodic lineup refresh) → verify per MTUN-09 → rollback is the
     same three steps with the value reversed.

  ## TEST PLAN
  - `cargo test`:
    - `muse` mode produces exactly today's `{base}/auto/vN` URLs (golden-string test against the
      recorded pre-flight baseline)
    - `maestro` mode produces `{maestro_base}/tuner/vN?sig=…` with a signature that
      `verify_channel` accepts
    - `maestro` mode with an unset base URL or key falls back to `muse` URLs and logs
    - Everything except the `URL` field is identical between modes for `/lineup.json`, `/muse.m3u`
      and `/xmltv.xml` (structural diff test)
    - `discover.json` is byte-identical across modes
    - `/muse.m3u`'s `tvg-id` derivation still yields `muse-{id}` under the new URL shape — note that
      `m3u::render` currently derives the id by splitting on the literal `"/auto/v"`, so this **will
      break** unless the derivation is changed to use the channel id directly. Fix it by threading
      the id through rather than by parsing the URL; parsing a URL to recover data we already have is
      the defect the new shape exposes.
  - Verify no hardcoded IPs, hostnames, or ports in new/modified files.

  ## EDGE CASES
  - `MUSE_TUNER_SERVING` set to an unrecognised value — refuse at startup with a message naming the
    two valid values, rather than silently defaulting (a typo'd `maestro ` with a trailing space must
    not silently serve the old path while an operator believes the cutover happened)
  - A signing key present in Muse but not Maestro — every tune 403s; MTUN-09's checklist catches it
    before the old path is removed, which is the entire reason MTUN-10 is a separate item
  - Channel created while in `maestro` mode — signed on the next lineup fetch, no special handling
  - An operator flipping the config without a Plex channel scan — old URLs remain cached client-side
    and keep working (because MTUN-10 has not yet removed them); this is a feature of the sequencing

- **Acceptance criteria:**
  - [ ] `tuner_stream_url` is the single construction site of the per-channel stream URL
  - [ ] `MUSE_TUNER_SERVING=muse` reproduces today's URLs byte-for-byte
  - [ ] `discover.json` and device identity are byte-identical across both modes
  - [ ] `maestro` mode with missing config falls back to `muse` and logs, rather than emitting a
        broken lineup (asserted by test)
  - [ ] An unrecognised `MUSE_TUNER_SERVING` value refuses to start
  - [ ] `m3u::render`'s `tvg-id` derivation no longer depends on parsing `/auto/v` out of the URL
  - [ ] Muse's `/auto/vN` handler is unmodified by this item
  - [ ] README documents the cutover and the rollback
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTUN-09: Cutover verification and rollback runbook
- **Priority:** Critical
- **Labels:** muse, maestro, tuner, verification, human-action
- **Agent:** <operator>
- **Estimate:** 1h
- **Type:** human-action
- **Blocked by:** MTUN-08
- **Description:** The gate between "the new path exists" and "the old path may be deleted." This is
  a human-action item on purpose: the only meaningful verification is a real Plex client tuning a
  real channel on a real television, and no automated test in this repo can perform it. Breaking
  this silently breaks live TV for people who are not participating in the migration.

  Run every check. Record each result in the Plane item. **MTUN-10 does not start until every line
  below passes.** If any line fails, flip `MUSE_TUNER_SERVING` back to `muse`, trigger a channel
  scan, confirm recovery, and file the failure as a follow-up rather than pressing on.

  ## Steps
  1. **Before the flip**, capture the baseline: `GET /discover.json` and `GET /lineup.json` saved
     verbatim (these were also recorded at pre-flight — confirm they still match).
  2. Flip `MUSE_TUNER_SERVING=maestro`, restart Muse, trigger a Plex channel scan.
  3. **Device identity unchanged** — Plex shows the same tuner device, no re-pair prompt, DVR
     settings and channel mapping intact, scheduled recordings still listed. `discover.json`'s
     `DeviceID` is identical to the baseline.
  4. **Lineup completeness** — `/lineup.json` lists every channel it listed before, with the same
     `GuideNumber` and `GuideName`; only `URL` differs.
  5. **Every channel tunes** — tune each channel in turn from Plex. Video and audio play.
  6. **Join-mid-stream is correct** — tune a channel whose current program started several minutes
     ago and confirm playback begins mid-programme, not from the top, and that it matches what the
     EPG says is airing.
  7. **A program boundary is crossed cleanly** — leave a channel playing across the end of the
     current programme and confirm it continues into the next without a stall or a dropped stream.
  8. **The guide still correlates** — `/xmltv.xml` programme entries still map to the right channels
     in the client (this is the `tvg-id` path MTUN-08's test also covers, verified live).
  9. **Muse is untouched by a channel** — with a channel playing, confirm Muse's `/health` is green,
     the worker loop is running, and no ffmpeg process exists under the `muse` PID.
  10. **Rollback rehearsal, performed not assumed** — flip back to `muse`, trigger a channel scan,
      confirm a channel tunes on the old path, then flip forward again. A rollback that has never
      been executed is not a rollback.
  11. Leave the system in `maestro` mode and observe for **at least 72 hours** of normal household
      use before MTUN-10. A migration of an in-use feature earns a soak period; the failures worth
      catching here (a slow leak, a re-poll bug at a day boundary) do not appear in an hour.

- **Acceptance criteria:**
  - [ ] Every step above executed and its result recorded in the Plane item
  - [ ] `DeviceID` and DVR configuration confirmed unchanged; no re-pair occurred
  - [ ] Every channel tuned successfully, including join-mid-stream and a program boundary
  - [ ] No ffmpeg process observed under the `muse` process while a channel plays
  - [ ] Rollback executed successfully and then reverted forward
  - [ ] 72-hour soak completed with no tuner-related incident

---

### MTUN-10: Remove the in-process ffmpeg spawn from Muse
- **Priority:** High
- **Labels:** muse, streaming, cleanup, crash-isolation
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** MTUN-09
- **Description:** The item that actually closes epic §4b. Delete `stream_channel`,
  `build_stream_response`, `resolve_file_path` and `spawn_ffmpeg` from `src/streaming/mod.rs`, and
  unregister `/auto/v:channel_id`. After this merges, **the `muse` binary spawns no long-lived
  ffmpeg process and the epic's crash-isolation claim is true without an asterisk.**

  Only the *spawning* goes. `streaming::ffmpeg` stays exactly where it is: it is a pure argument
  builder already shared with `crate::matching` (still-frame extraction), and Maestro calls it
  directly as a same-crate module. Deleting it would be a rewrite, not a migration.

  ## FILES
  - `src/streaming/mod.rs` — remove the handler, generator, resolver, and spawner; the module becomes
    a re-export shell over `ffmpeg` with a doc comment recording the migration
  - `src/http/mod.rs` — remove the `/auto/v:channel_id` route from `tuner_routes()` and update its
    doc comment
  - `src/config.rs` — `MUSE_TUNER_SERVING` loses its `muse` arm; the value becomes advisory-only or
    is removed (see APPROACH step 4)
  - `README.md`, `specs/S130-maestro-epic.md` — record the closure

  ## APPROACH
  1. Remove the handler and everything reachable only from it. Keep `streaming::ffmpeg` intact.
  2. `src/streaming/mod.rs`'s module doc is rewritten to state, in two sentences, that linear-channel
     serving moved to the `maestro` binary in S130-L and why (crash isolation, epic §2/§4b), with a
     pointer to `src/maestro/tuner/`. A future reader finding an empty-looking module deserves the
     reason, not an archaeology exercise.
  3. Muse retains the on-now endpoint (MTUN-02), the scheduler, the grid, the EPG, and all discovery
     endpoints. Confirm by grep that `tuner_routes()` still registers `/discover.json`,
     `/lineup_status.json`, `/lineup.json`, `/muse.m3u`, `/xmltv.xml` — and **only** those.
  4. `MUSE_TUNER_SERVING`: simplest correct move is to **keep the config key and make `muse` a
     startup error** naming this spec, for one release. An operator who rolls back Muse's binary but
     not its config should get a clear message rather than a silently-404ing `/auto/vN`. Remove the
     key entirely in a later cleanup.
  5. Amend `specs/S130-maestro-epic.md` §4b: mark the inconsistency **closed**, name this spec, and
     strike §1's asterisk. Amend §2's ownership table so "linear-channel serving" appears explicitly
     under Maestro. The epic named this violation honestly; it should record the resolution equally
     honestly.
  6. Update the `/streaming` live-DB test: the `resolve_file_path` assertion moves to Maestro's
     resolver tests (MTUN-05) or is deleted if MDLV-02's tests already cover the equivalent — do not
     leave a test asserting the behaviour of deleted code.

  ## TEST PLAN
  - `cargo test --workspace` — green after removal; no test references the deleted functions.
  - `grep -rn "spawn_ffmpeg\|build_stream_response\|stream_channel" src/` returns nothing outside
    `src/maestro/`.
  - **The isolation assertion, as a test:** `grep -rn "Command::new" src/ --include=*.rs` shows no
    long-lived spawn outside `src/maestro/` and `src/foundry/` (Foundry's transcode fabric is a
    separate, deliberate case); the still-frame extraction in `crate::matching` is short-lived and
    documented as such. Encode this as a test with a message explaining what it protects, so a future
    PR reintroducing a spawn into a Muse worker fails with a reason rather than a diff.
  - `/auto/vN` on a running `muse` returns `404`.
  - Verify no hardcoded infrastructure values.

  ## EDGE CASES
  - A Plex client with an old cached lineup still requesting `/auto/vN` — returns `404`; the next
    channel scan re-reads `/lineup.json`. This is the reason MTUN-09's soak comes first: after this
    item, rollback is no longer a config flip but a binary rollback
  - `crate::matching` depending on `streaming::ffmpeg` — it does (`build_still_args`); do not break it
  - Another in-flight spec adding a Muse-side use of `streaming::` — reconcile at merge; the grep test
    is what makes such a conflict visible

- **Acceptance criteria:**
  - [ ] `muse` spawns no long-lived ffmpeg process; the grep-based isolation test enforces it
  - [ ] `/auto/v{channel_id}` is unregistered and returns `404`
  - [ ] `streaming::ffmpeg` remains intact and `crate::matching` still compiles against it
  - [ ] Muse still serves all five discovery/EPG endpoints and only those
  - [ ] `src/streaming/mod.rs`'s doc comment records the migration and points at `src/maestro/tuner/`
  - [ ] `specs/S130-maestro-epic.md` §4b is amended to record the inconsistency as closed
  - [ ] `MUSE_TUNER_SERVING=muse` fails at startup with a message naming this spec
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTUN-11: The payoff, tested — SIGKILL a channel's ffmpeg, assert Muse is unaffected
- **Priority:** Critical
- **Labels:** maestro, testing, crash-isolation, integration
- **Agent:** claude
- **Estimate:** 3h
- **Blocked by:** MTUN-07, MTUN-10
- **Description:** The chaos test that turns this spec's claim into a property. Epic §10b requires
  it; MTRX-16 proves it for a transcode session; this proves it for the workload that actually
  motivated the epic's §4b admission.

  The assertion is deliberately one that should be *trivially* true once the migration is done — and
  that is the point. A test that passes trivially today is what tells you the day someone
  "simplifies" the two binaries back into one, or reintroduces a spawn into a Muse worker, and
  quietly deletes the isolation this whole epic was built to buy.

  ## FILES
  - `tests/maestro_tuner_isolation.rs` — new; extends MTRX-16's harness conventions
  - `README.md` — how to run it and on which hosts

  ## APPROACH
  1. **Gating**, identical to MTRX-16: `MAESTRO_TEST_FFMPEG=1` **and** a probe that the configured
     ffmpeg binary exists, **and** `MUSE_TEST_DATABASE_URL`. Skips with an explanatory `eprintln!`
     when unset, keeping `cargo test` green on the dev box (which has no ffmpeg — verified
     2026-07-31). Run it on the Muse deploy host or <host> through the compiler tool.
  2. **Fixture:** a synthetic source generated by ffmpeg itself (`testsrc` + `sine`, ~90s, known
     duration) into a temp dir, and a seeded linear channel with a program grid pointing at it. No
     library file, no QNAP dependency, no PII, deterministic.
  3. **The scenario:**
     a. Start both processes (or, in-test, the `maestro` router plus a live `muse` health endpoint —
        the test must exercise a real process boundary; an in-process harness proves nothing here and
        the item should say so).
     b. Tune `/tuner/v{id}` and read bytes until at least one full read has succeeded.
     c. Locate the ffmpeg child by PID from the supervisor and **`SIGKILL` it directly** — not
        through the session's own teardown. The point is an *unhandled* death.
  4. **Assert, in order:**
     - The `maestro` process is still alive and serving: `GET /health` returns 200 and a **second
       tune on another channel succeeds immediately**.
     - The killed session reaches a defined terminal state (closed with a failure `stop_reason`);
       the client sees a terminated stream, never a hang.
     - **No zombie remains** and the supervisor's child count returns to its pre-tune value.
     - **Muse never notices** — this is the payoff assertion, stated explicitly: `muse`'s `/health`
       is green throughout; its worker loop is still running; the scheduler still tops off windows;
       `/lineup.json`, `/xmltv.xml`, and the MTUN-02 on-now endpoint all still answer; and no Muse
       log line records an error attributable to the kill.
     - **No ffmpeg process exists under the `muse` PID at any point in the test** — the structural
       assertion, and the one that fails if MTUN-10 is ever reverted.
  5. Repeat the kill during a **program transition** (between two children) — the awkward corner,
     where a kill lands while the generator is between spawns.
  6. Repeat with `SIGKILL` on a **`SIGSTOP`ped** child if E's throttle applies to linear children,
     mirroring MTRX-16 step 4's final case; if throttle does not apply to `-c copy` pipes, record
     that as a one-line finding rather than writing a vacuous test.
  7. **Deliberate-break check**, run once by hand during development and recorded in the item: revert
     to the pre-MTUN-10 in-process handler, run the test, and confirm the "Muse never notices"
     assertion **fails**. A chaos test that has never been seen to fail is not known to test anything.

  ## TEST PLAN
  - The harness *is* the test plan; what is gated here is that it runs and passes on a host with
    ffmpeg and skips cleanly without one.
  - `cargo test` on the dev box → skips with an explanatory message; suite green.
  - `MAESTRO_TEST_FFMPEG=1 MUSE_TEST_DATABASE_URL=… cargo test maestro_tuner_isolation` on the Muse
    host or <host> → all assertions pass.
  - Verify no hardcoded IPs, hostnames, or library paths in the harness (the fixture is synthetic).

  ## EDGE CASES
  - The test host being slow enough that the first read has not completed before the kill — poll for
    a successful read with a bounded timeout rather than sleeping a fixed interval
  - A leftover child from a previous failed run — the harness records the pre-tune child count and
    asserts against the delta, never against an absolute zero
  - Killing during the very first spawn, before the response is committed — a legitimate variant;
    assert the clean `503`/`501` path rather than a truncated body
  - `muse` not running in the test environment — the harness must **fail loudly**, not skip: the
    entire assertion is about Muse being unaffected, so an absent Muse is a broken test, not an
    unconfigured one

- **Acceptance criteria:**
  - [ ] SIGKILLing a channel's ffmpeg leaves `maestro` alive, serving, and able to accept a new tune
  - [ ] Muse's health, workers, scheduler, and every tuner/EPG endpoint are verifiably unaffected
  - [ ] The killed session reaches a defined terminal state; the client never hangs
  - [ ] No zombie process remains and the child count returns to baseline
  - [ ] The test asserts no ffmpeg process exists under the `muse` PID at any point
  - [ ] The kill is exercised mid-program and at a program transition
  - [ ] The deliberate-break check was performed and its result recorded in the Plane item
  - [ ] The harness skips cleanly without ffmpeg and fails loudly without Muse
  - [ ] README documents how and where to run it
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

## 9. Behaviour contract additions

Append to Muse's behaviour spec (`specs/behavior/` or the repo's existing behaviour-spec location).

### API: GET /channels/{id}/onnow
- input: path `channel_id`, optional `limit`; bearer `MAESTRO_MUSE_TOKEN`
- output: `{ channel_id, resolved_at, current: OnNowEntry, upcoming: [OnNowEntry] }`
- verify:
  - `api_call("GET", "${MUSE_API_URL}/channels/1/onnow", null, 200)`
  - response contains no field whose value is an absolute filesystem path
  - `current.seek_ms >= 0` and `<= current.duration_ms`
- error_cases:
  - unauthenticated → 401
  - unknown channel → 404
  - non-linear channel → 400
  - no program scheduled → 503

### API: GET /tuner/v{channel_id} (Maestro)
- input: path `channel_id`, query `sig`
- output: `video/mp2t` chunked stream
- verify:
  - `api_call("GET", "${MAESTRO_URL}/tuner/v1?sig=${VALID_SIG}", null, 200)`
  - response header `Content-Type` == `video/mp2t`
  - response header `Accept-Ranges` == `none`
- error_cases:
  - missing/invalid signature → 403
  - tuner disabled → 404
  - concurrency cap reached → 503
  - ffmpeg binary absent → 501
  - no program scheduled → 503

### State: linear channel streaming
- entry: a signed tune request passes all gates and the first program spawns
- exit: client disconnect, playlist exhaustion, reaper timeout, or process death
- verify:
  - `port_listening("${MAESTRO_HOST}", "${MAESTRO_PORT}")`
  - `command_output_contains("pgrep -P ${MAESTRO_PID} ffmpeg", "")` is non-empty while streaming
  - `command_output_contains("pgrep -P ${MUSE_PID} ffmpeg", "")` is **empty at all times**
  - `api_health("${MUSE_API_URL}/health") == true` while a channel is streaming

---

## 10. Sequencing summary

```
MTUN-01 (move onnow)
   └─ MTUN-02 (Muse on-now endpoint)
         ├─ MTUN-03 (session kind)
         └─ MTUN-04 (signing, independent — may start in parallel)
               └─ MTUN-05 (Maestro tuner handler)
                     ├─ MTUN-06 (continuation)
                     ├─ MTUN-07 (supervision + caps)
                     └─ MTUN-08 (cutover switch)
                           └─ MTUN-09 (human verification + 72h soak)   ← HARD GATE
                                 └─ MTUN-10 (remove Muse's spawn)
                                       └─ MTUN-11 (chaos test)
```

MTUN-01 through MTUN-08 are additive and reversible: at every point the household's live TV is
served by the existing, untouched Muse path unless an operator has flipped one config value, and
flipping it back is a complete rollback. **MTUN-09 is the only irreversible boundary** — after
MTUN-10, rollback is a binary rollback rather than a config flip, which is precisely why the soak
period sits in front of it.

MTUN-11 lands after MTUN-10 deliberately: the assertion it makes ("no ffmpeg under the `muse` PID")
is only meaningful once the old spawn is gone, and writing it earlier would mean writing a test
designed to be revised.

---

## 11. Risks

1. **The 72-hour soak gets skipped under momentum.** The most likely failure of this spec is
   procedural, not technical: MTUN-08 works, everything looks fine in an hour, and MTUN-10 merges
   the same day. The failures the soak catches — a slow child leak, a re-poll bug at a day boundary,
   a signature key that drifts on the next secret re-materialisation — are exactly the ones that do
   not appear in an hour. The gate is written as a separate item with a human agent for this reason.
2. **`m3u::render`'s URL-parsing `tvg-id` derivation.** It recovers the channel id by splitting the
   stream URL on the literal `"/auto/v"`. The new URL shape breaks it, and the failure mode is
   *silent*: it falls back to `guide_number`, EPG correlation quietly degrades, and nothing errors.
   MTUN-08 fixes it by threading the id through; the risk is that a reviewer treats it as incidental
   cleanup rather than a correctness fix.
3. **Muse becoming a hot-path network dependency.** §7.2. Bounded by a short timeout and MTUN-06's
   look-ahead, but a Muse restart during a tune now drops a stream that a function call would have
   survived. Acceptable — Muse restarting already ended the stream when Muse served it — but it is a
   new *shape* of failure and will present differently in logs.
4. **Cap/`TunerCount` drift.** Two numbers that must agree, in two files. MTUN-07's assertion test is
   the only thing keeping them honest.
5. **Concurrent spec churn.** Spec D was being actively revised on 2026-08-01 while this spec was
   written. Re-read D before implementing MTUN-03 onward and reconcile any renamed type; this spec
   references D's contracts by intent, and an intent survives a rename where a symbol does not.
