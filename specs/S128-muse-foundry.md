# MUSE Foundry — media formatting, subtitles, and library organization

plane_project: MUSE
module: Muse
prefix: MUSEF
spec_id: S128-muse-foundry

## Metadata
- **Author:** Moose (operator) / Claude (scoping)
- **Session:** S128
- **Date:** 2026-07-27
- **Module version:** Muse (post-S125, main `d5f176e`)
- **Estimated total:** ~118h autonomous agent work across 6 phases
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7 (see "Module Contract compliance")
- **Context:** MUSE already owns the *acquisition* and *curation* halves of the
  media lifecycle (arr ingest, <media-service> requests, prowlarr reports, library
  scan + still-frame matching, taste model). It owns nothing between "a file
  landed" and "a client plays it well." That gap is currently assigned to two
  containers that have **never been configured and have never done any work**
  (`tdarr`: 0 libraries / 0 nodes; `bazarr`: 0 providers, *arr integration
  off) — see `docs/ARR-SUITE-GRAPH.md` for the full live survey. Foundry
  fills the gap natively: a transcode fabric with distributed worker nodes, a
  subtitle matcher/downloader, and a library organizer, unified under one
  policy engine, one job queue, one audit trail, and one Terminus-fronted
  control surface the assistant can drive.

> **Approval gate.** This spec is scoped but NOT ingested. Phases 2+ mutate
> real media files. Per the operator's instruction, Foundry's functions are
> exercised in the dev sandbox and approved before any policy is pointed at
> the live library. Phase 1 is read-only and safe to build immediately.

---

## Pre-flight

- Repository: `Muse` on Gitea (`MUSE_REPO`), branch `main` at `d5f176e`
- Working directory: the Muse checkout on the dev box
- Build/test host: **not the dev box** — submit via the Terminus compiler tool
  (`compiler_build(module="muse", ref=<branch>, mode=test)`), per skill v4.2
- Dependencies (runtime, on Foundry worker hosts only): `ffmpeg`/`ffprobe`
  (jellyfin-ffmpeg build preferred), `HandBrakeCLI`
- Vault secrets required (<secret-manager>, materialized at runtime — never authored
  into `.env` by hand): `MUSE_DATABASE_URL`, `OPENSUBTITLES_API_KEY`,
  `CHORD_CONTROL_URL`, `CHORD_JWT_SECRET`, `MUSE_FOUNDRY_NODE_TOKEN`
- Config (non-secret, `src/config.rs` helpers): `MUSE_FOUNDRY_ALLOWED_ROOTS`,
  `MUSE_FOUNDRY_SANDBOX_ROOT`, `MUSE_FOUNDRY_WORK_DIR`,
  `MUSE_FOUNDRY_ENABLE_MUTATION` (default **false**),
  `MUSE_FOUNDRY_FFMPEG_BIN`, `MUSE_FOUNDRY_HANDBRAKE_BIN`
- Infrastructure: Postgres reachable (`MUSE_DATABASE_URL`), Chord control API
  reachable for GPU leasing, the ARR host's sandbox volume mounted
- Baseline tests: current Muse `cargo test --workspace` count
- Baseline verify: current `docs/behavior-spec.md` score

**Plane project (verified live 2026-07-27):** Muse has its own project,
**`MUSE`** (54 existing items across the MUSEM / MUSEL / MUSEX / EMB-MUSE
sprints). The `moosenet-spec` skill's project list — `HARM`/`LUM`/`CHRD`/
`TERM`/`RAIL`/`HW`/`PSH` — is **stale**: `plane_list_projects` returns 13
projects including `MUSE`, and `plane_prefix_register` accepts `MUSE` as a
project value. This spec targets `MUSE` and takes the `MUSEF` prefix to match
the established `MUSE*` sprint family. (Skill correction filed as a follow-up.)

---

## Module Contract compliance

1. **Terminus-fronted.** Every Foundry capability is exposed as `foundry_*`
   tools on the Muse module surface, federated through Terminus. Foundry holds
   no forge/Plane/GitHub credentials and opens no egress except to its own
   configured subtitle providers and to Chord.
2. **Capability-gated.** The Foundry surface registers only when
   `MUSE_FOUNDRY_ALLOWED_ROOTS` is non-empty and a probe binary resolves.
   Absent those, Foundry is inert, never broken.
3. **Context-bus citizen.** Publishes `media.formatting.*` events (job
   started/completed, library compliance changed, subtitle acquired) and
   consumes playback context from `src/tautulli` + `src/tracker` so it can
   prioritize files the household is about to watch.
4. **Assistant-operable.** Every action — probe, plan, submit, cancel,
   organize, fetch subtitles — is invocable through the assistant via the
   `foundry_*` tools, not only via HTTP.
5. **Embeddable presentation.** Foundry renders inside the existing MUSE web
   module (`harmony-web /muse`, S126 design system), not as a standalone app.
6. **Sovereign + private.** No telemetry. Subtitle provider calls send a file
   hash and title, never a path or user identity. Provider use is opt-in per
   provider.
7. **Standalone-excellent first.** Phases 1–5 deliver a complete formatting
   system usable without the shell.

---

## Architecture

Four subsystems under `src/foundry/`, one policy engine, one queue.

```
                    ┌─────────────────────────────────────┐
                    │  Policy engine (client profiles)    │
                    │  Plex · Jellyfin · Emby · Kodi      │
                    └──────────────┬──────────────────────┘
                                   │ compliance verdict
   ┌──────────┐   probe   ┌────────▼────────┐   plan    ┌──────────────┐
   │ Library  ├──────────►│  Foundry core   ├──────────►│  Job queue   │
   │ scan     │           │  (probe+policy) │           │  (Postgres)  │
   └──────────┘           └─────────────────┘           └───────┬──────┘
                                                                │ lease
        ┌───────────────────────┬───────────────────────┬───────▼──────┐
        │ Forge (transcode)     │ Lexicon (subtitles)   │ Archivist    │
        │ HandBrake / ffmpeg    │ providers + matching  │ (organizer)  │
        └───────────┬───────────┴───────────┬───────────┴───────┬──────┘
                    │                       │                   │
              ┌─────▼─────┐           ┌─────▼─────┐       ┌─────▼─────┐
              │ muse-node │  …  N     │  sidecar  │       │ verify +  │
              │ (fabric)  │           │  writer   │       │ atomic mv │
              └───────────┘           └───────────┘       └───────────┘
```

- **Foundry core** — `probe` (ffprobe → typed stream model), `policy`
  (client-profile compliance matrix), `plan` (per-stream copy/re-encode/drop
  decision). Pure, read-only, deterministic, unit-testable without media.
- **Forge** — executes a plan via a pluggable encoder backend. Encodes to the
  work dir, verifies, then atomically swaps with retention. Never in-place.
- **Fabric** — the Tdarr-style distributed layer: `muse-node` agents register
  with the Muse server, advertise capabilities (codecs, hwaccel, cores, cache
  size), lease jobs, and stream progress. The server never pushes; nodes pull.
- **Lexicon** — subtitle inventory (embedded + sidecar), provider search
  (hash-first, then title/release match), scoring, sync verification, sidecar
  write. The Bazarr replacement.
- **Archivist** — library layout: naming/folder policy per library, dry-run
  plan → operator/assistant approval → seed-safe apply.

### Safety model (load-bearing — the library is 27 TB of irreplaceable data)

Five rails, each independently sufficient to prevent catastrophe:

1. **Default-deny root allowlist.** Every path operation resolves through
   `MUSE_FOUNDRY_ALLOWED_ROOTS`; a path outside it is a hard error. Default is
   the sandbox root only.
2. **Mutation kill-switch.** `MUSE_FOUNDRY_ENABLE_MUTATION` defaults false.
   With it false, Forge/Archivist produce plans and never touch a byte.
3. **Never in-place.** Output goes to the work dir on a different device;
   the original is replaced only after verification, and retained in a Foundry
   recycle bin for `MUSE_FOUNDRY_RETENTION_DAYS`.
4. **Verify-before-swap.** Duration within tolerance, expected stream count and
   types present, container parses, optional VMAF sample above floor. Any
   failure = keep the original, mark the job failed.
5. **Hardlink/seed guard.** `st_nlink > 1` means an active torrent hardlink;
   the file is skipped with an explicit reason unless the plan is copy-on-write.

---

## Phase 1 — Foundry core (read-only, no mutation)

