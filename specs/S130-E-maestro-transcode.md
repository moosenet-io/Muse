# S130-E — Maestro: transcode tier and segmented delivery

plane_project: MUSE
module: Muse
prefix: MTRX
spec_id: S130-E-maestro-transcode

## Metadata
- **Author:** Moose
- **Session:** S130
- **Date:** 2026-08-01
- **Repo / binary:** `moosenet/Muse`, the **`maestro`** `[[bin]]` target. Per the epic §2, Maestro
  is a second binary inside the existing Muse crate, **not** a new repo — so `module: Muse` above
  is deliberate and matches the epic's own metadata. All modules land under `src/maestro/`;
  `models/`, `config.rs`, `repo/` and `error.rs` are shared with `muse`.
- **Module version:** Muse v0.1 → v0.2 (adds the `maestro` binary's transcode tier)
- **Estimated total:** ~53h autonomous agent work across 16 items
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7 as established by the epic §9 — this spec adds no
  GUI surface, no new egress, and no credential of its own. It extends the `native` backend
  behind the existing `PlaybackBackend` trait, so clauses 1 (Terminus-fronted), 2
  (capability-gated), and 5 (embeddable presentation) are satisfied unchanged by spec B/D.
- **Context:** Tier 4 of the epic §6 ladder — the only tier with genuinely hard problems, and the
  one epic §10.1 names as the overrun risk. This spec builds a **single-rendition, software-only,
  segment-on-demand HLS** transcoder. **The difficulty here is session management, not ffmpeg
  invocation.** Argument construction is a pure function that is essentially done once written;
  what bleeds time is seek-during-transcode, segment-number consistency, throttling, and process
  lifecycle. The item ordering below reflects that: the simplest end-to-end path (one rendition,
  no seek, no throttle) is Milestone A and is **independently shippable**, then seek layers on,
  then throttle. If this spec is cut short, it is cut after a milestone, never mid-milestone.

---

## 0. Scope boundaries — read before writing any code

### In scope
Software transcode (`libx264` + `aac`), a single rendition, HLS segment-on-demand delivery,
seek, throttling, session lifecycle, subtitles, and the failure/backpressure surface.

### Explicitly OUT OF SCOPE
1. **Hardware acceleration — spec F.** No `-hwaccel`, no VAAPI/QSV/NVENC/AMF, no
   `h264_vaapi`-family encoder names, no Chord GPU-lease call, not even behind a disabled flag.
   The argument builder in MTRX-01 takes an encoder *selection* as an input value, so spec F
   extends it by adding a variant — it must not need to restructure it. **A PR in this spec that
   names a hardware encoder is rejected on that basis alone.**
2. **HDR tone-mapping — epic §8.3, out of scope through spec E.** No `zscale`, no `tonemap`, no
   `libplacebo`. An HDR source that cannot direct-play to the target is a **planning-tier
   decision** (spec C) that this spec simply executes as an SDR-unaware transcode; correct
   HDR→SDR handling is a spec F follow-up at most.
3. **Multi-bitrate adaptive *ladders* — deferred. The multi-variant *URL shape* — in scope, now.**
   This spec generates **exactly one rendition**: one encoder, one media playlist, no second
   `#EXT-X-STREAM-INF` entry, no variant-switching logic, no per-variant segment numbering to keep
   aligned. That is the expensive part and it stays out — the epic §6 device matrix is closed and
   on a LAN, so the bandwidth adaptation a ladder buys is mostly moot, while a ladder multiplies
   every hard problem in this spec.
   **But the client-facing entry point is a master playlist referencing that one media playlist
   from day one** (MTRX-04). A master playlist with a single `#EXT-X-STREAM-INF` costs a dozen
   bytes and one function today; converting clients from a bare media-playlist URL to a master
   URL later touches the browser player, the Cast receiver, and every stored/shared stream link.
   This is the **one** ABR concession worth making, and it is a URL-shape decision, not a feature.
   **Follow-up for the ladder itself:** a `S13x-maestro-abr` spec, sized honestly, after this one
   is live. Track it as a MUSE issue at this spec's close-out; do not silently omit it.
4. **Live/linear transcode.** Muse's existing linear-channel tuner (`src/streaming/`) stays
   `-c copy`, per-program-process, and untouched — see §1.
5. **Trick-play / I-frame playlists, thumbnails, chapter markers.** Not now.

---

## 1. Relationship to Muse's existing linear streamer — share it, do not fork it

`src/streaming/ffmpeg.rs` and `src/streaming/mod.rs` are not merely "prior art in the
constellation" — **since the epic §2 correction they are modules in the same crate**, two
directories away from where this spec's code lands. That changes the obligation from *imitate the
shape* to *share the code*. The single worst outcome of this spec would be two ffmpeg argument
builders in one binary's source tree that drift apart, which is exactly the failure the epic §2b
reconciliation exists to prevent for probe and `plan()`.

**Who owns what, stated so a reviewer can enforce it:**

| Builder | Owns | Emits |
|---|---|---|
| `src/streaming/ffmpeg.rs` (existing, MUSE-29/MUSEL-C1) | The **linear-channel tuner** and the still-frame extractor | `-c copy` to `mpegts` on stdout; one process per scheduled program; single-frame MJPEG |
| `src/maestro/transcode/args.rs` (this spec, MTRX-01) | The **Maestro transcode tier** | `libx264`/`aac` encode variants into numbered segment files |

They are genuinely different outputs and neither should try to serve the other's caller — the
linear channel stays `-c copy`, per-program-process, and untouched (§0 item 4), for the reasons
its own doc comment gives at length.

**What they share, and the mechanism that keeps them from forking (MTRX-01):** the pieces that are
identical in both — the `-hide_banner -loglevel error -y -nostdin` preamble, the
before-`-i` input-seek emission with its never-seek-non-positive guard, and the millisecond→
`{:.3}` seconds formatting — move into `src/media/ffmpeg_args.rs`, the shared media core the epic
§2b establishes. `src/streaming/ffmpeg.rs` is refactored to call them; `src/maestro/transcode/args.rs`
calls the same helpers and adds the encode variants on top. **The existing `streaming::ffmpeg`
tests are not modified** — they are full-argv-equality tests, so if the refactor changes even one
byte of the linear channel's command line they fail, which makes them a free correctness proof for
the extraction. That unmodified-tests requirement is an acceptance criterion on MTRX-01, and it is
the whole anti-fork mechanism.

**The discipline that carries over verbatim.** `streaming/ffmpeg.rs` contains *only* pure
functions; nothing in it spawns a process, and its tests assert on the **exact expected argv
vector**, not on "contains `-ss`". That is why it is testable on a host where ffmpeg is not
installed at all — which, as of 2026-08-01, is the dev box (see §1d). MTRX-01 reproduces that
discipline exactly: `src/maestro/transcode/args.rs` is pure, `src/maestro/transcode/session.rs` is
the one impure caller.

**Carries over — input seek before `-i`.** For the same reason as the linear streamer, but with an
important difference in *why*. There, `-ss` before `-i` is correct because it is a keyframe-nearest
demuxer seek with no decode, which suits a stream-copy pipeline. Here we are re-encoding, and
ffmpeg's input `-ss` is **accurate** when the output is re-encoded (it decodes and discards from the
preceding keyframe rather than snapping the output start to it). So input seek gives us *both* the
speed of a demuxer seek *and* frame-accurate output start — which is precisely what segment
alignment (MTRX-09) requires. Do not "fix" this to an output seek.

**Does NOT carry over — the per-program-process, no-segmenter model.** `streaming/mod.rs` chains
separate ffmpeg processes' stdout into one HTTP body to sidestep the concat filter's decode
requirement. That reasoning is sound *for a `-c copy` linear channel with heterogeneous inputs* and
is exactly wrong here: we have one input file, we are already paying for a decode, and the client
needs addressable, individually-fetchable, individually-cacheable segments with stable numbers — not
one unseekable byte stream. So Maestro's transcode tier uses **one ffmpeg process per session
writing numbered segment files to a scratch directory**, with an HTTP layer that serves those files
as they land. The chained-stdout model cannot express a seek at all, which is the whole difficulty of
this spec.

### 1b. Runtime posture — what same-repo does and does not buy us

Three consequences of the epic §2 correction that this spec must actively respect:

1. **Isolation is a process boundary, and it is real.** `maestro.service` runs in its own cgroup
   with its own `MemoryMax` and `MemorySwapMax=0`. An over-budget transcode is therefore OOM-killed
   *inside its own scope* rather than triggering host-wide swap thrash — the same protection the
   build system's cgroup caps give Plex. So MTRX-07's concurrency cap is defence-in-depth for
   latency and disk, not the only thing standing between a transcode and the host.
2. **Disk is NOT isolated by that boundary.** The segment scratch is an ordinary filesystem shared
   with everything else on the host, and the cgroup does nothing to bound it. This fleet has a
   documented history of disk-full incidents bricking services and presenting as unrelated failures.
   That is why MTRX-02 builds the budget before the first byte is written and MTRX-08 enforces it,
   rather than either being a follow-up.
3. **Deploy is one image with two bins.** `oci-publish.sh muse moosenet/Muse main muse maestro`,
   with both installed all-or-nothing under a shared rollback. Nothing in this spec introduces a
   second module, a second updater config, or a second mirror. A transcode change therefore
   redeploys `muse` too — which is fine, and is the trade the epic made deliberately.

**Foundry coordination (epic §2b) — one live requirement lands in this spec.** Foundry's
verify-and-swap can replace a media file *while Maestro is streaming it*. Maestro's side of that
contract is to **hold an open file descriptor for the source for the life of a session**, so an
unavoidable swap degrades to "current viewers finish the old file" rather than mid-film corruption.
That is a concrete obligation on MTRX-05 and is written into it. Note also that Foundry's `Forge`
(permanent re-encode for curation) and this spec's transcode tier are **different consumers of the
same `src/media/` core**, not competing implementations: Forge decides what to re-encode on disk
forever, this spec decides what to encode on the fly for one session.

### 1c. Two things this spec consumes and must not re-invent

**Signed, session-scoped, expiring stream URLs — defined by spec D (epic §8.7).** `<video>` cannot
set an `Authorization` header on segment fetches and a Cast receiver holds no Terminus cookie, so
the media plane is authenticated by an HMAC token minted at session start, not by the control
plane's bearer. **Every URL this spec emits — the master playlist, the media playlist, each segment,
the fMP4 init segment, each WebVTT sidecar — carries spec D's token, using spec D's signer, with
spec D's expiry semantics.** This spec does not define a signing scheme, does not add a second
token, and does not exempt any route "because it's just a segment": a segment URL is a file-read
capability and is exactly the thing worth signing. The consequence for MTRX-04 is concrete and easy
to miss — a playlist's segment URIs are relative, so the token must be appended as a query string to
**each** URI at render time, which means the playlist generator takes the signer as an input rather
than being a pure function of duration alone.

**`plan()` and `PlaybackPlan` live in the shared `src/media/decision/`, not under `src/maestro/`
(epic §2b).** They are shared with Foundry, which consumes the same decision engine at curation
time. This spec's argument builder is a **consumer** of that type: it may read every field, and it
may not add one, change one's meaning, or grow a second planner. If MTRX-01 or MTRX-13 wants
information the plan does not carry, the correct move is an amendment to spec C — not a local
inference in the transcode tier. A tier decision made in two places is a tier decision that will
eventually disagree with itself, which is the same failure mode as a forked argument builder (§1).

### 1d. Where the gates run — ffmpeg is not on the dev box

**Verified 2026-08-01: neither `ffmpeg` nor `ffprobe` is installed on the dev box.** Two rules
follow, and both are already reflected in every item's TEST PLAN below:

1. **No unit test in this spec may invoke a real ffmpeg binary.** Every impure item tests against a
   **stub binary** (a tiny helper that writes fake segment files and exits with a scripted code),
   and every pure item tests argv vectors. This is the same constraint the existing MUSE-29 tests
   were written under and it is why they are structured the way they are. The benefit is that the
   suite is green anywhere, which is what makes it a usable merge gate.
2. **Anything that genuinely needs ffmpeg runs on a host that has it** — the Muse deploy host or
   <host> — via the compiler tool, never an ad-hoc build on a shared host. That covers MTRX-03's
   operator spike and each milestone's manual end-to-end verification against a real client. Do
   not "fix" a missing-ffmpeg failure on the dev box by installing ffmpeg there; the correct
   response is that the test should not have needed it.

---

## 2. The session model in one page

```
PlaybackPlan (spec C)  ──▶  TranscodeSession
                              ├─ session_id, account_id (spec D)
                              ├─ scratch_dir/            (MTRX-02)
                              │    ├─ session.json       (pid + pid-starttime + generation)
                              │    ├─ init.mp4           (fMP4 only, generation 0's, immutable)
                              │    └─ seg{NNNNN}.{m4s|ts}
                              ├─ generation: u32         (bumped on every respawn — MTRX-10)
                              ├─ window_start_seg: u64   (what generation N was spawned at)
                              ├─ playhead_seg: u64       (inferred from segment GETs — MTRX-11)
                              └─ ffmpeg child (own process group)
```

