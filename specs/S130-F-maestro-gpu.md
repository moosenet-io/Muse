# S130-F — Maestro: opt-in hardware transcode + Chord GPU arbitration

plane_project: MUSE
module: Muse
prefix: MGPU
spec_id: S130-F-maestro-gpu

## Metadata
- **Author:** Moose
- **Session:** S130
- **Date:** 2026-08-01
- **Module version:** Maestro v0.1 (child spec F of `S130-maestro-epic`)
- **Estimated total:** ~26h autonomous agent work (10 items)
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7 via the parent epic §9; this spec adds no new
  GUI surface and no new egress beyond a loopback call to Chord's already-sanctioned
  control/proxy API.
- **Depends on:** `S130-E-maestro-transcode.md` (MTRX) — E owns the pure argument builder,
  the HLS session lifecycle, and the software encode path. F **extends** E; it does not fork it.
- **Repo:** the existing `moosenet/Muse` repo. Per epic §2, Maestro is a **second `[[bin]]` in the
  same crate** (`src/bin/maestro/main.rs`, modules under `src/maestro/`), sharing `config.rs`,
  `models/`, `repo/`, `error.rs`, and `metrics.rs` with `muse`. Isolation is a process/systemd/cgroup
  boundary, not a repository boundary. Every path in this spec is Muse-repo-relative.
- **Context:** Spec E gives Maestro a working software transcode path. This spec adds a
  **feature-gated, default-OFF hardware encode path** (VA-API on the integrated Radeon 8060S,
  gfx1151) plus the arbitration that makes it safe to run on a GPU the household's inference
  layer is actively using. The hardware path is built properly and shipped disabled; §12
  states exactly what evidence justifies enabling it.

---

## 1. Why this is a separate spec, and why it ships off

Two things are simultaneously true and the spec must honour both:

1. **The capability should exist.** A 4K HEVC→H.264 full transcode is the one workload where
   software encoding on the host CPU is genuinely expensive, and the hardware to avoid it is
   already in the box. Building it later, bolted onto a matured software path, is more
   expensive than building it now behind a flag.
2. **Turning it on is a fleet-wide resource decision, not a Maestro decision.** The GPU is a
   shared, idle-reaped pool that Chord arbitrates for inference the household uses
   interactively. A transcode that takes VRAM/GTT bandwidth without announcing itself does not
   present as "the transcode is heavy" — it presents as **"Lumina got slow"**, which is an
   expensive misdiagnosis (epic §10.5). The unified-GTT memory model makes this worse, not
   better: there is no separate VRAM pool to hide in — inference weights, KV cache, and the
   video engine's working set all come out of one ~120 GB GTT allocation.

So: build it, test it, instrument it, document it, **default it off**, and gate the flip on
telemetry (§12).

## 2. Scope

**In scope**
- VA-API hardware decode + scale + encode (H.264 and HEVC targets) as a variant of spec E's
  argument builder.
- Startup + on-demand capability detection with clean degradation to software.
- Cooperative GPU arbitration against Chord: admission check, advisory lease with heartbeat,
  release, mid-session yield.
- Config surface, separate hardware concurrency caps, mid-session-unavailable policy.
- Output parity/quality verification against the software path.
- Telemetry sufficient to answer "was enabling this worth it?"
- An enable/disable runbook including outside-in contention diagnosis.

**Out of scope (explicit)**
- **HDR tone-mapping.** The epic (§8.3) puts tone-mapping out of scope through spec E and says
  "a spec F follow-up at most." It is **not** part of this spec's exit criteria. See §11 —
  it is the classic scope sink and is deliberately parked with a written rationale rather than
  quietly absorbed.
- NVENC/QSV/other vendor encoders. One hardware family, the one we own.
- Changing Chord. §10 describes a Chord-side follow-up; this spec builds Maestro's side so that
  it works **today** with no Chord change and upgrades cleanly if that follow-up lands.
- Any change to the direct-play / remux / partial-transcode tiers (epic §6). Hardware encoding
  is only ever reached on tier 4.