### MUSEF-01: Foundry config, allowlisted roots, and safety rails
- **Priority:** Critical
- **Labels:** muse, foundry, safety, config
- **Agent:** claude
- **Estimate:** 4h
- **Phase:** 1
- **Description:** Establish the Foundry configuration surface and the path
  safety primitive every later item depends on. Nothing in Foundry may touch a
  path except through the resolver introduced here.

  ## FILES
  - `src/foundry/mod.rs` — new module, re-exports
  - `src/foundry/config.rs` — `FoundryConfig` loaded via `crate::config`
  - `src/foundry/paths.rs` — `ResolvedPath` + `PathGuard`
  - `src/config.rs` — add Foundry config helpers
  - `.env.example` — document new vars with safe defaults
  - `README.md` — Foundry section (new module, user-facing)

  ## APPROACH
  1. Add `FoundryConfig` with `allowed_roots: Vec<PathBuf>`, `work_dir`,
     `sandbox_root`, `enable_mutation: bool` (default false),
     `retention_days`, `ffmpeg_bin`, `handbrake_bin`. Read every value through
     `crate::config` helpers — never a scattered `std::env::var`.
  2. Implement `PathGuard::resolve(&self, p: &Path) -> Result<ResolvedPath>`:
     canonicalize, reject symlink escape, reject any path not under an allowed
     root. `ResolvedPath` is the only type later items accept — make the unsafe
     path unrepresentable rather than checked-by-convention.
  3. Add `PathGuard::require_mutation()` returning `Err` when
     `enable_mutation` is false, so a mutating call site cannot forget the gate.
  4. Register Foundry as capability-gated: an empty `allowed_roots` yields
     `None` and the whole surface stays unregistered (Module Contract §2).
  5. No secrets here — Foundry's secrets (provider keys, node token) arrive in
     MUSEF-16 and MUSEF-11 via `SecretManager::get()`.

  ## TEST PLAN
  - `cargo test -p muse foundry::paths` — resolve inside root succeeds;
    outside root, `..` traversal, and symlink-to-outside all rejected
  - Mutation gate returns `Err` when `enable_mutation` is false
  - Empty `allowed_roots` yields an unregistered surface
  - Verify no hardcoded IPs, hostnames, or org names in new/modified files
  - Verify secrets accessed via `SecretManager`, not `std::env::var`

  ## EDGE CASES
  - A root that does not exist at startup — log and drop that root, do not panic
  - Symlink pointing outside an allowed root — rejected (do not follow)
  - Path with non-UTF8 bytes — handled via `OsStr`, not lossy conversion
  - `work_dir` on the same device as an allowed root — warn (breaks rail 3)
  - Relative path input — resolved against nothing; rejected outright

- **Acceptance criteria:**
  - [ ] `ResolvedPath` cannot be constructed except through `PathGuard`
  - [ ] Traversal, symlink-escape, and outside-root paths are all rejected
  - [ ] `enable_mutation=false` blocks every mutating entry point
  - [ ] Empty `allowed_roots` leaves the Foundry surface unregistered
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README updated to document the Foundry module and its config
  - [ ] All existing tests still pass

### MUSEF-02: Media probe — ffprobe-backed typed stream model
- **Priority:** Critical
- **Labels:** muse, foundry, probe
- **Agent:** claude
- **Estimate:** 5h
- **Phase:** 1
- **Description:** Turn a media file into a typed, complete description of its
  container and streams. This is the input to every policy decision.

  ## FILES
  - `src/foundry/probe.rs`
  - `src/foundry/models.rs` — `MediaProbe`, `VideoStream`, `AudioStream`,
    `SubtitleStream`, `ContainerInfo`
  - `src/fixtures/foundry/` — captured ffprobe JSON fixtures

  ## APPROACH
  1. Invoke `ffprobe -v error -print_format json -show_format -show_streams
     -show_chapters` on a `ResolvedPath`, with a wall-clock timeout.
  2. Parse into typed models: codec name + profile + level, pixel format, bit
     depth, resolution, frame rate, HDR/colour metadata (`color_transfer`,
     `color_primaries`, `master_display`), audio codec/channels/layout/sample
     rate, subtitle codec + `language` + `title` + forced/default disposition,
     per-stream bitrate with a container-level fallback when `bit_rate` is
     `N/A` (observed on mkv and mpegts).
  3. Compute derived facts the policy engine needs: `is_hdr`, `is_10bit`,
     `is_lossless_audio`, `is_image_subtitle` (pgs/vobsub/dvbsub),
     `is_text_subtitle` (subrip/ass/ssa/mov_text), `effective_bitrate`.
  4. Normalize language tags to ISO 639-2/B, preserving the raw tag alongside
     the normalized one so a *mislabeled* tag stays visible (see MUSEF-05).
  5. Capture fixtures from the six sandbox samples and parse them in tests —
     no media files in the repo, only the ffprobe JSON.

  ## TEST PLAN
  - Parse all six captured fixtures; assert codec/profile/channels/subtitle
    disposition per fixture
  - `bit_rate: "N/A"` falls back to the container bitrate
  - mpegts fixture with a duplicated program block parses to one stream set
  - Missing/absent `ffprobe` binary yields a typed error, not a panic
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Multi-program mpegts (the m2ts sample emits duplicate stream blocks)
  - Zero-byte or truncated file — typed `ProbeError::Unreadable`
  - File with no video stream (audio-only) — valid, not an error
  - `channel_layout: "unknown"` (observed on the wmv sample)
  - ffprobe writes to stderr but exits 0 — treat stdout JSON as authoritative
  - Probe timeout on a huge remote file — bounded, returns `ProbeError::Timeout`

- **Acceptance criteria:**
  - [ ] All six sandbox fixtures parse to correct typed models
  - [ ] Bitrate falls back to container level when the stream reports `N/A`
  - [ ] HDR, 10-bit, lossless-audio and image-subtitle flags are derived
  - [ ] Raw and normalized language tags are both retained
  - [ ] Absent or failing `ffprobe` returns a typed error, never a panic
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MUSEF-03: Client compliance matrix and policy engine
- **Priority:** Critical
- **Labels:** muse, foundry, policy
- **Agent:** claude
- **Estimate:** 8h
- **Phase:** 1
- **Description:** Encode what each target client can direct-play, and evaluate
  a `MediaProbe` against a chosen profile set. This is the module's core
  intellectual content — everything else is plumbing around it.

  ## FILES
  - `src/foundry/policy/mod.rs`
  - `src/foundry/policy/profiles.rs` — built-in client profiles
  - `src/foundry/policy/evaluate.rs` — probe × profile → verdict
  - `migrations/00NN_foundry_policies.sql` — operator-defined policy overrides
  - `README.md` — policy documentation

  ## APPROACH
  1. Model a `ClientProfile` as declarative capability sets: allowed video
     codecs with max profile/level/bit-depth, allowed audio codecs with max
     channels, allowed containers, subtitle codecs supported natively vs
     requiring burn-in, and HDR tone-map behavior.
  2. Ship built-in profiles for **Plex**, **Jellyfin**, **Emby**, **Kodi**,
     each split into a conservative "broad client" tier (covers older/limited
     endpoints) and a "modern client" tier. Encode the real divergences: text
     subtitle handling (ASS/SSA rendering differs), HEVC support breadth,
     lossless-audio passthrough, mpegts/asf/avi container acceptance.
  3. `evaluate(probe, &[profiles]) -> ComplianceVerdict` returning per-stream
     `Compliant | Remux | Reencode { reason }` and an overall verdict that is
     the *worst* across the selected profiles — the "best native format" is the
     intersection of what all selected clients direct-play.
  4. Persist operator overrides in a `foundry_policies` table (profile
     selection per library, quality target, size floor) so policy is data, not
     a recompile. This is where `recyclarr`'s job lands.
  5. Every verdict carries a human-readable `reason` string — the assistant
     surfaces these verbatim, so they must be explanations, not error codes.

  ## TEST PLAN
  - The six sandbox fixtures evaluate to the documented verdicts:
    msmpeg4v3/avi, wmv3/asf and mpeg2video+pcm_bluray/m2ts all `Reencode`
    against every profile; h264+aac/mp4 and h264+ac3+subrip/mkv `Compliant`
    against every profile; hevc+ass/mkv `Compliant` on modern tiers and
    `Reencode`/`Remux` on the conservative Plex tier
  - Intersection semantics: selecting two profiles yields the stricter verdict
  - A DB policy override changes the verdict without a code change
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Unknown/rare codec not in any profile — `Reencode` with an explicit
    "unrecognized codec" reason, never silently `Compliant`
  - HDR source with an SDR-only profile — tone-map required, flagged distinctly
    from a plain re-encode (it is lossy in a way the operator should approve)
  - Lossless audio (`pcm_bluray`, TrueHD) — `Reencode` for size, but never
    down-mix channels without an explicit policy saying so
  - Image subtitles (PGS) with a text-only profile — extract-or-keep decision,
    never silent burn-in
  - Empty profile selection — error, not "everything is compliant"