**Invariant SN-1 (segment numbering).** Segment `n` always covers source time
`[n*D, (n+1)*D)` where `D` is the configured nominal segment duration, **for the entire life of
the session, across every respawn.** A respawn never renumbers. This single invariant is what
makes seek safe; everything in MTRX-09/MTRX-10 exists to preserve it.

**Invariant SN-2 (one writer).** At most one ffmpeg process per session may write into the scratch
directory. A respawn kills-and-reaps the old process *before* the new one is spawned, and the new
process's segment writes are validated against its own `generation`. A file written by a
stale writer is the corruption mode this spec is most afraid of.

---

## 3. Items

### MTRX-01: Pure ffmpeg argument builder — `PlaybackPlan` → argv
- **Priority:** Critical
- **Labels:** maestro, transcode, ffmpeg, pure
- **Agent:** codex
- **Estimate:** 4h
- **Description:** The pure function at the bottom of the whole tier. Takes a `PlaybackPlan`
  (spec C's decision-engine output) plus a segment-output descriptor, returns the exact argument
  vector to pass to the ffmpeg binary. **Software encoders only** — `libx264` for video, `aac` for
  audio, plus `copy` for any stream the plan says passes through. Spawns nothing.

  ## FILES
  - `src/media/ffmpeg_args.rs` — new; the **shared** primitives extracted from `streaming::ffmpeg`
    (preamble, input-seek emission, seconds formatting) — see §1
  - `src/streaming/ffmpeg.rs` — refactor to call the shared primitives. **Behaviour-preserving:
    its existing tests must not be edited.**
  - `src/maestro/transcode/args.rs` — new; the pure transcode builder + its exhaustive argv tests
  - `src/maestro/transcode/mod.rs` — new module root; re-export
  - `src/config.rs` — add the encoder-tuning knobs listed under APPROACH (shared config, per epic §2)
  - `README.md` — document the transcode tier's config knobs

  ## APPROACH
  0. **First, extract — do not copy.** Move the preamble/input-seek/formatting helpers out of
     `src/streaming/ffmpeg.rs` into `src/media/ffmpeg_args.rs` and have the linear-channel builder
     call them. Run the existing `streaming::ffmpeg` tests **unmodified**; they are full-argv
     equality assertions, so a green run is proof the linear channel's command line is byte-for-byte
     unchanged. Only then write the new builder on top of the same helpers. Doing this in the
     opposite order — new builder first, "unify later" — is how the two fork, and same-repo makes
     the fork worse, not better, because both would ship in one binary.
  1. Define `SegmentOutput { container: SegmentContainer, dir: PathBuf, start_number: u64,
     segment_seconds: f64, ts_offset_seconds: f64 }` and
     `enum SegmentContainer { Fmp4, MpegTs }`.
  2. Define `enum VideoEncode { Copy, Software }` and
     `enum AudioEncode { Copy, Software { channels: u8, normalize: Option<Loudnorm> } }`.
     **`VideoEncode` is the extension point for spec F** — F adds a `Hardware { .. }` variant and
     nothing else in this file changes shape. Do not add it now.
  2b. **Audio normalisation slot — add the variant now, leave it off.** `Loudnorm { i: f64, tp: f64,
     lra: f64 }` (EBU R128 targets, defaulting to `I=-16, TP=-1.5, LRA=11`) renders as an
     `-af loudnorm=I=..:TP=..:LRA=..` on the audio branch. Default is `None` and this spec ships no
     way to turn it on — spec C decides when it is set, and a follow-up decides the policy (the
     obvious one being quiet-hours listening on a film with a 20 dB dynamic range). **The reason to
     add the field today is cost asymmetry, not eagerness:** `PlaybackPlan` is about to be frozen
     behind golden fixtures in spec C, so adding an `Option` field now is one line and one extra
     argv test, while adding it after the fixtures exist means regenerating every golden test in
     spec C and every argv test here. Note also that single-pass `loudnorm` is the only usable form
     in a streaming context — the two-pass measured form requires a full analysis pass over the file
     before encoding starts, which is incompatible with segment-on-demand. Say so in the source so
     nobody "upgrades" it to two-pass and adds a minutes-long startup delay.
  3. `pub fn build_transcode_args(plan: &PlaybackPlan, out: &SegmentOutput) -> Vec<String>`:
     - Preamble mirroring the Muse linear streamer: `-hide_banner -loglevel error -y`, plus
       `-nostdin` (this process's stdin is `Stdio::null()` and must never consume terminal input).
     - Input seek **before** `-i` when `out.start_number > 0`:
       `-ss {start_number * segment_seconds:.3}`. Never emit `-ss` for a non-positive value
       (defensive, exactly as the Muse builder does).
     - `-i {plan.source_path}`.
     - Stream selection from the plan: explicit `-map` for the chosen video/audio streams
       (never rely on ffmpeg's default stream picking — a plan that chose the commentary track must
       get the commentary track).
     - Video: `Copy` → `-c:v copy`. `Software` → `-c:v libx264 -preset {X264_PRESET}
       -crf {X264_CRF} -profile:v {plan.target_h264_profile} -level {plan.target_h264_level}
       -pix_fmt yuv420p`, plus `-maxrate`/`-bufsize` when the plan carries a bitrate ceiling, plus
       scaling `-vf scale=...` only when the plan asks for a resolution change.
     - **Keyframe forcing — load-bearing for alignment:**
       `-force_key_frames expr:gte(t,n_forced*{segment_seconds})`. `t` here is time in the *output*,
       which begins at the seek point, and the seek point is always an exact multiple of
       `segment_seconds` (MTRX-09), so forced keyframes land on global segment boundaries in every
       generation. Also emit `-sc_threshold 0` so scene-change detection cannot insert an
       unrequested keyframe that splits a segment.
     - Audio: `Copy` → `-c:a copy`. `Software` → `-c:a aac -b:a {AAC_BITRATE} -ac {channels}`.
     - `-output_ts_offset {ts_offset_seconds:.3}` — makes generation N's presentation timestamps
       continue the global timeline rather than restarting at zero.
     - Muxer, per container:
       - `MpegTs` → `-f segment -segment_time {D} -segment_start_number {N}
         -segment_format mpegts -segment_time_delta 0.05 -break_non_keyframes 0
         {dir}/seg%05d.ts`
       - `Fmp4` → `-f hls -hls_time {D} -hls_playlist_type vod -hls_segment_type fmp4
         -hls_fmp4_init_filename init.mp4 -hls_flags independent_segments
         -start_number {N} -hls_segment_filename {dir}/seg%05d.m4s {dir}/.ffmpeg-internal.m3u8`
         — note we let ffmpeg write a playlist we then **ignore**; MTRX-04 generates the playlist
         we actually serve. `-f segment` cannot emit an fMP4 init segment, which is why the two
         containers take different muxers. Document that asymmetry in the source.
  4. No `std::env::var` anywhere in this file; every tuning knob arrives as a parameter resolved
     from `config.rs` by the caller. Nothing here is secret-shaped, so no `SecretManager` use.

  ## TEST PLAN
  - `cargo test maestro::transcode::args` — full argv-vector equality tests, following the
    `src/streaming/ffmpeg.rs` house style (assert the whole `Vec<String>`, not `contains`).
  - Golden cases: full transcode fMP4 from zero; full transcode MPEG-TS from segment 250;
    video-copy + audio-transcode (the epic §6 tier-3 case); audio-copy + video-transcode.
  - `normalize: None` (the default) emits **no** `-af` at all; `normalize: Some(..)` emits exactly
    one single-pass `loudnorm` filter with the configured targets.
  - Ordering invariants: `-ss` strictly precedes `-i`; `-force_key_frames` present on every
    `VideoEncode::Software` case and absent on every `Copy` case.
  - Negative: `start_number == 0` emits no `-ss`; a negative/`NaN` offset emits no `-ss`.
  - `grep -rE 'vaapi|qsv|nvenc|amf|videotoolbox|hwaccel|tonemap|zscale' src/maestro/transcode/` returns
    nothing — the out-of-scope guard, as an actual test.
  - **`cargo test streaming::ffmpeg` passes with those tests unmodified** — the anti-fork proof
    that the shared extraction did not change the linear channel's command line.
  - `grep -c '\-hide_banner' src/` shows the literal in `src/media/ffmpeg_args.rs` only — there is
    exactly one place the preamble is constructed in the whole crate.
  - Verify no hardcoded IPs, hostnames, or org names in new/modified files.

  ## EDGE CASES
  - A source path containing spaces, quotes, or a leading `-` — arguments are a `Vec<String>`
    passed to `Command::args`, never a shell string; add a test with a `-`-leading filename.
  - A plan with no audio stream at all (silent film, or an audio-less rip) — emit `-an`, do not
    emit a dangling `-c:a`.
  - A plan whose `segment_seconds` is not an integer — the `{:.3}` formatting must be stable and
    locale-independent.
  - `segment_seconds <= 0` — reject at the type boundary (constructor returns `Result`), never
    emit a `-segment_time 0` that makes ffmpeg spin.

- **Acceptance criteria:**
  - [ ] Shared primitives live in `src/media/ffmpeg_args.rs` and are called by **both** builders
  - [ ] The existing `streaming::ffmpeg` tests pass **unedited** after the refactor (anti-fork proof)
  - [ ] `build_transcode_args` is pure — the module imports nothing from `std::process`
  - [ ] Golden argv tests pass for all four encode combinations, both containers
  - [ ] `-ss` precedes `-i` in every seeking case; absent in every non-seeking case
  - [ ] `-force_key_frames` + `-sc_threshold 0` present on every software-video-encode case
  - [ ] `AudioEncode::Software` carries an `Option<Loudnorm>` slot, defaulting to `None` and
        emitting no `-af` when unset (negative test)
  - [ ] The hardware/HDR grep guard test passes (no spec F or §8.3 vocabulary present)
  - [ ] README documents the new config knobs
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-02: Segment scratch store — layout, budget accounting, cleanup primitives
- **Priority:** Critical
- **Labels:** maestro, transcode, disk, ops
- **Agent:** codex
- **Estimate:** 3h
- **Description:** The on-disk substrate every later item stands on. A scratch root, one
  directory per session, a `session.json` sidecar carrying enough to identify an orphan safely,
  and pure accounting functions for the disk budget. **The fleet has a documented history of
  disk-full incidents brick-ing a host** — a transcoder writing unbounded segment files is exactly
  that failure waiting to happen, so the budget is built in the first disk-touching item, not
  retrofitted.

  ## FILES
  - `src/maestro/transcode/scratch.rs` — new; layout, sidecar, budget accounting
  - `src/config.rs` — `MAESTRO_SCRATCH_ROOT`, `MAESTRO_SCRATCH_BUDGET_MB`,
    `MAESTRO_SCRATCH_SESSION_BUDGET_MB`, `MAESTRO_SCRATCH_MIN_FREE_MB`
  - `README.md` — document the scratch root and its budget

  ## APPROACH
  1. Layout: `{scratch_root}/{session_id}/` with `session.json`, `init.mp4` (fMP4 only), and
     `seg{NNNNN}.{m4s|ts}`. `session_id` is a UUID — never anything derived from a file path
     (path traversal) and never a sequential integer (collides across restarts).
  2. `session.json` records `{ session_id, created_unix, container, segment_seconds, generation,
     ffmpeg_pid, ffmpeg_pid_starttime }`. The **pid-starttime** (field 22 of `/proc/{pid}/stat`)
     is what makes the MTRX-07 orphan sweep safe against PID reuse: a bare pid may by then belong
     to an unrelated process, and killing it would be a genuine fleet incident.
  3. Pure functions, unit-testable with no filesystem: `segment_filename(n, container)`,
     `parse_segment_filename(&str) -> Option<u64>` (fail-closed — anything not matching
     `seg\d{5,}\.(m4s|ts)` is `None`, never a partial parse), `budget_verdict(used_bytes,
     session_used_bytes, free_bytes, limits) -> BudgetVerdict::{Ok, ReapNeeded, Refuse}`.
  4. Impure helpers: `create_session_dir`, `remove_session_dir` (best-effort, logs and continues —
     a cleanup failure must never propagate into a playback error), `dir_usage_bytes`,
     `filesystem_free_bytes`.
  5. Every path is joined from the configured root; `session_id` is validated as a UUID before
     any join, so no caller-supplied string ever reaches a path component.
  6. **The scratch root is configurable and must NOT be placed on a removable-card-backed volume.**
     This is a specific, earned rule, not generic caution: this fleet has lost a physical volume
     from a card-backed scratch VG and then run half-missing and read-only for three days, with the
     symptom presenting as bogus compiler gates and `EIO` from `systemctl` rather than as a disk
     fault. A transcoder that writes continuously to such a volume would both accelerate its failure
     and make the resulting mess harder to attribute. Requirements: `MAESTRO_SCRATCH_ROOT` has **no
     default** (an unset value is a startup error, never a silent `/tmp`); the README states the
     card-backed prohibition and the "not the host's root filesystem" preference; and the
     operator-facing pre-flight (§6) records which volume was chosen and why.

  ## TEST PLAN
  - `cargo test maestro::transcode::scratch` — pure filename/parse/budget tests.
  - `parse_segment_filename` fail-closed cases: `seg1.ts`, `../etc/passwd`, `seg00001.m4s.tmp`,
    `seg00001`, `SEG00001.TS` all return `None`.
  - Budget: below both limits → `Ok`; over the session limit → `ReapNeeded`; under
    `MIN_FREE_MB` free → `Refuse` regardless of the other numbers (fail-closed on real disk
    pressure, which is the case that actually bricks a host).
  - Round-trip: `parse_segment_filename(segment_filename(n, c)) == Some(n)` for n in
    `{0, 1, 99999, 100000, u32::MAX as u64}` — note the format must not wrap at 5 digits.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - Scratch root does not exist or is not writable at startup — fail **loudly at startup**, not
    lazily on the first playback. A misconfigured scratch root that only surfaces mid-film is the
    worst version of this bug.
  - Scratch root on a tmpfs — legal and fast, but the budget must then be *tighter* than the disk
    budget because it is RAM. Document; do not special-case in code.
  - `session_id` that parses as a UUID but names a directory that already exists — treat as a
    collision, refuse, log.
  - Segment index exceeding 5 digits (a >6-hour source at 6s segments is ~3600 segments, so this
    is headroom, not a live risk) — `%05d` widens naturally; assert the round-trip test above.

- **Acceptance criteria:**
  - [ ] Segment filename round-trip holds across the tested index range
  - [ ] `parse_segment_filename` rejects every traversal/partial-parse case (negative test)
  - [ ] `budget_verdict` returns `Refuse` on low free space regardless of other inputs
  - [ ] `session.json` carries pid **and** pid-starttime
  - [ ] Unwritable/absent/unset scratch root fails at startup with a clear message (no default)
  - [ ] README states the scratch root must not sit on a removable-card-backed volume
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-03: Verify CMAF/fMP4-in-HLS against the real Cast devices we own
- **Priority:** Critical
- **Labels:** maestro, transcode, cast, spike, human-action
- **Agent:** <operator>
- **Estimate:** 2h
- **Type:** human-action
- **Description:** **Confirmed by architectural review as an explicit early spike: do this first.**
  One afternoon against the household's real Chromecast, written down, with MPEG-TS HLS as the
  guaranteed fallback. It is cheap, it is a hardware fact no amount of reasoning
  substitutes for, and it decides a default the rest of the spec is built around. Google's Cast
  supported-media documentation confirms HLS, MP2T, and MP4, but does **not** explicitly confirm
  **fMP4/CMAF segments inside an HLS playlist**. fMP4 is the better container (one init segment,
  smaller per-segment overhead, and the same segments would later serve DASH). MPEG-TS is the
  guaranteed-supported fallback and is the format Muse already produces for its linear tuner, so
  that path is familiar ground. Rather than pick on documentation alone or build both and discover
  the answer in month two, spend two hours proving it on the actual devices in the household.

  ## Steps
  1. Produce two static test outputs from one short source with a hand-run ffmpeg (this is an
     operator spike on an operator workstation, not agent code, and not on a shared build host):
     one fMP4/CMAF HLS set (init + `.m4s` + playlist), one MPEG-TS HLS set (`.ts` + playlist).
  2. Serve both from a plain static HTTP server on the LAN.
  3. Cast each to **every Cast-capable device the household actually owns** — record model, and
     for each: does it start, does it seek, does it play to the end without stall.
  4. Record the result in the MTRX-03 Plane item as a table (device → fMP4 verdict → TS verdict).
  5. State the resulting default in the item: `Fmp4` if every device passes, `MpegTs` otherwise.

  ## Why it is a blocker for the default, not for the build
  MTRX-01 and MTRX-04 implement **both** containers regardless of the outcome — the config knob
  `MAESTRO_SEGMENT_CONTAINER` exists either way, and MPEG-TS remains a supported, tested,
  first-class path forever. What this item decides is only which value ships as the default and
  which path gets the deeper soak testing. So the build never waits on it; only the default does.

- **Acceptance criteria:**
  - [ ] Every Cast-capable device in the household tested against both containers
  - [ ] Start / seek / play-to-end recorded per device per container
  - [ ] A default container decided and written into the Plane item with the evidence
  - [ ] The decision is reflected in `MAESTRO_SEGMENT_CONTAINER`'s documented default

---

### MTRX-04: HLS playlist generation — master + media, from known duration
- **Priority:** Critical
- **Labels:** maestro, transcode, hls, pure
- **Agent:** codex
- **Estimate:** 5h
- **Blocked by:** MTRX-03 (default container selection only — both containers are implemented
  regardless, so implementation may start immediately)
- **Description:** The trick that makes segment-on-demand work: because spec A's probe gives us
  the **exact source duration up front**, we can emit the **complete, final media playlist before a
  single segment exists**. The client sees a normal VOD playlist listing every segment; segments
  materialize as it requests them. This is what avoids the live-playlist dance (rolling windows,
  `#EXT-X-MEDIA-SEQUENCE` churn, reload timing) entirely — and it is what lets a client seek to
  minute 90 immediately, because minute 90 is already in the playlist it was handed.

  ## FILES
  - `src/maestro/transcode/playlist.rs` — new; the pure generator
  - `src/maestro/transcode/mod.rs` — re-export

  ## APPROACH
  0. **Two playlists, one rendition (see §0.3).** `pub fn master_playlist(...) -> String` emits
     `#EXTM3U`, `#EXT-X-VERSION`, the subtitle `#EXT-X-MEDIA` group (MTRX-12), and exactly **one**
     `#EXT-X-STREAM-INF:BANDWIDTH=..,AVERAGE-BANDWIDTH=..,CODECS="..",RESOLUTION=..` followed by the
     media playlist URI. `BANDWIDTH` comes from the plan's target bitrate (or a computed estimate);
     `CODECS` must be a real RFC 6381 string (`avc1.<profile><constraints><level>,mp4a.40.2`) derived
     from the plan's H.264 profile/level — a wrong or absent `CODECS` is a common cause of a Cast
     device refusing to start, so derive it, do not hardcode a guess. **`master.m3u8` is the URL
     handed to every client**; nothing outside this module ever links a media playlist directly.
     Adding a second `#EXT-X-STREAM-INF` later is then a one-function change with no client impact.
  1. `pub fn media_playlist(duration_seconds: f64, segment_seconds: f64, container:
     SegmentContainer, session_id: Uuid, signer: &StreamUrlSigner) -> String` — deterministic given
     its inputs, no I/O, no clock read of its own (the signer supplies expiry, so a test injects a
     fixed one and the output stays golden-testable).
  2. Segment count is `ceil(duration / segment_seconds)`; the final segment's `#EXTINF` is the
     **remainder**, not the nominal duration. Getting this wrong makes players either cut the last
     seconds off or hang waiting for a segment that will never be that long.
  3. Emit: `#EXTM3U`, `#EXT-X-VERSION:7` (6 for MPEG-TS — version 7 is what fMP4 requires),
     `#EXT-X-TARGETDURATION:{ceil(segment_seconds)}`, `#EXT-X-MEDIA-SEQUENCE:0`,
     `#EXT-X-PLAYLIST-TYPE:VOD`, `#EXT-X-INDEPENDENT-SEGMENTS`, and for fMP4 an
     `#EXT-X-MAP:URI="init.mp4"` line before the first segment. Terminate with `#EXT-X-ENDLIST` —
     the playlist is final on first emission and never re-fetched.
  4. Segment URIs are **relative** (`seg00000.m4s?t=<token>`), so the playlist stays correct
     regardless of the external path prefix. This is not a stylistic choice: an absolute URI would
     bake a host into a document the client resolves, and S1 forbids that host appearing anywhere
     anyway.
  4b. **Every URI carries spec D's signed session token** (§1c) as a query parameter — the master
     playlist's media-playlist URI, every segment URI, the `#EXT-X-MAP` init URI, and every
     subtitle URI. The media plane is not behind the control plane's bearer, so an unsigned segment
     URI is an unauthenticated file read. Two consequences worth stating because they are the
     easy mistakes: the signer is an *input* to the generator (which is why `media_playlist` is not
     a pure function of duration alone), and **the token's expiry must exceed the playback
     duration** — a 30-minute token on a 2-hour film hands the client a playlist whose tail
     segments 403 ninety minutes in. Take the required lifetime from the plan's duration plus a
     margin, and assert it: a token expiring before `duration_seconds` is a hard error at
     generation time, not a mystery mid-film failure.
  5. Golden-file tests: check a handful of rendered playlists into `tests/fixtures/playlist/` and
     assert byte equality. A playlist is a wire format; treat a diff as a breaking change.

  ## TEST PLAN
  - `cargo test maestro::transcode::playlist` — golden-file equality (with a fixed injected signer)
    for: exact-multiple duration, non-multiple duration (remainder `#EXTINF`), sub-one-segment
    duration, both containers, and the master playlist.
  - Master playlist has exactly **one** `#EXT-X-STREAM-INF`, and its `CODECS` string matches the
    plan's H.264 profile/level (table-driven across the profiles spec C can emit).
  - **Every URI in both playlists carries a token** — assert by parsing the rendered playlists and
    checking that no URI line lacks the query parameter (negative test; this is the one that
    catches a newly-added URI kind silently shipping unsigned).
  - A signer whose expiry is earlier than `duration_seconds` → hard error, no playlist produced.
  - `#EXT-X-MAP` present iff `container == Fmp4`.
  - Sum of all `#EXTINF` values equals `duration_seconds` within 1ms.
  - Segment count matches `ceil(duration / segment_seconds)` across a property-style sweep of
    durations and segment lengths.
  - Negative: `duration_seconds <= 0` returns an error, not an empty-but-valid playlist.
  - Verify no hardcoded IPs, hostnames, or URLs in new/modified files.

  ## EDGE CASES
  - Duration shorter than one segment — a one-entry playlist whose `#EXTINF` is the duration.
  - Duration that is an exact multiple — must **not** emit a trailing zero-length segment.
  - A probe duration that disagrees with reality (VFR source, broken container header) — the
    playlist is authoritative for the client and ffmpeg is authoritative for the bytes; if ffmpeg
    ends early, the tail segments are handled by the MTRX-14 "segment that will never exist" path.
    Document this as the known, bounded consequence of trusting the probe.
  - `duration` as `NaN`/infinite from a malformed probe — reject at the boundary.

- **Acceptance criteria:**
  - [ ] A **master** playlist referencing one media playlist is the client-facing entry point
  - [ ] `CODECS` is derived from the plan, not hardcoded
  - [ ] Golden playlist fixtures match byte-for-byte for all tested shapes
  - [ ] Final-segment `#EXTINF` is the remainder, and `#EXT-X-ENDLIST` is always present
  - [ ] `#EXT-X-MAP` emitted iff fMP4; `#EXT-X-VERSION` correct per container
  - [ ] Every emitted URI is relative **and** signed with spec D's session token (negative test)
  - [ ] A token expiring before the media duration is rejected at generation time
  - [ ] Non-positive / non-finite duration returns an error (negative test)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-05: Single-generation transcode session — spawn, supervise, track segments
- **Priority:** Critical
- **Labels:** maestro, transcode, session, process
- **Agent:** claude
- **Estimate:** 6h
- **Blocked by:** MTRX-01, MTRX-02, MTRX-04
- **Description:** The **first impure item** and the heart of Milestone A. One session, one ffmpeg
  process, spawned at segment 0, transcoding the file linearly to completion. **No seek, no
  throttle** — those are MTRX-10 and MTRX-11 and adding either here is how this spec overruns. The
  goal is a session that can be created, observed, and destroyed correctly, with an ffmpeg process
  whose lifecycle is genuinely owned.

  ## FILES
  - `src/maestro/transcode/session.rs` — new; `TranscodeSession`, spawn, supervise, teardown
  - `src/maestro/transcode/manager.rs` — new; `SessionManager` (the registry; concurrency cap is MTRX-07)
  - `src/config.rs` — `MAESTRO_FFMPEG_BIN`, `MAESTRO_SEGMENT_SECONDS`,
    `MAESTRO_SEGMENT_CONTAINER`

  ## APPROACH
  1. `SessionManager` owns `HashMap<Uuid, Arc<TranscodeSession>>` behind an async lock, created
     from `AppState` at startup. Sessions are in-memory; the DB (spec D) owns the *playback*
     session, this owns the *transcode* session — they are related by id, not merged.
  2. `TranscodeSession::start(plan, scratch, config)`: create the scratch dir → build argv via
     `build_transcode_args` → spawn.
  3. **Spawn discipline** (this is the part that is easy to get subtly wrong):
     - `stdin(Stdio::null())`, `stdout(Stdio::null())`, `stderr(Stdio::piped())` — ffmpeg writes
       nothing to stdout in segment mode; stderr is captured for diagnosis.
     - `process_group(0)` — the child leads its own process group, so teardown signals the
       **group**, not just the pid. ffmpeg spawns no children today, but a signal to a group is
       the correct default and costs nothing.
     - `kill_on_drop(true)` — a dropped session never leaks a process, matching the existing Muse
       `streaming::spawn_ffmpeg` posture.
     - Immediately persist pid + pid-starttime into `session.json` (MTRX-02) **before** returning,
       so a crash between spawn and registration still leaves an orphan the MTRX-07 sweep can find.
  4. A supervisor task per session: reads stderr into a bounded ring buffer (last ~64 KiB — ffmpeg
     stderr is unbounded and must never be an OOM vector), `wait()`s the child, and on exit records
     `SessionOutcome::{Completed, Failed{code, tail}, Killed}`. The `wait()` is what prevents
     zombies; it must run even on the kill path.
  5. `classify_spawn_error` — reuse the exact shape of Muse's
     `streaming::ffmpeg::classify_spawn_error`: `NotFound` → the ffmpeg binary is absent on this
     host, which is a deployment gap and a `501 Not Implemented`, distinct from any other spawn
     failure which is an honest `503`. Do not collapse these.
  6. Teardown `stop()`: signal the group, then `wait()` with a grace period, then `SIGKILL`,
     then `wait()` again, then remove the scratch dir. Never `remove_dir_all` before the process is
     confirmed reaped — that is how you get a process writing into a recreated directory.
  7. `segment_exists(n)` / `highest_contiguous_segment()` by scanning the scratch dir, with a
     short-TTL in-memory cache so a segment poll loop does not `readdir` at request rate.
  8. **Hold an open fd on the source for the life of the session** (epic §2b, Foundry coordination).
     Foundry's verify-and-swap can replace the underlying file mid-playback; on Linux an open fd
     keeps the original inode alive, so an unavoidable swap degrades to "current viewers finish the
     old file" instead of a mid-film corruption. Open it in `start()`, keep it on the session, and
     — importantly — **carry the same fd across a MTRX-10 respawn** rather than re-opening by path,
     since re-opening is exactly when the swapped-in file would be picked up. Pass the descriptor to
     ffmpeg as `/dev/fd/{n}` with the fd marked inheritable, keeping the argv path stable and
     testable. Where `/dev/fd` is unavailable, fall back to the path and log that the swap-safety
     guarantee is degraded — do not fail playback over it.

  ## TEST PLAN
  - `cargo test maestro::transcode::session` — the session state machine with a **stub
    binary** (a tiny shell/rust helper that writes N fake segment files then exits), never real
    ffmpeg. Per the constellation gate rule, ffmpeg may be entirely absent on the gate host, so
    no test may invoke it.
  - Stub cases: exits 0 after writing segments → `Completed`; exits non-zero → `Failed` with the
    stderr tail captured; killed mid-run → `Killed` and no zombie (assert the child is reaped).
  - `classify_spawn_error` distinguishes `NotFound` (→501) from `PermissionDenied` (→503).
  - Teardown removes the scratch dir and leaves no `session.json`.
  - Stderr ring buffer is bounded — feed the stub 10 MB of stderr, assert retained bytes ≤ cap.
  - **Foundry swap safety:** with a session open on a temp source file, `rename(2)` a different
    file over that path → the session's held fd still reads the original content, and the argv
    handed to the stub is unchanged. This is the epic §2b contract as an executable test.
  - Verify no hardcoded IPs or org names in new/modified files.
  - Verify no `std::env::var` for anything token/key/password-shaped (S7 grep).

  ## EDGE CASES
  - ffmpeg binary missing entirely → 501, session never registered, scratch dir cleaned up.
  - ffmpeg exits 0 having written **zero** segments (unreadable/zero-length source) → `Failed`,
    not `Completed`; a "successful" transcode with no output is a failure.
  - The source file disappears mid-transcode (library remount, QNAP hiccup) → ffmpeg dies; the
    supervisor records it; MTRX-14 owns what the client sees.
  - Scratch dir removed out from under a running session by an operator → segment reads 404; the
    supervisor must not panic.
  - Two `start()` calls racing for the same session id → the manager's lock makes the second a
    clean collision error, not two writers (invariant SN-2).

- **Acceptance criteria:**
  - [ ] A session spawns, writes segments (stub), and is observable via `highest_contiguous_segment`
  - [ ] Teardown leaves no zombie process and no scratch directory
  - [ ] Missing ffmpeg binary → 501-shaped error; other spawn failures → 503-shaped
  - [ ] Zero-segment "success" is classified `Failed` (negative test)
  - [ ] Stderr capture is bounded
  - [ ] The source fd is opened once and held for the session's life, surviving a respawn
        (Foundry swap-safety, epic §2b)
  - [ ] No test invokes the real ffmpeg binary
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-06: Segment + playlist HTTP endpoints — serve segments as they land
- **Priority:** Critical
- **Labels:** maestro, transcode, http, api
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MTRX-05
- **Description:** **Milestone A closes here.** Three routes turn the session into playable HLS:
  create a session, fetch the playlist, fetch a segment. The interesting case is the one the
  scope note calls out explicitly: **the client fetches the playlist before any segment exists**,
  then immediately fetches segment 0 while ffmpeg is still starting up. That request must wait, not
  404 — but it must wait *boundedly*, because an unbounded wait is a connection-exhaustion bug.

  ## FILES
  - `src/maestro/http/transcode.rs` — new; the three handlers
  - `src/maestro/http/mod.rs` — route registration
  - `README.md` — document the transcode endpoints

  ## APPROACH
  1. Routes. **The control plane and the media plane authenticate differently and this split is
     deliberate** (epic §8.6/§8.7): session creation is control (bearer, via `proxy_maestro`);
     playlist and segment fetches are media, served direct from Maestro and authenticated by spec
     D's signed session token, because `<video>` cannot set a header and a Cast receiver holds no
     cookie.
     - `POST /transcode/sessions` — **control plane**, bearer auth. Body carries the
       `PlaybackPlan` reference (spec C/D shape); returns `{ session_id, master_url }` where
       `master_url` is the signed `master.m3u8`. **Returns as soon as the process is spawned**, not
       when the first segment lands — the client's next request is a playlist, which needs no
       segments at all.
     - `GET /transcode/sessions/{id}/master.m3u8` — **media plane**, signed. The client entry point.
     - `GET /transcode/sessions/{id}/index.m3u8` — media plane, signed. The MTRX-04 media playlist,
       served from the known duration so it is correct on the very first call. `Content-Type:
       application/vnd.apple.mpegurl`, `Cache-Control: no-store`.
     - `GET /transcode/sessions/{id}/{filename}` — media plane, signed. `init.mp4` or
       `seg{NNNNN}.{m4s|ts}`, resolved through `parse_segment_filename` (MTRX-02) so nothing
       caller-supplied touches a path join.
  1b. **Signature verification is a shared extractor**, applied to every media-plane route by
     construction rather than per-handler — a route added later must not be able to forget it.
     Verify the token binds to **this** `session_id` (a valid token for another session is a
     rejection, not a pass) and is unexpired. Invalid/expired → `403`, and the response body must
     not distinguish "bad signature" from "unknown session".
  2. **Await-until-lands**, the core of this item:
     - If the segment file exists **and** the writer has moved past it (i.e. segment `n+1` exists,
       or the session has exited), serve it. A segment file that exists but is still being written
       is a **partial file** — serving it produces a truncated segment and a stall that looks like
       a codec bug. The `n+1`-exists rule is the cheap, correct completeness test for a sequential
       segment muxer.
     - Otherwise, wait on a per-session notify with a timeout of `MAESTRO_SEGMENT_WAIT_SECS`
       (default 30). On timeout → `503` with `Retry-After: 1`.
     - If the session has exited and the segment still does not exist → `404` (it is never coming).
  3. Serve with `Content-Length` and an explicit content type (`video/iso.segment` for `.m4s`,
     `video/mp2t` for `.ts`, `video/mp4` for `init.mp4`). Range requests on a segment are
     unnecessary but harmless — support them if axum gives it free, do not build it.
  4. Every segment GET records `playhead_seg = n` on the session — this is the signal MTRX-11's
     throttle and MTRX-07's idle timeout both consume. **Wire the recording now** even though
     nothing reads it yet; it is one line here and a refactor later.
  5. Errors are the existing Maestro error type → HTTP mapping, never ad-hoc responses.

  ## TEST PLAN
  - `cargo test maestro::http::transcode` — handler tests against a stub-driven session.
  - Playlist requested with zero segments on disk → `200` and a complete playlist (the
    initial-playlist-before-any-segment case, asserted explicitly).
  - Segment requested before it exists, then the stub writes it → the request completes with the
    bytes rather than 404.
  - Segment requested that never arrives, session still alive → `503` after the configured wait,
    not a hung connection.
  - Segment requested that never arrives, session exited → `404`.
  - Partial-file guard: segment `n` exists, `n+1` does not, session alive → the request **waits**
    rather than serving a partial (negative test — the one most likely to regress).
  - Path traversal: `GET .../%2e%2e%2fetc%2fpasswd` and `../../session.json` → `400`, never a
    filesystem read.
  - **Auth matrix, table-driven across every media-plane route:** no token → `403`; expired token
    → `403`; a valid token minted for a *different* session → `403` (negative test — the
    cross-session case a naive "is the signature valid" check passes); correct token → `200`.
  - The signed routes reject a control-plane bearer alone, and `POST /transcode/sessions` rejects a
    stream token alone — the two planes do not substitute for each other.
  - Verify no hardcoded IPs, hostnames, or URLs in new/modified files.

  ## EDGE CASES
  - Unknown `session_id` → `404`, and the response must not distinguish "never existed" from
    "reaped" in a way that leaks session ids.
  - `session.json` requested through the segment route → rejected by `parse_segment_filename`.
  - Client requests a segment far beyond the current window — under Milestone A this simply waits
    until the linear transcode reaches it (correct, if slow); MTRX-10 turns this into a seek.
  - Two clients requesting the same not-yet-written segment — both wait on the same notify; the
    notify must be broadcast, not single-consumer.
  - HEAD on a segment — answer from existence, do not read the body.

- **Acceptance criteria:**
  - [ ] `master.m3u8` is the returned entry point and both playlists serve correctly
  - [ ] Every media-plane route verifies the signed session token via a shared extractor
  - [ ] A token valid for another session is rejected (negative test)
  - [ ] Playlist is served correctly with zero segments on disk
  - [ ] A pending segment request completes when the segment lands
  - [ ] A pending segment request times out as `503` + `Retry-After`, never hangs
  - [ ] A segment that can never exist returns `404`
  - [ ] Partial (still-being-written) segments are never served (negative test)
  - [ ] Path traversal attempts are rejected before any filesystem access
  - [ ] README documents the three endpoints
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

> **★ MILESTONE A — independently shippable.** With MTRX-01..06 merged, Maestro can transcode a
> file that cannot direct-play and serve it as HLS to a real client, start to finish. No seek, no
> throttle, no subtitles. This is a genuinely useful capability and a legitimate stopping point if
> the sprint is cut. **Do not begin MTRX-09 until MTRX-06 is merged and verified against a real
> client.**

---

### MTRX-07: Session reaping, orphan sweep, concurrency cap, kill semantics
- **Priority:** Critical
- **Labels:** maestro, transcode, lifecycle, ops
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MTRX-05
- **Description:** The lifecycle discipline that keeps a transcoder from becoming a fleet
  liability. Four related mechanisms: idle reaping, startup orphan sweep, a hard concurrency cap,
  and kill semantics that leave nothing behind. This is deliberately sequenced **immediately after
  Milestone A and before seek**, because seek multiplies process churn and doing it on an
  unsupervised process model would be building on sand.

  ## FILES
  - `src/maestro/transcode/reaper.rs` — new; the periodic reaper + the startup sweep
  - `src/maestro/transcode/manager.rs` — concurrency cap, admission
  - `src/maestro/transcode/session.rs` — kill semantics
  - `src/config.rs` — `MAESTRO_SESSION_IDLE_SECS`, `MAESTRO_MAX_TRANSCODES`, `MAESTRO_MAX_SESSIONS`,
    `MAESTRO_KILL_GRACE_SECS`

  ## APPROACH
  1. **Idle reaping.** A session whose last segment GET is older than
     `MAESTRO_SESSION_IDLE_SECS` (default 300) is reaped: stop the process, remove the scratch
     dir, drop the registry entry. A periodic task ticks at `idle_secs / 5`, bounded. This is the
     single mechanism that handles "the client closed the tab", "the Cast device was unplugged",
     and "the browser crashed" — all three present identically as *segment requests stopped*, so
     none of them needs its own detection.
  2. **Startup orphan sweep — two passes, because there are two kinds of orphan.** A crashed
     Maestro must not leave a transcoder melting a core for the remaining two hours of a film, and
     it must not leave scratch directories accumulating until the disk fills.
     - **Pass 1 — orphan directories.** On boot the registry is empty by definition, so **every**
       session directory under the scratch root is an orphan. For each: read `session.json`; if it
       names a live pid **whose `/proc/{pid}` start-time matches the recorded one**, signal that
       process group; then remove the directory. **The start-time check is mandatory** — a bare pid
       from a previous boot may now belong to an unrelated fleet process, and killing it would be a
       real incident. A directory with a missing or unparseable `session.json` is removed without
       signalling anything.
     - **Pass 2 — orphan processes.** Pass 1 finds nothing if the scratch directory was already
       removed but the process survived (a partial cleanup, an operator `rm -rf`, a crash between
       the two). So also enumerate running processes whose executable is the configured ffmpeg
       binary **and** whose argv references the configured scratch root, and kill any whose
       session id is not in the store. Scoping the match to *our* scratch root is what makes this
       safe: it cannot match Muse's linear-channel ffmpeg processes (which write to stdout, not to
       the scratch root) or anything else on the host. Never kill by binary name alone.
  3. **Hard caps, in config from day one, failing closed at the API.**
     - `MAESTRO_MAX_TRANSCODES` (default 2, tuned to the host's cores, not aspirational) bounds
       concurrent **transcode** sessions — the CPU-holding kind this spec creates.
     - `MAESTRO_MAX_SESSIONS` is the overall session cap spec D owns; this spec **respects** it and
       does not redefine it. A transcode admission checks both.
     - Admission is checked in the manager under the same lock that registers the session, so a cap
       cannot be raced past.
     - Over either cap → a **typed** error (`TranscodeError::CapacityExceeded { limit, kind }`)
       mapped to `503` with a machine-readable reason the GUI renders as **"server busy"**. This is
       the point of typing it: the failure a user should see is a polite "too many streams right
       now", not a wedged request, not a stack trace, and above all not an OOM — the caps exist so
       the box degrades by refusing work rather than by falling over.
     - **Never queue.** A queued transcode is a playback attempt the user is watching a spinner
       for; failing fast is the honest answer.
  4. **Kill semantics** — the ordering matters and each step exists for a reason:
     - `SIGCONT` **first**. A session paused by MTRX-11's throttle is `SIGSTOP`ped, and a stopped
       process never handles `SIGTERM` — it would sit un-reaped until `SIGKILL`. Sending `SIGCONT`
       unconditionally before any teardown makes the throttle and the reaper compose. This is the
       single most likely lifecycle bug in the whole spec and it costs one line.
     - `SIGTERM` to the **process group**, then `wait()` with `MAESTRO_KILL_GRACE_SECS` (default 5).
     - On timeout, `SIGKILL` to the group, then `wait()` again — unconditionally, so no zombie.
     - Only **after** the child is confirmed reaped, remove the scratch directory.
  5. Expose reaper counters (`sessions_reaped_idle`, `orphans_swept`, `admissions_refused`) for
     MTRX-15.

  ## TEST PLAN
  - `cargo test maestro::transcode::reaper` — with stub children.
  - Idle reaping: a session with no segment GETs past the threshold is reaped; one with recent
    GETs is not.
  - Orphan sweep: a fabricated scratch dir with a `session.json` naming (a) a dead pid, (b) a live
    pid with a **mismatched** start-time, (c) a live pid with a matching start-time → assert only
    (c) is signalled, and all three directories are removed. **(b) is the load-bearing negative
    test** — it proves we cannot kill an innocent process.
  - Orphan **process** sweep: a stub process whose argv references the scratch root and whose
    session is unknown is killed; an otherwise-identical stub whose argv references a *different*
    root is left alone (negative test — the guard against killing the linear-channel streamer).
  - Concurrency cap: the (cap+1)-th concurrent start is refused with
    `CapacityExceeded` → `503`; concurrent starts racing the cap never exceed it; the error
    serializes with a reason the GUI can render.
  - Kill semantics: a `SIGSTOP`ped stub is still torn down within the grace window (proves the
    `SIGCONT`-first ordering); a stub that ignores `SIGTERM` is `SIGKILL`ed and reaped.
  - Assert no orphaned child processes remain after the test module completes.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - `/proc` unreadable or absent (a non-Linux dev machine) — the sweep degrades to
    "remove the directory, signal nothing", never to "kill by bare pid".
  - Reaping a session while a segment request is in-flight on it — the in-flight request resolves
    to `404`/`410` (MTRX-14), never a panic on a dropped `Arc`.
  - Clock jump (NTP step) making every session look idle — use a monotonic instant for idle
    accounting, never wall-clock.
  - Scratch root shared with another Maestro instance (misconfiguration) — the sweep would delete
    the other instance's live sessions. Guard with an instance lock file in the scratch root and
    refuse to start on a conflicting live lock.

- **Acceptance criteria:**
  - [ ] Idle sessions are reaped; active sessions are not
  - [ ] Orphan sweep never signals a pid whose start-time does not match (negative test)
  - [ ] Orphan **process** sweep kills scratch-root-scoped strays only (negative test)
  - [ ] `MAESTRO_MAX_TRANSCODES` and `MAESTRO_MAX_SESSIONS` both enforced; over-cap returns a typed
        `CapacityExceeded` → `503` "server busy", never queues, never OOMs
  - [ ] A `SIGSTOP`ped session is still torn down within the grace window
  - [ ] No zombies and no leaked scratch directories after any teardown path
  - [ ] A conflicting scratch-root lock refuses startup
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-08: Disk hygiene — scratch budget enforcement and eviction
- **Priority:** High
- **Labels:** maestro, transcode, disk, ops
- **Agent:** codex
- **Estimate:** 3h
- **Blocked by:** MTRX-02, MTRX-07
- **Description:** Wires MTRX-02's budget accounting into live enforcement. **Justification is
  concrete, not theoretical:** the fleet has a documented history of disk-full incidents that
  presented as unrelated failures (bogus compiler gates, `EIO` from `systemctl`, a bricked
  gateway) and cost real diagnosis time. A transcoder is the highest-volume writer we would ever
  deploy, so it gets a hard budget from the start and a refusal path that is louder than the
  failure it prevents.

  ## FILES
  - `src/maestro/transcode/scratch.rs` — eviction
  - `src/maestro/transcode/reaper.rs` — budget check on each tick
  - `src/maestro/transcode/manager.rs` — pre-admission free-space check

  ## APPROACH
  1. **Pre-admission:** before spawning, check `filesystem_free_bytes` against
     `MAESTRO_SCRATCH_MIN_FREE_MB`. Below it → refuse the session with a `503` naming disk
     pressure. Refusing playback is strictly better than filling the host's root filesystem.
  2. **Per-session cap:** a session's scratch usage above `MAESTRO_SCRATCH_SESSION_BUDGET_MB`
     triggers **behind-the-playhead eviction** — delete completed segments with index
     `< playhead_seg - MAESTRO_SEGMENT_KEEP_BEHIND` (default 10). Those segments have been played
     and, if re-requested, are regenerable by the MTRX-10 seek path, so deleting them is cheap.
     Ahead-of-playhead segments are **never** evicted; they are the work the throttle deliberately
     did.
  3. **Global cap:** total scratch above `MAESTRO_SCRATCH_BUDGET_MB` → evict behind-playhead
     across all sessions oldest-first; if still over, reap the least-recently-active **idle**
     session outright. **Never reap a session with a recent segment GET to make room** — that
     kills someone's film to make room for someone else's, and failing the new request is the
     correct trade.
  4. **The behaviour at quota, stated as a rule rather than left to fall out of the code:
     refuse new transcode sessions, never wedge live ones.** In order — evict behind-playhead
     segments; then reap idle sessions; then refuse admission with the MTRX-07 `CapacityExceeded`
     path (reason: disk). A live session with a recent segment GET is never starved, never paused,
     and never reaped to make room. The asymmetry is deliberate: a refused new stream is a user
     seeing "server busy" and trying again, while a wedged live stream is a film stopping in the
     middle, and the second is much worse than the first.
  5. Log every eviction and every refusal at `warn` with the numbers. A silent eviction that later
     causes a stall is unmaintainable.

  ## TEST PLAN
  - `cargo test maestro::transcode::scratch::budget` — against a temp dir with fabricated
    segment files.
  - Over the session cap → only segments below `playhead - keep_behind` are removed; ahead-of-
    playhead files survive (negative test).
  - Over the global cap with one idle and one active session → the idle one is reaped, the active
    one is untouched (negative test — the most important one here).
  - Below `MIN_FREE_MB` → new session admission refused with the disk-pressure error.
  - Eviction never removes `init.mp4` — the fMP4 init segment is required for the whole session
    and is not a numbered segment.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - `statvfs` failing (unusual filesystem) — fail **closed**: treat as disk pressure and refuse,
    rather than assuming space. Absence of a reading is never read as "plenty free".
  - Scratch on tmpfs — the same accounting applies but the pressure is RAM; the refusal path is
    identical, so no special case, only a documented tighter default.
  - A client seeking backwards into an evicted region — this is exactly the MTRX-10 respawn path,
    which is why eviction behind the playhead is safe. Assert the interaction once MTRX-10 lands.
  - Eviction racing an in-flight read of the same segment — on Linux the open fd keeps the inode
    alive, so the read completes; document rather than lock.

- **Acceptance criteria:**
  - [ ] Ahead-of-playhead segments are never evicted (negative test)
  - [ ] An active session is never reaped to make room for a new one (negative test)
  - [ ] `init.mp4` is never evicted
  - [ ] Low free space refuses admission and fails closed on an unreadable `statvfs`
  - [ ] At quota the system refuses new sessions and never wedges a live one (negative test)
  - [ ] Every eviction and refusal is logged with the numbers
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-09: Segment-alignment math (pure)
- **Priority:** Critical
- **Labels:** maestro, transcode, seek, pure
- **Agent:** codex
- **Estimate:** 3h
- **Blocked by:** MTRX-04
- **Description:** **This item exists separately from MTRX-10 on purpose.** Seek is the classic
  hard problem in a home-built transcoder, and the reason it is hard is almost never the process
  management — it is that a respawn silently changes the mapping between segment numbers and source
  time, and the corruption that follows is *silent*: the client plays the wrong content, or drifts
  audio, or stalls at a boundary, with nothing in any log. So the mapping is a pure function with
  its own exhaustive tests, proven before any process is killed.

  ## FILES
  - `src/maestro/transcode/align.rs` — new; the alignment math
  - `src/maestro/transcode/mod.rs` — re-export

  ## APPROACH
  1. The invariant, restated as the module doc comment (SN-1): **segment `n` covers source time
     `[n*D, (n+1)*D)` for the life of the session, across every respawn.**
  2. `pub fn segment_index_for_time(t_seconds: f64, d: f64) -> u64` = `floor(t / d)`, saturating at
     0, with an epsilon guard so a `t` that is floating-point-just-below a boundary (e.g.
     `59.999999999`) does not land a segment early. Use `(t / d + EPS).floor()` with
     `EPS = 1e-6`, and test the boundary explicitly.
  3. `pub fn aligned_seek_seconds(segment_index: u64, d: f64) -> f64` = `n * d` — computed from
     the **index**, never carried forward as an accumulated float. Accumulating `+= d` per segment
     drifts; multiplying does not.
  4. `pub fn respawn_plan(requested_seg: u64, d: f64) -> RespawnPlan { start_number, seek_seconds,
     ts_offset_seconds }` where `start_number == requested_seg`, `seek_seconds ==
     aligned_seek_seconds(requested_seg, d)`, and `ts_offset_seconds == seek_seconds`. These three
     are always derived together from one index — **they are the argument triple that MTRX-01
     consumes**, and deriving them in one place is what makes it impossible to pass a `-ss` that
     disagrees with a `-start_number`.
  5. `pub fn should_respawn(requested: u64, window_start: u64, highest_ready: u64, lookahead: u64)
     -> bool` — true when `requested < window_start` (behind the current generation, so the current
     process will never produce it) **or** `requested > highest_ready + lookahead` (so far ahead
     that waiting for the linear transcode is worse than restarting). Both are seeks; they differ
     only in direction.
  6. Document *why* alignment works end-to-end, in the source, in four sentences: input `-ss` is
     frame-accurate under re-encode; the seek point is always an exact multiple of `D`;
     `-force_key_frames expr:gte(t,n_forced*D)` therefore places keyframes on global boundaries;
     `-start_number` and `-output_ts_offset` make the new generation's files and timestamps
     continue the same numbering and timeline. A future reader who does not have this chain in
     their head will otherwise "simplify" one of the four and break the other three.

  ## TEST PLAN
  - `cargo test maestro::transcode::align` — exhaustive, this is the cheapest insurance in the spec.
  - `segment_index_for_time` at exact boundaries for `D ∈ {2, 4, 6, 10}` and `t` at
    `n*D`, `n*D - 1e-9`, `n*D + 1e-9` — the boundary case that the epsilon guard exists for.
  - Round-trip: `segment_index_for_time(aligned_seek_seconds(n, d), d) == n` for `n` across
    `0..10_000` and several `d` — this is the invariant SN-1 property test.
  - No-drift: `aligned_seek_seconds(10_000, 6.0)` equals `60_000.0` exactly, not an accumulated
    approximation.
  - `respawn_plan` always returns `start_number == requested` and `seek == ts_offset`.
  - `should_respawn` truth table across behind / inside-window / far-ahead / exactly-at-lookahead.
  - Negative: `d <= 0` or non-finite → error, never a division producing infinity.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - A seek to `t = 0` on a session already at generation 0 — `should_respawn` is false; do not
    restart a process to go where it already is.
  - A seek past the end of the source — clamp to the last valid segment index derived from the
    probe duration, never spawn ffmpeg with a `-ss` beyond EOF (it exits 0 with no output, which
    MTRX-05 correctly but confusingly reports as `Failed`).
  - Non-integer `D` (e.g. 5.5s) — every function must be correct; test it.
  - `requested == window_start` exactly — inside the window, no respawn.

- **Acceptance criteria:**
  - [ ] The SN-1 round-trip property holds across the tested index/duration sweep
  - [ ] Boundary cases at `n*D ± 1e-9` land on the correct segment
  - [ ] `aligned_seek_seconds` shows no accumulated drift at high indices
  - [ ] `respawn_plan` cannot produce a mismatched `start_number` / `-ss` / `ts_offset` triple
  - [ ] `should_respawn` truth table verified in both directions
  - [ ] Non-positive / non-finite `D` returns an error (negative test)
  - [ ] The four-sentence alignment rationale is in the source, not only in this spec
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-10: Seek — kill and respawn at a segment-aligned offset
- **Priority:** Critical
- **Labels:** maestro, transcode, seek, session
- **Agent:** claude
- **Estimate:** 5h
- **Blocked by:** MTRX-06, MTRX-07, MTRX-09
- **Description:** Applies MTRX-09's proven math to a live session. A seek beyond the transcoded
  window kills the ffmpeg process and respawns it at a segment-aligned offset, preserving the
  global segment numbering. The client is never told a seek happened — it requests segment 900,
  waits a moment, and receives segment 900.

  ## FILES
  - `src/maestro/transcode/session.rs` — `respawn_at(segment_index)`, generation counter
  - `src/maestro/http/transcode.rs` — segment handler consults `should_respawn`
  - `src/config.rs` — `MAESTRO_SEEK_LOOKAHEAD_SEGMENTS`, `MAESTRO_RESPAWN_RATE_LIMIT_PER_MIN`

  ## APPROACH
  1. Segment handler: on a GET for segment `n`, call `should_respawn(n, window_start,
     highest_ready, lookahead)`. False → the MTRX-06 await path, unchanged. True → `respawn_at(n)`,
     then await.
  2. `respawn_at(n)`, under the session's own lock so two concurrent seeks serialize:
     - Bump `generation`.
     - **Tear down the old process completely** using MTRX-07's semantics (`SIGCONT` → `SIGTERM`
       → grace → `SIGKILL` → `wait()`). Invariant SN-2 means the new process must not start until
       the old one is *confirmed reaped* — a still-draining ffmpeg writing `seg00901.m4s` while the
       new generation also writes it is the silent-corruption case this whole design guards against.
     - Compute `respawn_plan(n, D)` and build argv from it via MTRX-01. Never hand-assemble the
       triple at this call site.
     - **fMP4 only:** the init segment must not be replaced. Point the new generation's
       `-hls_fmp4_init_filename` at a generation-scoped temp name; on first write, compare with the
       existing `init.mp4` — identical (the expected case, since codec parameters are unchanged) →
       discard the temp; different → the session is unrecoverable, fail it rather than serve a
       mismatched init (a client that already fetched the original init would decode garbage).
     - Set `window_start = n`, clear the segment-existence cache, wake the notify.
  3. **Rate-limit respawns** at `MAESTRO_RESPAWN_RATE_LIMIT_PER_MIN` (default 12) per session. A
     client that scrubs the timeline aggressively, or a buggy player re-requesting a reaped
     segment in a loop, would otherwise kill and respawn ffmpeg continuously and pin a core doing
     no useful work. Over the limit → serve `503` + `Retry-After` rather than respawn.
  4. Segments already on disk from a **previous** generation stay valid and stay served — SN-1
     guarantees their content is correct regardless of which generation wrote them. This is the
     payoff for all of MTRX-09's rigor: a backwards seek into an already-transcoded region needs
     no respawn at all.
  5. Record `seeks_total` and `respawns_total` for MTRX-15.

  ## TEST PLAN
  - `cargo test maestro::transcode::seek` — with the stub binary, asserting on the **argv the
    stub was invoked with** (the pure builder is already proven, so this tests the wiring).
  - Forward seek beyond the lookahead → exactly one respawn, with `start_number`/`-ss`/`ts_offset`
    matching `respawn_plan`.
  - Backwards seek into a region already on disk → **zero** respawns, segment served from disk
    (negative test — the case a naive implementation gets wrong by respawning unconditionally).
  - Small forward seek inside the lookahead → zero respawns, the await path handles it.
  - Numbering continuity: after a respawn at 500, the stub's written filenames begin at
    `seg00500`, and segments 0..499 from generation 0 are still present and served.
  - Concurrent seeks to 300 and 700 → serialized, exactly one live process afterwards, and the
    final `window_start` is one of the two (never a mix).
  - Rate limit: the (limit+1)-th respawn in a minute returns `503` instead of spawning.
  - No zombies after a respawn storm (spawn/kill 20 generations, assert reaped).
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - Seek while the session is `SIGSTOP`ped by the throttle — the `SIGCONT`-first teardown ordering
    from MTRX-07 makes this work; add an explicit test once MTRX-11 lands.
  - Seek to a segment beyond the probe-derived end — clamped by MTRX-09; the request then resolves
    via the MTRX-14 "never coming" path rather than spawning a doomed process.
  - The old process dies on its own between the `should_respawn` check and the teardown — teardown
    must be idempotent over an already-exited child.
  - A respawn whose spawn **fails** (ffmpeg removed mid-session, disk pressure) — the session must
    end in a definite failed state with the client getting a real error, never a session that is
    registered but has no writer and no error.
  - Two clients on one session seeking in opposite directions (a shared Cast session) — the rate
    limiter plus serialization bounds the damage; the last seek wins.

- **Acceptance criteria:**
  - [ ] A forward seek beyond the window respawns exactly once with a correct argument triple
  - [ ] A backwards seek into an on-disk region does **not** respawn (negative test)
  - [ ] Segment numbering is continuous and correct across generations (SN-1 verified live)
  - [ ] The old process is confirmed reaped before the new one spawns (SN-2 verified)
  - [ ] The fMP4 init segment is never replaced by a respawn
  - [ ] Concurrent seeks serialize to exactly one live process
  - [ ] Respawn rate limiting prevents scrub-thrash
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-11: Throttling — bounded look-ahead window with pause and resume
- **Priority:** Critical
- **Labels:** maestro, transcode, throttle, cpu
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MTRX-06, MTRX-07
- **Description:** **The epic names a missing throttle as the #1 way home-built transcoders melt a
  CPU, and it is right.** Without this, someone opening a 2-hour film, watching four minutes, and
  closing the tab causes a full 2-hour transcode — at, say, 4x realtime that is thirty minutes of
  pinned cores for four minutes of watching, on a host that also runs Chord's inference workload.
  Multiply by the concurrency cap and the box is unusable. The fix: transcode a **bounded window
  ahead of the playhead**, pause when far enough ahead, resume as it drains.

  ## FILES
  - `src/maestro/transcode/throttle.rs` — new; the pure policy + the controller task
  - `src/maestro/transcode/session.rs` — pause/resume
  - `src/config.rs` — `MAESTRO_LOOKAHEAD_SEGMENTS`, `MAESTRO_LOOKAHEAD_RESUME_SEGMENTS`,
    `MAESTRO_THROTTLE_ENABLED`

  ## APPROACH
  1. **Pure policy first**, per the epic §7.3 discipline:
     `pub fn throttle_decision(playhead: u64, highest_ready: u64, running: bool, high: u64,
     low: u64) -> ThrottleAction::{Pause, Resume, Hold}`. **Hysteresis is mandatory** — pause at
     `highest_ready - playhead >= high` (default 30 segments ≈ 3 minutes at `D=6`), resume at
     `<= low` (default 12). A single threshold makes the process flap between `SIGSTOP` and
     `SIGCONT` every second at the boundary, which is its own pathology.
  2. **Mechanism: `SIGSTOP` / `SIGCONT` on the process group.** Chosen deliberately over the
     alternatives: `-re` (realtime input pacing) still transcodes the *entire* file, just slowly —
     it bounds the rate but not the total work, so the 2-hour-film case still burns 2 hours of CPU;
     and killing/respawning per window is exactly the expensive operation MTRX-10 rate-limits. A
     stopped process holds its memory and fds but consumes **zero CPU**, which is precisely the
     resource we are protecting. Document the memory-holding trade-off in the source — it is real,
     it is bounded by the concurrency cap, and it is the right trade on this host.
  3. **Playhead** is `max(segment index requested)` from MTRX-06's recording, not a client-reported
     position. Deriving it from actual segment GETs means it is correct for every client without a
     playback-position API, and it degrades correctly: a client that stops requesting stops
     advancing the playhead, the transcoder pauses, and the MTRX-07 idle timeout eventually reaps
     it. **A disappearing client and a paused client are the same signal, handled once.**
  4. Controller: one task per session ticking ~1s, applying the decision. Idempotent — `Pause` on
     an already-paused session is a no-op, never a second `SIGSTOP`.
  5. `MAESTRO_THROTTLE_ENABLED` defaults **on**. It exists as an escape hatch for diagnosis, not as
     an opt-in; shipping this off would ship the exact failure the epic warns about.
  6. Interaction contracts, both already handled and both worth an explicit test:
     - Teardown of a paused session works because of MTRX-07's `SIGCONT`-first ordering.
     - A seek (MTRX-10) resets `playhead` and `highest_ready` together, so the first decision after
       a respawn is `Resume`/`Hold`, never a spurious `Pause`.
  7. Record `throttle_pauses_total` and `paused_seconds_total` for MTRX-15.

  ## TEST PLAN
  - `cargo test maestro::transcode::throttle` — pure policy truth table first: below low →
    `Resume` if paused else `Hold`; above high → `Pause` if running else `Hold`; **between low and
    high → `Hold` in both states** (the hysteresis band, the case a naive implementation flaps in).
  - Controller with a stub: window fills → `SIGSTOP` observed; playhead advances → `SIGCONT`
    observed; no signal issued while inside the hysteresis band (negative test).
  - The headline scenario, as an explicit test: a session where the client requests segments
    0..40 and then stops → the process is paused and **the stub is never asked to write beyond
    `playhead + high`**. This is the "don't transcode a 2-hour film for 4 minutes of watching"
    assertion; name it so.
  - Teardown of a paused session completes within the grace window.
  - Post-seek: the first decision after a respawn is never `Pause`.
  - Idempotence: repeated `Pause` decisions issue exactly one `SIGSTOP`.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - Playhead jumping backwards after a backwards seek — `highest_ready - playhead` must be
    computed as a saturating signed difference, never an underflowing `u64` subtraction that
    produces a gigantic number and an instant pause.
  - A source shorter than the look-ahead window — the process finishes before any pause; the
    controller must handle an exited child without signalling it.
  - Client fetching segments faster than realtime (a Cast device pre-buffering aggressively) —
    the window never fills, no pause, correct.
  - Throttle disabled by config — the controller task still runs but only reports metrics; do not
    branch the whole session model on the flag.
  - A paused session during the MTRX-08 global-budget sweep — a paused session is still an
    *active* session and must not be reaped to make room.

- **Acceptance criteria:**
  - [ ] Pure `throttle_decision` truth table verified, including the hysteresis band (negative test)
  - [ ] The "4 minutes watched of a 2-hour film" scenario transcodes only the bounded window
  - [ ] Pause/resume signals are idempotent
  - [ ] A paused session tears down within the kill grace window
  - [ ] Backwards seek cannot underflow the window computation (negative test)
  - [ ] Throttle is enabled by default
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

> **★ MILESTONE B — the tier is production-safe.** MTRX-01..11 gives seek and bounded CPU. This is
> the point at which the transcode tier can be enabled for the household without supervision.

---

### MTRX-12: Text subtitles → WebVTT sidecar, no transcode
- **Priority:** High
- **Labels:** maestro, transcode, subtitles
- **Agent:** codex
- **Estimate:** 4h
- **Blocked by:** MTRX-05
- **Description:** Text-based subtitles (SRT, ASS/SSA, MOV_TEXT, WebVTT) are extracted to a
  **WebVTT sidecar** and referenced from the playlist. **This never forces a video transcode** —
  it is a cheap, separate, short-lived ffmpeg invocation over the subtitle stream alone, and it is
  available even on a direct-play or remux session. Charset and font handling for ASS is where the
  real work is.

  ## FILES
  - `src/maestro/transcode/subtitles.rs` — new; extraction argv (pure) + the sidecar cache
  - `src/maestro/http/transcode.rs` — the `.vtt` route
  - `src/maestro/transcode/playlist.rs` — `#EXT-X-MEDIA` subtitle group
  - `src/config.rs` — `MAESTRO_SUBTITLE_CHARSET_FALLBACK`, `MAESTRO_SUBTITLE_CACHE_MB`

  ## APPROACH
  1. Pure builder `build_subtitle_extract_args(source, stream_index, out_path) -> Vec<String>`:
     `-hide_banner -loglevel error -y -nostdin [-sub_charenc {CS}] -i {source}
     -map 0:s:{index} -c:s webvtt -f webvtt {out}`. Same argv-equality test discipline as MTRX-01.
  2. **Charset.** SRT files from the wild are frequently not UTF-8 (CP1252, CP1251, Shift-JIS,
     ISO-8859-x). ffmpeg's `-sub_charenc` is the mechanism, but the *detection* is ours: attempt
     UTF-8 validation on the first N KiB of the extracted stream; on failure, retry once with
     `MAESTRO_SUBTITLE_CHARSET_FALLBACK` (default `CP1252` — the most common Western case). If that
     also fails validation, serve the sidecar with replacement characters rather than failing the
     stream: **mojibake subtitles are strictly better than no playback**, and a hard failure here
     would block the film over a cosmetic defect.
  3. **ASS/SSA.** Converting to WebVTT discards positioning, styling, and karaoke effects — that is
     inherent to the target format, not a bug, and it must be stated in the source and the README
     so nobody "fixes" it later by burning in ASS by default (which would force a full video
     transcode for every anime episode). Fonts embedded as MKV attachments are irrelevant on the
     sidecar path since nothing is rendered server-side; **font handling only matters on the
     burn-in path (MTRX-13)** and is owned there.
  4. Sidecar caching: written into the session scratch dir (so MTRX-07/08 reap it for free),
     keyed by `(media_file_id, stream_index)`. Extraction is idempotent and cheap; do not build a
     cross-session cache tier for it.
  5. Playlist: an `#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",...,URI="sub{index}.m3u8"` entry per
     text track, each pointing at a one-entry WebVTT sub-playlist. Note that this is the **one**
     place a second playlist exists — it is not an ABR variant and does not make this a master
     playlist.

  ## TEST PLAN
  - `cargo test maestro::transcode::subtitles` — argv-equality tests for the extraction builder,
    with and without `-sub_charenc`.
  - Charset ladder: a fixture that is valid UTF-8 → one attempt; a CP1252 fixture → second attempt
    succeeds; an undecodable fixture → sidecar served with replacements, **stream not failed**
    (negative test).
  - Playlist: `#EXT-X-MEDIA` entries appear once per text track and never for image-based tracks.
  - A source with zero subtitle streams → no `#EXT-X-MEDIA` lines, no extraction spawned.
  - Sidecars land inside the session scratch dir and are removed by teardown.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - A subtitle stream ffmpeg cannot convert (a malformed or exotic text codec) — log, omit that
    one track from the playlist, keep the others and keep playback.
  - Extremely large subtitle streams (a full transcript track) — bound by `MAESTRO_SUBTITLE_CACHE_MB`.
  - Duplicate language tags across tracks — disambiguate the `NAME` attribute; do not drop tracks.
  - A forced-subtitles track — pass the disposition through as `FORCED=YES` when the probe reports
    it; spec A's `MediaInfo` carries the disposition, so do not re-probe here.
  - A subtitle track whose `#EXT-X-MEDIA` `DEFAULT`/`AUTOSELECT` should follow the plan's chosen
    track — read it from the plan, do not guess.

- **Acceptance criteria:**
  - [ ] Text subtitles are extracted to WebVTT without spawning a video transcode
  - [ ] The charset ladder degrades to replacement characters rather than failing playback (negative test)
  - [ ] ASS styling loss is documented in source and README
  - [ ] Sidecars live in the session scratch dir and are reaped with it
  - [ ] A source with no subtitles produces no subtitle playlist entries
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-13: Image-based subtitle burn-in
- **Priority:** Medium
- **Labels:** maestro, transcode, subtitles
- **Agent:** codex
- **Estimate:** 3h
- **Blocked by:** MTRX-01, MTRX-12
- **Description:** PGS and VOBSUB are **bitmap** subtitle formats. They cannot become WebVTT
  without OCR, so the only option is compositing them onto the video — which forces a full video
  transcode even when the video would otherwise have copied. **Spec C already surfaces this
  consequence when it builds the `PlaybackPlan`: the plan arrives with the video tier already
  escalated and a burn-in flag set. This item consumes that decision — it does not re-derive it,
  and it must never escalate a tier on its own.** A tier decision made in two places is a tier
  decision that will eventually disagree with itself.

  ## FILES
  - `src/maestro/transcode/args.rs` — the burn-in filter branch
  - `README.md` — document the burn-in cost

  ## APPROACH
  1. When `plan.burn_in_subtitle_stream` is `Some(index)`, emit an `-filter_complex` overlay
     instead of a plain `-vf`: `[0:v][0:s:{index}]overlay[v]` with `-map "[v]"`, replacing the
     default video `-map`. Compose correctly with a scale filter when the plan also asks for a
     resolution change (scale **before** overlay, so the subtitle bitmap is composited at output
     resolution rather than scaled twice).
  2. **Assert, do not infer:** if `burn_in_subtitle_stream` is set while `plan.video` is
     `VideoEncode::Copy`, that is an invalid plan — return an error naming spec C as the source of
     the inconsistency. This is the guard that keeps the two-places-deciding failure from ever
     shipping silently.
  3. **Fonts.** Bitmap subtitles carry their own pixels, so no font is needed for PGS/VOBSUB —
     state that explicitly, because it is the natural place a reader assumes fonts matter. Fonts
     would only matter for a **text**-subtitle burn-in via the `subtitles`/`ass` filter, which this
     spec does **not** do (text goes to WebVTT sidecars per MTRX-12, precisely to avoid needing
     `libass`, fontconfig, and an MKV attachment-extraction path in a musl-static binary). If text
     burn-in is ever wanted — for a client with no subtitle rendering at all — it is a separate,
     honestly-sized follow-up, not an extension here.
  4. `-copyts` interactions: the overlay path must still respect `-output_ts_offset`; add a
     respawn-with-burn-in argv test to prove alignment survives.

  ## TEST PLAN
  - `cargo test maestro::transcode::args::burn_in` — argv-equality for: burn-in from zero;
    burn-in plus scale (assert filter ordering); burn-in at a respawn offset (assert `-ss`,
    `-start_number`, `-output_ts_offset` all still correct).
  - Negative: `burn_in` set with `VideoEncode::Copy` → error, no argv produced.
  - Negative: no `-vf`/`-filter_complex` conflict — the two are never emitted together.
  - `grep -rE 'libass|fontconfig|force_style' src/maestro/transcode/` returns nothing (the deliberate
    non-goal, as a test).
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - A plan naming a burn-in stream index that does not exist — ffmpeg fails at spawn; surface it as
    a session failure with the stderr tail, not a silent no-subtitle stream.
  - Multiple image subtitle tracks — exactly one may be burned in; the plan chooses, this item
    never picks.
  - VOBSUB in a separate `.idx`/`.sub` pair rather than muxed — out of scope for this item; the
    plan simply will not select it (spec A's probe reports muxed streams).
  - Burn-in on a session that later seeks — alignment is unaffected because the filter graph does
    not touch timestamps; assert it in the respawn argv test above.

- **Acceptance criteria:**
  - [ ] Burn-in emits a correct `-filter_complex` overlay with correct scale ordering
  - [ ] A burn-in plan with a copy video tier is rejected as invalid (negative test)
  - [ ] Segment alignment arguments are unchanged by burn-in at a respawn offset
  - [ ] No `libass`/fontconfig dependency is introduced (grep test)
  - [ ] README documents that burn-in forces a full video transcode
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-14: Backpressure and failure semantics
- **Priority:** High
- **Labels:** maestro, transcode, reliability, api
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MTRX-06, MTRX-08, MTRX-10
- **Description:** Consolidates every failure path into one documented, tested taxonomy. Most of
  the individual mechanisms exist by now across MTRX-05..11; this item makes the **client-visible
  contract** coherent, so a player sees a consistent, actionable status rather than a different
  improvisation per failure mode. The failures are not exotic — they are the normal operating
  conditions of a home media server.

  ## FILES
  - `src/maestro/transcode/failure.rs` — new; the taxonomy and its mapping
  - `src/maestro/http/transcode.rs` — apply the mapping
  - `README.md` — the failure-response table

  ## APPROACH — the taxonomy, and the reasoning per row
  | Condition | Response | Why |
  |---|---|---|
  | ffmpeg dies mid-stream, retryable (transient I/O, source blip) | `503` + `Retry-After: 2`, session marked `Recovering`, **one** automatic respawn at the current playhead | A single retry converts most transient library-mount hiccups into an invisible rebuffer |
  | ffmpeg dies mid-stream, non-retryable (bad exit twice, unreadable source) | `502`, session marked `Failed`, stderr tail surfaced in the session record | Retrying a deterministic failure just burns CPU and delays the honest answer |
  | Client disappears mid-segment | Nothing — the write fails, the request task drops | Already handled: the playhead stops, the throttle pauses, the reaper eventually reaps. **No separate detection.** |
  | Segment request for a window already reaped/evicted | Treated as a **seek** (MTRX-10 respawn), subject to the respawn rate limit | Transparent recovery beats a `410` the player has no strategy for |
  | Segment request the source can never satisfy (past the real EOF) | `404` | Definite and final; a player can end playback |
  | Disk full / below min-free | New sessions `503` with a disk-pressure reason; existing sessions evict behind-playhead first (MTRX-08) | Protect the host; degrade the newest request, not the in-flight one |
  | Concurrency cap reached | `503` + a human-readable "too many active streams" reason | Fail fast; never queue a request a user is staring at |
  | ffmpeg binary absent | `501` | A deployment gap, not a transient condition — same distinction Muse's `classify_spawn_error` already draws |
  | Respawn rate limit exceeded | `503` + `Retry-After` | Bounds scrub-thrash without failing the session |

  1. Encode the table as a Rust enum with an explicit `-> StatusCode` mapping, so the table and the
     code cannot drift. Test every arm.
  2. Every failure records a structured `SessionFailure { kind, at_segment, ffmpeg_exit,
     stderr_tail }` on the session, readable by spec H's activity surface. **Sanitize the stderr
     tail before it is stored or logged** — it contains the full source file path (S6/S1); redact
     to the basename.
  3. The **one** automatic retry is per-session, not per-segment: a session that has already
     auto-recovered once and dies again is `Failed`. An unbounded retry loop against a genuinely
     broken file is indistinguishable from a fork bomb from the host's perspective.

  ## TEST PLAN
  - `cargo test maestro::transcode::failure` — every taxonomy arm maps to its documented status.
  - Stub dies once → one automatic respawn, playback continues; dies again → `Failed`, **no**
    second retry (negative test).
  - Segment request for an evicted-behind segment → respawn path, not `410`/`404`.
  - Segment past EOF → `404`, never a spawned process.
  - Disk-pressure and cap-reached both → `503` with distinguishable reasons.
  - Client-disconnect: drop an in-flight segment request → no session-level error recorded, no
    panic, throttle subsequently pauses (negative test — a disconnect is not a failure).
  - Stderr tail sanitization: a stub emitting a full path stores only the basename (S1/S6).
  - Verify no hardcoded IPs or org names in new/modified files.
  - Verify no secret-shaped value is read via `std::env::var` (S7 grep).

  ## EDGE CASES
  - ffmpeg exits 0 but early (truncated source) — the remaining segments become "can never exist"
    `404`s; the client ends playback at the real content end. Do not report this as a failure.
  - A failure during the automatic retry's own spawn — collapse straight to `Failed`, do not
    recurse.
  - Disk fills *during* a session rather than before it — eviction runs first; only if eviction
    cannot recover the budget does the session fail.
  - A client that ignores `Retry-After` and hammers — the respawn rate limiter is the backstop.

- **Acceptance criteria:**
  - [ ] Every taxonomy arm has a test asserting its documented status code
  - [ ] Exactly one automatic retry per session; a second death is terminal (negative test)
  - [ ] A client disconnect is never recorded as a session failure (negative test)
  - [ ] stderr tails are path-sanitized before storage or logging
  - [ ] README carries the failure-response table
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-15: Transcode observability — session metrics and the activity surface
- **Priority:** Medium
- **Labels:** maestro, transcode, metrics, api
- **Agent:** codex
- **Estimate:** 2h
- **Blocked by:** MTRX-07, MTRX-11, MTRX-14
- **Description:** Exposes what the tier is doing, so a slow evening is diagnosable without SSH.
  Also the data spec H's Server Activity panel needs from the `native` backend — without it, that
  panel can describe a Plex session but goes blank for a Maestro one.

  ## FILES
  - `src/maestro/transcode/metrics.rs` — new; counters and the snapshot type
  - `src/maestro/http/transcode.rs` — `GET /transcode/sessions` (list) and `/transcode/metrics`
  - `README.md` — document the metrics

  ## APPROACH
  1. Per-session snapshot: `session_id`, `account_id`, item reference, container, tier (from the
     plan), `playhead_seg`, `highest_ready_seg`, `generation`, `paused`, `scratch_bytes`,
     `respawns`, `state`. **No source file path** in any response — S1, and the account boundary
     from epic §8.1 means one account must not learn another's library layout.
  2. Process-level counters: `sessions_started/completed/failed`, `orphans_swept`,
     `admissions_refused`, `throttle_pauses`, `paused_seconds`, `respawns`, `evictions`,
     `scratch_bytes_total`. Prometheus text format, consistent with the fleet's existing exporters.
  3. `GET /transcode/sessions` is the shape spec H consumes. Keep it a plain snapshot list — no
     pagination, no filtering; the concurrency cap means this list is never long.

  ## TEST PLAN
  - `cargo test maestro::transcode::metrics` — counters increment on the expected transitions.
  - The session snapshot contains **no** file path and no source directory (negative test, S1).
  - Prometheus output parses and every documented counter is present.
  - `GET /transcode/sessions` with zero sessions → `200` and an empty list, not `404`.
  - Verify no hardcoded IPs or org names in new/modified files.

  ## EDGE CASES
  - A session reaped between snapshot construction and serialization — snapshot from a consistent
    clone, never hold the manager lock across serialization.
  - Counters after a process restart reset to zero — expected for a Prometheus counter; do not
    persist them.
  - An account with no sessions requesting the list — empty list, and never another account's rows.

- **Acceptance criteria:**
  - [ ] Per-session snapshots and process counters are exposed
  - [ ] No file paths or source directories appear in any response (negative test)
  - [ ] Prometheus output is well-formed and complete
  - [ ] Empty session list returns `200`, not `404`
  - [ ] README documents the metrics
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MTRX-16: Host-gated validation harness — ffprobe self-validation and the crash-isolation chaos test
- **Priority:** High
- **Labels:** maestro, transcode, testing, integration
- **Agent:** claude
- **Estimate:** 4h
- **Blocked by:** MTRX-06 (self-validation), MTRX-10 (alignment cases), MTRX-11 (chaos under throttle)
- **Description:** The two tests that cannot run on the dev box and are worth the plumbing anyway.
  **The transcoder validates the transcoder:** run `ffprobe` over Maestro's own HLS output and
  assert it is well-formed. This is cheap and brutal, and it catches the class of defect no unit
  test in this spec can — a segment whose *arguments* were perfectly constructed but whose *bytes*
  are misaligned, non-decodable, or timestamp-discontinuous after a respawn. Unit tests prove we
  asked ffmpeg for the right thing; only this proves ffmpeg gave us the right thing back.
  Alongside it, **the chaos test the epic §10b requires**: SIGKILL ffmpeg mid-session and assert
  Muse never notices. The whole sidecar design exists to buy that isolation, and an isolation claim
  that is never tested is a hope, not a property.

  ## FILES
  - `tests/maestro_delivery_validation.rs` — new; both harnesses, feature/env-gated
  - `README.md` — how to run them and on which hosts

  ## APPROACH
  1. **Gating.** Both harnesses are gated on `MAESTRO_TEST_FFMPEG=1` **and** a probe that the
     configured ffmpeg/ffprobe binaries exist, and they **skip cleanly with an explanatory
     `eprintln!` when unset** — the same convention `src/streaming/mod.rs` already uses for its
     `MUSE_TEST_DATABASE_URL`-gated test. This keeps `cargo test` green on the dev box (§1d) while
     making the harness a real gate on a host that has ffmpeg. Run them on the Muse deploy host or
     <host> via the compiler tool; never install ffmpeg on the dev box to make them run there.
  2. **Fixture.** A tiny synthetic source generated once by ffmpeg itself (`testsrc` + `sine`,
     ~90 seconds, known duration) rather than a library file — no PII, no QNAP dependency, no
     multi-GB fixture in git, and a deterministic input. Generate into a temp dir at test start.
  3. **Self-validation assertions**, per container and per scenario:
     - `ffprobe -v error -show_streams -show_format` on the **master playlist URL** succeeds and
       reports the expected codecs, resolution, and a duration matching the source within one
       segment. Probing the master exercises the real client entry path, `CODECS` string and all.
     - Every individual segment probes clean — no decode errors on stderr.
     - **Timestamp continuity across a respawn**, the assertion that justifies this item:
       drive a seek (MTRX-10), then probe the segments spanning the generation boundary and assert
       the first PTS of segment `n` equals `n * D` within a frame, and that there is no gap or
       overlap at the boundary. This is exactly the silent segment-alignment corruption MTRX-09's
       pure tests cannot see, because they prove the arithmetic and not the muxer's behaviour.
     - fMP4 only: `init.mp4` is byte-identical before and after a respawn (MTRX-10's guarantee,
       verified against real bytes rather than against our comparison logic).
     - Playlist duration sums to the probed duration within a frame.
  4. **Chaos test — the isolation proof:**
     - Start a transcode session, let it produce segments, then `SIGKILL` the ffmpeg child directly
       (not via the session's own teardown — the point is an *unhandled* death).
     - Assert, in order: the supervisor records `Failed`/`Recovering` per MTRX-14; the client sees
       a defined status (the one automatic retry, then a real error), never a hang; **the `maestro`
       process itself is still alive and serving** — a second session can be started immediately;
       no zombie remains; the scratch directory is cleaned.
     - Assert **Muse never notices**: the `muse` binary's health endpoint stays green throughout,
       and no Muse-side worker, tuner, or tracker records an error attributable to the kill.
       Because the two are separate processes with separate cgroups (§1b), this should be trivially
       true — which is the point. A test that is trivially true today is what tells you when
       someone later "simplifies" the two binaries into one and quietly deletes the isolation.
     - Repeat the kill under **throttle-paused** state (MTRX-11) — a `SIGSTOP`ped process that is
       then `SIGKILL`ed is the awkward corner, and MTRX-07's `SIGCONT`-first ordering is what makes
       the surrounding teardown survive it.
  5. Record the **sustained concurrent 1080p CPU transcode count** on the chosen host while the
     harness is up (epic §10b asks for this measurement once, in this spec) and write it into the
     MTRX-16 Plane item as the regression baseline. Also record time-to-first-segment and seek
     latency against §10b's budgets (transcode start < 5s, seek < 3s) — measure and report; do not
     fail the gate on a budget miss, file it.

  ## TEST PLAN
  - The harness *is* the test plan; what is gated here is that it runs and passes on a host with
    ffmpeg, and skips cleanly without one.
  - `cargo test` on the dev box → both harnesses skip with an explanatory message, suite green.
  - `MAESTRO_TEST_FFMPEG=1 cargo test maestro_delivery_validation` on the Muse host or <host> → all
    assertions pass for both containers.
  - Deliberate-break check, run once by hand during development and recorded in the item: perturb
    the respawn to use an unaligned offset and confirm the continuity assertion **fails**. A
    validation harness that has never been seen to fail is not known to validate anything.
  - Verify no hardcoded IPs, hostnames, or library paths in the harness (the fixture is synthetic).

  ## EDGE CASES
  - ffmpeg present but built without `libx264` — the gate probe must check the *encoder*, not just
    the binary, and skip with a message naming the missing encoder.
  - The harness leaving processes or scratch behind on its own failure — wrap in a teardown guard
    that runs on panic; a test harness that leaks ffmpeg processes is its own disk/CPU incident.
  - A slow host making the timing measurements noisy — report them, never assert on them.
  - Running the harness concurrently with a real playback session on a shared host — it counts
    against `MAESTRO_MAX_TRANSCODES`; note it in the README so nobody runs it during a film.

- **Acceptance criteria:**
  - [ ] `ffprobe` validates the master playlist, every segment, and playlist/probe duration agreement
  - [ ] Timestamp continuity across a respawn boundary is asserted against real bytes
  - [ ] `init.mp4` is proven byte-identical across a respawn (fMP4)
  - [ ] SIGKILL of ffmpeg mid-session leaves `maestro` serving, no zombie, scratch cleaned
  - [ ] The chaos test asserts Muse's health is unaffected throughout (the isolation proof)
  - [ ] The kill-while-throttle-paused corner is covered
  - [ ] Both harnesses skip cleanly on a host without ffmpeg; `cargo test` stays green on the dev box
  - [ ] Concurrent-transcode capacity and the §10b latency budgets are measured and recorded
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

## 4. Sequencing summary

| Milestone | Items | Independently shippable? |
|---|---|---|
| **Pre-flight** | MTRX-03 (Cast container spike — start day one) | n/a, decides a default |
| **A — simplest end-to-end** | MTRX-01, 02, 04, 05, 06 | **Yes.** One rendition, no seek, no throttle. Real playback of a real file to a real client. |
| **A′ — production-safe lifecycle** | MTRX-07, 08 | Yes. Nothing leaks, nothing melts the disk. |
| **B — seek** | MTRX-09, 10 | Yes. |
| **C — throttle** | MTRX-11 | Yes, and this is the item that makes the tier safe to leave on. |
| **D — subtitles** | MTRX-12, 13 | Yes, each independently. |
| **E — polish + proof** | MTRX-14, 15, 16 | Yes. MTRX-16 is the ffprobe self-validation and the crash-isolation chaos test; it can be pulled forward to right after Milestone B if seek behaviour is in any doubt. |

**The overrun guard:** if this spec must be cut, cut it at a milestone boundary. Every milestone
leaves a working, deployable transcode tier. The failure mode to avoid is landing half of MTRX-10
and half of MTRX-11 and having neither seek nor throttle — which is exactly what happens when items
are worked in parallel by eagerness rather than by the `Blocked by` graph above.

## 5. Config knobs introduced

| Key | Default | Item |
|---|---|---|
| `MAESTRO_FFMPEG_BIN` | `ffmpeg` | MTRX-05 |
| `MAESTRO_SEGMENT_SECONDS` | `6` | MTRX-01 |
| `MAESTRO_SEGMENT_CONTAINER` | decided by MTRX-03 | MTRX-03 |
| `MAESTRO_X264_PRESET` / `_CRF` | `veryfast` / `21` | MTRX-01 |
| `MAESTRO_AAC_BITRATE` | `192k` | MTRX-01 |
| `MAESTRO_SCRATCH_ROOT` | — (**required**, no default; never a card-backed volume) | MTRX-02 |
| `MAESTRO_SCRATCH_BUDGET_MB` / `_SESSION_BUDGET_MB` / `_MIN_FREE_MB` | `20480` / `4096` / `5120` | MTRX-02/08 |
| `MAESTRO_SEGMENT_KEEP_BEHIND` | `10` | MTRX-08 |
| `MAESTRO_SEGMENT_WAIT_SECS` | `30` | MTRX-06 |
| `MAESTRO_SESSION_IDLE_SECS` | `300` | MTRX-07 |
| `MAESTRO_MAX_TRANSCODES` | `2` | MTRX-07 |
| `MAESTRO_MAX_SESSIONS` | (spec D owns; respected here) | MTRX-07 |
| `MAESTRO_KILL_GRACE_SECS` | `5` | MTRX-07 |
| `MAESTRO_SEEK_LOOKAHEAD_SEGMENTS` | `15` | MTRX-10 |
| `MAESTRO_RESPAWN_RATE_LIMIT_PER_MIN` | `12` | MTRX-10 |
| `MAESTRO_LOOKAHEAD_SEGMENTS` / `_RESUME_SEGMENTS` | `30` / `12` | MTRX-11 |
| `MAESTRO_THROTTLE_ENABLED` | `true` | MTRX-11 |
| `MAESTRO_SUBTITLE_CHARSET_FALLBACK` | `CP1252` | MTRX-12 |
| `MAESTRO_SUBTITLE_CACHE_MB` | `64` | MTRX-12 |
| `MAESTRO_TEST_FFMPEG` | unset (test-gate only) | MTRX-16 |

None of these are secret-shaped; all resolve through the **shared** `src/config.rs` (one config
module for both binaries, per epic §2), never scattered `std::env::var`, and none may ever hold a
credential (S7). Every key is `MAESTRO_`-prefixed so a reader can tell at a glance which binary
consumes it.

## 6. Pre-flight

- [ ] Prefix `MTRX` confirmed via `plane_prefix_check` → `plane_prefix_register` →
      `plane_prefix_promote` (epic §11 registers the whole family; verify `MTRX` specifically)
- [ ] Spec D merged — the session model, `account_id` (Muse's account id-space, per epic §8.1),
      and the **signed stream-URL signer** every URL in this spec depends on (§1c). Without the
      signer, MTRX-04 and MTRX-06 have nothing to call; do not stub one "temporarily"
- [ ] Spec D's path-safety allowlist (epic §10b, Foundry's `MUSE_FOUNDRY_ALLOWED_ROOTS` pattern)
      is in force — this spec hands source paths to ffmpeg and inherits that default-deny check
      rather than re-implementing it
- [ ] Spec C merged — `PlaybackPlan`, including the burn-in consequence MTRX-13 consumes
- [ ] Spec A merged — `src/media/` exists as the shared core, so MTRX-01's `ffmpeg_args`
      extraction has a home rather than inventing a module boundary this spec does not own
- [ ] Spec A's backfill number reviewed: **what fraction of the library would direct-play to the
      devices we own.** Epic §6 is explicit that this number sizes this spec honestly — if it is
      high, land Milestone A and reassess before committing sprints to B and C
- [ ] The `maestro` `[[bin]]` target exists and `maestro.service` is deployed with its own cgroup
      (`MemoryMax`, `MemorySwapMax=0`) — spec B's standing-up work, confirmed here because MTRX-07's
      concurrency cap is sized against it
- [ ] `ffmpeg` present **on the Maestro deploy host**, with `libx264` and `aac` confirmed
      (`ffmpeg -encoders`). It is **not** on the dev box (§1d) — verify on the host that runs it,
      and run any ffmpeg-touching gate there or on <host> via the compiler tool
- [ ] A scratch filesystem chosen with headroom for `MAESTRO_SCRATCH_BUDGET_MB` plus
      `MIN_FREE_MB`; **not** on a removable-card-backed volume and not on the host's root
      filesystem. Record the chosen volume and the reasoning in the MTRX-02 Plane item
- [ ] Baseline: `cargo test` green on Muse main; record the count (this is the same suite the
      `muse` binary gates on — a Maestro regression fails Muse's gate, which is the point)