**Deploy dividend, stated up front (detail in §4.2):** the hardware path is **subprocess-only** —
ffmpeg does all the VA-API work, Maestro only passes flags. Enabling GPU transcode therefore adds
no linked library and does **not** force the shared `muse`/`maestro` OCI image off its musl-static
default. Given the fleet has lost days to the musl/`TARGET_NATIVE` split before (TERM #558), this is
a real dividend of the one-image model and it is protected by an acceptance criterion, not left to
chance.

## 3. Standing constraints inherited from the epic §7 and §10b

ffmpeg stays a subprocess (no libav bindings); no GPL code; the argument builder stays pure and
golden-fixture tested; every optional capability returns `None`/degrades when unconfigured;
secrets via `SecretManager::get()`; S1 — no literal hosts/IPs/ports/tokens in spec or source.

From epic §10b specifically, the clauses this spec touches:
- **Observability.** The metrics in MGPU-08 extend the epic's required per-session metric set
  (active sessions, tier distribution, transcode realtime-ratio, segment latency); they do not
  duplicate or rename them. The per-session structured log line gains the encoder fields rather
  than becoming a second log line.
- **State inventory.** Hardware transcoding changes nothing about segment scratch, which stays
  bounded, quota-enforced, and **not** on a removable-card-backed volume. Worth restating because a
  faster encoder fills scratch faster; the quota is what stops that becoming a disk incident.
- **Testing across the process boundary.** The `FakeBackend` and the SIGKILL-mid-session chaos test
  must still pass with hardware enabled — a hardware session that is SIGKILLed must release its
  lease (MGPU-04's `Drop` path) and still leave Muse untouched.
- **Rollback.** `MUSE_MAESTRO_HWACCEL_ENABLED` is a kill switch in the same spirit as
  `MAESTRO_DEFAULT_BACKEND=plex`: one line, no rebuild, no redeploy of a different image.

---

## 4. Verified ground truth about Chord's GPU arbitration (do not re-derive)

Confirmed by reading Chord `main` on 2026-08-01. **This corrects a common assumption:**

- The lease endpoints are on Chord's **proxy** router, not the control router:
  `POST /v1/gpu-exclusive/acquire`, `POST /v1/gpu-exclusive/release`,
  `GET /v1/gpu-exclusive/status`. Body: `{"holder": "<label>", "force": <bool>}`.
  All three require the same JWT auth as every other Chord endpoint.
- `GET /admin/activity` lives on the **control** router (also JWT-authed) and returns
  `{serving, inflight, idle_secs, last_request_unix}`.
- **The existing lease is EXCLUSIVE and EVICTING.** A fresh grant makes Chord return a
  structured `503 {"error":"gpu_exclusively_held"}` on *every* inference path, and it
  best-effort **evicts resident Ollama models** and stops the managed diffusion daemon.
  A re-acquire by the same holder is a heartbeat and does **not** re-evict. A different live
  holder gets `409 gpu_exclusively_held`. A fresh grab while a client request is in flight is
  refused with `409 gpu_yield_client_busy` unless `force:true` (the S125 client-yield guard).
  Abandoned locks expire after `CHORD_GPU_EXCLUSIVE_TTL_SECS` (default 600s).
- `GET /v1/gpu-exclusive/status` returns `{held, holder, since, last_heartbeat, expired, ttl_secs}`,
  where **`held` is `!expired`** — so an abandoned lock reports `held:false, expired:true` and is
  freely takeable by anyone. Expiry is `now - last_heartbeat > ttl`.
- **The keep-resident eviction exemption is NOT on Chord `main`.** `GPU_EXCLUSIVE_EXEMPT_KEEP_RESIDENT`
  (and `MODEL_KEEP_RESIDENT`) exist only on an unmerged branch. **As shipped today, a fresh exclusive
  grant evicts every resident Ollama model, including the assistant's pinned working set.** Do not
  write this spec — or its code — against the branch. This makes the conclusion below stronger, not
  weaker.
- `/v1/embeddings` is deliberately *not* gated by the lock; every other inference path is.

**Therefore Maestro must NOT take the exclusive lease for a transcode.** Holding it for the
90 minutes of a film would 503 the household's assistant for 90 minutes *and*, on today's `main`,
unload the whole resident working set at the moment of acquisition. That primitive belongs to MINT
sweeps and the heavy compiler, which legitimately want the whole GPU briefly.

**Two further constraints that fall out of Chord's implementation:**

- **Holder labels are substring-matched.** Chord's idle watchdog classifies a lock as a *compiler*
  lease by case-insensitive substring against `compiler,build,bld` (`MINT_IDLE_COMPILER_LEASE_HOLDERS`),
  and a live compiler lease makes Chord defer its idle watchdog and skip lazy-restore. Maestro's
  holder label must therefore **not** contain `compiler`, `build`, or `bld`. `maestro_transcode`
  is safe and is asserted in a unit test.
- **A Maestro transcode is invisible to MINT's own yield check.** MINT (S125) refuses to grab the
  GPU when Chord reports `inflight > 0` — but `inflight` counts *Chord inference requests*, and a
  Maestro ffmpeg subprocess is not one. So the courtesy is currently one-directional: Maestro can
  see MINT, MINT cannot see Maestro. This is exactly the gap the §10 shared-lease follow-up closes,
  and until it does, Maestro yields rather than expecting to be yielded to.

**What Maestro does instead — the cooperative protocol (MGPU-04):**
1. **Admission check** before starting a hardware session: read `GET /v1/gpu-exclusive/status`
   (a live lock held by anyone else ⇒ deny) and `GET /admin/activity` (deny if `serving` or
   `idle_secs` below a configured floor, when the config demands an idle GPU).
2. **Advisory registration** for the session's duration so the state is externally visible and
   attributable, with a heartbeat on a timer.
3. **Yield**: the same poll that heartbeats also watches for an exclusive lock appearing. When
   MINT or the compiler takes the GPU, Maestro's in-flight hardware sessions follow the
   configured mid-session policy (§MGPU-05) — the assistant/compiler always wins.
4. **Release** on session end, on process shutdown, and on any error path. A lease is never
   held across an idle session, and never outlives the ffmpeg child.

This is implemented behind a `GpuArbiter` trait with two implementations so the upgrade is a
config flip, not a rewrite:
- `ObserveOnlyArbiter` (**default, works today, needs no Chord change**) — steps 1, 3, 4 with
  registration kept local and exported as telemetry.
- `SharedLeaseArbiter` — the same shape against a future Chord *shared/advisory* lease
  (a non-evicting `mode:"shared"` grant). **That Chord change is a CHRD-project follow-up, not
  part of this spec.** Until it exists, `ObserveOnly` is the only wired implementation.

## 4.1 Mid-session revocation — DECIDED: revocable, yield at the next segment boundary

The GPU is idle-reaped and shared. A MINT sweep, a coding task, or the heavy compiler will
eventually want it **while a transcode is running**. That case is not an edge case; on a busy
evening it is the normal case. The spec decides it rather than leaving it to the operator:

> **Decision: a Maestro hardware session is REVOCABLE. On revocation it falls back to the software
> encoder at the next segment boundary, keeping the same session id and playlist.** The viewer sees
> a CPU-load shift and possibly a small quality-parameter change, not an interruption. The
> alternative — a non-revocable lease with a hard duration bound — was rejected: bounding it short
> enough to protect the assistant (a minute or two) makes hardware pointless for a film, and
> bounding it long enough to be useful (a film's length) is exactly the multi-hour assistant
> blackout this spec exists to prevent.

`finish` and `abort` remain configurable (MGPU-05) for operators who want a different trade, but
`yield_to_software` is the default and the documented recommendation, and **revocability is a
property of the design, not of the config** — the other two options change *what happens on
revocation*, never *whether Maestro can be revoked*.

The hard bound is kept anyway as belt-and-braces, not as the primary mechanism:
`MUSE_MAESTRO_HWACCEL_MAX_LEASE_SECS` (default `7200`) caps any single hardware lease, so a wedged
session cannot hold the GPU indefinitely even if every revocation path fails.

### What Chord must expose (its side of the contract)

**Today, with no Chord change** — this is what `ObserveOnlyArbiter` is built against and it is
sufficient for the decided behaviour:
- `GET /v1/gpu-exclusive/status` already publishes `{held, holder, since, last_heartbeat, expired}`.
  Maestro polls it every `MUSE_MAESTRO_HWACCEL_POLL_SECS` (default `10`) and treats a live lock held
  by anyone else as a revocation signal. **Worst-case yield latency = poll interval + one segment
  duration**, which must be documented in the runbook and asserted in a test.

**What Chord SHOULD expose to close this properly** (the §10 CHRD follow-up — named here so the
Chord spec has a concrete contract to implement, not a vague aspiration):
1. **A shared, non-evicting registration.** `POST /v1/gpu-shared/acquire` with
   `{"holder", "kind":"transcode", "revocable":true}` that does **not** 503 inference paths and does
   **not** evict resident models. Its only job is to make a transcode *visible*.
2. **Visibility in the exclusive path.** `POST /v1/gpu-exclusive/acquire` should report existing
   shared holders in its response so MINT can see a transcode in progress — closing the
   one-directional blindness noted in §4. MINT's existing `inflight > 0` yield check does not and
   cannot see an ffmpeg subprocess.
3. **A revocation flag the holder polls.** `revoke_requested: bool` on the shared lease's status,
   set when an exclusive acquirer is waiting. **Poll, not callback** — Maestro must not need an
   inbound port and Chord must not need egress into Maestro.
4. **A bounded grace period.** Chord waits `grace_secs` (or until the shared holder releases,
   whichever is first) before granting the exclusive lease. The grace floor is one segment duration
   plus margin; anything shorter forces the mid-segment splice that MGPU-05 forbids.

Maestro's `SharedLeaseArbiter` is written against exactly this shape so adopting it is a config
flip. **None of it is required for this spec to ship**, and this spec must not block on it.

## 4.2 Deploy consequence — the hardware path is subprocess-only, and that is load-bearing

Because `muse` and `maestro` ship as **two bins in one OCI image** (epic §2), anything this spec
adds to the crate's link requirements applies to the *whole image*, including the `muse` binary.
The fleet has been bitten by exactly this before: `oci-publish.sh` defaults to **musl-static**, and
a module that needed `TARGET_NATIVE=1` sat silently un-deployed until someone noticed (TERM #558).
So the linking question has to be answered explicitly, not assumed.

**Answer: this spec's hardware path is subprocess-only. It adds no linked library and no new
dynamic dependency, and the image stays on the musl-static default.**

Stated plainly, per epic §7.1 (*ffmpeg is a subprocess, not a linked library*):

- **All VA-API work happens inside the `ffmpeg` binary.** Maestro contributes *argument strings*
  — `-init_hw_device vaapi=va:<device>`, `-hwaccel vaapi`, `-vf scale_vaapi=…`, `-c:v h264_vaapi`.
  It never opens a VA-API context itself. There is no `libva`, no `libdrm`, no `ffmpeg-next`, no
  libav binding, no `pkg-config` step, and no `build.rs` addition anywhere in this spec.
- **Capability detection is also subprocess + filesystem only** (MGPU-01): two `ffmpeg` invocations
  whose *stdout is parsed as text*, a `std::fs` existence/metadata check on the render node, and a
  permission check via the `libc` crate's `access` — `libc` is already in the dependency tree and
  is musl-clean. The VA-API smoke test is likewise just another `ffmpeg` subprocess.
- **The Chord arbitration client (MGPU-04) must use the crate's existing `reqwest` with `rustls`**,
  never `native-tls`/OpenSSL. Pulling OpenSSL in for a loopback JSON call is the one realistic way
  this spec could accidentally force `TARGET_NATIVE=1` on the whole image, and it is forbidden here.
- **The parity harness (MGPU-07) shells out to `ffmpeg`/`ffprobe`** and is `#[ignore]`d by default,
  so it changes neither the dependency graph nor the CI build.

**Therefore:** enabling GPU transcoding is a *runtime* decision on the host (does its `ffmpeg` have
VA-API, does the render node exist, is the service user in the render group) and never a *build*
decision. The published image is byte-identical whether the feature is on or off. This is a genuine
advantage of the same-repo/one-image model and it is only preserved by keeping the rule above.

**Enforcement, so this cannot regress quietly:** MGPU-02's acceptance criteria include a
dependency-diff check — the merged change must add no new entry to `Cargo.lock` that requires a C
toolchain or `pkg-config`, and `oci-publish.sh muse moosenet/Muse main muse maestro` must continue
to succeed on the musl-static default with **no** `TARGET_NATIVE=1`. If some future variant of this
work genuinely needs a linked library, that is a separate spec whose first paragraph must
acknowledge it is moving the *entire Muse image* to native-glibc, with an operator decision to match.

---

## 5. Items

### MGPU-01: VA-API capability detection with pure parsers
- **Priority:** High
- **Labels:** maestro, gpu, transcode
- **Agent:** claude
- **Estimate:** 3h
- **Description:** Detect, at startup and on demand, what the host ffmpeg + driver actually
  support. A missing, broken, or permission-denied VA-API stack must degrade to software
  cleanly — it must never fail the process, never panic, and never let a session start on a
  path that would produce broken output.

  ## FILES
  - `src/maestro/gpu/mod.rs` — new module (Maestro-only; nothing here is reachable from the `muse` bin)
  - `src/maestro/gpu/capability.rs` — pure parsers + `HwCapability` model + the probe runner
  - `src/maestro/backend/capabilities.rs` — spec B's `BackendCapabilities` descriptor; populate its
    hardware-encode fields from the probe (do not add a parallel capability concept)
  - `tests/fixtures/hwcaps/` — captured real command output for the parser fixtures

  ## APPROACH
  1. Model the result: `HwCapability { hwaccels: Vec<String>, encoders: Vec<String>,
     render_node: Option<PathBuf>, render_node_readable: bool, render_node_writable: bool,
     vaapi_smoke_ok: bool, probed_at: DateTime<Utc>, failure: Option<String> }`, plus
     `HwCapability::supports(codec: HwCodec) -> bool`.
  2. **Pure parsers, separately unit-tested against captured fixtures:**
     `parse_hwaccels(stdout: &str) -> Vec<String>` (from `ffmpeg -hide_banner -hwaccels`),
     `parse_encoders(stdout: &str) -> Vec<String>` (from `ffmpeg -hide_banner -encoders`,
     tolerant of the leading capability-flag columns). No process spawning in these functions.
  3. Impure probe runner `probe(cfg: &HwAccelConfig) -> HwCapability`: runs the two ffmpeg
     commands with a short timeout, then checks the render node from config (a DRM render-node
     path, **from config, never a literal in source**) for existence and R/W access for the
     service user.
  4. **Smoke test, not just presence.** A listed `h264_vaapi` encoder proves nothing if the
     driver cannot initialise. Encode a ~10-frame synthetic clip
     (`-f lavfi -i testsrc2=size=320x240:rate=10 -frames:v 10`) through the full
     `-init_hw_device vaapi ... -vf format=nv12,hwupload -c:v h264_vaapi` chain to
     `-f null -`. Non-zero exit or empty output ⇒ `vaapi_smoke_ok=false` with the stderr tail
     captured into `failure` (truncated, no paths beyond the configured device).
  5. Every failure mode returns a populated `HwCapability` with `failure: Some(..)` — never an
     `Err` that propagates to a request path, never a panic, never a process exit.
  6. Log the capability summary once at startup at INFO, and re-probe on demand via a
     `refresh()` that is rate-limited to at most once per `MUSE_MAESTRO_HWACCEL_REPROBE_SECS`.
  7. **Feed spec B's `BackendCapabilities`.** The probe runs ONCE at Maestro startup and its result
     populates the native backend's capability descriptor (e.g. `hw_encode: Option<HwEncodeCaps>`,
     `None` when unsupported). Everything downstream — session planning, the Activity panel, the
     assistant-operable tool surface — reads the descriptor, never re-probes. This is what makes a
     host with a broken VA-API stack degrade **silently at startup** rather than erroring or
     re-discovering the same failure on every session. If spec B's descriptor type is named
     differently, extend B's type; do not introduce a second capability concept.
  8. Use config helpers for the ffmpeg binary path and device path — no raw `std::env::var`,
     no string literals for device paths.

  ## TEST PLAN
  - `cargo test` — parser unit tests against fixtures: a host with VA-API, a host with none, a
    truncated/garbage stdout, and an ffmpeg that printed only a banner.
  - Unit test: probe with a nonexistent device path ⇒ `render_node: None`, `failure` set,
    no error returned.
  - Unit test: probe with a device path that exists but is unreadable ⇒ `render_node_readable
    = false`, `supports()` false for every codec.
  - Manual on a capable host: `probe()` reports `vaapi_smoke_ok = true`.
  - Manual on a host with no DRM render node at all: process starts normally, logs a single INFO,
    software path unaffected.
  - Unit test: the populated `BackendCapabilities` reports `hw_encode: None` on an unsupported host
    and `Some(..)` with the right codec list on a supported one.
  - Unit test: the probe runs exactly once at startup — a second session does not re-invoke ffmpeg
    (assert via a call-counting fake).
  - Verify no hardcoded IPs, hostnames, or device paths in new/modified files.

  ## EDGE CASES
  - `ffmpeg` binary missing entirely ⇒ `failure` set, no panic (spec E's existing
    `classify_spawn_error` shape is the precedent — reuse it).
  - ffmpeg present but built without VA-API ⇒ `hwaccels` lacks `vaapi`, encoders lack
    `*_vaapi` ⇒ unsupported, cleanly.
  - Encoder listed but driver init fails (the classic "looks supported, produces nothing")
    ⇒ caught by the smoke test, which is the entire reason it exists.
  - Probe command hangs ⇒ timeout kills the child; treat as unsupported.
  - Render node exists but the service user is not in the `render`/`video` group ⇒ readable
    check fails ⇒ unsupported, with a `failure` string that names the group problem so the
    runbook fix is obvious.

- **Acceptance criteria:**
  - [ ] `parse_hwaccels` and `parse_encoders` are pure, spawn nothing, and pass against ≥4 captured fixtures
  - [ ] A missing/unreadable render node yields an unsupported `HwCapability`, never an `Err` on a request path and never a panic
  - [ ] An encoder that is listed but fails to initialise is reported unsupported (smoke test)
  - [ ] Probe failures are logged once at startup, not per request
  - [ ] The probe populates spec B's `BackendCapabilities` descriptor; nothing downstream re-probes per session
  - [ ] No hardcoded infrastructure values or device paths in new/modified code
  - [ ] All existing tests still pass

---

### MGPU-02: Config surface — off by default, hardware caps distinct from software caps
- **Priority:** High
- **Labels:** maestro, gpu, config
- **Agent:** claude
- **Estimate:** 2h
- **Description:** One clear enable flag defaulting to **false**, plus hardware-specific
  concurrency caps and an explicit policy for what happens when the GPU becomes unavailable
  mid-session. Hardware sessions are cheap on CPU and expensive on a scarce shared resource, so
  they must not inherit the software concurrency limit.

  ## FILES
  - `src/config.rs` — add the `MUSE_MAESTRO_HWACCEL_*` fields to the crate's single env-reading
    door, following the existing `// --- ITEM-ID: description ---` banner convention
  - `src/maestro/config.rs` — `HwAccelConfig` as a Maestro-local sub-config built by
    `from_config(cfg: &crate::config::Config)`, mirroring the `foundry::config::FoundryConfig`
    pattern (sub-config structs never read env themselves)
  - `.env.example` — document every new var with its default and a one-line rationale
  - `README.md` — a short "hardware transcoding (opt-in)" section pointing at the runbook

  ## APPROACH
  1. `HwAccelConfig { enabled: bool, device: PathBuf, codecs: Vec<HwCodec>, max_sessions: usize,
     require_idle_gpu: bool, idle_floor_secs: u64, lease_wait_secs: u64,
     mid_session_policy: MidSessionPolicy, reprobe_secs: u64, arbiter: ArbiterKind }`.
  2. Env vars (all read through config helpers, never raw `std::env::var` at a call site):
     - `MUSE_MAESTRO_HWACCEL_ENABLED` — **default `false`.** The single flag. When false, nothing
       in this spec runs: no probe, no arbiter, no Chord call, no code path change.
     - `MUSE_MAESTRO_HWACCEL_DEVICE` — DRM render-node path. No default baked into source as a
       literal in a way that can drift; if unset while enabled, the feature reports itself
       unconfigured and degrades to software (the epic §7.4 convention).
     - `MUSE_MAESTRO_HWACCEL_CODECS` — comma list, default `h264`. HEVC output is opt-in on top.
     - `MUSE_MAESTRO_HWACCEL_MAX_SESSIONS` — **default `1`**, deliberately lower than the software
       cap. Distinct knob, not a multiplier of the software limit.
     - `MUSE_MAESTRO_HWACCEL_REQUIRE_IDLE_GPU` (default `true`) and
       `MUSE_MAESTRO_HWACCEL_GPU_IDLE_FLOOR_SECS` (default `120`) — the admission thresholds
       consumed by MGPU-04.
     - `MUSE_MAESTRO_HWACCEL_LEASE_WAIT_SECS` — default `0`. **Zero means never queue**: if the GPU
       is not available at admission, go software immediately. A viewer pressing play must not
       wait on a MINT sweep.
     - `MUSE_MAESTRO_HWACCEL_MID_SESSION_POLICY` — `yield_to_software` (default) | `finish` | `abort`.
     - `MUSE_MAESTRO_HWACCEL_POLL_SECS` — default `10`. The revocation-detection interval (§4.1);
       worst-case yield latency is this plus one segment duration.
     - `MUSE_MAESTRO_HWACCEL_MAX_LEASE_SECS` — default `7200`. Belt-and-braces hard bound so a
       wedged session cannot hold the GPU forever (§4.1).
     - `MUSE_MAESTRO_HWACCEL_REPROBE_SECS` — default `300`.
     - `MUSE_MAESTRO_HWACCEL_ARBITER` — `observe_only` (default) | `shared_lease` | `none`.
       `none` is a documented escape hatch for a host where Chord is not present at all; it is
       **not** a way to skip arbitration on the shared GPU host.
  3. `MidSessionPolicy` is an enum with a `FromStr` that rejects unknown values loudly at
     startup (fail-fast on config, never silently default a typo to the permissive option).
  4. Chord endpoint/credentials: base URL from config (`CHORD_CONTROL_URL` /
     `CHORD_PROXY_URL`), JWT via `SecretManager::get()` — never `std::env::var`, never a
     literal. If the secret is absent, the arbiter reports unconfigured and hardware stays off.

  ## TEST PLAN
  - `cargo test` — default config has `enabled == false` and `max_sessions == 1`.
  - Unit test: `MUSE_MAESTRO_HWACCEL_MID_SESSION_POLICY=nonsense` ⇒ startup config error naming the
    variable and the valid values.
  - Unit test: `enabled=true` with `device` unset ⇒ config resolves to "unconfigured", not an
    error, and `HwAccelConfig::effective_enabled()` is false.
  - Unit test: hardware cap is independent — setting the software concurrency limit does not
    change `max_sessions`.
  - Verify no hardcoded IPs, hostnames, or tokens in new/modified files.
  - Verify the Chord JWT is fetched via `SecretManager`, not `std::env::var`.

  ## EDGE CASES
  - `enabled=true` on a host with no GPU ⇒ MGPU-01 reports unsupported ⇒ every session is
    software; a single WARN at startup, not per session.
  - `max_sessions=0` ⇒ treated as "hardware disabled", logged as such (not as an infinite cap).
  - Both `enabled=true` and `arbiter=none` on the shared GPU host ⇒ allowed but logged at WARN
    with an explicit "no arbitration — GPU contention will be invisible" message.

- **Acceptance criteria:**
  - [ ] `MUSE_MAESTRO_HWACCEL_ENABLED` defaults to false and, when false, no probe/arbiter/Chord call occurs
  - [ ] Hardware concurrency cap is a distinct knob defaulting to 1, independent of the software cap
  - [ ] An invalid `MID_SESSION_POLICY` fails startup with a message naming the valid values (negative test)
  - [ ] `enabled=true` with an unconfigured device degrades to software rather than erroring
  - [ ] Chord credentials come from `SecretManager`, not env vars
  - [ ] `.env.example` and README document every new variable and its default
  - [ ] No new `Cargo.lock` entry requiring a C toolchain or `pkg-config`; the image still publishes on the musl-static default with no `TARGET_NATIVE=1` (§4.2)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MGPU-03: Hardware variant of spec E's pure argument builder
- **Priority:** Critical
- **Labels:** maestro, gpu, transcode
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Extend — do not fork — spec E's pure transcode argument builder with a
  hardware variant selected by config. Decode, scale, and encode stay on-GPU where the input
  codec permits, so frames never round-trip through system memory unnecessarily. **The
  builder's purity and testability must survive**: same function shape, same golden-fixture
  discipline, one more input.

  ## FILES
  - `src/maestro/transcode/args.rs` — spec E's builder; add the encode-target dimension
  - `src/maestro/transcode/args_hw.rs` — VA-API-specific argument fragments (kept separate for
    readability, still pure, called by `args.rs`)
  - `tests/fixtures/args/` — golden argv fixtures, hardware cases added alongside E's

  ## APPROACH
  1. **Take spec E's names as authoritative.** If E's builder is not exactly
     `build_transcode_args(plan: &TranscodePlan) -> Vec<String>` in `src/maestro/transcode/args.rs`,
     adopt E's actual names rather than renaming E's code. This item adds a field, not a
     module boundary.
  2. Add `EncodeTarget { Software, Vaapi { device: PathBuf, codec: HwCodec } }` as a field on
     the plan (or as a second builder parameter if E's plan type is deliberately
     device-agnostic). The **selection** of the variant happens in MGPU-05, outside the
     builder; the builder itself remains a total function of its inputs with no I/O, no clock,
     no env reads.
  3. Hardware argv shape (the load-bearing detail): `-init_hw_device
     vaapi=va:<device> -filter_hw_device va -hwaccel vaapi -hwaccel_output_format vaapi`
     before the input, then a filter chain that stays in VA-API surfaces
     (`scale_vaapi=w=…:h=…` — **not** `scale`), then `-c:v h264_vaapi` / `hevc_vaapi` with
     rate control expressed as the VA-API encoder actually accepts it (`-rc_mode`, `-b:v`,
     `-maxrate`, `-bufsize`, `-qp` where applicable) rather than copying x264's `-crf`/`-preset`
     vocabulary, which the VA-API encoders do not share.
  4. **Mixed-hardware fallback within the hardware path:** if the *input* codec is not
     hardware-decodable but the target encoder is available, emit a software-decode →
     `format=nv12,hwupload` → hardware-encode chain. This is a distinct, separately-fixtured
     shape, not an accidental one.
  5. Audio, subtitle, container, and segmenting arguments are **unchanged** and shared with the
     software path — the hardware variant only touches the video decode/scale/encode span.
     Any divergence there is a bug and is asserted against in tests.
  6. Golden fixtures: every hardware case gets a committed expected-argv file, generated the
     same way E generates its software fixtures.

  ## TEST PLAN
  - `cargo test` — golden-argv tests: h264_vaapi full-GPU chain; hevc_vaapi; software-decode +
    hwupload mixed chain; scale-down; no-scale passthrough resolution.
  - Property/assertion test: for the same plan, software and hardware argv agree on every
    audio, subtitle, container, and segment argument — only the video span differs.
  - Assertion test: no hardware argv contains a software-only flag (`-crf`, `-preset`) and no
    software argv contains `-init_hw_device`.
  - Assertion test: the builder is called twice with identical inputs and returns identical
    output (purity smoke).
  - Verify the builder module still spawns no process and reads no env (grep for `Command::`
    and `env::var` in `src/maestro/transcode/args*.rs` returns nothing).
  - Verify no hardcoded IPs, device paths, or org names in new/modified files.

  ## EDGE CASES
  - Odd/non-even target dimensions ⇒ VA-API scalers reject them; round to even in the plan and
    fixture the behaviour.
  - 10-bit HEVC input to an 8-bit H.264 target ⇒ explicit `format=nv12` in the chain; this is a
    pixel-format conversion, **not** tone-mapping (see §11).
  - Anamorphic / non-square-pixel sources ⇒ SAR must be preserved identically to the software
    path; fixtured.
  - Interlaced source ⇒ if deinterlacing is in E's software chain, the hardware chain uses
    `deinterlace_vaapi`; if E does not deinterlace, neither does this — no silent divergence.

- **Acceptance criteria:**
  - [ ] The hardware variant is a config-selected branch of spec E's existing builder, not a second builder
  - [ ] The builder remains pure: no process spawn, no env read, no clock, verified by test and grep
  - [ ] Golden argv fixtures cover full-GPU, mixed software-decode, and both target codecs
  - [ ] Software and hardware argv are identical outside the video decode/scale/encode span (asserted)
  - [ ] Negative test: hardware argv never contains `-crf`/`-preset`; software argv never contains `-init_hw_device`
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MGPU-04: Chord GPU arbitration client — acquire, heartbeat, release, yield
- **Priority:** Critical
- **Labels:** maestro, gpu, chord, arbitration
- **Estimate:** 5h
- **Agent:** claude
- **Description:** The load-bearing item. Maestro must never take the GPU silently. It asks
  Chord whether the GPU is available, announces itself for the duration, heartbeats, releases
  on every exit path, and yields the moment an exclusive holder appears. See §4 for the
  verified Chord contract — in particular, **Maestro must not take Chord's exclusive lease**,
  which 503s all inference and evicts resident models.

  ## FILES
  - `src/maestro/gpu/arbiter.rs` — `GpuArbiter` trait, `ObserveOnlyArbiter`, `NoopArbiter`,
    `SharedLeaseArbiter` (feature-complete but unwired pending the Chord follow-up)
  - `src/maestro/gpu/chord_client.rs` — thin typed client over Chord's endpoints
  - `src/maestro/gpu/decision.rs` — **pure** admission/yield decision functions

  ## APPROACH
  1. Pure decision core first, so the interesting logic is testable without a network:
     `fn decide_admission(status: &GpuStatus, activity: &GpuActivity, cfg: &HwAccelConfig,
     active_hw_sessions: usize) -> AdmissionDecision` returning
     `Grant | DenySoftware { reason }`. Reasons are a closed enum
     (`ExclusiveHeldByOther`, `ChordServing`, `IdleFloorNotMet`, `AtSessionCap`,
     `ChordUnreachable`, `Unconfigured`, `Unsupported`) — every one of them is a telemetry
     label in MGPU-08.
     `fn decide_yield(status: &GpuStatus, policy: MidSessionPolicy) -> YieldAction`
     returning `Continue | SwitchToSoftware | Abort`.
  2. `ChordClient` calls, all JWT-authed, all with short timeouts (≤2s) and **fail-safe
     semantics — unreachable Chord always means DENY, never assume the GPU is free**
     (the same posture the build scheduler takes with `fleet_quiet`):
     - `GET /v1/gpu-exclusive/status` — is anyone holding the exclusive lock?
     - `GET /admin/activity` — `{serving, inflight, idle_secs, last_request_unix}`.
  3. `ObserveOnlyArbiter` (default): `acquire()` runs `decide_admission`; on `Grant` it records
     a local lease record (holder label `maestro_transcode`, session id, acquired_at) and
     starts a heartbeat task on a `MUSE_MAESTRO_HWACCEL_POLL_SECS` timer that re-reads status and
     evaluates `decide_yield`. `release()` clears the record and stops the task.
  4. `SharedLeaseArbiter`: identical shape, additionally POSTing a **non-evicting shared**
     grant to Chord. **Not wired by default** — it is a compile-time-complete implementation
     awaiting a CHRD-project change (§10 follow-up). It must not be selectable in a way that
     silently no-ops if the endpoint 404s: an unknown-endpoint response is a hard config error
     at startup, not a runtime degradation.
  5. **Never hold a lease across an idle session.** The lease's lifetime is bound to an
     RAII guard tied to the ffmpeg child, not to the HLS session object: when the child exits,
     stalls past the session's idle timeout, or the session is torn down, the guard drops and
     releases. Additionally a reaper releases any lease whose session has had no segment
     request in `MUSE_MAESTRO_HWACCEL_LEASE_IDLE_SECS`.
  6. `Drop` + a shutdown hook release on process exit; a released-twice call is idempotent.
  7. `lease_wait_secs = 0` (default) means `acquire()` never blocks — one check, then software.
  8. **Do not depend on `terminus-rs` for this.** Terminus has a working client
     (`terminus_rs::intake::gpu_authority`, with an `ExclusiveGuard` and a `GpuLock` trait), and it
     was evaluated. It is rejected for three reasons: it drives the **exclusive** lease Maestro must
     never take (§4); its `acquire` has root-ish side effects (`systemctl stop` on policy services,
     `/`-level lock files) that are wrong for a media sidecar; and its useful internals
     (`chord_call`, the heartbeat thread) are module-private, so reuse means taking the whole
     `terminus-rs` dependency — which would also put the musl/no-OpenSSL posture of §4.2 at risk.
     The protocol is three JSON endpoints; Maestro implements ~60 lines against them. **Mirror
     `gpu_authority`'s proven behaviours** rather than its code: heartbeat well under the TTL,
     stop the heartbeat *before* releasing so none races in after, treat a heartbeat that returns
     `new_grant:true` as evidence Chord restarted (log it loudly), and fail closed on auth errors
     while failing open on transport errors only where the fail-open is safe — which, for Maestro,
     it is not: unreachable Chord means software (§MGPU-04 acceptance).

  ## TEST PLAN
  - `cargo test` — pure decision tests: exclusive lock held by another ⇒ `DenySoftware
    { ExclusiveHeldByOther }`; `serving=true` with `require_idle_gpu` ⇒ `ChordServing`;
    `idle_secs` below floor ⇒ `IdleFloorNotMet`; at cap ⇒ `AtSessionCap`; all clear ⇒ `Grant`.
  - Unit test: `decide_admission` with a Chord error/timeout input ⇒ `DenySoftware
    { ChordUnreachable }` (fail-safe, negative test — this must never return `Grant`).
  - Unit test: `decide_yield` for each `MidSessionPolicy` when a lock appears.
  - Integration test with a mock Chord HTTP server: acquire → heartbeat ×2 → lock appears →
    yield action emitted → release; assert exactly one release and no leaked task.
  - Integration test: dropping the guard without an explicit release still issues a release.
  - Integration test: mock Chord returns 401 ⇒ arbiter reports unconfigured, hardware disabled,
    no retry storm (bounded backoff, logged once).
  - Verify the JWT is read via `SecretManager` and never logged (grep the tracing calls).
  - Verify no hardcoded IPs, hostnames, or ports in new/modified files.

  ## EDGE CASES
  - Chord restarts mid-session (status 500s then recovers) ⇒ transient failures do not
    immediately yield; yield only after `N` consecutive failed polls (configurable, default 3),
    and the failure counter resets on success.
  - Two Maestro instances on one host ⇒ holder labels include the instance/session id so
    telemetry attributes correctly; the session cap is per-process and this limitation is
    documented rather than pretended away.
  - Clock skew between Maestro and Chord ⇒ never compute lease expiry from Chord's `since`
    string; use Chord's own expiry semantics and Maestro's local monotonic clock for heartbeats.
  - The GPU host legitimately has no Chord (`arbiter=none`) ⇒ admission always grants, and the
    startup WARN from MGPU-02 makes the posture visible.

- **Acceptance criteria:**
  - [ ] Maestro never calls `POST /v1/gpu-exclusive/acquire` (asserted by a grep test in CI) — the exclusive, model-evicting lease is not Maestro's to take
  - [ ] Admission and yield logic are pure functions with ≥8 unit tests including the unreachable-Chord fail-safe
  - [ ] An unreachable or 401ing Chord results in software transcode, never a hardware session
  - [ ] A lease is released on session end, on ffmpeg exit, on guard drop, and on process shutdown; release is idempotent
  - [ ] No lease persists across an idle session (reaper test)
  - [ ] A SIGKILLed hardware session releases its lease and leaves Muse untouched (extends the epic §10b chaos test)
  - [ ] The holder label contains none of `compiler`/`build`/`bld`, which Chord substring-matches as a compiler lease (§4)
  - [ ] Chord JWT via `SecretManager`, never logged
  - [ ] The Chord client uses the existing `reqwest`+`rustls` stack; no `native-tls`/OpenSSL dependency is introduced (§4.2)
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MGPU-05: Session integration — selection, caps, and the mid-session policy
- **Priority:** High
- **Labels:** maestro, gpu, transcode, sessions
- **Estimate:** 4h
- **Agent:** claude
- **Description:** Wire capability (MGPU-01), config (MGPU-02), builder variant (MGPU-03), and
  arbiter (MGPU-04) into spec E's session lifecycle: choose the encode target at session start,
  enforce the hardware concurrency cap, and handle the GPU going away mid-session.

  ## FILES
  - `src/maestro/transcode/session.rs` — spec E's session lifecycle; add encode-target selection
  - `src/maestro/gpu/selector.rs` — the pure `select_encode_target` function
  - `src/maestro/transcode/mod.rs` — hardware session counter/semaphore

  ## APPROACH
  1. `fn select_encode_target(cfg, caps: &HwCapability, admission: AdmissionDecision,
     plan: &TranscodePlan) -> (EncodeTarget, SelectionReason)` — **pure**. Hardware requires
     all of: enabled, configured, capability supports the target codec, admission granted,
     under the hardware cap. Anything else ⇒ `Software` with a reason.
  2. Hardware sessions take a separate `Semaphore` sized `max_sessions`; failure to acquire it
     immediately (no wait, per `lease_wait_secs=0`) ⇒ software. Software sessions keep spec E's
     own cap untouched.
  3. **Mid-session policy:**
     - `yield_to_software` (default) — finish the current segment, then transparently restart
       the encode as software **from the next segment boundary**, keeping the same session id
       and HLS playlist so the client sees a continuous stream. Release the lease at the
       switch. Count it in the fallback metric with `mid_session=true`.
     - `finish` — keep the hardware encode to the end of the session (bounded by the fact that
       the assistant will be contending). Documented as the option that prioritises the viewer
       over the assistant; **not** the default.
     - `abort` — end the session with a clean error the player can retry; for operators who
       want contention to be loud rather than absorbed.
  4. The switch must be **segment-aligned**. A mid-segment codec/parameter change is exactly
     how home-built transcoders produce unplayable output; if the current segment cannot be
     completed within a bounded time, fall back to `abort` rather than emitting a spliced
     segment.
  5. Selection reason and mid-session events are attached to the session record so MGPU-08 and
     the Activity panel (spec H) can show *why* a given playback used what it used.

  ## TEST PLAN
  - `cargo test` — `select_encode_target` truth table: every deny reason ⇒ `Software`; all-clear
    ⇒ `Vaapi`.
  - Unit test: cap of 1 ⇒ second concurrent hardware-eligible session selects software
    immediately (no blocking).
  - Integration test with a mock arbiter: policy `yield_to_software` ⇒ session id and playlist
    continuity preserved across the switch, lease released once, fallback metric incremented.
  - Integration test: policy `abort` ⇒ session ends with the documented error code.
  - Integration test: **revocation latency** — a lock appears at T; assert the hardware encode has
    stopped by T + `POLL_SECS` + one segment duration, and that the number matches what the runbook
    documents. This is the assertion that makes §4.1's contract real rather than aspirational.
  - Integration test: `MAX_LEASE_SECS` elapses with no revocation ⇒ the session yields to software
    anyway (belt-and-braces bound).
  - Integration test: a segment that cannot complete within the bound ⇒ abort, never a spliced
    segment (negative test).
  - Verify no hardcoded infrastructure values in new/modified files.

  ## EDGE CASES
  - Admission granted but the ffmpeg child fails to start on the hardware path ⇒ immediate
    one-shot software retry for the same session; counted as `fallback_reason=hw_spawn_failed`.
  - GPU disappears (driver reset / device node vanishes) mid-session ⇒ treated as a yield event
    and additionally triggers a capability re-probe.
  - Client seeks during a mid-session switch ⇒ seek wins; the new position starts on the
    already-selected target, no double switch.
  - `enabled=true` but every session denies ⇒ no error state; the deny-reason metric is the
    signal, and the runbook (MGPU-09) reads it.

- **Acceptance criteria:**
  - [ ] `select_encode_target` is pure and covered by a full truth table including every deny reason
  - [ ] Hardware concurrency is capped independently of software and never blocks a viewer
  - [ ] `yield_to_software` preserves session id and playlist continuity, segment-aligned
  - [ ] Revocation latency is bounded and asserted at `POLL_SECS` + one segment duration (§4.1)
  - [ ] `MAX_LEASE_SECS` bounds any single hardware lease even if every revocation path fails
  - [ ] A hardware spawn failure falls back to software for the same session without a client-visible error
  - [ ] Negative test: no session ever emits a segment spliced across an encoder change
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MGPU-06: Fail-safe degradation — never a dead process, never broken output
- **Priority:** High
- **Labels:** maestro, gpu, reliability
- **Estimate:** 2h
- **Agent:** claude
- **Description:** A broken VA-API stack is the expected steady state on most hosts, and a
  half-broken one is the dangerous state. This item makes both safe: the process never dies
  from a hardware problem, and a hardware session that is producing garbage is detected and
  replaced rather than served.

  ## FILES
  - `src/maestro/transcode/session.rs` — output sanity gate on the first segment
  - `src/maestro/gpu/health.rs` — hardware-failure classification + a circuit breaker

  ## APPROACH
  1. **First-segment sanity gate.** For a hardware session, the first produced segment is
     validated before it is served: non-zero size, parses as the expected container, and
     reports a plausible duration and the expected resolution. A failed gate discards the
     session's output, falls back to software, and trips the breaker.
  2. **Circuit breaker.** `MUSE_MAESTRO_HWACCEL_FAILURE_THRESHOLD` (default 3) consecutive hardware
     failures within `MUSE_MAESTRO_HWACCEL_FAILURE_WINDOW_SECS` (default 900) disables the hardware
     path for `MUSE_MAESTRO_HWACCEL_BREAKER_COOLDOWN_SECS` (default 3600) and logs once at WARN.
     This prevents a broken driver from converting every playback into a slow double-encode.
  3. Classify hardware failures from ffmpeg stderr into a closed enum
     (`DeviceOpenFailed`, `EncoderInitFailed`, `SurfaceAllocFailed`, `Unknown`) with a **pure**
     `classify_hw_failure(stderr_tail: &str) -> HwFailure` and fixtured stderr samples.
  4. Every hardware error path is `Result`-handled to a software fallback. No `unwrap`, no
     `expect`, no panic in the hardware modules — enforced by a clippy lint config on those
     modules and asserted in review.

  ## TEST PLAN
  - `cargo test` — `classify_hw_failure` against ≥4 captured stderr fixtures.
  - Unit test: breaker trips after N failures, stays tripped for the cooldown, resets after.
  - Integration test: a stubbed encoder that produces a zero-byte first segment ⇒ output is
    never served, session falls back to software, breaker increments.
  - Integration test: an encoder that produces a segment of the wrong resolution ⇒ same.
  - Chaos test: device node removed between probe and session start ⇒ software session, process
    alive, single WARN.
  - Verify no `unwrap`/`expect`/`panic!` in `src/maestro/gpu/**` (grep test).
  - Verify no hardcoded infrastructure values in new/modified files.

  ## EDGE CASES
  - ffmpeg exits 0 but produces an empty file (a real VA-API failure mode) ⇒ caught by the size
    check, which is why exit code alone is not the gate.
  - Breaker trips while sessions are in flight ⇒ existing sessions continue under the
    mid-session policy; only new sessions are affected.
  - A transient failure during a MINT sweep should not permanently trip the breaker ⇒ failures
    attributable to a yield/denial are not counted as hardware failures.

- **Acceptance criteria:**
  - [ ] A hardware failure never terminates the process and never surfaces as a client error when software can serve
  - [ ] The first hardware segment is validated before being served; invalid output is discarded, not delivered (negative test)
  - [ ] The circuit breaker trips, holds for the cooldown, and resets, with one WARN per trip
  - [ ] No `unwrap`/`expect`/`panic!` in the GPU modules (grep test)
  - [ ] Yield/denial events are not counted as hardware failures
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MGPU-07: Output parity verification — prove the hardware path is actually correct
- **Priority:** High
- **Labels:** maestro, gpu, testing, quality
- **Estimate:** 3h
- **Agent:** codex
- **Description:** "Hardware output is fine" is an assertion until it is measured. This item
  builds the harness that measures it: same duration, same resolution, same aspect ratio, no
  colour shift, actually playable — hardware output compared against the software output of the
  same source and plan.

  ## FILES
  - `tests/maestro_hw_parity.rs` — the harness (`#[ignore]` by default; runs only on a capable host)
  - `tests/fixtures/parity/` — short synthetic sources (generated by `lavfi`, committed as a
    generator script, not as binary blobs)
  - `scripts/maestro-hw-parity.sh` — operator-runnable wrapper that prints a pass/fail table

  ## APPROACH
  1. Sources are **generated, not committed**: `testsrc2`, `smptebars`, and a colour-ramp clip
     at 1080p and 2160p, 8-bit and 10-bit, ~10s each. This keeps the repo clean and the sources
     reproducible, and sidesteps any licensing question about sample media.
  2. For each source × plan, encode twice (software and hardware) and compare with `ffprobe`
     and ffmpeg filters:
     - **Duration** within ±1 frame interval.
     - **Frame count** exactly equal.
     - **Resolution** and **SAR/DAR** exactly equal.
     - **Colour metadata** — `color_primaries`, `color_transfer`, `color_space`, and
       `pix_fmt` compared field by field against the software output. A mismatch here is the
       "everything looks washed out / too green" bug, and it is a hard fail, not a warning.
     - **Similarity floor** — `ffmpeg -lavfi ssim` (and `psnr`) of hardware vs software output.
       Assert mean SSIM ≥ `PARITY_SSIM_MIN` (default 0.95) and no single frame below
       `PARITY_SSIM_FRAME_MIN` (default 0.90). Hardware encoders are legitimately not
       bit-identical to x264; the floor is what distinguishes "different encoder" from "broken".
     - **Playability** — the output demuxes and decodes end to end with zero decode errors
       (`-v error -f null -` produces empty stderr), and, for HLS output, the playlist and every
       segment parse and the segment durations sum to the source duration within tolerance.
  3. Also assert the **cost** side so the harness doubles as the evidence generator for §12:
     record wall-clock encode time and peak host CPU for both paths, and emit them as a small
     JSON report the runbook can cite.
  4. `#[ignore]`d by default with a clear skip message on hosts without VA-API, so `cargo test`
     stays green everywhere. CI runs the software half unconditionally as a control.

  ## TEST PLAN
  - `cargo test` (normal) — the harness compiles, is skipped, and the skip is visible.
  - `cargo test -- --ignored` on a capable host — all parity assertions pass for every
    source × plan combination.
  - Deliberate-break test: run the harness against an intentionally wrong plan (e.g. forced
    `yuv420p` vs `nv12` mismatch introduced in a test-only fixture) and assert the harness
    **fails** — a verification harness that cannot fail is not a verification harness.
  - `scripts/maestro-hw-parity.sh` prints a readable table and exits non-zero on any failure.
  - Verify no hardcoded infrastructure values, absolute paths, or org names in new files.

  ## EDGE CASES
  - Hardware encoder produces a legitimately different but acceptable bitrate ⇒ bitrate is
    reported, not asserted; SSIM is the quality gate.
  - Very short clips make frame-count comparisons brittle at segment boundaries ⇒ use sources
    that are an exact multiple of the segment duration.
  - 10-bit source to 8-bit target legitimately changes `pix_fmt` ⇒ the expected value comes
    from the *software* output for the same plan, never from the source, so the comparison is
    always software-vs-hardware, never source-vs-hardware.

- **Acceptance criteria:**
  - [ ] Parity harness compares duration, frame count, resolution, SAR/DAR, and all four colour metadata fields exactly
  - [ ] SSIM/PSNR floors are asserted with configurable thresholds and a per-frame minimum
  - [ ] Output playability is asserted by a zero-decode-error full decode pass
  - [ ] The harness is proven able to fail (deliberate-break test)
  - [ ] Harness is `#[ignore]`d by default so `cargo test` is green on hosts without VA-API
  - [ ] Encode wall-clock and peak CPU are recorded for both paths as a JSON report
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MGPU-08: Telemetry — the evidence that decides whether this was worth it
- **Priority:** High
- **Labels:** maestro, gpu, observability
- **Estimate:** 2h
- **Agent:** claude
- **Description:** Per-session encoder used, GPU lease wait time, and fallback-to-software
  counts, exported in Maestro's existing metrics surface and attached to the session record.
  Without this, "should we turn hardware on?" is answered by vibes.

  ## FILES
  - `src/metrics.rs` — new metric definitions in the crate's shared metrics module, following its
    existing `OnceLock<Registry>` + closed-`const`-label convention
  - `src/maestro/gpu/telemetry.rs` — recording helpers
  - `src/maestro/transcode/session.rs` — session-record fields

  ## APPROACH
  1. Metrics (names follow whatever prefix spec E/B established for Maestro; the *shape* is
     what matters):
     - `maestro_transcode_sessions_total{encoder="software|vaapi_h264|vaapi_hevc"}` — counter.
     - `maestro_hw_fallback_total{reason=<AdmissionDenyReason|hw_spawn_failed|first_segment_invalid|mid_session_yield|breaker_open>}`
       — counter. The reason label is the closed enum from MGPU-04/05/06, so a fallback is
       never unattributed.
     - `maestro_gpu_lease_wait_seconds` — histogram. With `lease_wait_secs=0` this is
       near-zero by construction; it becomes meaningful if queuing is ever enabled, and it is
       the number that proves whether waiting would have helped.
     - `maestro_hw_sessions_active` — gauge (must return to 0; a stuck gauge is the leak alarm).
     - `maestro_gpu_admission_checks_total{result="grant|deny"}` and
       `maestro_gpu_yield_events_total{action}`.
     - `maestro_transcode_realtime_ratio{encoder}` — histogram of encoded-seconds per
       wall-second. **This is the headline number for §12**: it is what tells you whether
       software was already fast enough.
     - `maestro_hw_breaker_state` — gauge 0/1.
  2. Session record gains `encoder_used`, `selection_reason`, `lease_wait_ms`,
     `mid_session_switches`, so spec H's Activity panel can show per-playback truth.
  3. One structured INFO log line per session close summarising encoder, reason, realtime
     ratio, and switches — greppable, and the thing the runbook tells an operator to read.
  4. No PII, no file paths, no titles in metric labels (label cardinality *and* sovereignty).

  ## TEST PLAN
  - `cargo test` — every fallback reason increments the counter with the right label
    (table-driven over the closed enum, so a new reason without a metric fails the test).
  - Unit test: `maestro_hw_sessions_active` returns to 0 after normal close, after abort, and
    after a mid-session yield.
  - Unit test: metric labels contain no file paths or titles (negative test on a session with a
    path-like title).
  - Manual: scrape the metrics endpoint with hardware disabled ⇒ the hardware metrics exist and
    read zero, rather than being absent (so a dashboard does not break on the flip).
  - Verify no hardcoded infrastructure values in new/modified files.

  ## EDGE CASES
  - Hardware disabled ⇒ metrics still registered at zero, so before/after comparison is possible
    from the same dashboard.
  - A session that never starts an encode (direct play) ⇒ no transcode metrics emitted at all.
  - Label cardinality: `reason` is a closed enum, never a free-form string from ffmpeg stderr.

- **Acceptance criteria:**
  - [ ] Per-session encoder, lease wait, and fallback reason are all recorded and exported
  - [ ] Every fallback reason in the closed enum has a corresponding metric label, enforced by a table-driven test
  - [ ] `maestro_hw_sessions_active` provably returns to zero on every exit path
  - [ ] `maestro_transcode_realtime_ratio` is recorded for both encoders
  - [ ] Metric labels contain no titles, paths, or other PII (negative test)
  - [ ] Hardware metrics are registered at zero when the feature is disabled
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MGPU-09: Enable/disable runbook + outside-in contention diagnosis
- **Priority:** High
- **Labels:** maestro, gpu, docs
- **Estimate:** 2h
- **Agent:** gemini
- **Type:** documentation
- **Description:** The operator-facing document for turning hardware transcoding on, turning it
  off, and — the part that actually saves an evening — telling from the outside whether a
  stutter is GPU contention rather than a Maestro bug or a network problem.

  ## AUDIENCE
  Operator (Moose), and any future agent debugging a "playback is stuttering" or "Lumina got
  slow" report.

  ## OUTLINE
  - **What this is and why it defaults off** (~200 words) — the shared-GPU argument, one
    paragraph, no hedging.
  - **Prerequisites** (~150 words) — VA-API-capable ffmpeg, render node present, the service
    user in the render group, Chord reachable and its JWT provisioned.
  - **Enable** (~250 words) — the exact ordered steps: confirm prerequisites → run
    `scripts/maestro-hw-parity.sh` and require a pass → set `MUSE_MAESTRO_HWACCEL_ENABLED=true` with
    `MUSE_MAESTRO_HWACCEL_MAX_SESSIONS=1` → restart via the module's normal deploy path → verify the
    startup capability log line → play one known-transcoding title and confirm
    `encoder_used=vaapi_*` in the session-close log.
  - **Disable / rollback** (~150 words) — flip the flag, restart; in-flight sessions finish
    under the mid-session policy. Emphasise that disabling is always safe and never loses a
    session.
  - **Is this stutter GPU contention?** (~400 words) — the decision tree, in order:
    1. `maestro_hw_fallback_total{reason="mid_session_yield"}` rising ⇒ something else took the
       GPU. Confirm with Chord's `GET /v1/gpu-exclusive/status` — a holder like a MINT sweep or
       the compiler names itself.
    2. `maestro_hw_sessions_active > 0` at the same time as high Chord latency ⇒ we are the
       contender, not the victim.
    3. `maestro_transcode_realtime_ratio` below 1.0 ⇒ genuinely not keeping up, regardless of
       who holds what.
    4. All of the above quiet ⇒ it is not GPU contention. Look at network, disk read on the
       library mount, or the client.
    Include the "presents as *Chord is slow*" warning explicitly, since that is the
    misdiagnosis this whole section exists to prevent.
  - **Known-good and known-bad symptoms** (~200 words) — what a healthy startup log looks like;
    what a permissions failure looks like; what a listed-but-broken encoder looks like.
  - **Config reference table** (~200 words) — every variable, default, and effect.

  ## SOURCES
  - `src/maestro/gpu/capability.rs`, `src/maestro/gpu/arbiter.rs`, `src/maestro/gpu/decision.rs`
  - `src/config.rs` and `src/maestro/config.rs` (the `HwAccelConfig` block)
  - `S130-maestro-epic.md` §5, §10.5
  - This spec §4 (the verified Chord contract) and §12 (the evidence bar)

  ## TONE
  Direct operational reference. No hardcoded infrastructure values — env var placeholders only.
  Every diagnostic step must be a command or a metric an operator can actually run, not advice.

- **Acceptance criteria:**
  - [ ] Runbook committed at `docs/maestro-hardware-transcoding.md` and linked from README
  - [ ] Enable procedure requires a passing parity run before the flag is flipped
  - [ ] Contention decision tree distinguishes "we are the victim" from "we are the cause"
  - [ ] Every config variable is documented with its default
  - [ ] No hardcoded infrastructure values, IPs, hostnames, or ports anywhere in the document

---

### MGPU-10: Evidence review — decide whether to enable, on data
- **Priority:** Medium
- **Labels:** maestro, gpu, ops
- **Agent:** <operator>
- **Estimate:** 30m
- **Type:** human-action
- **Description:** After spec E has been live long enough to produce real numbers, review them
  against the bar in §12 and make an explicit, recorded decision to enable or leave disabled.
  This item exists so the decision is made once, on evidence, rather than drifting.
- **Steps:**
  1. Collect ≥14 days of spec E telemetry: transcode session count, the realtime-ratio
     histogram for the software encoder, peak concurrent transcodes, and host CPU saturation
     during them.
  2. Compare against the §12 bar. If none of the three conditions is met, record "leave
     disabled" and the numbers that say so — that is a successful outcome, not a wasted spec.
  3. If the bar is met, run `scripts/maestro-hw-parity.sh` on the target host and require a pass.
  4. Enable per the MGPU-09 runbook with `MUSE_MAESTRO_HWACCEL_MAX_SESSIONS=1`, observe for a week,
     then decide whether to raise the cap.
  5. Record the decision and the numbers in the Plane item before closing it.

---

## 6. Dependency and ordering

MGPU-01 and MGPU-02 are independent and start in parallel. MGPU-03 needs spec E merged.
MGPU-04 is independent of the ffmpeg work and can be built in parallel with 01–03 (its pure
core needs nothing). MGPU-05 needs 01–04. MGPU-06 needs 05. MGPU-07 needs 03 and 05. MGPU-08
needs 04 and 05. MGPU-09 needs everything. MGPU-10 is gated on live telemetry, not on code.

**Upstream of all of it:** the §7.0 entry condition. Per the epic §4, F depends on
"E **+ E's telemetry**" — none of these items may be created in Plane until E's
transcode-frequency number exists and clears the §12 build gate.

## 7. Pre-flight

### 7.0 ENTRY CONDITION — mechanical, not advisory

Per the epic §4: **F's Plane items are not created until spec E reports its transcode-frequency
telemetry.** This is a gate on *writing the items at all*, not on merging them. Do not run
`plane_prefix_promote` for MGPU or create a single MGPU issue until the following is recorded:

- [ ] Spec A's **direct-play fraction** is known: what share of the library direct-plays to the
      devices we actually own (epic §6).
- [ ] Spec E has run in production for **≥14 days** and reported: total transcode sessions, the
      **share of playbacks that reach tier 4** (full video re-encode), the software
      `realtime_ratio` histogram, and peak concurrent full transcodes.
- [ ] Those numbers are checked against the §12 bar and the decision to build is **recorded in the
      epic's Plane item** with the numbers attached.

**The number that justifies building F at all:** full-transcode sessions must be **≥10% of
playbacks** *and* the software `realtime_ratio` must fall below **~1.2×** on a material share of
them (or peak concurrency must reach 2+ with the host CPU saturated). If tier-4 transcode is under
~5% of playbacks, or software sustains >2× realtime, **F should not be built yet** — the correct
action is to record that and revisit after the device matrix or the library changes.

**This is a live possibility, not a formality.** The epic's whole §6 inversion is the bet that most
playback direct-plays. If that bet is right, F is optimising something that barely happens, on a GPU
the household is using for something it values more. §10b already anticipates this: the Maestro-host
recommendation is "run alongside Muse for the CPU tiers, and revisit only if spec F is ever
justified." A recorded "not justified, here are the numbers" is a successful outcome of this gate.

### 7.1 Standard pre-flight (once 7.0 passes)

- [ ] Spec E (MTRX) merged; its argument-builder and session module names recorded so MGPU-03
      extends rather than forks them
- [ ] Spec B's `BackendCapabilities` descriptor exists and its hardware-encode field shape is agreed
      with B's author, so MGPU-01 extends it rather than inventing a parallel one
- [ ] `plane_prefix_check` → `register` → `promote` for `MGPU` (the epic's pre-flight registers
      it; confirm rather than assume)
- [ ] Confirm the intended Maestro host's ffmpeg is built with VA-API, the render node exists,
      and the service user can open it — **if not, MGPU-01 still ships; it just always reports
      unsupported.** Do not block the spec on the hardware being ready.
- [ ] Confirm Chord's JWT secret is provisioned for Maestro in <secret-manager>. **Known fleet
      hazard:** an unprovisioned Chord JWT has previously produced silent, false-green
      behaviour elsewhere in the fleet. Here it is fail-safe by design (401 ⇒ hardware off),
      but it must be verified rather than assumed, or the feature will look "enabled but never
      used" with no obvious cause.
- [ ] Confirm the `maestro` `[[bin]]` target exists in the Muse `Cargo.toml` and its unit ships in
      the shared OCI image (`OCI_INSTALL` carries both `muse` and `maestro`) — spec B's pre-flight;
      confirm rather than assume, since MGPU-02's musl/no-`TARGET_NATIVE` criterion is checked
      against that publish command (§4.2)
- [ ] Baseline: `cargo test` green on Muse main; record the count

## 8. Exit criteria

1. All ten items merged, each through the full pipeline with `post-merge.sh` run.
2. `MUSE_MAESTRO_HWACCEL_ENABLED` defaults to false and the default build behaves **byte-identically**
   to spec E — same argv, same sessions, same metrics values (asserted, not assumed).
3. On a VA-API-capable host with the flag on, `cargo test -- --ignored` parity passes.
4. On a host with no VA-API, with the flag on, every session is software, the process is
   healthy, and exactly one WARN is logged at startup.
5. Maestro never acquires Chord's exclusive lease (grep test in CI).
6. `oci-publish.sh muse moosenet/Muse main muse maestro` succeeds on the **musl-static default**
   after all ten items merge — no new linked library, no `TARGET_NATIVE=1` (§4.2). Both bins
   deploy and health-gate as one all-or-nothing image.
7. Revocation is proven: a lock appearing mid-transcode yields to software within
   `POLL_SECS` + one segment duration, segment-aligned, session id preserved (§4.1).
8. The runbook exists and its contention decision tree has been walked once, end to end, by the
   author against live metrics.

**Explicitly NOT exit criteria** — none of these may be added to this spec's definition of done:
- **HDR tone-mapping** (§11). Fenced by the epic §8.3 and re-fenced here. It is the known scope
  sink for this spec; if it appears in a PR against an MGPU item, that is out-of-scope creep and
  the review should reject it on that basis alone.
- A Chord shared-lease / revocation endpoint (§4.1, §10) — a CHRD-project follow-up.
- The feature being enabled in production (§12 and MGPU-10 decide that separately, on numbers).

## 9. Risks

1. **We become the contention we were trying to arbitrate.** Mitigated by default-off, cap of
   1, `require_idle_gpu`, and yield-on-lock — but the honest residual is that arbitration is
   cooperative and advisory until the Chord follow-up lands. Documented, not hidden.
2. **The parity harness rots.** It is `#[ignore]`d, so it only runs when someone remembers.
   Mitigated by making `scripts/maestro-hw-parity.sh` a required step in the enable runbook, so it runs
   at exactly the moment it matters.
3. **VA-API on gfx1151 is less mature than the Intel/NVIDIA paths.** Encoder support, rate
   control modes, and 10-bit behaviour may differ from the documentation. MGPU-01's smoke test
   and MGPU-07's parity floors are the defence: we assert what the host actually does rather
   than what the driver claims.
4. **Complexity added for a benefit that never materialises.** This is a real risk and §12 is
   the answer: if the evidence bar is never met, the correct outcome is a well-built, tested,
   permanently-off feature. That is cheaper than the same feature bolted on under pressure
   later, but it is not free, and MGPU-10 should be willing to conclude "leave it off."

## 10. Follow-ups (not this spec)

- **CHRD: a non-evicting shared GPU lease.** Chord's only lease today is exclusive and evicts
  resident models (§4). A `mode:"shared"` advisory grant — visible in
  `GET /v1/gpu-exclusive/status`, non-evicting, non-503ing — would let Maestro announce itself
  properly instead of merely observing, and would let MINT see a transcode in progress and
  defer. `SharedLeaseArbiter` is written to consume it. **This is a Chord-repo spec, not a
  Maestro item.**
- Bringing linear-channel / channel-tuner transcodes (if they land) under the same arbitration.
- Raising the hardware session cap above 1 once contention data exists.

## 11. HDR tone-mapping — deliberately parked

The epic (§8.3) places HDR tone-mapping out of scope through spec E and says that if it belongs
anywhere it is here. **It is not in this spec's scope and not in its exit criteria.**

Why it is parked rather than absorbed: tone-mapping is the highest-effort, lowest-certainty
piece of the entire playback stack. It is GPU- and driver-dependent, its correctness is
subjective in a way that resists the golden-fixture discipline the epic §7.3 mandates, and
every home media project that has attempted it has lost more time to it than to everything else
combined. Bolting it onto a spec whose actual job is "arbitrate a shared GPU safely" would put
the arbitration work — the part that protects the household's assistant — behind an
open-ended colour-science problem.

**The correct current behaviour, unchanged by this spec:** direct-play HDR to capable devices
(epic §6, and the closed device matrix makes this genuinely viable). An SDR target that needs
HDR content gets whatever spec E already does; this spec does not change it in either direction.

If tone-mapping is wanted later it should be its own spec with its own acceptance criteria,
its own reference clips, and its own honest assessment of whether the device matrix actually
needs it. `MGPU-03`'s pixel-format handling (10-bit → `nv12`) is a **format conversion, not
tone-mapping**, and the distinction is asserted in that item's edge cases specifically so a
future reader does not mistake one for the other.

## 12. The honest assessment: what evidence would justify building this, and what would justify turning it on

The GPU this spec contends for is shared with inference the household uses interactively, and
the host CPU is a 16-core Zen 5. **Software x264 may simply be sufficient**, and if it is, the
right decision is to never build this, or to build it and leave it off forever and be glad it was
cheap to leave off.

There are **two distinct gates**, both reading the same numbers:

| Gate | When | Who | Effect of failing |
|---|---|---|---|
| **Build gate** (§7.0) | Before any MGPU Plane item exists | Epic owner | F is not written; revisit later |
| **Enable gate** (MGPU-10) | After all ten items merge | Operator | Feature ships, stays off |

The build gate is the stricter and more consequential of the two, because it is the one that can
save the whole spec's effort. The enable gate only decides a config flag.

Enable (or build) only if **at least one** of these is true, measured over ≥14 days:

1. **Software cannot keep up.** `maestro_transcode_realtime_ratio` for the software encoder
   drops below ~1.2× on a material fraction (>10%) of transcode sessions. Below 1.0× is a
   stutter; 1.0–1.2× is no headroom for a second stream.
2. **Concurrency exceeds the CPU.** Peak concurrent full transcodes regularly reaches 2+ **and**
   host CPU saturates during them such that another Constellation workload on the same host is
   measurably delayed.
3. **Transcoding is common enough to matter at all.** Full-transcode sessions are a meaningful
   share of playbacks. Per the epic §6, spec A's direct-play backfill number sizes this: if
   most of the library direct-plays to the devices we own, tier-4 transcode is an edge case and
   accelerating it optimises something that barely happens.

**Evidence that would argue against enabling, and should be treated as decisive:**
- Software transcodes comfortably above 2× realtime for the resolutions actually in the library.
- Concurrent full transcodes are rare (the household is small and the device matrix is closed).
- The GPU is busy with inference during exactly the hours playback happens — i.e. evenings. In
  that case hardware transcoding would fall back to software precisely when it was needed, and
  would have bought contention risk in exchange for nothing.

**The most likely outcome, stated plainly so nobody is surprised by it:** the epic's §6 inversion
bets that most playback direct-plays to a small, closed, modern device matrix. If that bet is
right — and it probably is — full transcode is rare, software handles the rare cases at >2×
realtime, and **this spec's correct end state is "built, tested, permanently off"** or "not built
at all". Both are successes. The failure mode to avoid is not "we didn't enable it"; it is
"we enabled it, contended with the assistant every evening, and diagnosed it as Chord being slow."

A well-built feature that the data says to leave disabled is a correct outcome. §7.0 records the
numbers before the work starts; MGPU-10 records them again before the flag flips.
