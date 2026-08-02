# S130-A — Promote the probe to the shared media core, and persist it for the library

> ## ⚠ THIS SPEC IS 91 COMMITS STALE. THE PLANE ITEMS SUPERSEDE IT.
>
> The `Module version` below declares baseline `e8499aa`. That predates the entire **FOUNDRY-03..30**
> wave, which added `src/foundry/hdr.rs` (1,233 lines), `directplay.rs`, `ladder.rs`, `rendition.rs`,
> `validate.rs`, `reaper.rs` and `marks.rs`. Several items below therefore describe as *missing*
> things that are **already built, reviewed and shipped**.
>
> This was not caught by review. It was caught when MPRB-02's premise — "`run_ffprobe` has no timeout,
> no output cap, and blocks the calling thread" — turned out to be false: that file is **1,698 lines**,
> not the 948 cited, and already carries all three defences. A verification pass over the remaining
> items then found the same rot in **five of eight**.
>
> **Build from the Plane items, not from this document.** Each was fact-checked against the tree and
> records what is actually absent:
>
> | Item | Plane | Status after verification |
> |---|---|---|
> | MPRB-01 | #137 | **MERGED** (`d84cac8`) |
> | MPRB-02 | #139 | RESCOPED — timeout/cap/reap already shipped |
> | MPRB-03 | #140 | **PARTIAL + STRUCTURAL CORRECTION — see the warning on that item before writing code** |
> | MPRB-04 | #141 | PARTIAL — the survey walker exists and has already run over 16,221 files; ffprobe *is* installed |
> | MPRB-05 | #142 | ACCURATE, but migration `0109` is taken — use **0113** |
> | MPRB-06 | #143 | ACCURATE (`scan.rs:507`, not ~500) |
> | MPRB-07 | #144 | ACCURATE — nothing exists yet |
> | MPRB-08 | #145 | ACCURATE — axum is still **0.7**, so `:id` is right, not `{id}` |
> | MPRB-09 | #146 | PARTIAL — a decision-level census already ran; only the *distributional* one is missing |
> | MPRB-10 | #147 | PARTIAL — "install ffprobe" is dead; it is installed and the sweep has run |
>
> **The most dangerous item is MPRB-03.** As written it instructs creating a second HDR/Dolby-Vision
> classifier and a third image-subtitle codec list. `foundry::directplay::may_delete_original`
> consumes the *existing* classifier to refuse deleting a source whose HDR/DV the output does not
> preserve — so a second, drifting classifier can authorize an **irreversible** delete of a Dolby
> Vision master. This repo has already paid for this exact shape once: `predicted_deletion_refusals`
> restated the deletion gate instead of calling it and was wrong **by 20x**. One rule, one home.
>
> Two bitmap-subtitle codec lists *already* disagree on `main` (`subtitles/discover.rs:40` vs
> `foundry/directplay.rs:154`, over the aliases `pgssub`/`dvdsub`/`dvbsub`) — tracked as **#149**.
>
> **Standing lesson: a spec is a plan, not ground truth. Verify every factual claim against the tree
> before building from it.**

plane_project: MUSE
module: Muse
prefix: MPRB
spec_id: S130-A-maestro-probe

## Metadata
- **Author:** Moose (operator) / Claude (scoping)
- **Session:** S130
- **Date:** 2026-08-01
- **Module version:** Muse `main` @ `e8499aa`
- **Estimated total:** ~56h autonomous agent work across 9 code items + 1 operator item
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7 — this ships **inside Muse**, adds no new egress,
  no new credential, and no new UI surface beyond two read-only endpoints behind the
  existing bearer.
- **Parent epic:** `specs/S130-maestro-epic.md` (§4 child spec **A**; blocks child spec **C**).
- **Context:** ⚠️ **This spec was rewritten 2026-08-01 after fast-forwarding the local
  checkout 64 commits to `origin/main` (`e8499aa`).** Its first draft was written against a
  stale tree and scoped "build an ffprobe layer" — which would have rebuilt **948 lines of
  shipped, well-designed code**. `src/foundry/probe.rs` already contains the ffprobe
  invocation *and* a pure `parse_probe_json` parser, with `MediaProbe`, `VideoStream`,
  `AudioStream`, `SubtitleStream`, `AttachmentStream` and a structured `ProbeError`. Per
  epic §2b the corrected scope is **promote, then extend**.

---

## What already exists, verified in-tree at `e8499aa`

Read `src/foundry/probe.rs` before writing a line of this spec's code. It is good, and its
module doc reaches the same conclusions this epic did, independently:

| Already built | Where |
|---|---|
| `build_ffprobe_args` — pure argv builder, **already includes `-show_chapters`** | `probe.rs:42` |
| `run_ffprobe(ffprobe_bin, &ResolvedPath) -> Result<MediaProbe, ProbeError>` | `probe.rs:265` |
| `parse_probe_json(&str) -> Result<MediaProbe, ProbeError>` — pure, total, never panics | `probe.rs:432` |
| `MediaProbe`: container, duration, format bitrate, size, video/audio/subtitle/attachment streams, data + unindexed + other counts, chapter count, title | `probe.rs:57` |
| Cover art (`attached_pic`) filtered out of `video`, counted not vanished | `probe.rs:479` |
| `"N/A"` / string-vs-number numerics / negative / NaN all → `None`, never `0` | `probe.rs:396` |
| Uppercase `LANGUAGE` tag read; `und` → `None` | `probe.rs:470` |
| `ProbeError::{ToolMissing, Spawn, ExitFailure, MalformedOutput, NoStreams}` + stderr truncation | `probe.rs:191` |
| Three-state `ToolState::{Present, Missing, Unusable}`, `Capabilities::can_probe()` | `capability.rs:35` |
| `PathGuard`/`ResolvedPath` — a probe cannot be handed an unvalidated path | `paths.rs:41` |
| ~20 parser tests including one captured real ffprobe document (`H264_MKV`) | `probe.rs:552` |

**Two corrections to the brief that produced this spec, both in the code's favour:**
1. `-show_chapters` does **not** need adding — it has been in the argv since MUSEF-02, with
   a test asserting it (`probe.rs:859`). Nothing to do.
2. The honesty rule already holds throughout: an unobserved fact is `None`, never a benign
   default, and a failed probe is a `ProbeError`, never an empty `MediaProbe`. Every item
   below inherits that rule; **do not weaken it to make a new field convenient.**

Consumers to keep green: `src/foundry/{mod,plan,forge}.rs` (and `survey.rs` transitively).

## What is genuinely missing — this spec's actual subject

1. **Nothing persists a probe.** Foundry probes on demand at curation time and throws the
   result away. `media_files.media_info` is still `{"container": "<file extension>"}`,
   written by `src/library/scan.rs:~500`, exactly as `migrations/0009_media_files.sql:28`
   promised codec/resolution/HDR and never delivered. **This gap is the spec.**
2. **The invocation is unbounded and blocking.** `run_ffprobe` calls
   `std::process::Command::output()` — no timeout, no output cap, and a synchronous block
   inside what will become an async worker. Survivable for an operator-triggered Foundry
   probe of one file; not survivable for a worker sweeping a **network-mounted read-only
   QNAP share**, where a stalled mount blocks forever.
3. **No HDR classification.** The colour fields are not even parsed, so HDR10/HLG/DV cannot
   be distinguished — and epic §8.3 depends on classifying HDR precisely so it can decline
   to tone-map it.
4. **The playback-decision fields are absent**: profile, level, bit depth, frame rate,
   sample rate, channel layout, image-vs-text subtitles. Foundry's curation policy did not
   need them; spec C's `DeviceProfile` matching cannot work without them.
5. **No census.** Nobody knows what is in the library, so epic §6's direct-play fraction —
   the number that decides whether spec E is the centrepiece or an edge case — does not
   exist.

---

## Placement and naming (epic §2b)