- **Acceptance criteria:**
  - [ ] Built-in profiles exist for Plex, Jellyfin, Emby and Kodi, each with a
        conservative and a modern tier
  - [ ] Verdicts across multiple profiles take the strictest result
  - [ ] All six sandbox fixtures produce the documented verdicts
  - [ ] Operator policy overrides are read from the DB, not compiled in
  - [ ] Every verdict carries a human-readable reason
  - [ ] Tone-map and down-mix are distinct, individually-gated decisions
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README documents the profiles and the intersection semantics

### MUSEF-04: Transcode plan generator
- **Priority:** High
- **Labels:** muse, foundry, planning
- **Agent:** claude
- **Estimate:** 6h
- **Phase:** 1
- **Description:** Turn a compliance verdict into a concrete, reviewable
  execution plan — the exact per-stream operations, target container, and
  estimated output size. A plan is data; executing it is MUSEF-08.

  ## FILES
  - `src/foundry/plan.rs`
  - `src/foundry/models.rs` — `TranscodePlan`, `StreamOp`, `PlanEstimate`

  ## APPROACH
  1. For each stream emit a `StreamOp`: `Copy`, `Transcode { codec, params }`,
     `Extract { to_sidecar }`, or `Drop { reason }`. Default to `Copy` — a plan
     that re-encodes a compliant stream is a bug, and size reduction alone
     never justifies re-encoding audio.
  2. Choose the target container by policy (mkv default; mp4 when policy
     requires it), and pick the video encoder from the executing node's
     advertised capabilities (MUSEF-14) rather than hardcoding x264.
  3. Estimate output size from source bitrate, target quality, and duration —
     used for the disk-headroom check (the library is 84% full) and to reject a
     plan whose estimate exceeds the source without an explicit override.
  4. Make the plan serializable and diffable so it can be shown to the operator
     or assistant for approval before execution, and stored on the job row.
  5. Preserve chapters, attachments (fonts — required for ASS rendering), and
     all metadata tags by default; dropping any of them is an explicit op.

  ## TEST PLAN
  - The avi fixture plans video `Transcode` + audio `Copy` (ac3 is compliant)
  - The m2ts fixture plans video `Transcode` + audio `Transcode` (pcm is not)
  - A fully compliant mkv plans to a no-op with zero ops
  - Font attachments survive a plan that keeps an ASS subtitle stream
  - An estimate exceeding source size is rejected without an override flag
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Every stream compliant — emit an empty plan, not a pointless remux
  - Source with 12 audio tracks — plan is bounded; policy decides which survive
  - Estimate unavailable (no duration) — the plan is still valid but flagged
    `estimate: unknown` and the headroom check refuses it
  - ASS subtitle kept but fonts dropped — reject the plan as internally
    inconsistent

- **Acceptance criteria:**
  - [ ] Compliant streams always plan to `Copy`
  - [ ] A fully compliant file yields an empty (no-op) plan
  - [ ] Chapters, attachments and metadata are preserved unless explicitly dropped
  - [ ] Plans are serializable, diffable, and storable on a job row
  - [ ] A plan estimated larger than its source is rejected without an override
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MUSEF-05: Library compliance report + read-only surface
- **Priority:** High
- **Labels:** muse, foundry, http, reporting
- **Agent:** claude
- **Estimate:** 5h
- **Phase:** 1
- **Description:** Sweep a library, probe every file, evaluate against policy,
  and produce a compliance report. Read-only — the first thing the operator and
  the assistant can actually use, and the proof Phase 1 works before anything
  mutates.

  ## FILES
  - `src/foundry/report.rs`
  - `src/foundry/http.rs` — `/foundry/probe`, `/foundry/report`, `/foundry/policies`
  - `src/http/mod.rs` — mount the Foundry router
  - `migrations/00NN_foundry_probe_cache.sql`
  - `README.md`

  ## APPROACH
  1. Reuse `src/library/scan.rs`'s walker posture (read-only, symlink-aware) to
     enumerate candidates under a `ResolvedPath`.
  2. Probe with bounded concurrency, caching results keyed by
     `(path, size, mtime)` so a re-report is cheap on an unchanged library.
  3. Aggregate: counts by verdict, by container, by codec, top offenders by
     estimated reclaimable bytes, and a **metadata-defects** section — files
     whose audio/subtitle language tags are absent or implausible (the wmv
     sample's English audio tagged `swe`).
  4. Expose read-only endpoints behind the existing `src/http/auth.rs` layer.
  5. Emit `media.formatting.report_completed` on the context bus.

  ## TEST PLAN
  - Report over the sandbox `src-readonly` dir classifies all six samples
  - Probe cache hit on an unchanged file; miss after an mtime change
  - Endpoints reject unauthenticated requests
  - A file outside the allowlist is never probed even if it is inside the walked tree
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Library with 100k files — streaming aggregation, bounded memory
  - Unreadable file (permissions) — counted as `unprobeable`, not fatal
  - NFS stall mid-sweep — bounded timeout per file, sweep continues
  - Report requested while another sweep runs — return the in-progress handle,
    do not start a second sweep on the same root

- **Acceptance criteria:**
  - [ ] A report over the sandbox classifies all six samples correctly
  - [ ] The probe cache avoids re-probing unchanged files
  - [ ] Metadata defects (implausible language tags) are reported separately
  - [ ] Endpoints require authentication
  - [ ] Sweeps are bounded in memory and tolerate unreadable files
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README documents the report endpoints
  - [ ] All existing tests still pass

---

## Phase 2 — Forge: transcode execution (mutation, sandbox-gated)

### MUSEF-06: Encoder backend abstraction (HandBrake + ffmpeg)
- **Priority:** High · **Labels:** muse, foundry, encode · **Agent:** claude
- **Estimate:** 6h · **Phase:** 2
- **Description:** A `Encoder` trait with `HandBrakeBackend` and `FfmpegBackend`
  implementations that render a `TranscodePlan` into an argv, execute it with
  bounded resources, and stream structured progress.

  ## FILES
  - `src/foundry/encode/mod.rs`, `handbrake.rs`, `ffmpeg.rs`, `progress.rs`

  ## APPROACH
  1. `trait Encoder { fn supports(&self, plan) -> bool; fn build_argv(&self,
     plan, in, out) -> Vec<OsString>; async fn run(...) -> EncodeOutcome }`.
  2. HandBrake for whole-file quality-targeted encodes (proven in the sandbox:
     `-e x264 -q 21 --encoder-preset veryfast -E copy --audio-fallback aac
     --all-audio --all-subtitles`); ffmpeg for stream-surgical work (remux,
     extract, metadata repair) where HandBrake's model is too coarse.
  3. Parse progress from each tool's output into a common
     `Progress { percent, fps, eta, pass }`, emitted on a channel.
  4. Run under a bounded child process with a wall-clock ceiling and a
     no-progress stall detector; kill the child on stall, never on slow-but-alive.
  5. `build_argv` is a pure function — unit-tested without executing anything.

  ## TEST PLAN
  - `build_argv` snapshot tests per plan shape for both backends
  - Progress parsing over captured HandBrake and ffmpeg output
  - Stall detector kills a no-output child; a slow child with output survives
  - Backend selection prefers ffmpeg for a pure-remux plan
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Filename with quotes/newlines/unicode — argv built as `OsString` vector,
    never a shell string
  - Encoder binary missing — typed error at `supports()` time, not mid-run
  - Child writes progress only to stderr — both streams parsed
  - Plan requiring an unsupported filter — `supports()` returns false, planner
    picks the other backend or the job fails cleanly

- **Acceptance criteria:**
  - [ ] Both backends implement `Encoder` and are selected by plan shape
  - [ ] argv is built as an `OsString` vector, never a shell string
  - [ ] Progress parses to a common structure from both tools
  - [ ] A stalled child is killed; a slow-but-progressing child is not
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MUSEF-07: Job queue, persistence, and state machine
- **Priority:** Critical · **Labels:** muse, foundry, queue · **Agent:** claude
- **Estimate:** 6h · **Phase:** 2
- **Description:** Durable job queue in Postgres with an explicit state machine,
  priority, dedup, lease expiry, and full audit history.

  ## FILES
  - `src/foundry/queue/mod.rs`, `state.rs`, `repo.rs`
  - `migrations/00NN_foundry_jobs.sql`

  ## APPROACH
  1. States: `Queued → Leased → Running → Verifying → {Completed | Failed |
     Cancelled}`, plus `Blocked { reason }` for hardlink/headroom refusals.
     Every transition is recorded in `foundry_job_events` with a timestamp,
     actor, and reason.
  2. Dedup on `(path, plan_hash)` — resubmitting an identical plan for an
     unchanged file returns the existing job rather than queueing a duplicate.
  3. Leases carry an expiry; an expired lease returns the job to `Queued` with
     an incremented attempt count, and a job exceeding `max_attempts` goes to
     `Failed` rather than looping forever.
  4. Priority ordering with a household-activity boost fed from the context bus
     (a title someone is about to watch jumps the queue).
  5. All DB access through the existing `src/repo` layer and the service's own
     pool (the S9-pg application-data-plane exception), never fleet `pg_*` tools.

  ## TEST PLAN
  - Full state-machine transition tests including illegal transitions rejected
  - Dedup returns the existing job for an identical `(path, plan_hash)`
  - Expired lease requeues and increments attempts; `max_attempts` → `Failed`
  - Concurrent lease attempts on one job — exactly one wins
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Server restart with jobs in `Running` — leases expire and requeue
  - Same file queued under two different plans — both allowed, distinct hashes
  - Clock skew between server and node — expiry computed server-side only
  - Cancel arriving while `Verifying` — completes verification, then discards

- **Acceptance criteria:**
  - [ ] Illegal state transitions are rejected
  - [ ] Every transition is recorded with actor, timestamp and reason
  - [ ] Identical `(path, plan_hash)` submissions dedup to one job
  - [ ] Expired leases requeue; `max_attempts` exhaustion fails the job
  - [ ] Exactly one concurrent lease attempt succeeds
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MUSEF-08: Verify-then-swap executor
- **Priority:** Critical · **Labels:** muse, foundry, safety · **Agent:** claude
- **Estimate:** 8h · **Phase:** 2
- **Description:** The single place Foundry replaces a real file. Encodes to the
  work dir, verifies the output, guards hardlinks and headroom, then atomically
  swaps with retention. This item carries the most risk in the spec and gets the
  widest review panel.

  ## FILES
  - `src/foundry/forge/execute.rs`, `verify.rs`, `swap.rs`, `recycle.rs`

  ## APPROACH
  1. Pre-flight: `PathGuard::require_mutation()`; `st_nlink > 1` → `Blocked`;
     free space on the destination device < estimate × safety factor →
     `Blocked`; destination device differs from work device → required.
  2. Encode to `work_dir/<job_id>/<name>.tmp` via MUSEF-06.
  3. Verify: re-probe the output; duration within tolerance of the source;
     expected streams present with expected codecs; container parses; optional
     VMAF over N sampled segments above the policy floor. Any failure → keep
     the source untouched, mark `Failed`, retain the output for inspection.
  4. Swap: move the source into the Foundry recycle bin (same device where
     possible, copy+verify+unlink across devices), then move the verified
     output into place. Ordering guarantees a crash never leaves *no* file.
  5. Record source and output probes, VMAF score, byte delta and every decision
     on the job row.

  ## TEST PLAN
  - Sandbox end-to-end: the avi sample transcodes, verifies and swaps; the
    recycle bin holds the original; a re-probe confirms the new codec
  - Hardlinked file (`st_nlink > 1`) is `Blocked`, never touched
  - Insufficient headroom is `Blocked` before any encode starts
  - Injected verification failure leaves the source byte-identical
  - Simulated crash between recycle and install leaves a recoverable state
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Source modified during the encode (mtime/size change) — abort the swap
  - Recycle bin full or unwritable — refuse the swap, do not delete
  - Cross-device recycle — copy, verify checksum, then unlink; never rename
  - Output larger than source — refuse unless the plan carried an override
  - Concurrent job targeting the same path — the queue's dedup plus a per-path
    lock make this impossible; assert it rather than handling it

- **Acceptance criteria:**
  - [ ] No swap occurs unless verification passes in full
  - [ ] Hardlinked files are blocked and never modified
  - [ ] Headroom is checked before any encode begins
  - [ ] The original is always recoverable from the recycle bin after a swap
  - [ ] A source modified mid-encode aborts the swap
  - [ ] A crash at any point leaves either the original or the verified output
        in place, never neither
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MUSEF-09: Chord GPU lease + hardware acceleration selection
- **Priority:** Medium · **Labels:** muse, foundry, chord, gpu · **Agent:** claude
- **Estimate:** 5h · **Phase:** 2
- **Description:** The GPU is a shared pool arbitrated by Chord. Foundry leases
  it through Chord's control API before a hwaccel encode and releases it after
  — it never assumes the device is free.

  ## FILES
  - `src/foundry/encode/hwaccel.rs`, `src/foundry/chord_lease.rs`

  ## APPROACH
  1. Detect available hwaccel per node (VAAPI/AMF/QSV/NVENC/none) by probing
     the encoder's device list once at node registration, cached.
  2. Before a hwaccel job, acquire a lease from `CHORD_CONTROL_URL` with a TTL
     and a stated RAM/VRAM budget; on refusal or unreachability, **fall back to
     CPU encoding**, never wait indefinitely and never proceed without a lease.
  3. Release on completion, failure, or cancellation; the TTL is the backstop
     so a crashed node cannot hold the GPU.
  4. Secrets (`CHORD_JWT_SECRET`) via `SecretManager::get()`.
  5. Quality parity check: hwaccel presets are tuned to match the CPU preset's
     quality floor, since hardware encoders at default settings are visibly worse.

  ## TEST PLAN
  - Lease acquired → hwaccel argv; lease refused → CPU argv
  - Chord unreachable → CPU fallback, job still completes
  - Lease released on success, failure and cancellation paths
  - TTL expiry releases a lease held by a killed node
  - Verify secrets accessed via `SecretManager`, not `std::env::var`

  ## EDGE CASES
  - Lease granted then Chord revokes mid-encode — finish the current file, do
    not renew (killing a 40-minute encode is worse than a short overrun)
  - Node advertises hwaccel that fails at runtime — demote to CPU, mark the
    capability unavailable for the session
  - Two nodes on one GPU host — leases are per-device, not per-node

- **Acceptance criteria:**
  - [ ] A hwaccel encode never starts without a granted lease
  - [ ] Refusal or unreachability falls back to CPU, never blocks
  - [ ] Leases are released on every exit path and expire by TTL
  - [ ] Secrets accessed via `SecretManager`, not env vars
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-10: Health-check and corruption detection pass
- **Priority:** Medium · **Labels:** muse, foundry, health · **Agent:** claude
- **Estimate:** 4h · **Phase:** 2
- **Description:** A non-mutating scan that decodes files to find corruption,
  truncation and container errors — Tdarr's health-check equivalent.

  ## FILES
  - `src/foundry/health.rs`

  ## APPROACH
  1. Quick mode: `ffmpeg -v error -i <f> -f null -` over sampled segments.
     Deep mode: full decode. Both read-only.
  2. Classify findings: `Truncated`, `DecodeErrors { count }`,
     `ContainerError`, `MissingStream`, `Clean`.
  3. Record on a `foundry_health` table; surface in the MUSEF-05 report and to
     the assistant. Never auto-delete or auto-repair — findings are advisory.
  4. Schedule as a low-priority queue job type so health checks yield to
     transcodes.

  ## TEST PLAN
  - A deliberately truncated sandbox copy is detected as `Truncated`
  - A clean sample reports `Clean`
  - Health jobs are preempted by transcode jobs at equal priority
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - A file that decodes with warnings but plays fine — warnings are not errors
  - Deep-mode scan of a 60 GB UHD file — bounded, cancellable, resumable
  - Health check on a file currently being transcoded — skipped

- **Acceptance criteria:**
  - [ ] Truncated and corrupt files are detected; clean files report clean
  - [ ] The pass never modifies or deletes anything
  - [ ] Findings surface in the compliance report
  - [ ] No hardcoded infrastructure values in new/modified code

---

## Phase 3 — Fabric: distributed transcode nodes

### MUSEF-11: Node registration, heartbeat, and authentication
- **Priority:** High · **Labels:** muse, foundry, fabric · **Agent:** claude
- **Estimate:** 6h · **Phase:** 3
- **Description:** Worker nodes register with the Muse server, authenticate with
  a shared token, advertise capabilities, and heartbeat. Server-side only.

  ## FILES
  - `src/foundry/fabric/mod.rs`, `registry.rs`, `auth.rs`
  - `migrations/00NN_foundry_nodes.sql`

  ## APPROACH
  1. `POST /foundry/nodes/register` with a `NodeCapabilities` payload: encoders
     available, hwaccel devices, logical cores, RAM, cache dir size, reachable
     library roots (a node that cannot see a path must never be given its jobs).
  2. Authenticate with `MUSE_FOUNDRY_NODE_TOKEN` via `SecretManager::get()`,
     constant-time compared. Node identity is the token plus a node-supplied
     stable ID; re-registration updates rather than duplicates.
  3. Heartbeat every N seconds carrying current load; a node missing
     `heartbeat_timeout` is marked `Offline` and its leases expire (MUSEF-07).
  4. Nodes are never pushed work — they poll. This means no inbound
     connectivity to nodes is required, which is what makes a node deployable
     anywhere on the network.

  ## TEST PLAN
  - Registration with a valid token succeeds; invalid token rejected
  - Re-registration updates the existing node row
  - Missed heartbeats mark the node offline and expire its leases
  - A node not advertising a root is never offered jobs under that root
  - Verify secrets accessed via `SecretManager`, not `std::env::var`

  ## EDGE CASES
  - Two nodes claiming the same stable ID — second registration rejected
  - Node reachable-roots change between registrations — capability updated,
    in-flight leases for now-unreachable roots are requeued
  - Token rotation — both old and new accepted during a grace window

- **Acceptance criteria:**
  - [ ] Nodes authenticate with a constant-time token comparison
  - [ ] Capabilities including reachable roots are recorded and updated
  - [ ] Offline nodes have their leases expired
  - [ ] A node is never offered work for a root it cannot see
  - [ ] Secrets accessed via `SecretManager`, not env vars
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-12: Job lease, claim, and progress streaming
- **Priority:** High · **Labels:** muse, foundry, fabric · **Agent:** claude
- **Estimate:** 5h · **Phase:** 3
- **Description:** The pull protocol: a node asks for work matching its
  capabilities, receives a lease, streams progress, and reports the outcome.

  ## FILES
  - `src/foundry/fabric/lease.rs`, `progress.rs`, `src/foundry/http.rs`

  ## APPROACH
  1. `POST /foundry/nodes/{id}/lease` → the highest-priority job the node can
     actually run (encoder + hwaccel + reachable root), atomically leased.
  2. `POST /foundry/jobs/{id}/progress` with the common `Progress` structure;
     progress also renews the lease, so a working node never loses its job.
  3. `POST /foundry/jobs/{id}/complete` with the output location and probe, or
     `/fail` with a reason. The server performs verification and swap
     (MUSEF-08) — a node never swaps a library file itself.
  4. Bounded lease count per node from its advertised concurrency.

  ## TEST PLAN
  - Lease returns only capability-matching jobs
  - Progress renews the lease; silence expires it
  - Completion hands off to server-side verify+swap
  - Per-node concurrency cap is enforced
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Node completes a job whose lease already expired and was requeued — the
    completion is rejected and the output discarded
  - Progress for a cancelled job — accepted, node told to stop
  - Node with zero matching jobs — empty response, backoff hint

- **Acceptance criteria:**
  - [ ] Nodes only receive jobs they can run
  - [ ] Progress renews leases; silence expires them
  - [ ] Nodes never perform the library-file swap themselves
  - [ ] A completion against an expired lease is rejected
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-13: `muse-node` worker binary
- **Priority:** High · **Labels:** muse, foundry, fabric, binary · **Agent:** claude
- **Estimate:** 6h · **Phase:** 3
- **Description:** The standalone worker: a small binary that runs anywhere with
  ffmpeg/HandBrake and the library mounted, and needs no inbound connectivity.

  ## FILES
  - `src/bin/muse_node/main.rs`, `client.rs`, `runner.rs`
  - `Cargo.toml` — new `[[bin]]`
  - `README.md` — node deployment

  ## APPROACH
  1. Config from env: server URL, node token (via `SecretManager`), cache dir,
     concurrency, advertised roots.
  2. Loop: register → heartbeat → lease → run encoder → stream progress →
     complete. Exponential backoff on an empty lease or an unreachable server.
  3. Graceful shutdown: finish or cleanly abandon in-flight jobs (releasing the
     lease) on SIGTERM, so a node restart never strands work.
  4. Cache hygiene: clear per-job scratch on completion; refuse to lease when
     the cache dir is below a free-space floor.
  5. Ship in the module's OCI image alongside the server binary
     (`OCI_INSTALL` multi-bin, skill v4.5), so the existing updater deploys it.

  ## TEST PLAN
  - Node registers, leases a sandbox job, transcodes and reports completion
  - Unreachable server → backoff, no crash, no busy loop
  - SIGTERM releases in-flight leases
  - Cache below the free-space floor stops leasing
  - Verify secrets accessed via `SecretManager`, not `std::env::var`

  ## EDGE CASES
  - Clock skew — node never computes lease expiry locally
  - Server restarts mid-job — node's progress re-establishes the lease or is
    told the job was requeued, and it aborts
  - Encoder binary disappears mid-session — node deregisters that capability

- **Acceptance criteria:**
  - [ ] `muse-node` runs standalone with no inbound connectivity required
  - [ ] Backoff on empty leases and server unavailability, never a busy loop
  - [ ] SIGTERM releases in-flight leases
  - [ ] Cache free-space floor stops further leasing
  - [ ] Ships in the module OCI image alongside the server binary
  - [ ] README documents node deployment
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-14: Scheduler — capability matching and fair dispatch
- **Priority:** Medium · **Labels:** muse, foundry, fabric · **Agent:** claude
- **Estimate:** 4h · **Phase:** 3
- **Description:** Decide which node gets which job: capability match, priority,
  locality (a node on the same host as the file avoids network I/O), and
  fairness so one library cannot starve another.

  ## FILES
  - `src/foundry/fabric/scheduler.rs`

  ## APPROACH
  1. Filter by hard constraints (encoder, hwaccel, reachable root, free cache).
  2. Rank by: job priority, then locality, then node idleness.
  3. Weighted round-robin across libraries so a 5,000-file backlog in one
     library does not starve a 10-file backlog in another.
  4. Emit the chosen node and the reason on the job event log — scheduling
     decisions must be explainable to the operator.

  ## TEST PLAN
  - Hard constraints filter correctly; a job with no eligible node stays queued
  - Local node preferred over remote at equal priority
  - Fairness: two libraries with unequal backlogs both progress
  - Every dispatch records its reason
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - No eligible node for any job — queue idles cleanly, surfaced in status
  - All nodes saturated — no lease granted, backoff hint returned
  - A single job eligible only on one node that keeps failing it — attempts
    cap applies and the job fails rather than pinning that node forever

- **Acceptance criteria:**
  - [ ] Hard constraints are respected absolutely
  - [ ] Locality and idleness break priority ties
  - [ ] No library can starve another
  - [ ] Every dispatch decision is explainable from the event log
  - [ ] No hardcoded infrastructure values in new/modified code

---

## Phase 4 — Lexicon: subtitles (the Bazarr strangler)

### MUSEF-15: Subtitle inventory and language normalization
- **Priority:** High · **Labels:** muse, foundry, subtitles · **Agent:** claude
- **Estimate:** 4h · **Phase:** 4
- **Description:** Know what subtitles already exist — embedded streams and
  sidecar files — before searching for any. Includes the metadata-repair path
  for mislabeled language tags.

  ## FILES
  - `src/foundry/lexicon/mod.rs`, `inventory.rs`, `language.rs`
  - `migrations/00NN_foundry_subtitles.sql`

  ## APPROACH
  1. Embedded streams come from the MUSEF-02 probe. Sidecars are discovered by
     the conventional patterns (`<base>.<lang>.srt`, `.forced.`, `.hi.`,
     `.sdh.`, plus the `.ass` files present in the anime libraries).
  2. Normalize language to ISO 639-2/B, retaining the raw tag. Flag implausible
     tags — the wmv sample's English audio marked `swe` is the canonical case.
  3. Record desired-vs-present per title from a per-library language profile,
     producing a **wanted queue**.
  4. Distinguish forced/SDH/HI variants — they are not interchangeable, and
     treating them as one is Bazarr's most common user complaint.

  ## TEST PLAN
  - Inventory over the sandbox finds the embedded ASS and subrip streams
  - Sidecar patterns including `.forced.` and `.sdh.` are classified correctly
  - An implausible language tag is flagged, not silently trusted
  - The wanted queue reflects desired-minus-present
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Sidecar with no language in the name — recorded as `und`, flagged
  - Multiple sidecars for one language (a re-sync attempt) — both kept, ranked
  - Embedded and sidecar both present for one language — not "wanted"
  - Non-UTF8 sidecar encoding — detected and recorded; conversion is MUSEF-18

- **Acceptance criteria:**
  - [ ] Embedded and sidecar subtitles are both inventoried
  - [ ] Forced, SDH and HI variants are distinguished
  - [ ] Implausible language tags are flagged rather than trusted
  - [ ] A wanted queue is produced from a per-library language profile
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-16: Subtitle provider abstraction + first adapter
- **Priority:** High · **Labels:** muse, foundry, subtitles · **Agent:** claude
- **Estimate:** 6h · **Phase:** 4
- **Description:** A provider trait plus an OpenSubtitles adapter doing
  hash-first, then title/release-based search.

  ## FILES
  - `src/foundry/lexicon/provider/mod.rs`, `opensubtitles.rs`, `hash.rs`

  ## APPROACH
  1. `trait SubtitleProvider { async fn search(&self, query) ->
     Vec<SubtitleCandidate>; async fn download(&self, id) -> SubtitleBytes }`.
  2. Implement the OpenSubtitles moviehash (first+last 64 KiB plus file size) —
     a hash match is a near-certain sync match and is always preferred.
  3. Fall back to title + year + season/episode + release-group matching using
     the release name Muse already parses in `src/library/scan.rs`.
  4. API key via `SecretManager::get("OPENSUBTITLES_API_KEY")`. Respect rate
     limits with a token bucket; a 429 backs off rather than failing the queue.
  5. Privacy (Module Contract §6): send only the hash, title and language —
     never a filesystem path or any user identifier.

  ## TEST PLAN
  - Moviehash computed correctly against a known-value fixture
  - Provider search and download parse correctly via `httpmock`
  - Rate limiting backs off on 429 without failing the wanted queue
  - No path or user identifier appears in any outbound request
  - Verify secrets accessed via `SecretManager`, not `std::env::var`

  ## EDGE CASES
  - File smaller than 128 KiB — hash undefined, fall back to title search
  - Provider returns a subtitle for the wrong episode — caught by MUSEF-17
  - Provider unreachable — the wanted item stays wanted, retried with backoff
  - API key absent — the provider is simply not registered (capability gating)

- **Acceptance criteria:**
  - [ ] Hash-first search with title-based fallback
  - [ ] Only hash, title and language leave the system — never paths or identity
  - [ ] Rate limits are respected; 429 backs off without failing the queue
  - [ ] An absent API key leaves the provider unregistered, not erroring
  - [ ] Secrets accessed via `SecretManager`, not env vars
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-17: Candidate scoring and sync verification
- **Priority:** High · **Labels:** muse, foundry, subtitles · **Agent:** claude
- **Estimate:** 5h · **Phase:** 4
- **Description:** Pick the right subtitle and prove it is actually in sync
  before writing it. This is what separates Foundry from "download the top hit."

  ## FILES
  - `src/foundry/lexicon/score.rs`, `sync.rs`

  ## APPROACH
  1. Score candidates on: hash match (dominant), release-group match, duration
     match, uploader rating, download count, HI/forced correctness.
  2. Verify sync structurally: parse the subtitle's first and last cue
     timestamps and check they fall within the media duration with sane margins;
     reject a candidate whose last cue exceeds the runtime or whose first cue
     is implausibly late.
  3. Enforce a minimum score threshold — below it, leave the item wanted rather
     than write a bad subtitle. A wrong subtitle is worse than none.
  4. Record the chosen candidate, its score, and every rejection reason.

  ## TEST PLAN
  - Hash match outranks a higher-rated non-hash candidate
  - A candidate whose last cue exceeds runtime is rejected
  - Below-threshold scoring leaves the item wanted
  - Rejection reasons are recorded
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Subtitle for an extended cut against a theatrical file — duration mismatch
    catches it
  - Empty subtitle file (0 cues) — rejected
  - Malformed timestamps — parse failure is a rejection, not a crash
  - All candidates rejected — item stays wanted, reason recorded

- **Acceptance criteria:**
  - [ ] Hash matches dominate scoring
  - [ ] Structural sync verification rejects out-of-range candidates
  - [ ] Below-threshold results leave the item wanted rather than writing
  - [ ] Every rejection reason is recorded
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-18: Sidecar writer and wanted-queue worker
- **Priority:** High · **Labels:** muse, foundry, subtitles · **Agent:** claude
- **Estimate:** 4h · **Phase:** 4
- **Description:** Write the chosen subtitle to disk with correct naming and
  encoding, and run the wanted queue on a schedule.

  ## FILES
  - `src/foundry/lexicon/writer.rs`, `worker.rs`
  - `src/workers.rs` — spawn the Lexicon worker

  ## APPROACH
  1. Write `<base>.<lang>[.forced][.sdh].srt` next to the media file, through
     `PathGuard` and behind the mutation gate, atomically (temp + rename).
  2. Normalize encoding to UTF-8, converting known legacy encodings; never
     write a file whose encoding cannot be determined.
  3. Worker: poll the wanted queue on an interval, respecting an adaptive
     backoff for items that have failed repeatedly (Bazarr's adaptive-searching
     idea, which is the one setting its config had usefully set).
  4. Emit `media.formatting.subtitle_acquired` on the context bus.

  ## TEST PLAN
  - Sidecar written with correct name for plain, forced and SDH variants
  - Legacy-encoded input is converted to UTF-8; undeterminable input is refused
  - Write is atomic (no partial file visible)
  - Repeated failures back off rather than hammering providers
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Existing sidecar at the target name — replaced only if the new score is
    higher; the old one goes to the recycle bin
  - Read-only media directory — the item is blocked with a clear reason
  - Media file renamed between search and write — re-resolve or abandon

- **Acceptance criteria:**
  - [ ] Sidecars are named correctly for plain, forced and SDH variants
  - [ ] Output is always UTF-8; undeterminable encodings are refused
  - [ ] Writes are atomic and pass through the mutation gate
  - [ ] Replacing an existing sidecar retains the old one
  - [ ] No hardcoded infrastructure values in new/modified code

---

## Phase 5 — Archivist: the library organizer

### MUSEF-19: Layout policy per library
- **Priority:** High · **Labels:** muse, foundry, organizer · **Agent:** claude
- **Estimate:** 5h · **Phase:** 5
- **Description:** Declare the target folder and file naming for each library,
  in the conventions Plex, Jellyfin, Emby and Kodi all parse correctly.

  ## FILES
  - `src/foundry/archivist/mod.rs`, `layout.rs`
  - `migrations/00NN_foundry_layouts.sql`

  ## APPROACH
  1. Model a `LayoutPolicy` as templates: folder template, file template, season
     folder template, plus rules for colon replacement, year placement, edition
     tags, and multi-episode joining.
  2. Ship defaults that all four clients parse: `Title (Year)/Title (Year)
     [Edition] [Quality].ext` for movies, `Series (Year)/Season NN/Series -
     SNNENN - Episode Title.ext` for series, with external-ID tags
     (`{tmdb-NNNN}`) which Jellyfin and Kodi use for exact matching.
  3. Import the *existing* per-instance naming formats from the surveyed *arr
     config as the starting point for each library, so Foundry's default layout
     matches what the operator already has rather than proposing a mass rename.
  4. Policies are DB rows, editable by the operator or assistant.

  ## TEST PLAN
  - Templates render correctly for movie, series, multi-episode and edition cases
  - Colon and illegal-character replacement produces filesystem-safe names
  - Rendered names round-trip through Muse's own release parser
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Title containing path separators or reserved characters
  - Name exceeding filesystem limits — truncated at a safe boundary, ID tag kept
  - Series with absolute-numbered anime episodes — a distinct template
  - Missing year — template degrades rather than rendering `(0)`

- **Acceptance criteria:**
  - [ ] Default layouts parse correctly on Plex, Jellyfin, Emby and Kodi
  - [ ] Rendered names are filesystem-safe and length-bounded
  - [ ] Existing *arr naming is importable as a per-library starting point
  - [ ] Layouts are DB-editable, not compiled in
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-20: Organize planner (dry-run diff)
- **Priority:** Critical · **Labels:** muse, foundry, organizer, safety · **Agent:** claude
- **Estimate:** 5h · **Phase:** 5
- **Description:** Produce the complete set of proposed moves/renames as a
  reviewable diff, with zero side effects. Dry-run is the default and the only
  mode available until a plan is explicitly approved.

  ## FILES
  - `src/foundry/archivist/plan.rs`

  ## APPROACH
  1. For each matched media item, render the target layout and diff against the
     current path. Emit `Move`, `Rename`, `CreateDir`, `LeaveAlone`, or
     `Conflict { reason }`.
  2. Classify companion files: subtitles, posters, `.nfo`, trailers and extras
     travel with the media; scene junk (`.sfv`, `.nfo.7z`, `READ_ME.txt`,
     sample dirs) is classified `Junk` and proposed for the recycle bin —
     **never deleted by the planner**.
  3. Detect collisions where two sources map to one target and mark both
     `Conflict` rather than picking a winner.
  4. Plans are content-addressed and stored, so approval refers to an exact
     plan and a changed library invalidates it.

  ## TEST PLAN
  - Planning over the sandbox `staging` scene-release dir produces the expected
    move set, carries the poster and `.nfo`, and classifies `.nfo.7z` and
    `READ_ME.txt` as junk
  - Two items mapping to one target both become `Conflict`
  - Planning has zero filesystem side effects (verified by mtime comparison)
  - A library change invalidates a stored plan
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Already-correct file — `LeaveAlone`, not a no-op move
  - Case-only rename on a case-insensitive mount — handled via a two-step
  - Unmatched file (Muse has no metadata row) — never moved, listed separately
  - Target directory exists with other content — merge, do not clobber

- **Acceptance criteria:**
  - [ ] Planning has zero filesystem side effects
  - [ ] Companion files travel with their media; junk is classified, not deleted
  - [ ] Collisions are reported as conflicts, never silently resolved
  - [ ] Unmatched files are never proposed for movement
  - [ ] Plans are content-addressed and invalidated by library changes
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-21: Organize apply engine (seed-safe)
- **Priority:** Critical · **Labels:** muse, foundry, organizer, safety · **Agent:** claude
- **Estimate:** 6h · **Phase:** 5
- **Description:** Execute an approved plan without breaking active torrents or
  losing a file. Second-highest-risk item in the spec after MUSEF-08.

  ## FILES
  - `src/foundry/archivist/apply.rs`

  ## APPROACH
  1. Require an explicit approved plan ID plus the mutation gate. A plan whose
     content hash no longer matches the library is refused.
  2. For each `Move`: if `st_nlink > 1`, the file is hardlinked (an active
     torrent) — create a new hardlink at the target and unlink the source, which
     preserves the seeding inode; if that fails (cross-device), mark `Blocked`
     rather than copying and breaking the seed.
  3. Same-device moves are `rename(2)` — atomic. Cross-device is
     copy → verify checksum → unlink.
  4. Journal every operation before performing it, so an interrupted apply is
     resumable and auditable.
  5. Junk goes to the recycle bin, never `unlink` directly.

  ## TEST PLAN
  - Sandbox apply over the staging scene release produces the target layout
  - A hardlinked file is relinked, and the source inode is preserved
    (verified by inode number and link count)
  - Cross-device hardlinked move is blocked, not copied
  - Interrupted apply resumes from the journal with no lost files
  - Junk lands in the recycle bin, not deleted
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Target appears between plan and apply — `Conflict`, skip, continue
  - Source vanished — recorded and skipped, apply continues
  - Partial apply then failure — journal enables exact resume
  - Permissions failure mid-apply — stop that item, continue others, report

- **Acceptance criteria:**
  - [ ] Apply requires an approved, still-valid plan and the mutation gate
  - [ ] Hardlinked (seeding) files preserve their inode or are blocked
  - [ ] Cross-device moves verify a checksum before unlinking the source
  - [ ] Every operation is journaled and an interrupted apply is resumable
  - [ ] Junk goes to the recycle bin, never a direct unlink
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

### MUSEF-22: Library hygiene classification
- **Priority:** Medium · **Labels:** muse, foundry, organizer · **Agent:** claude
- **Estimate:** 3h · **Phase:** 5
- **Description:** Classify everything in a library that is not a media file, so
  the operator can see what is clutter, what is a needed companion, and what is
  a genuine orphan.

  ## FILES
  - `src/foundry/archivist/hygiene.rs`

  ## APPROACH
  1. Classify by extension and context: `Media`, `Companion` (subtitle, poster,
     nfo, fanart, theme), `SceneMetadata` (`.sfv`, `.nfo.7z`, release `.txt`),
     `Archive` (a genuine multipart `.rar`/`.7z` set holding media), `Sample`,
     `Orphan` (companion with no media), `Unknown`.
  2. Size-aware: distinguish a 1–5 KB compressed `.nfo` stub from a real
     multi-gigabyte archive set — the surveyed library's `.7z` files are almost
     entirely the former, and treating them as un-extracted media would be wrong.
  3. Report only. Extraction of genuine archives is `unpackerr`'s remaining job
     and is proposed as a follow-up, not built here.

  ## TEST PLAN
  - Sandbox staging dir classifies mkv/poster/nfo/nfo.7z/READ_ME correctly
  - A small `.7z` is `SceneMetadata`; a large multipart set is `Archive`
  - A subtitle with no sibling media is `Orphan`
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - `.nfo` used as a media file by some tools — size and content sniffing
  - Extras/behind-the-scenes directories — `Companion`, never junk
  - Hidden files and NAS artifacts (`@Recycle`, `.@__thumb`) — excluded entirely

- **Acceptance criteria:**
  - [ ] Classification distinguishes scene metadata stubs from real archives by size
  - [ ] Companion files and extras are never classified as junk
  - [ ] NAS artifact directories are excluded
  - [ ] The pass is report-only
  - [ ] No hardcoded infrastructure values in new/modified code

---

## Phase 6 — Terminus tooling and the assistant control surface

### MUSEF-23: `foundry_*` Terminus tool module
- **Priority:** Critical · **Labels:** muse, foundry, terminus, tools · **Agent:** claude
- **Estimate:** 8h · **Phase:** 6
- **Description:** The Terminus surface Lumina drives Foundry through. Without
  this the module fails Module Contract §1 and §4. Implemented in the
  **Terminus** repo against Muse's HTTP surface — its own PR there, gated
  independently.

  ## FILES *(Terminus repo)*
  - `src/muse/foundry/mod.rs`, `client.rs`, `tools.rs`
  - `src/registry.rs` — register the Foundry tool set
  - `README.md`

  ## APPROACH
  1. Tools, each a thin typed wrapper over a Muse endpoint:
     - **Inspect:** `foundry_probe`, `foundry_report`, `foundry_health`
     - **Policy:** `foundry_policy_list`, `foundry_policy_set`
     - **Transcode:** `foundry_plan`, `foundry_job_submit`, `foundry_job_status`,
       `foundry_job_cancel`, `foundry_queue`
     - **Fabric:** `foundry_node_list`, `foundry_node_drain`
     - **Subtitles:** `foundry_sub_status`, `foundry_sub_search`, `foundry_sub_fetch`
     - **Organize:** `foundry_organize_plan`, `foundry_organize_apply`
     - **Audit:** `foundry_audit`
  2. **Mutating tools are guarded.** `foundry_job_submit` against a non-sandbox
     root, `foundry_organize_apply`, and `foundry_policy_set` route through the
     registry's guarded-tool approval gate (the same posture as `pg_ddl` and
     `ansible`), requiring a per-occurrence operator approval. Read-only tools
     are ungated.
  3. Muse credentials via `SecretManager::get()`; no Foundry token is ever
     returned by a tool or written to a log.
  4. Every tool returns structured JSON with the human-readable reasons from the
     policy engine intact, so the assistant explains rather than reports codes.

  ## TEST PLAN
  - Each tool's request/response parsing via `httpmock`
  - Mutating tools refuse without approval; read-only tools do not require it
  - `foundry_organize_apply` cannot be invoked without a plan ID
  - No token or path secret appears in any tool response
  - Verify secrets accessed via `SecretManager`, not `std::env::var`
  - Verify all calls to Muse go through this module, not a new/direct API client

  ## EDGE CASES
  - Muse unreachable — typed error, never a hang
  - A job submitted for a path outside the allowlist — Muse refuses; the tool
    surfaces the refusal reason verbatim
  - Approval granted then the plan invalidated — apply refuses, re-plan required