**`src/media/` is the shared core**, consumed by *both* Maestro (play time: "can this device
play this file right now?") and Foundry (curation time: "should this file be permanently
re-encoded?"). Spec C lands `DeviceProfile` and its `plan()` in `src/media/` beside
`plan_transcode`, as visible siblings rather than rivals.

**Naming decision: the type stays `MediaProbe`.** The epic says "`MediaProbe` becomes the
epic's `MediaInfo` — one type, not two", and one type is exactly what this delivers — but
renaming it costs a mechanical diff across `plan.rs` (1,435 lines), `forge.rs` (2,766) and
`policy.rs` (483) purely to satisfy a word choice, and `MediaProbe` is the more accurate
name: it is *the result of probing a file*, not a general bag of media metadata. What the
epic calls `MediaInfo` is therefore two concrete things here:

- **`MediaProbe`** — the probe result type. One of them, promoted, extended in place.
- **`MediaInfoDoc`** — the *persisted envelope* written to `media_files.media_info`:
  `{schema_version, probe: MediaProbe, flat compatibility keys}`. A storage format is a
  genuinely different concern from an in-memory value — it is versioned, it must survive
  rolling deploys, and it must stay backward-compatible with rows written in 2026. Giving
  the two one name would force every future storage-format change to look like a change to
  the probe.

`src/media/mod.rs` documents this mapping in its module doc for readers arriving from the
epic. **No other renames.**

---

## The untrusted-input posture

**ffprobe output is untrusted input, and so is the file it read.** The library holds decades
of files nobody audited. `parse_probe_json` is already total and non-panicking — that half
is done and must stay done. The half that is missing is at the *process* boundary:

1. **Bounded wall clock.** Every invocation runs under `MUSE_PROBE_TIMEOUT_SECS` (default
   30), with the child killed and reaped on expiry. A stalled NFS/QNAP path blocks in the
   kernel indefinitely, and a timeout that leaks a zombie is not a timeout.
2. **Bounded output.** stdout is read into a capped buffer (`MUSE_PROBE_MAX_OUTPUT_BYTES`,
   default 8 MiB). `Command::output()` is unbounded by construction; a file with 100k
   chapters must not OOM a 2–4 GB container.
3. **Bounded structure.** Cap streams at `MAX_STREAMS` (512) and persisted tag strings at
   `MAX_TAG_LEN` (512). Titles and language tags are attacker-influenced metadata that land
   in Postgres and then in a browser.
4. **No new panicking paths.** No `unwrap`, `expect`, `[]` indexing, division by a parsed
   value, or `as` narrowing on anything derived from ffprobe output. `r_frame_rate` of
   `"0/0"` is real and common — it is `None`, not a divide-by-zero.

A CI-visible negative test for each is required, not optional.

---

## The stored probe-state taxonomy (used by items 05, 07, 08, 09)

"Failed" is not one thing, and collapsing it into one is what makes a backfill unauditable.

| State | Means | Response |
|---|---|---|
| `ok` | Probed, parsed, nothing suspicious | Done |
| `unreadable` | **The file could not be read at all** — mount absent, ENOENT, EIO, timeout, spawn failure | **Retryable.** Almost always infrastructure, not the file |
| `probe_failed` | **ffprobe ran on a readable file and could not make sense of it** — non-zero exit, malformed output, no streams | **Terminal on the first attempt.** A retry cannot change the answer; it is a library-health finding |
| `suspicious` | **Parsed fine, but does not describe playable media** — zero/absent duration, no video *and* no audio, zero dimensions, size/duration/bitrate inconsistent by an order of magnitude | **Neither retried nor ignored — needs human eyes** |

`suspicious` earns its keep. A retry cannot fix it (the parse succeeded), and treating it as
`ok` feeds a zero-duration or video-less probe into spec C's `plan()`, which then makes a
confident, wrong playback decision. It is also a real library-health signal: a truncated
download and a failed remux both land there.

The existing `ProbeError` variants map onto the first three: `ToolMissing`/`Spawn` and the
new `Timeout` → `unreadable`; `ExitFailure`/`MalformedOutput`/`NoStreams` → `probe_failed`.
`suspicious` is derived from a *successful* parse and must never be inferred from an error.

---

## Pre-flight

- **`git fetch` and confirm you are on current `origin/main` before surveying anything.**
  This spec's first draft was written against a 64-commit-stale checkout and scoped work
  that was already shipped. That is the cheapest possible lesson to reuse.
- Repository: `Muse` on Gitea (`moosenet/Muse`), one isolated worktree per item off fresh
  `origin/main`
- Build/test host: **not the dev box** — `compiler_build(module="muse", ref=<branch>, mode=test)`
- Register the prefix `MPRB`: `plane_prefix_check` → `plane_prefix_register` →
  `plane_prefix_promote`
- **Read `src/foundry/probe.rs`, `capability.rs` and `paths.rs` in full.** MPRB-01 moves
  them; an item that reimplements any of them is rejected on sight.
- Baseline: `cargo test` green on `main`; **record the test count**. Every item's criteria
  include "all existing tests still pass", and MPRB-01's whole point is that Foundry's ~20
  probe tests plus the `plan.rs`/`forge.rs` suites stay green through a pure move.
- `ffprobe` is **absent on the Muse deploy host and on the dev box** (verified 2026-07-31, and the reason
  the pure/impure split exists). The whole test suite must keep passing without it; the live
  backfill needs it installed on the Muse host — an ops prerequisite of MPRB-10, not of any
  code item.
- Confirm `MUSE_LIBRARY_ROOT` is present and **read-only** on the Muse host. Nothing here
  writes to it; a writable mount means the deployment posture is wrong — report it.
- Record the `media_files` row count and how many have a non-null `media_info`. This sizes
  MPRB-07 and is the denominator for MPRB-09.
- DB migrations are **not** auto-applied (skill v4.6): MPRB-05's migration goes through the
  `pg_ddl` operator door, sequenced with/before the OCI deploy.

---

## Item map

| Item | Delivers | Blocked by |
|---|---|---|
| MPRB-01 | **Promote** `probe`/`capability`/`paths` to `src/media/`, Foundry green | — |
| MPRB-02 | Harden the invocation: timeout, output cap, async-safe, retryable split | MPRB-01 |
| MPRB-03 | Extend the stream model + HDR classification | MPRB-01 |
| MPRB-04 | Golden fixture corpus, collected from the real library | MPRB-03 |
| MPRB-05 | `MediaInfoDoc` envelope, migration, accessor, state taxonomy | MPRB-03 |
| MPRB-06 | Scan integration — new files are probed on arrival | MPRB-04, MPRB-05 |
| MPRB-07 | Resumable, rate-limited backfill worker + metrics | MPRB-05, MPRB-06 |
| MPRB-08 | `/probe/:id/why` debug endpoint | MPRB-05 |
| MPRB-09 | Coverage report + written artifact | MPRB-05 |
| MPRB-10 | Operator: run the live backfill, publish the artifact | MPRB-07, MPRB-09 |

**MPRB-01 is a strict prerequisite of everything and must merge alone**, before any item
edits probe behaviour — a move and a behaviour change in one diff is a review nobody can do.

---

### MPRB-01: Promote `probe`, `capability` and `paths` to the shared `src/media/` core
- **Priority:** Critical
- **Labels:** muse, media, refactor
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Move Foundry's probe layer into the shared core **essentially as-is**,
  with every existing test green. A pure move: no behaviour change, no new field, no renamed
  type. Behaviour changes start at MPRB-02, in their own reviewable diffs.

  The move is required because these modules are Foundry-internal today and Foundry is
  **default-deny and inert** unless an operator sets `MUSE_FOUNDRY_ALLOWED_ROOTS` — so on a
  stock deployment the library would be unprobeable. Nothing in them is curation-specific.

  ## FILES
  - `src/media/mod.rs` — new; module doc stating the shared-core contract and the
    `MediaProbe`/`MediaInfoDoc` naming decision
  - `src/media/probe.rs` ← moved from `src/foundry/probe.rs`
  - `src/media/capability.rs` ← moved from `src/foundry/capability.rs`
  - `src/media/paths.rs` ← moved from `src/foundry/paths.rs`
  - `src/foundry/mod.rs` — drop the moved `pub mod`s, add `pub use crate::media::{...}`
    re-exports
  - `src/foundry/{plan,forge,survey,policy,config}.rs` — import-path updates only
  - `src/main.rs` — register `mod media;`

  ## APPROACH
  1. `git mv` the three files. **The diff for each must be import lines and nothing else** —
     verify with `git diff -M --stat` showing rename detection, and re-read the body diff to
     confirm it is free of logic changes. If a hunk touches a function body, split it into
     MPRB-02 or MPRB-03 where it can be reviewed as a change.
  2. `src/foundry/mod.rs` keeps `pub use crate::media::probe::{...}` / `capability::{...}` /
     `paths::{...}`, so Foundry's own modules and any external caller compile unchanged. The
     re-exports are documented as a permanent compatibility surface, not a deprecation —
     Foundry legitimately consumes the shared core forever.
  3. `paths.rs` moves too, and that is deliberate rather than incidental: `run_ffprobe` takes
     a `ResolvedPath`, so "I forgot to validate this path" is a compile error rather than a
     review catch. That property is worth more to a worker sweeping the whole library than
     it ever was to Foundry, so it must survive the move rather than being loosened to
     `&Path` for convenience. Muse builds its own **read-only** `PathGuard` rooted at
     `MUSE_LIBRARY_ROOT` (`mutation_enabled = false`) — step 4 — a second, independent guard
     that shares no configuration with Foundry's.
  4. Add `MediaCore::from_config(&Config)` in `src/media/mod.rs`: resolves the ffprobe binary
     (`MUSE_PROBE_FFPROBE_BIN`, falling back to the existing `MUSE_FOUNDRY_FFPROBE_BIN`, then
     `"ffprobe"` — documented precedence, so an operator who already configured Foundry does
     not configure it twice), builds the read-only library `PathGuard`, and runs
     `capability::detect` once at startup, exposing `can_probe()` so later consumers degrade
     on a host without ffprobe instead of failing per-file. All values are non-secret
     behavioural config read through `src/config.rs`, the crate's single env door — no
     `std::env::var` in `src/media/`.
  5. Update `docs/` and `README.md` references to the old paths.

  ## TEST PLAN
  - `cargo test` — **every existing Foundry test passes unmodified**, at the recorded
    baseline count; a test that needed editing means the move was not pure
  - `git diff -M` shows renames with import-only body changes
  - `MediaCore::from_config` on a host with no ffprobe reports `can_probe() == false` and
    does not error at startup
  - The library `PathGuard` is read-only: `resolve_for_mutation` on a library path is refused
    (negative test)
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - `MUSE_LIBRARY_ROOT` unset — the guard is inert, `can_probe()` may still be true, and
    every probe consumer degrades to today's behaviour. Never a startup failure
  - Foundry configured *and* a library root configured — two independent guards, no shared
    state; each path resolves through its own roots
  - An external caller still importing `crate::foundry::probe::...` — compiles via the shim
  - A merge conflict with concurrent Foundry work — this item is small and should merge first
    for exactly that reason

- **Acceptance criteria:**
  - [ ] `probe`, `capability` and `paths` live in `src/media/` and Foundry consumes them
        through re-export shims
  - [ ] Every pre-existing test passes **unmodified** at the recorded baseline count, and the
        rename diffs contain no logic changes (regression check)
  - [ ] No type is renamed and no field is added in this item
  - [ ] `MediaCore::from_config` resolves the ffprobe binary with documented precedence and
        builds a **read-only** library guard that refuses mutation (negative test)
  - [ ] A host without ffprobe starts cleanly with `can_probe() == false`
  - [ ] No hardcoded infrastructure values in new/modified code

---

### MPRB-02: Harden the invocation — timeout, output cap, async-safety, retryability
- **Priority:** Critical
- **Labels:** muse, media, probe, reliability
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MPRB-01
- **Description:** `run_ffprobe` is fine for an operator probing one file and unsafe for a
  worker sweeping a network mount: `std::process::Command::output()` has **no timeout, no
  output cap, and blocks the calling thread**. On a stalled QNAP mount it blocks forever,
  inside what MPRB-07 will make an async worker. This item fixes the process boundary and
  changes nothing about parsing.

  ## FILES
  - `src/media/probe.rs` — `run_ffprobe`, `ProbeError`, the new async entry point
  - `src/config.rs` — `probe_timeout_secs`, `probe_max_output_bytes`
  - `README.md`, `.env.example` — the new `MUSE_PROBE_*` vars

  ## APPROACH
  1. Add `ProbeError::Timeout { secs }` and `ProbeError::OutputTooLarge { cap }`. Keep every
     existing variant and its `Display` text — `ToolMissing`'s message is already the right
     operator diagnostic and `capability.rs` depends on the distinction.
  2. Add `pub async fn run_ffprobe_async(bin, &ResolvedPath, limits) -> Result<MediaProbe,
     ProbeError>` using `tokio::process::Command` with `kill_on_drop(true)`,
     `stdin(Stdio::null())`, piped stdout/stderr:
     - wrap in `tokio::time::timeout`; on expiry `start_kill()` **then `wait()` to reap**,
       and return `Timeout`. A test asserts no zombie remains — a kill without a reap is how
       a long-running worker accumulates defunct children until the container's pid limit
       stops it doing anything at all.
     - read stdout incrementally with a capacity check per iteration rather than `output()`;
       over the cap ⇒ kill, reap, `OutputTooLarge`.
  3. Keep the synchronous `run_ffprobe` as a thin wrapper so Foundry's callers are untouched,
     but give it the same timeout and cap. A blocking call from async code stays wrong, so
     document that async callers use `run_ffprobe_async`, and have MPRB-06/07 do so.
  4. Add `ProbeError::is_retryable()` and `ProbeError::state()` (the taxonomy above):
     `ToolMissing`/`Spawn`/`Timeout` → retryable / `unreadable`;
     `ExitFailure`/`MalformedOutput`/`NoStreams` → terminal / `probe_failed`. One `match`,
     **no wildcard arm**, so a future variant is a compile error rather than a silent default
     into the wrong bucket.
  5. **Argv injection guard.** `build_ffprobe_args` already passes the path as its own argv
     element, with a test proving it is never shell-interpolated. Add a `--` terminator
     before the path and reject a path whose first byte is `-` with `ProbeError::Spawn`. A
     file literally named `-loglevel` exists in exactly the kind of library this scans, and
     the `ResolvedPath` guard checks *location*, not *shape*.
  6. `MUSE_PROBE_TIMEOUT_SECS` (default 30) and `MUSE_PROBE_MAX_OUTPUT_BYTES` (default
     8388608), read through `config.rs`.

  ## TEST PLAN
  - `cargo test` — stub `ffprobe` scripts written to a temp dir, so the suite still passes on
    a host with no real ffprobe
  - A stub that sleeps past the timeout yields `Timeout` within ~2x, and the child is reaped
    (no defunct process for the recorded pid) — negative test
  - A stub emitting more than the cap yields `OutputTooLarge`, not an OOM
  - A stub exiting non-zero still yields `ExitFailure` with truncated stderr (unchanged)
  - `is_retryable()`/`state()` map every variant, asserted exhaustively
  - A path beginning with `-` is refused before spawn
  - Every pre-existing probe test still passes unmodified
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Stalled NFS/QNAP mount — the timeout is the only defence; do **not** add a
    `std::fs::metadata` pre-check, which would itself hang
  - Child killed by a signal (no exit code) — the existing `ExitFailure { code: None }` path
  - ffprobe writes to stderr and exits 0 — `-v quiet` already makes stdout JSON-only; keep
    treating a successful exit's stdout as authoritative
  - Non-UTF-8 filename — the path stays an `OsStr` through to the argv
  - Timeout shorter than a legitimate probe of a huge remote file — configurable, and
    MPRB-07's per-file duration histogram is how an operator discovers they set it too low

- **Acceptance criteria:**
  - [ ] Every invocation is bounded by a configurable wall clock and output cap; expiry kills
        **and reaps** the child (negative test asserts no zombie)
  - [ ] An async entry point exists and no probe call blocks a tokio worker thread
  - [ ] `is_retryable()`/`state()` map every `ProbeError` variant with no wildcard arm
  - [ ] A path beginning with `-` is refused before spawn
  - [ ] Parsing behaviour is unchanged — every pre-existing probe test passes unmodified
        (regression check)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MPRB-03: Extend the stream model with the playback fields, and classify HDR
- **Priority:** Critical
- **Labels:** muse, media, probe, hdr
- **Agent:** claude
- **Estimate:** 8h
- **Blocked by:** MPRB-01
- **Description:** Foundry's curation policy needed codec, dimensions and bitrate. Spec C's
  `DeviceProfile` matching needs more: profile/level, bit depth, frame rate, colour metadata,
  sample rate, channel layout, and image-vs-text subtitles — plus an **HDR classification**,
  which nothing currently derives. Purely additive to `MediaProbe`; every existing field
  keeps its meaning and every existing test stays green.

  ## FILES
  - `src/media/probe.rs` — new fields on `VideoStream`/`AudioStream`/`SubtitleStream`, new
    `RawStream` fields, parser extensions
  - `src/media/hdr.rs` — `HdrFormat`, `classify(&VideoStream) -> HdrFormat`
  - `src/media/derive.rs` — the derived accessors spec C consumes

  ## APPROACH
  0. **No argv change, and no second probe.** `build_ffprobe_args` already passes
     `-show_streams`, so ffprobe is **already returning** `profile`, `level`,
     `avg_frame_rate`, `bits_per_raw_sample`, `color_transfer` and `color_primaries` in the
     JSON Muse fetches today — the private `RawStream` struct (`probe.rs:337-358`) simply
     does not extract them. **This item is "parse more of the document we already have",
     not "re-probe the library."** Price it accordingly: the cost is deserialisation
     fields and tests, and **no additional ffprobe invocation, no argv change, and no extra
     I/O against the library mount.**
     The backfill's I/O cost (MPRB-07) is a separate and unavoidable thing — nothing has
     ever *persisted* a probe, so every file is probed once regardless — but it is one pass,
     not one pass per field added here.
     **This is a real unblock for spec C**, not a nicety: until these fields land, C's
     ceilings that depend on them correctly return `CannotDecide` rather than guessing.
  1. **New raw fields** (all `Option<serde_json::Value>` / `Option<String>` with
     `#[serde(default)]`, matching the existing permissive `RawStream` idiom — that idiom is
     load-bearing and its doc comment explains why): `profile`, `level`,
     `bits_per_raw_sample`, `r_frame_rate`, `avg_frame_rate`, `color_primaries`,
     `color_transfer`, `color_space`, `color_range`, `sample_rate`, `channel_layout`,
     `side_data_list`, and `disposition.{comment, hearing_impaired}`.
  2. **New helper `as_ratio(&Option<Value>) -> Option<f64>`** for `"24000/1001"`.
     **A zero denominator returns `None`** — `"0/0"` is what ffprobe emits for a stream with
     no meaningful rate and is the single most likely panic site in this whole spec. It
     follows the existing `as_u64`/`as_f64` convention exactly: unparseable, negative or
     non-finite is `None`, never `0`.
  3. **`VideoStream` gains** `profile`, `level`, `bit_depth`, `frame_rate_fps`,
     `avg_frame_rate_fps`, `color_primaries`, `color_transfer`, `color_space`, `color_range`,
     `hdr_format`, `is_hdr`. **Bit depth** comes from `bits_per_raw_sample`, else is derived
     from `pix_fmt` via a documented table (`yuv420p10le` ⇒ 10, `yuv420p` ⇒ 8, …), else
     `None`. When derived rather than observed, say so — step 6.
  4. **`AudioStream` gains** `profile`, `sample_rate_hz`, `channel_layout`, `default`,
     `forced`. **`SubtitleStream` gains** `is_image_based`, `hearing_impaired`.
     `is_image_based` is true for `pgs`/`hdmv_pgs_subtitle`/`dvd_subtitle`/`vobsub`/
     `dvb_subtitle`/`xsub`, false for `subrip`/`ass`/`ssa`/`mov_text`/`webvtt`, and for an
     **unknown codec defaults to `true`** — fail-closed, because rendering an image sub as
     text produces garbage on screen while the reverse merely costs CPU.
  5. **HDR classification**, first match wins — the order *is* the algorithm:
     - **Dolby Vision** — a `dovi`/`dvhe`/`dvh1` codec tag, or a DOVI configuration record in
       `side_data_list`. DV wins over HDR10 because a profile-8 file legitimately signals
       both, and DV presence is the decision-relevant fact for a client that cannot do DV.
     - **HLG** — `color_transfer` `arib-std-b67`.
     - **HDR10+** — PQ transfer plus SMPTE-2094 dynamic metadata side data.
     - **HDR10** — transfer `smpte2084`/`pq` with `bt2020` primaries.
     - **Sdr** — a recognised SDR transfer (`bt709`, `smpte170m`, `iec61966-2-1`).
     - **Unknown** — transfer absent or unrecognised. **Do not default to SDR.** An
       unlabelled file is a fact spec C should see as unlabelled; calling it SDR is how a
       tone-mapping decision gets made on a lie. This is the rule `probe.rs` already applies
       to `"N/A"`, applied to colour.
     `HdrFormat` serialises snake_case and is `#[serde(other)]`-tolerant, so a future variant
     does not break an already-stored row.
  6. **A `notes: Vec<String>` field on `MediaProbe`** (capped at 32 entries), recording
     *derived-not-observed* facts: bit depth inferred from `pix_fmt`, an unknown subtitle
     codec assumed image-based, `bt2020` primaries with a `bt709` transfer. The same honesty
     rule the module already enforces, extended to inferences — spec C makes real decisions
     on these values and must be able to tell an observation from a guess. Deliberately
     **not** an error channel: a note never changes a verdict.
  7. **Derived accessors** in `src/media/derive.rs`, all total, consumed by spec C rather
     than reimplemented there: `is_hdr()`, `is_10bit()`, `resolution_class()`
     (`Sd`/`Hd`/`FullHd`/`Uhd`/`Unknown`), `has_lossless_audio()`, `has_image_subtitles()`,
     `default_audio()`, `audio_languages()`, and `effective_bitrate_bps()` returning both a
     value and its **source** (`container` → sum-of-streams → `size/duration` → `None`).
     `primary_video()` already exists and already excludes cover art — reuse it; do not
     define "the video stream" a second time.
  8. **`suspicion(&MediaProbe) -> Option<Suspicion>`** for the taxonomy's `suspicious` state:
     `NoStreamsOfInterest` (no video **and** no audio), `ZeroDuration`, `ZeroDimensions`,
     `DurationBitrateInconsistent`. Pure, total, and never inferred from a `ProbeError` — a
     suspicion is a statement about a *successful* parse. Note the asymmetry: missing *one*
     of video/audio is legitimate (`probe.rs` already has a test for the audio-only case);
     missing *both* is not.
  9. **Caps**: streams at `MAX_STREAMS` (512) ⇒ `ProbeError::MalformedOutput`; tag strings
     truncated at `MAX_TAG_LEN`.

  ## TEST PLAN
  - `cargo test` — every pre-existing probe test passes unmodified
  - `as_ratio("0/0")`, `("24000/1001")`, `("")`, `("garbage")`
  - Bit depth: observed `bits_per_raw_sample` wins; `yuv420p10le` derives 10 **and adds a note**
  - Each HDR variant classifies from its fixture; DV + HDR10 ⇒ `DolbyVision`
  - **Absent `color_transfer` ⇒ `Unknown`, never `Sdr`** (the negative test)
  - `is_10bit()` and `is_hdr()` are independent — a 10-bit SDR file proves it
  - An unknown subtitle codec is image-based and noted
  - `suspicion()` returns each variant for its case, `None` for a healthy file, and `None`
    for an audio-only file
  - A 600-stream document is `MalformedOutput`, not a panic
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Cover art must still never be selected as `primary_video` — the existing test guards it
  - `channel_layout: "unknown"` — kept verbatim; `channels` still typed
  - DV profile 5 (no HDR10 base layer) vs profile 8 — both `DolbyVision`; the profile stays
    in `VideoStream::profile` for spec C to discriminate
  - `bt2020` primaries with a `bt709` transfer (a mislabelled remux) — not HDR, add a note
  - `width`/`height` of 0 — already `None` via `as_u32`; keep it that way so an aspect-ratio
    calculation cannot divide by zero
  - A file with no video at all — every video accessor returns `None`/false, no panic

- **Acceptance criteria:**
  - [ ] `MediaProbe` gains profile/level/bit depth/frame rate/colour/sample rate/channel
        layout/image-vs-text fields, purely additively, with every pre-existing test passing
        unmodified (regression check) — **and `build_ffprobe_args` is unchanged**, since
        every one of these fields is already present in the JSON ffprobe returns today
  - [ ] HDR10, HDR10+, HLG, Dolby Vision and SDR classify correctly; DV wins when both are
        signalled
  - [ ] An absent or unrecognised transfer classifies `Unknown`, never `Sdr` (negative test)
  - [ ] Derived-not-observed facts are recorded in `notes` and never change a verdict
  - [ ] `is_10bit()` and `is_hdr()` are independent axes
  - [ ] `suspicion()` flags no-streams/zero-duration/zero-dimensions/inconsistent-size and
        returns `None` for a healthy or audio-only file
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MPRB-04: Golden fixture corpus — collected from the real library
- **Priority:** Critical
- **Labels:** muse, media, probe, test
- **Agent:** claude
- **Estimate:** 8h
- **Blocked by:** MPRB-03
- **Description:** `probe.rs` already has ~20 parser tests, one of which (`H264_MKV`) is a
  captured real document and the rest synthesised to isolate a specific behaviour. That is a
  good unit-test suite and it stays. What it is not is a **corpus** — a breadth-first sample
  of what this particular 20-year-old library actually contains, which is the regression net
  spec C/D/E need. **Extend, do not duplicate.**

  **Every fixture is COLLECTED FROM THE REAL LIBRARY. None is fabricated**, and the
  collection is specified here rather than left to the implementer's imagination. A
  hand-written fixture tests the parser against the author's *belief* about ffprobe output;
  only a captured one tests it against ffprobe, and only a captured one contains this
  library's particular weirdness. **No media files enter the repo — only the JSON**, a few
  hundred KB of text.

  ## FILES
  - `tests/golden/probe/` — the captured `*.json` files, `<NN>-<short-description>.json`
  - `tests/golden/probe/README.md` — what each fixture is, which collection wave it came
    from, what it is the regression test *for*, and the capturing ffprobe version. Paths are
    described by characteristic, **never** reproduced (S1/PII)
  - `src/media/probe.rs` — a `#[cfg(test)] mod golden` using `include_str!`, so the tests
    have no filesystem dependency and pass in any working directory
  - `src/bin/probe_capture.rs` — the operator capture + survey tool

  ## APPROACH
  1. **`probe_capture` first.** `probe_capture <path> <fixture-name>` runs the probe and
     **scrubs before writing**: `format.filename` → `"FIXTURE_PLACEHOLDER.<ext>"`; drop tag
     keys carrying paths, encoder identity or personal names (`comment`, `encoder`,
     `ENCODER`, `DESCRIPTION`, `copyright`, `SUMMARY`); pretty-print with stable key order
     for reviewable diffs.
  2. **Migrate the existing captured fixture.** `H264_MKV` moves from an inline `const` into
     the corpus, with its assertions retargeted at the file. The **synthesised inline tests
     stay inline** — they isolate one behaviour each, and a golden file would make them less
     legible, not more.
  3. **Wave 1 — targeted survey.** Add `probe_capture --survey <root> --sample <n>`: walks
     the read-only library mount, probes a bounded random sample **under the same rate limit
     MPRB-07 uses** (it hits the same network mount; a survey is not an excuse to stampede
     it), buckets results by the characteristics below, and reports which buckets it can fill
     and from how many candidates. The operator then captures one exemplar per bucket.
     | # | Characteristic | Proves |
     |---|---|---|
     | 01 | H.264 High + AAC stereo, MP4 | the direct-play baseline (epic §6 tier 1) |
     | 02 | HEVC Main10 HDR10, MKV | 10-bit + PQ/bt2020 classification |
     | 03 | Dolby Vision profile 8 (DV + HDR10) | DV wins over HDR10 |
     | 04 | HLG | `arib-std-b67` transfer |
     | 05 | TrueHD/Atmos | lossless detection, >8 channels |
     | 06 | AC-3 5.1 + AAC stereo | default-disposition audio selection |
     | 07 | PGS image subtitles | `is_image_based` true |
     | 08 | SRT text subtitles | `is_image_based` false |
     | 09 | 3+ audio, 5+ subtitle languages | language handling at scale |
     | 10 | MPEG-2 in VOB | legacy container, `N/A` bitrates |
     | 11 | VP9 in WebM | non-MP4/MKV container |
     | 12 | AV1 | modern codec |
     | 13 | Variable frame rate (`r_frame_rate` ≠ `avg_frame_rate`) | both rates retained |
     | 14 | No audio stream | not suspicious, not an error |
     | 15 | Corrupt / unprobeable | `ProbeError`; exit code + stderr in a sidecar |
     | 16 | Matroska with font attachments | the existing attachment handling, on a real file |
     Fixture 15 is structurally different — it stores the exit code and stderr excerpt rather
     than stdout, and asserts the **error** path. It is the most important fixture in the
     set: a library this old contains files like it, and a panic there takes down a worker.
  4. **Wave 2 — discovery during the live backfill.** The survey sees a sample; the backfill
     sees everything, and it is the first time anyone has looked at the whole library. Two of
     its outputs come back here as a follow-on commit, closed out by MPRB-10:
     - every codec/container in MPRB-09's coverage report that **no fixture covers** — the
       long tail is exactly where a parser breaks, and nobody knows what is in it until the
       backfill runs;
     - the first rows to land in `probe_failed` and `suspicious` — real broken files from
       this library, better fixture-15 material than anything found on purpose.
     Cap the additions at ~10, prefer breadth of characteristic over volume, and record the
     wave in the README.
  5. **Assert facts, not blobs.** One test per fixture asserting the specific things it
     exists to prove — codec, profile, bit depth, HDR class, channels, subtitle kind,
     languages, frame rates, bitrate source. A whole-struct snapshot fails on every additive
     field and gets rubber-stamped into meaninglessness within two sprints.
  6. **One corpus-level test** iterating every `*.json`: it parses, and its `MediaProbe`
     round-trips through serde unchanged. A newly added fixture is then covered by the
     invariant before anyone writes its specific assertions.
  7. README: reproducing a mis-parse as a fixture is the required first step of any probe bug
     report — fixture, then fix.

  ## TEST PLAN
  - `cargo test` — per-fixture tests plus the corpus test, alongside the existing inline suite
  - The whole suite passes with **no ffprobe on the host** (`include_str!`)
  - `probe_capture` scrubbing: a synthesised input containing a path and an `encoder` tag
    produces a fixture containing neither
  - The corpus test fails loudly on an unparseable fixture (proving the net is armed)
  - `--survey` respects the rate limit and never writes to the library mount
  - PII gate clean over `tests/golden/probe/`
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - A characteristic cannot be sourced from the library — record the gap in the README and
    open a follow-up; **do not fabricate the JSON**
  - A fixture legitimately contains a person's name in a *media title* — keep the title,
    scrub only paths/encoder/comment tags, and say so in the README
  - ffprobe version skew renames a field — the corpus test catches it; pin the capturing
    version in the README
  - A fixture over ~200 KB (dozens of streams) — keep it; that is the realistic stress case

- **Acceptance criteria:**
  - [ ] A captured corpus is checked in with a README naming each fixture's purpose,
        collection wave and ffprobe version — **every fixture captured, none fabricated**
  - [ ] The existing captured `H264_MKV` is migrated into the corpus and the synthesised
        inline unit tests are left intact (regression check)
  - [ ] `probe_capture --survey` walks the read-only mount under the backfill rate limit and
        reports which characteristic buckets it can fill
  - [ ] Each fixture has a test asserting its characteristic facts, not a blob snapshot
  - [ ] The corrupt/unprobeable fixture asserts a typed `ProbeError` and no panic (negative test)
  - [ ] A corpus-level test parses and serde-round-trips every fixture, and the suite passes
        on a host with no ffprobe
  - [ ] No hardcoded infrastructure values in new/modified code

---

### MPRB-05: Persist the probe — `MediaInfoDoc`, the migration, and the one typed reader
- **Priority:** Critical
- **Labels:** muse, media, db, migration
- **Agent:** claude
- **Estimate:** 7h
- **Blocked by:** MPRB-03
- **Description:** **The gap this whole spec exists for.** Foundry probes on demand and
  discards the result; nothing has ever persisted a probe for the library. Keep
  `media_files.media_info` as `jsonb`, but make it a **versioned document with exactly one
  typed reader**. Today's rows are `{"container": "mkv"}` from `scan.rs` and must keep
  working.

  ## FILES
  - `migrations/0109_media_files_probe.sql` — new
  - `src/media/doc.rs` — `MediaInfoDoc`, `StoredMediaInfo`, `ProbeState`, the flat projection
  - `src/models/media_file.rs` — the new columns, the accessor
  - `src/repo/media_file.rs` — `set_probe_result`, `set_probe_error`, `list_needing_probe`,
    `probe_progress`
  - `tests/` — the grep guard

  ## APPROACH
  1. **Migration** (additive, idempotent, in the house style of
     `0108_play_sessions_plex_refs.sql`):
     ```sql
     ALTER TABLE media_files
         ADD COLUMN IF NOT EXISTS media_info_version int,
         ADD COLUMN IF NOT EXISTS probed_at          timestamptz,
         ADD COLUMN IF NOT EXISTS probe_state        text,
         ADD COLUMN IF NOT EXISTS probe_error        text,
         ADD COLUMN IF NOT EXISTS probe_attempts     int NOT NULL DEFAULT 0;
     ```
     `probe_state` is **text with a `CHECK`**, not a Postgres enum: the crate's existing enum
     (`release_type_kind`) is a stable domain type, whereas this one will plausibly gain a
     variant, and extending a checked text column is a migration whose deploy can be ordered
     independently of the code that uses it.
     Two partial indexes: the backfill queue predicate (`WHERE media_info_version IS NULL OR
     media_info_version < 1`) and the audit predicate (`WHERE probe_state IN
     ('probe_failed','suspicious')`).
     Never edit `0009_media_files.sql`; the new migration's header records that it delivers
     what 0009's line-28 comment promised and never shipped.
  2. **The document.**
     ```
     MediaInfoDoc {
       schema_version: u16,        // MEDIA_INFO_SCHEMA_VERSION = 1
       probe: MediaProbe,          // the whole promoted type, verbatim
       // flat compatibility projection — step 3
       container, video_codec, audio_codec, resolution, width, height,
       file_extension: Option<String>,  // step 4 — from the path, not from the probe
     }
     ```
     `media_info_version` duplicates `schema_version` **only** so the backfill query is
     indexable (`jsonb ->> 'schema_version'` is not, absent a functional index). The document
     is authoritative; the column is a hint; `set_probe_result` writes both in one statement
     so they cannot diverge.
  3. **The flat projection is a deliverable, not a convenience.** Verified in the tree:
     `Terminus/constellation-web/src/panels/muse/MediaDetailPanel.tsx:76-85` already reads
     `media_info` and renders whichever of `container`, `video_codec`, `audio_codec`,
     `resolution`, `width`, `height` are present — **flat, top-level keys**, none of which
     has ever been populated. The panel has permanent dead pixels today. Emitting these keys
     lights it up with **zero constellation-web change and no `dist/` rebuild**.
     - **Derived, never independently sourced**: built from `primary_video()` /
       `default_audio()` / `probe.container` at serialisation time, so they cannot drift. A
       test asserts the projection equals the derivation for every golden fixture.
     - `container` keeps the **exact key and shape the legacy document used** (a bare string
       like `"mkv"`), making a v1 document a strict superset of the legacy one — so the panel
       keeps working throughout the window between deploy and backfill completion. Note
       `MediaProbe::container` is the *raw* `format_name` (`"matroska,webm"`); the flat key
       carries the normalised value via `foundry::policy::normalize_container`, because
       changing what an already-rendered field means is a GUI regression wearing a schema
       change's clothes.
     - `resolution` is `"{w}x{h}"`, absent when either dimension is unknown — never `"0x0"`.
  4. **`file_extension: Option<String>` — the one fact the probe cannot know.** `MediaProbe`
     carries container, duration, bitrate, size, the stream vectors, counts and title, and
     **no path and no filename** — correctly, since it is a description of a file's
     *contents*. But ffmpeg uses a shared demuxer for Matroska and WebM, so `format_name` is
     the literal string `"matroska,webm"` for a `.mkv` **and** a `.webm`, and spec C needs to
     tell them apart for one narrow case.
     - **Persisted here, at the layer that holds the path**, because this layer owns it and
       spec C does not: `set_probe_result` takes the `media_files.relative_path` it is
       already updating, lowercases the final extension, and stores it on the document. It is
       **not** a field on `MediaProbe` and the parser never sees it — a pure function of a
       JSON document must not acquire a filename input.
     - **It is a hint, and the constraint is load-bearing** (spec C has committed to the
       matching rule): it may only resolve what codec-whitelist inference over the stream
       lists left **unproven**, and **may never override inference that succeeded**. A scene
       release is a filename, not an authority — a `.mkv` extension on a file whose streams
       say otherwise is a mislabelled file, and believing the label over the bytes is how a
       playback decision goes wrong in the one case this field exists to help.
     - Absent, empty, or absurdly long (> 16 chars) ⇒ `None`. C ships fine without it and
       merely returns `CannotDecide` more often, so this must never become a dependency.
  5. **One reader:**
     ```rust
     pub enum StoredMediaInfo {
         Absent,
         Legacy(LegacyMediaInfo),         // no schema_version ⇒ pre-S130, container only
         V1(Box<MediaInfoDoc>),
         UnknownVersion { version: u16 }, // written by a NEWER binary; do not guess
     }
     impl MediaFile { pub fn stored_media_info(&self) -> StoredMediaInfo }
     ```
     A corrupt v1 document degrades to `UnknownVersion` rather than erroring — **a bad row
     must not break a list endpoint**. A version above `MEDIA_INFO_SCHEMA_VERSION` is opaque,
     never partially parsed: during a rolling deploy an older binary genuinely sees newer
     documents.
  6. **Repo functions.**
     - `set_probe_result(pool, id, relative_path, &MediaProbe)` — writes `media_info`, `media_info_version`,
       `probed_at`, clears `probe_error`, resets `probe_attempts`, and sets `probe_state` to
       `ok` **or `suspicious`** from `suspicion()` (MPRB-03 step 8). A suspicious result **is
       still stored** — it parsed, and partial data serves `/why` better than a null — but
       stored *labelled*. `probe_error` then carries the `Suspicion` description, so one
       column answers "what is wrong with this file" for all three unhappy states.
     - `set_probe_error(pool, id, &ProbeError)` — `probe_error` (truncated to 1 KB),
       `probed_at`, `probe_attempts + 1`, `probe_state` from `ProbeError::state()`, and
       **leaves `media_info` untouched** so a failed re-probe never destroys a good result.
     - `list_needing_probe(pool, after_id, limit, max_attempts)` — **keyset** pagination on
       `id > after_id` over the queue predicate, with `probe_attempts < max_attempts`.
       Keyset, not OFFSET: the backfill resumes from a cursor and OFFSET degrades
       quadratically over a large library.
     - `probe_progress(pool)` — one count per state plus `total`, `legacy`, `unprobed`,
       `permanently_failed`. `suspicious` counts as *probed* for completion and as *needing
       attention* for the report; conflating those two questions is what makes a backfill
       look finished when it is not.
  7. **Grep guard.** A test scanning `src/` for `media_info["`, `media_info.get("`,
     `media_info ->>` outside `src/media/doc.rs` and `src/models/media_file.rs`, failing with
     a message pointing at `stored_media_info()`. Same enforcement idea as
     constellation-web's single-`fetch`-site rule (epic §7.8), and it exists because "never
     ad-hoc key access" is otherwise a convention that survives until the next hurried change.

  ## TEST PLAN
  - `cargo test` — accessor unit tests over synthesised JSON
  - A pre-S130 `{"container":"mkv"}` reads as `Legacy { container: Some("mkv") }`; `NULL`
    reads as `Absent`
  - A v1 document round-trips `MediaProbe` → jsonb → `stored_media_info()` unchanged
  - `{"schema_version": 99, …}` reads as `UnknownVersion`, and a structurally corrupt v1
    document degrades rather than erroring (negative test)
  - The flat projection matches the derivation for every golden fixture; `resolution` is
    absent, not `"0x0"`, when a dimension is unknown
  - `file_extension` is lowercased from the persisted `relative_path`; a `.mkv` and a
    `.webm` with the identical `"matroska,webm"` `format_name` are distinguishable in the
    stored document
  - An absent, empty or >16-char extension stores `None`, and `MediaProbe` itself still
    carries no path or filename (negative test: the parser signature is unchanged)
  - A suspicious result stores both `probe_state = 'suspicious'` and its `media_info`, and is
    not returned by `list_needing_probe`
  - The grep guard fails on a deliberately-added `media_info["container"]` in an unauthorised
    file and passes on a clean tree
  - DB-gated (`db_gated` idiom): `set_probe_result` then `set_probe_error` leaves
    `media_info` intact and increments `probe_attempts`
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Rolling deploy: an old binary reads a v2 document — `UnknownVersion`, never partial
  - `media_info_version` disagrees with the document (hand-edited row) — document wins, warn once
  - `media_info` holding a JSON array or scalar — `Legacy` with everything `None`
  - A file with no extension at all, or a directory-style path — `file_extension: None`;
    downstream this simply means C has no tiebreaker and answers `CannotDecide`
  - An extension that disagrees with the streams (a `.avi` full of HEVC) — stored verbatim
    anyway. It is a hint, and recording what the filename claims is not the same as
    believing it; the never-override rule is what makes storing it safe
  - Migration run twice — fully idempotent
  - Migration applied while the service runs — additive-only, so the running binary is
    unaffected (the S127 lesson: ship the migration *with* the deploy)

- **Acceptance criteria:**
  - [ ] `migrations/0109_media_files_probe.sql` is additive, idempotent and adds both partial
        indexes, with a `CHECK`-constrained `probe_state`; `stored_media_info()` is the only
        reader of the jsonb, enforced by a grep guard test
  - [ ] Legacy container-only rows read as `Legacy` and stay eligible for backfill; a
        newer/corrupt document degrades instead of erroring a list endpoint (negative test)
  - [ ] The v1 document carries the flat `container`/`video_codec`/`audio_codec`/`resolution`/
        `width`/`height` keys, derived from the probe and asserted equal to it, with
        `container` keeping the legacy key and shape
  - [ ] `file_extension` is persisted from the path (never from the probe, whose signature is
        unchanged) and documented as a hint that may only resolve what inference left
        unproven and may never override it
  - [ ] `probe_state` distinguishes `ok`/`unreadable`/`probe_failed`/`suspicious`, and
        `probe_progress` reports each separately
  - [ ] `set_probe_error` never overwrites a previously-good `media_info`
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MPRB-06: Library scan probes on arrival — so the backfill is a migration, not a treadmill
- **Priority:** High
- **Labels:** muse, media, library
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MPRB-04, MPRB-05
- **Description:** Replace the `{"container": "<ext>"}` stub at `src/library/scan.rs:~500`
  (in `record_matched_file`) with a real probe.

  **This item is what makes the backfill a one-time job.** Every newly acquired file enters
  through `library/scan.rs`. If the scanner keeps writing extension-only `media_info`, then
  `media_info` *regresses* for every new acquisition, the backfill becomes a permanent
  recurring sweep, and MPRB-09's coverage number decays from the day it is published. The
  ingest path must probe **inline** or **enqueue** — never neither.

  **A probe failure must never fail a scan.** The scanner's job is to find files; one
  unreadable file cannot abort a library walk.

  ## FILES
  - `src/library/scan.rs` — `record_matched_file`, the scan context and `ScanReport`
  - `src/library/mod.rs` — thread `MediaCore` through
  - `src/web/dashboard.rs` — surface the new report counters

  ## APPROACH
  1. Add `media: Option<MediaCore>` to the scan context plus `MUSE_PROBE_ON_SCAN` (default
     **true**; `false` restores today's behaviour exactly). `None`, or
     `capability.can_probe() == false`, ⇒ today's container-only path, unchanged — Module
     Contract §2 / epic §7.4: an unconfigured capability degrades, it does not break.
  2. In `record_matched_file`, after `upsert_scanned` returns `(media_file, file_changed)`:
     probe only when `file_changed` **or** `stored_media_info()` is not `V1`. A
     byte-identical, already-probed file is skipped — this is what keeps a rescan of a 27 TB
     library cheap.
  3. Success ⇒ `set_probe_result`; failure ⇒ `set_probe_error`; continue either way. Count
     into `ScanReport` (`probed`, `probe_failed`, `probe_skipped`) and log one summary line
     (counts only, no paths at `info`).
  4. **Bound concurrency** with a `tokio::sync::Semaphore` sized by
     `MUSE_PROBE_SCAN_CONCURRENCY` (default 2), using `run_ffprobe_async` (MPRB-02). The
     library is a network-mounted read-only share (epic §10.3); an unbounded fan-out of
     ffprobe processes across it degrades playback for the whole household while it runs.
  5. **The enqueue fallback.** When probing is off, or the runner self-disabled mid-scan
     (`ToolMissing`), the row simply keeps `media_info_version = NULL` — already the
     backfill's queue predicate — so MPRB-07 picks it up. **No separate queue table**: the
     same absence-of-a-current-version signal serves both the one-time migration and the
     steady-state catch-up, so there is one mechanism rather than two that can disagree.
     The consequence, stated plainly: **the backfill worker stays enabled permanently**,
     idling over an all-probed library. Its pending gauge is then a live health signal — a
     persistently non-zero value means the ingest path stopped probing, which is exactly the
     regression this item prevents.
  6. **The GUI lights up for free.** Once a scan writes a v1 document, `MediaDetailPanel`'s
     existing fields populate with no constellation-web change and no `dist/` rebuild — so
     this item's endpoint tests must assert the API response carries those flat keys, since
     the panel's correctness now depends on a contract Muse owns and the panel cannot see.

  ## TEST PLAN
  - `cargo test` — scan tests with a stub probe injected
  - With probing disabled, a scan produces byte-identical `media_info` to today (regression check)
  - A probe failure on one file leaves the scan running and increments `probe_failed` (negative test)
  - An unchanged, already-v1 file is not re-probed (the stub asserts it was not called)
  - Concurrency never exceeds the configured limit (a counting stub asserts the high-water mark)
  - A file ingested while probing is disabled is subsequently returned by `list_needing_probe`
  - The media-file API response for a probed file carries the six flat keys the panel reads
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - ffprobe absent — `capability.can_probe()` is false at startup, so the scan never spawns
    per-file; a mid-scan `ToolMissing` disables probing for the rest of that run and logs once
  - A file that disappears between the walk and the probe — `unreadable`, counted, continue
  - Scan cancelled mid-run — written results persist; the next scan skips them naturally
  - Timeouts dominating a scan (stalled mount) — visible in the report; do not add an
    auto-abort heuristic here

- **Acceptance criteria:**
  - [ ] A scan writes a real v1 document for each newly-matched or changed file
  - [ ] A probe failure is recorded and the scan runs to completion (negative test)
  - [ ] An unchanged, already-probed file is not re-probed, and concurrency is bounded by
        config to a value safe for a network mount
  - [ ] A file ingested with probing disabled is picked up by the backfill queue — **no newly
        acquired file is ever left permanently extension-only**
  - [ ] With probing disabled the scan's output is unchanged from pre-S130 behaviour
        (regression check)
  - [ ] `MediaDetailPanel.tsx`'s existing `video_codec`/`resolution`/`width`/`height` fields
        render real values for a probed file, with no constellation-web change and no
        `dist/` rebuild
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MPRB-07: Resumable, rate-limited backfill worker
- **Priority:** High
- **Labels:** muse, media, worker, metrics
- **Agent:** claude
- **Estimate:** 8h
- **Blocked by:** MPRB-05, MPRB-06
- **Description:** Probe the **existing** library — the rows a scan will not revisit. The
  constraint that shapes the item: the library is a **network-mounted, read-only QNAP share**
  shared with live playback, so a naive probe-everything loop presents to the household as
  "Plex is slow", an expensive thing to misdiagnose (epic §10.3, §10.5).

  ## FILES
  - `src/media/backfill.rs` — the worker
  - `src/workers.rs` — spawn alongside the existing workers
  - `src/metrics.rs` — the progress metrics
  - `src/http/ops.rs`, `src/http/mod.rs` — `POST /ops/probe/backfill`
  - `src/config.rs` — the `MUSE_PROBE_BACKFILL_*` knobs

  ## APPROACH
  1. Config: `MUSE_PROBE_BACKFILL_ENABLED` (default **false** — an operator turns it on
     deliberately; it touches the live mount), `_RATE_PER_MIN` (30), `_BATCH` (50),
     `_CONCURRENCY` (1), `_INTERVAL_SECS` (60), `_MAX_ATTEMPTS` (3).
  2. **Resumability lives in the database.** Each pass calls `list_needing_probe(after_id,
     batch, max_attempts)` and writes each result immediately. The cursor is implicit — the
     *absence* of a current-version document is the queue. A restart loses at most the
     in-flight probes and re-derives everything else from the table. No checkpoint file, no
     worker-state row, nothing to corrupt.
  3. **Single-flight** via a Postgres session advisory lock on a constant key documented in
     the module. A pass that cannot take it logs at `debug` and returns — two Muse instances,
     or a manual `/ops` kick racing the timer, must not double the load on the mount.
  4. **Rate limiting is per-file, not per-batch**, with accumulated tokens capped at one
     bucket's worth. Batch-level sleeping produces a burst then silence — exactly the herd
     shape being avoided — and an uncapped bucket grants a burst after a host suspend.
  5. **Retry dispatches on the state taxonomy, not on "did it fail":**
     - `unreadable` — **retryable**; `probe_attempts` increments, and a row reaching
       `MAX_ATTEMPTS` leaves the queue as `permanently_failed`.
     - `probe_failed` — **terminal on the first attempt**. ffprobe already ran on a readable
       file; three more timeouts against the mount cannot change the answer.
     - `suspicious` — **never retried, never ignored.** It leaves the queue immediately and
       stays visible to `/why` and the coverage report.
     `ProbeError::is_retryable()`/`state()` (MPRB-02) is the mechanical form.
  6. **Observability** (`src/metrics.rs`, following the existing PROMEX-03 pattern and its
     cardinality discipline — closed label sets, never a path or title as a label):
     - `muse_probe_backfill_processed_total{state}` — `ok|suspicious|probe_failed|unreadable`,
       so a mount outage and a wave of corrupt files look different on a graph
     - `muse_probe_backfill_pending` — gauge, refreshed per pass from `probe_progress()`
     - `muse_probe_backfill_duration_seconds` — per-file probe wall time (this is how an
       operator discovers the timeout is set too low)
     - `muse_probe_backfill_last_pass_unix` — a stalled worker shows as a stale timestamp,
       which is the failure mode that actually happens
  7. `POST /ops/probe/backfill` (protected, existing bearer) runs **one** pass synchronously
     and returns its counts — for the operator and for MPRB-10, without waiting on a timer.
  8. Log per pass at `info`: processed, per-state counts, pending, and ETA at the current
     rate. The ETA is what tells the operator whether to raise the rate.

  ## TEST PLAN
  - `cargo test` — worker tests with a stub probe and a stub clock
  - Rate limiting: with `RATE_PER_MIN=60`, N probes take ≥ (N-1) simulated seconds, and no
    burst follows a long gap
  - Resumability: run a pass, drop the worker mid-batch, restart — nothing probed twice,
    nothing pending skipped
  - The advisory lock prevents a concurrent second pass, which processes zero rows and does
    not error (negative test)
  - `unreadable` retries to `MAX_ATTEMPTS`; `probe_failed` is terminal on the first attempt
    (no second spawn); `suspicious` leaves the queue and is never re-probed
  - Metrics are registered and appear in `gather_text()`
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Disabled (the default) — the worker is not spawned; the `/ops` kick returns a clear
    "disabled" response rather than silently doing nothing
  - **Steady state after the migration** — the worker stays enabled and idles, catching what
    the scan path missed (MPRB-06 step 5). A persistently non-zero pending gauge is the
    intended alarm, not noise to silence by disabling the worker
  - The mount vanishes mid-backfill — every probe is `unreadable` (retryable); the pass
    completes with errors and the next retries. No auto-disable heuristic in this item
  - Rows deleted between listing and probing — the UPDATE affects 0 rows; fine, not an error
  - A file that consistently times out — 3 × 30s, then `permanently_failed`

- **Acceptance criteria:**
  - [ ] The backfill resumes across a restart with no double-probing and no skips
  - [ ] Per-file rate limiting holds and accumulated tokens are capped so no burst follows a gap
  - [ ] A concurrent second pass is prevented by an advisory lock (negative test)
  - [ ] Retry dispatches on the taxonomy: `unreadable` retries to `MAX_ATTEMPTS`,
        `probe_failed` is terminal on the first attempt, `suspicious` is never retried and
        never silently dropped
  - [ ] Progress is observable: per-state counters, pending gauge, per-file duration
        histogram, last-pass timestamp
  - [ ] Default-off, and when disabled changes no existing behaviour (regression check)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MPRB-08: `/probe/:id/why` — the debug read surface
- **Priority:** Medium
- **Labels:** muse, media, http
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MPRB-05
- **Description:** A `/why`-style endpoint showing the stored probe for an item **and
  explaining its state**. When spec C makes a playback decision the operator disagrees with,
  this is the first thing anyone opens — a decision is only ever as good as the probe under it.

  ## FILES
  - `src/media/http.rs` — the handlers
  - `src/http/mod.rs` — `probe_routes()` nested into the **protected** router

  ## APPROACH
  1. Routes use axum `:id` path params, **not** `{id}` — the crate is on axum 0.7 where brace
     routes silently 501 (a house gotcha with its own memory note):
     - `GET /probe/:media_file_id/why`
     - `GET /probe/item/:media_item_id/why` — every file of an item, since a TV item has many
       and the operator usually starts from the item
  2. Response:
     ```json
     { "media_file_id": 1, "relative_path": "...", "state": "ok",
       "probed_at": "...", "probe_error": null, "probe_attempts": 0,
       "media_info": { "...the v1 document..." },
       "derived": { "hdr_format": "hdr10", "is_10bit": true, "resolution_class": "uhd",
                    "effective_bitrate_bps": 42000000, "effective_bitrate_source": "container",
                    "primary_video_index": 0, "audio_languages": ["eng","fra"] },
       "notes": ["bit depth derived from pix_fmt"],
       "why_not": null }
     ```
     `state` ∈ `ok | suspicious | probe_failed | unreadable | legacy | unprobed |
     unknown_version`. **`why_not` is a human sentence for every non-`ok` state** — "never
     probed: backfill is disabled", "unreadable after 3 attempts: Timeout after 30s",
     "parsed, but the container reports no duration — needs review", "written by schema
     version 2". The explanation is the point; a bare `null` sends the operator to the logs,
     which is what this exists to prevent.
  3. `derived` and `notes` come from MPRB-03's accessors — **not** reimplemented. The
     endpoint must show what spec C will see, or it is worse than useless.
  4. Behind the existing `auth::require_api_token` shared bearer, reached from
     constellation-web through `proxy_muse` (epic §9.1), so it needs no auth of its own.
     **Read-only**: no route here writes.
  5. `relative_path` is returned (existing Muse endpoints already do, so no new disclosure);
     the absolute path and the library root are **not**.

  ## TEST PLAN
  - `cargo test` plus the existing `endpoint_tests` golden-JSON idiom (`tests/golden/`)
  - Golden responses for `ok`, `suspicious`, `probe_failed`, `legacy`, `unprobed`,
    `unknown_version` — each with a non-null `why_not` where applicable
  - An unknown id returns 404 with the crate's standard error body; no bearer returns 401
    (negative tests)
  - The item-level route returns every file of a multi-file TV item
  - No absolute filesystem path appears in any response body
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - An item with zero files — `200` with an empty list, not a 404
  - A corrupt document — `unknown_version` with a `why_not`, never a 500
  - A large probe (the many-stream fixture) — returned in full; already bounded by `MAX_STREAMS`
  - Called mid-write by the backfill — a plain read, no lock needed

- **Acceptance criteria:**
  - [ ] `GET /probe/:media_file_id/why` returns the stored document plus derived facts and notes
  - [ ] Every non-`ok` state carries a human-readable `why_not`, including `suspicious`
  - [ ] `derived` is produced by the MPRB-03 accessors, not reimplemented
  - [ ] Read-only and behind the existing bearer: 401 without a token, 404 for an unknown id
        (negative test)
  - [ ] No absolute filesystem path or library root appears in a response body
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MPRB-09: Coverage report — the census that sizes spec E
- **Priority:** High
- **Labels:** muse, media, reporting
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MPRB-05
- **Description:** Aggregate the backfilled library into epic §6's question: **what fraction
  would direct-play to the devices we actually own.** §6 is explicit that this number decides
  whether spec E is the centrepiece or an edge case, and that **no sprints are committed to E
  before A answers it** — which makes the report a deliverable with a written artifact, not a
  debug endpoint.

  ## FILES
  - `src/media/coverage.rs` — the aggregate query and report model
  - `src/media/http.rs` — `GET /probe/coverage` (JSON)
  - `src/http/ops.rs` — `POST /ops/probe/coverage-report` (renders Markdown)
  - `docs/reports/probe-coverage.md` — the artifact (committed in MPRB-10)

  ## APPROACH
  1. **One aggregate SQL pass**, grouping on jsonb expressions with the counting done in
     Postgres. Streaming every row into Rust is a memory event on a 2–4 GB container.
  2. Contents:
     - **Coverage first:** total, probed at v1, legacy, unprobed, and each unhappy state.
       Denominator honesty — "92% H.264" over a 30%-probed library is a lie.
     - Video codec distribution (count, share, total bytes), container distribution, audio
       codec distribution, channel-count histogram, HDR distribution, bit depth, resolution
       class, subtitle kind (image/text/none)
     - **The headline:** `direct_play_candidate_share` — primary video H.264 **and** default
       audio in AAC/AC-3/E-AC-3/MP3, in MP4/MKV. A deliberately **conservative proxy**,
       labelled as such inline, because the authoritative answer needs spec C's real
       `DeviceProfile` matching. Stating the assumptions in the output is what stops it being
       quoted later as if it were the real number.
  3. `GET /probe/coverage` returns JSON (protected, read-only).
     `POST /ops/probe/coverage-report` returns the same data as **Markdown** for the operator
     to commit. The renderer is a pure function over the report struct, golden-tested — the
     artifact must be diffable and regenerable, not hand-maintained prose.
  4. Header records: generation timestamp, total files, probed share, Muse git SHA, and
     `MEDIA_INFO_SCHEMA_VERSION`. A coverage report without its denominators and schema
     version is not evidence.
  5. **No PII** (S1): aggregate counts only — no titles, paths, library names or hostnames.
     This file is committed to a repo that mirrors publicly.

  ## TEST PLAN
  - `cargo test` — the Markdown renderer against a fixed struct (golden output)
  - DB-gated: seed a fixture library with known codecs; assert every distribution and the
    direct-play share exactly
  - A zero-probed library reports 0% and does **not** divide by zero (negative test)
  - Legacy/unprobed rows are counted as such and excluded from codec distributions;
    `suspicious` rows are broken out rather than folded into `ok`
  - PII gate clean over `docs/reports/`
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Empty library — a valid report of zeros, not an error
  - Rows at a future schema version — their own bucket, never folded into v1
  - A codec on 1 file in 200k — keep it; the long tail is exactly what a transcode spec needs
    to see, and it is MPRB-04 wave-2's input. Do not truncate the distribution
  - Shares rounding to 100% with a residue — one decimal place, raw counts beside every share

- **Acceptance criteria:**
  - [ ] The report covers video codec, container, audio codec, channels, HDR, bit depth,
        resolution class and subtitle kind
  - [ ] Coverage denominators (probed / legacy / unprobed / per unhappy state) accompany every
        share, and the header carries the schema version and git SHA
  - [ ] `direct_play_candidate_share` is computed and explicitly labelled a conservative proxy
        pending spec C
  - [ ] A zero-probed library produces a valid zero report with no division by zero (negative test)
  - [ ] The Markdown renderer is pure and golden-tested, and the artifact contains no PII
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MPRB-10: Run the live backfill and publish the coverage artifact
- **Priority:** High
- **Labels:** muse, media, ops
- **Agent:** <operator>
- **Estimate:** 2h operator time (plus unattended backfill wall-clock)
- **Type:** human-action
- **Blocked by:** MPRB-07, MPRB-09
- **Description:** The probe layer is done when the library is probed and epic §6's number
  exists on paper — not when it compiles. This is the gate on committing sprints to spec E.
- **Steps:**
  1. **Install ffmpeg/ffprobe on the Muse host.** It is absent on the Muse deploy host today, which is why
     nothing has ever probed the library. Confirm `ffprobe -version`.
  2. Apply `migrations/0109_media_files_probe.sql` through the operator-guarded **`pg_ddl`**
     door, sequenced **with or before** the image swap — migrations are not auto-applied
     (skill v4.6) and shipping the code without it breaks the read path.
  3. Deploy the sanctioned way: `oci-publish.sh muse moosenet/Muse main <bins…>` →
     `constellation-update.sh --force --skip-idle muse`. **Never a hand-built binary swap** —
     the nightly updater compares OCI digests and reverts one.
  4. Kick a single pass: `POST /ops/probe/backfill`. Read the per-state counts. If
     `unreadable` dominates, the mount is the problem — stop and fix that before enabling
     the timer.
  5. Enable `MUSE_PROBE_BACKFILL_ENABLED=true` at the default 30 files/min. Watch
     `muse_probe_backfill_*` and, more importantly, watch whether household playback
     degrades. Raise the rate only if it does not.
  6. **Confirm the GUI win**: open a probed title in constellation-web's Muse Media Detail
     panel and confirm codec/resolution now render where they were previously blank. No panel
     change or `dist/` rebuild should have been needed.
  7. When `muse_probe_backfill_pending` reaches zero (or plateaus on permanently-failed
     rows), run `POST /ops/probe/coverage-report`, commit the output to
     `docs/reports/probe-coverage.md`, and merge it through the normal pipeline.
  8. **Close out MPRB-04 wave 2**: capture fixtures for every codec/container the report shows
     that no fixture covers, plus the first `probe_failed`/`suspicious` files, and commit them.
  9. Report the headline numbers into the epic: probed share, H.264 vs HEVC vs other, HDR
     share, `direct_play_candidate_share`. **Spec E stays unscheduled until this exists.**
 10. Review the `suspicious` and `permanently_failed` lists. Files that never probe are a real
     library-health finding (truncated downloads, failed remuxes) and belong in a follow-up
     item, not silently in a metric.

---

## What this spec deliberately does not do

- **No rebuild of anything on `main`.** The ffprobe wrapper, the pure parser, the stream
  model, the path guard and the capability detector exist and are good. An item that
  reimplements them is rejected.
- **No rename of `MediaProbe`**, and no gratuitous churn through `plan.rs`/`forge.rs`.
- **No playback decisions.** No `DeviceProfile`, no direct-play/remux/transcode planning —
  that is spec C, which consumes this and stays pure. Foundry's existing `plan_transcode`
  answers a different question (should this be *permanently* re-encoded?) and is untouched.
- **No writes to any media file.** Not a byte. No metadata repair, no tag rewriting, no
  remux. The library mount is read-only and nothing here needs it not to be.
- **No language-tag "correction".** A probe reports what a tag says. `und` is already
  normalised to `None`; anything else is reported as tagged. A tag may be surfaced as
  *suspected-wrong* with a stated evidence source, never rewritten — `swe` is a valid code,
  and nothing in a probe establishes what language audio actually is.
- **No tone-mapping or HDR conversion** — out of scope through spec E per epic §8.3. This
  spec only *classifies* HDR.
- **No Maestro code.** Maestro does not exist yet (spec B); nothing here depends on it.