- **Acceptance criteria:**
  - [ ] All listed `foundry_*` tools are registered and callable
  - [ ] Mutating tools are behind the guarded-tool approval gate
  - [ ] Read-only tools require no approval
  - [ ] All calls to Muse go through this module, not a direct API client
  - [ ] No new script or process reads Muse's token directly
  - [ ] Secrets accessed via `SecretManager`, not env vars
  - [ ] README updated to document the Foundry tool set
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-24: Muse `/foundry` HTTP control endpoints
- **Priority:** High · **Labels:** muse, foundry, http · **Agent:** claude
- **Estimate:** 4h · **Phase:** 6
- **Description:** Consolidate and document the full `/foundry` endpoint set
  that MUSEF-23 consumes, behind the existing auth layer.

  ## FILES
  - `src/foundry/http.rs`, `src/http/mod.rs`, `README.md`, `docs/behavior-spec.md`

  ## APPROACH
  1. Complete the router: probe/report/health, policies, plan/jobs/queue,
     nodes, subtitles, organize, audit.
  2. Authentication via `src/http/auth.rs`; mutating routes additionally require
     the mutation gate to be enabled server-side, so a leaked token cannot
     mutate a server configured read-only.
  3. Add behavior-spec API contracts for the mutating routes, with env-var
     placeholders for any URL.
  4. Document every endpoint in the README.

  ## TEST PLAN
  - Endpoint tests via the existing `src/endpoint_tests.rs` harness
  - Unauthenticated requests rejected on every route
  - Mutating routes refuse when the server mutation gate is off, even with a
    valid token
  - `harmony verify` score does not regress
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Long-running report requested synchronously — returns a handle, not a hang
  - Cancel for an unknown job — 404, not 500
  - Concurrent identical submissions — deduped by MUSEF-07

- **Acceptance criteria:**
  - [ ] Every `foundry_*` tool has a corresponding documented endpoint
  - [ ] All routes require authentication
  - [ ] Mutating routes refuse when the server mutation gate is off
  - [ ] Behavior spec contracts added with env-var placeholders
  - [ ] README documents the endpoint set
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-25: Audit trail and assistant-facing summaries
- **Priority:** Medium · **Labels:** muse, foundry, audit · **Agent:** claude
- **Estimate:** 4h · **Phase:** 6
- **Description:** A complete, queryable record of everything Foundry did to the
  library, and the summaries the assistant speaks from.

  ## FILES
  - `src/foundry/audit.rs`, `migrations/00NN_foundry_audit.sql`

  ## APPROACH
  1. Append-only audit rows for every mutation: actor (operator/assistant/node),
     job, before/after probe, byte delta, decision reasons, recycle-bin location.
  2. Sanitize before writing: tokens and keys → `***REDACTED***`, values over
     1 KB truncated to 200 chars + `...(truncated)`.
  3. Summary views the assistant narrates from — "reclaimed N GB across M files
     this week; 3 blocked because they are actively seeding" — returning facts,
     which the persona layer rephrases (never embellishes) per the Soul Contract.
  4. Retention and a restore path: an audit row is enough to restore a file from
     the recycle bin.

  ## TEST PLAN
  - Every mutating path writes an audit row
  - Tokens and oversized values are redacted/truncated
  - A restore driven purely from an audit row recovers the original file
  - Summary queries return facts, not prose
  - Verify no hardcoded infrastructure values in new/modified files

  ## EDGE CASES
  - Audit write failure — the mutation is refused rather than performed unaudited
  - Recycle-bin entry expired past retention — restore reports it clearly
  - Very high job volume — audit writes are batched, never dropped

- **Acceptance criteria:**
  - [ ] Every mutation is audited before it is considered complete
  - [ ] An unauditable mutation is refused, not performed
  - [ ] Secrets and oversized values are redacted per S6
  - [ ] A file is restorable from its audit row alone
  - [ ] No hardcoded infrastructure values in new/modified code

### MUSEF-26: Documentation, behavior spec, and sandbox acceptance run
- **Priority:** High · **Labels:** muse, foundry, docs · **Agent:** claude
- **Estimate:** 4h · **Phase:** 6
- **Type:** documentation
- **Description:** The operator-facing documentation and the end-to-end sandbox
  acceptance run that gates pointing Foundry at the live library.

  ## AUDIENCE
  Moose (operator) and future contributors.

  ## OUTLINE
  - What Foundry is and what it replaced (~300 words)
  - The five safety rails and why each exists (~400 words)
  - Client profiles and choosing a policy (~400 words)
  - Deploying a `muse-node` (~300 words)
  - Subtitle providers and language profiles (~250 words)
  - Organizing a library: plan, review, apply (~350 words)
  - The `foundry_*` tool reference (~400 words)
  - Sandbox acceptance checklist (~300 words)

  ## SOURCES
  - `docs/ARR-SUITE-GRAPH.md`, `specs/S128-muse-foundry.md`
  - `src/foundry/**`, the Terminus `src/muse/foundry/**` module

  ## TONE
  Technical reference, direct. No hardcoded infrastructure values — env-var
  placeholders in every example.

- **Acceptance criteria:**
  - [ ] `docs/foundry.md` covers every section in the outline
  - [ ] `docs/behavior-spec.md` gains Foundry state and API contracts
  - [ ] The sandbox acceptance checklist is executable and passes end to end
  - [ ] No hardcoded infrastructure values anywhere in the documentation

---

## Sequencing and gates

| Phase | Items | Est. | Mutates? | Gate to proceed |
|---|---|---|---|---|
| 1 — Core | MUSEF-01..05 | 28h | No | Compliance report over the live library is correct |
| 2 — Forge | MUSEF-06..10 | 29h | Sandbox only | Sandbox transcode + verify + swap + rollback proven |
| 3 — Fabric | MUSEF-11..14 | 21h | Sandbox only | ≥2 nodes transcode sandbox jobs concurrently |
| 4 — Lexicon | MUSEF-15..18 | 19h | Sandbox only | Correct subtitle fetched and sync-verified |
| 5 — Archivist | MUSEF-19..22 | 19h | Sandbox only | Seed-safe apply proven, inode preserved |
| 6 — Control | MUSEF-23..26 | 20h | — | Assistant drives all of the above via `foundry_*` |

**The live-library gate.** `MUSE_FOUNDRY_ALLOWED_ROOTS` stays pointed at the
sandbox for all six phases. Extending it to a real library is a deliberate
operator action taken after the MUSEF-26 acceptance run passes — not a step in
any item here. `tdarr` and `bazarr` are decommissioned only after that.

**Review posture.** MUSEF-08 and MUSEF-21 are the two items that can destroy
irreplaceable data. Both get the widest available review panel and adversarial
rounds, per the concurrent-pipeline gate discipline. Everything else takes the
routine gate.

## Known follow-ups (deliberately out of scope)

1. **Fix `sonarr_anime`'s root folder** (Finding A) — an ops action on a live
   container, not a code change; do it independently of this spec.
2. **Retire the 4 inert *arr instances** (Finding B) — ops action.
3. **Genuine archive extraction** — `unpackerr`'s remaining function; a
   follow-up spec once MUSEF-22's classification proves out.
4. **`plane_prefix_promote` for `MUSEF`** — the prefix is claimed in the
   runtime overlay under project `MUSE`; promoting it to the git-versioned
   baseline (`data/prefix_registry.toml`) opens a Terminus PR and should ride
   the normal pipeline. The provisional `MFDY` claim has been retired.
5. **Correct the `moosenet-spec` skill's Plane project list** — it omits
   `MUSE` (and `DOCS`/`INFRA2`/`CTX`/`FRG`/`TRIG`, which still exist in Plane).
   A spec author following the skill alone would misfile every Muse spec, as
   this one initially did. Worth a small skill edit.
