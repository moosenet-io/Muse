# Maestro — device profiles and the playback decision engine

plane_project: MUSE
module: Muse
prefix: MDEC
spec_id: S130-C-maestro-decision

## Metadata
- **Author:** <operator> (Moose)
- **Session:** S130
- **Date:** 2026-08-01
- **Module version:** Maestro v0.1 (child spec C of `S130-maestro-epic.md`)
- **Estimated total:** ~55h autonomous agent work
- **North-Star layer:** module
- **Module-Contract:** meets §4 clauses 1–7 as scoped. No egress and no credential of its own — the
  only network-facing surfaces added (`/why`, `/capabilities`) are reached through `proxy_maestro` on
  the Terminus gateway (clause 1); an unknown device degrades to a conservative baseline rather than
  breaking (clause 2); presentation is spec G's (clause 5). Clause 3 (context bus) is **deferred to
  spec D** — the decision engine holds no session, so it publishes nothing.
- **Repo layout (epic §2 + §2b):** one repo, one crate, second `[[bin]]`. The pure core is **not**
  Maestro-private: it is the shared media core at **`src/media/`**, and this spec owns
  **`src/media/decision/`** — deliberately a sibling of `src/media/probe/` (spec A's promotion of
  `foundry::probe`) rather than a rival. Only the two HTTP handlers sit on Maestro's router
  (`src/maestro/http/`). Two naming notes: **`src/decision/` is already taken** — it is Muse's
  taste-scoring module (`mod.rs`, `scoring.rs`) — and `decision` beat `plan` for this directory
  because `plan` is the *noun the function returns*, and because `src/foundry/plan.rs` already owns
  that word in the curation half of the tree.
- **Depends on:** `S130-A-maestro-probe.md`. This spec consumes **`MediaProbe`** — the shipped
  `src/foundry/probe.rs` type that spec A promotes to `src/media/probe/` and extends. It does
  **not** define a `MediaInfo`, and any earlier draft language saying it did is superseded.
- **Context:** Maestro's brain. Epic §6's inversion — direct play first, transcode last — is not a
  philosophy but an algorithm, and this is where it lives; everything downstream (D delivery, E
  segmenting, F hardware) executes the plan this produces. The epic also says to concentrate test
  effort here, for a specific reason: a decision bug presents as *"the video won't play on the TV"*,
  which tells you nothing about which branch fired, and a wrong transcode is indistinguishable from a
  right one from the sofa — you find it in a CPU graph a week later. So the engine is pure, its
  decisions are structured data, and the cross-product of representative files against every device
  we own is a committed asset.

  **This document was rewritten 2026-08-01 after the local checkout was fast-forwarded 64 commits to
  `e8499aa`.** Earlier drafts were written against a stale tree and proposed building things that are
  already shipped. What follows is written against the real tree, verified by reading it.

---

## 1. Two planners, one substrate — read this before calling this spec a duplicate

`src/foundry/plan.rs` (1,435 lines, shipped, on `main`) already contains a pure, total
`plan_transcode`. This spec adds a second pure planner. **They answer different questions, and a
reviewer who does not see the distinction should stop at this table:**

| | `foundry::plan::plan_transcode` (shipped) | `media::decision::plan` (this spec) |
|---|---|---|
| **Question** | *Should this file be permanently re-encoded for the library?* | *Can THIS DEVICE play this file right now?* |
| **Domain** | Curation | Playback |
| **Inputs** | `&MediaProbe`, **`&TranscodePolicy`** — one house standard | `&[MediaProbe]`, **`&DeviceProfile`**, `&PlaybackRequest` — per-device capabilities, per-request tracks |
| **Output** | `TranscodeDecision` — `AlreadyOptimal` / `Transcode{plan,args,reasons}` / `CannotDecide` | `PlaybackPlan` — `DirectPlay` / `Remux` / `Transcode` / `Unplayable` / `CannotDecide` |
| **Cardinality** | One file, one policy | **Several files** (quality versions) against **one device**, choosing the best source |
| **Emits an argv?** | Yes — it plans an actual encode | **No.** Argv construction is spec D/E's, from the plan |
| **Consequence of the answer** | A file on disk is rewritten, irreversibly | A stream is served, and nothing is mutated |
| **When** | Offline, curation sweep | Per session, on the hot path |
| **When it says "no"** | Leave the file alone; the library is unharmed | The user sees nothing — refusal has a product cost curation does not pay |

The last two rows are why one function cannot serve both. A curation planner is free to be maximally
conservative, because refusing costs nothing but a file staying as it is. A playback planner that
refuses has failed the user, so it must distinguish *"this device genuinely cannot"* from *"we lack a
fact"* and hand the second to a caller that owns a fallback policy (§3).

Equally, a `TranscodePolicy` and a `DeviceProfile` are not the same object wearing different hats: a
policy is one house standard applied to everything, a profile is a per-device capability table where
"HEVC Main10 up to L5.1, but only on this generation" is the entire point. Collapsing them would
force the curation sweep to re-encode the whole library down to the worst device we own.

**What this spec must not do** is rebuild anything on the other side of that line. Foundry keeps its
worker fabric, `forge.rs`, verify-and-swap, the recycle bin, the mutation kill-switch, allowlisted
roots, and its curation `TranscodePolicy`. This spec adds **no ability to mutate a file**, and the
absence of an argv builder in its output is the structural proof of that.

## 1b. What already exists, and what this spec reuses verbatim

Verified by reading the tree at `e8499aa`, not inferred:

| Symbol | Where | How this spec uses it |
|---|---|---|
| `MediaProbe`, `VideoStream`, `AudioStream`, `SubtitleStream`, `AttachmentStream` | `foundry/probe.rs` (948) | **Consumed as the input type.** No parallel `MediaInfo` is defined. |
| `bitrate_verdict()` / `BitrateVerdict{WithinCeiling,Exceeds,Unknown}` | `foundry/plan.rs` | **Called verbatim** for the device bitrate ceiling. Its one-sided container-bitrate inference is exactly right here too. |
| `scale_to_fit()` | `foundry/policy.rs` | **Called verbatim** for aspect-preserving downscale. |
| `normalize_container()`, `Container` | `foundry/policy.rs` | Reused, with **one extension** (MDEC-01: a `WebM` variant — see finding 2). |
| `container_holds_attachments()` | `foundry/plan.rs` | Reused when judging whether a remux would silently drop subtitle fonts. |
| `Undecidable` | `foundry/plan.rs` | **Reused as the shared vocabulary** for "we lack a fact", extended with playback-only variants. |
| `TranscodeReason`'s *shape* | `foundry/plan.rs` | The house convention this spec follows: a payload-carrying enum + `Display`, not a stringly reason. |
| `parse_probe_json()` | `foundry/probe.rs` | Used by the golden fixtures, so they exercise the real parser. |
| `capability.rs` `ToolState{Present,Missing,Unusable}` | `foundry/capability.rs` (446) | Not used here (no host tooling in a pure function); noted so spec F inherits it rather than writing a second one. |

**Three findings from reading the tree that materially shape this spec:**

1. **`MediaProbe` does not carry the fields half these ceilings need** — no codec `profile`, no
   `level`, no frame rate, no bit depth (only `pix_fmt`), no HDR class. **But ffprobe is already being
   asked for them:** `build_ffprobe_args` passes `-show_streams`, whose JSON contains `profile`,
   `level`, `avg_frame_rate`, `bits_per_raw_sample`, `color_transfer` and `color_primaries` — the
   parser's private `RawStream` simply does not extract them. So the ask on spec A is **"extract more
   fields from JSON you already fetch"**, not "re-probe the library", which is a materially cheaper
   request and should be stated that way when the dependency is negotiated. Until those fields exist,
   the ceilings depending on them resolve to `CannotDecide` rather than silently passing (§3) — the
   discipline `plan.rs` already established.
2. **`Container` cannot distinguish WebM from Matroska, and `format_name` alone never will.**
   `policy.rs:197` reads `if has("matroska") || has("webm")`, mapping both to `Container::Matroska`.
   That is not an oversight: **ffmpeg uses one demuxer for both, so `format_name` is the literal
   string `"matroska,webm"` for a `.mkv` *and* for a `.webm`** — both `has()` calls fire on the same
   input either way. The function's own doc comment already documents this ("Matroska reports
   `matroska,webm`"), and `policy.rs:365` is a test pinning exactly that behaviour.

   Harmless for curation; **load-bearing for playback**, because Chromecast plays WebM and not MKV.
   So the refinement cannot live in `normalize_container` — that test is the constraint, and honouring
   it is what makes "Foundry's tests pass unmodified" achievable rather than aspirational. MDEC-01
   therefore adds the variant to the shared vocabulary but derives it **from content, in the decision
   module**, leaving `normalize_container` untouched.
3. **`plan.rs` independently reached this spec's design conclusions** — purity for testability on a
   host with no ffmpeg, structured reasons so tests assert on logic rather than prose, and a refusal
   to fabricate a benign default for an unobserved fact. Its module doc argues all three in nearly the
   words earlier drafts of this spec used. That convergence is validation, and also an instruction:
   **follow the existing convention rather than inventing a parallel one.** Concretely, this spec
   drops its earlier `Reason { subject, code, observed, limit }` struct in favour of `plan.rs`'s
   payload-carrying enum shape (MDEC-04).

**Where sharing needs new code:** neither module has a codec-name normaliser —
`policy::accepts_video_codec` does raw string comparison. MDEC-01 adds `media::codec` and uses it. It
does **not** retrofit Foundry in the same sprint: changing the matching semantics of 8,200 shipped
lines to tidy a duplicate is how a decision-engine sprint becomes a curation-regression sprint.
Adoption is filed as a follow-up.

## 2. Why purity is the load-bearing constraint

`plan()` performs **no I/O, no async, no filesystem access, no clock read, no randomness, no
logging** — the same posture `plan_transcode` documents, for the same reasons plus one more:

1. A pure function needs no fixture server, no ffmpeg, and no tokio runtime to test, so a matrix case
   costs one struct literal. Hundreds of cases become affordable, and hundreds is what "exhaustive
   over the cross-product" means (MDEC-08). It also makes property-based testing nearly free (MDEC-11).
2. Every failure is reproducible from its inputs alone. A `/why` dump **is** a complete bug report:
   paste it into a test and the bug reproduces offline.
3. **`ffprobe` and `ffmpeg` are absent from both the dev box and <host>** (epic §11, verified
   2026-07-31). Purity is not a preference here; it is the only way this logic is testable at all on
   the machines the work happens on. `plan.rs` says the same in its module doc.

MDEC-03 enforces it mechanically: a guard test greps the module for `tokio`, `async`, `.await`,
`std::fs`, `std::net`, `std::process`, `SystemTime`, `Instant`, `rand`, `tracing::`, and `std::env`.
Anything genuinely dynamic (a measured network ceiling, a client's declared capabilities) enters as a
**field on the `DeviceProfile` the caller composes**. The signature never grows a hidden input.

## 3. Outcomes: the tier lattice, plus the two ways of saying no

```
DirectPlay  <  Remux  <  Transcode{video: Copy}  <  Transcode{video: Encode}  <  Unplayable
                                                                              (  CannotDecide  )
```

`plan()` returns the **cheapest tier that satisfies every constraint**, never the first tier that
happens to work. The invariant, enforced by MDEC-07 and MDEC-11: *every escalation carries at least
one reason that justifies exactly that escalation.*

There are deliberately **two** negative outcomes, and conflating them is the mistake this section
exists to prevent:

- **`Unplayable { refusal }`** — a *decided* no. We know everything we need, and this device cannot
  play this file. `Refusal` is closed: `HdrRequiresToneMapping`, `TranscodeDisabledByPolicy`,
  `BurnInRequiredButTranscodeDisabled`, `NoSupportedVideoTarget`, `NoSupportedAudioTarget`,
  `NoSupportedContainer`, `DeviceDeclaresNoCapabilities`.
- **`CannotDecide { why: Undecidable }`** — an *undecided* no, reusing `plan.rs`'s own enum. We lack a
  fact: no codec name, no dimensions, no profile/level (finding 1), no HDR class against an SDR
  target. `plan.rs`'s rule applies unchanged — **unknown must never resolve to "fine"** — because
  `DirectPlay` is a claim that every dimension was checked and passed, exactly as `AlreadyOptimal` is.

**The playback-specific addition: who owns the fallback.** Curation can leave an undecidable file
alone at zero cost. Playback cannot — a refused session is a black screen. So the engine reports
`CannotDecide` honestly and the **caller** owns the product decision, via `MAESTRO_UNDECIDABLE_POLICY`
(`transcode_safe` — plan the most compatible transcode the device accepts and mark it `degraded`; or
`refuse` — surface it). The engine never silently picks one. This keeps `plan.rs`'s honesty while
accepting that the two domains pay different prices for saying no.

`Refusal::HdrRequiresToneMapping` is also where epic §8.3 is encoded **in the type system**: the
honest answer is representable and a tone-mapped plan has nowhere to live. Adding tone-mapping later
means deleting a variant and answering every match arm it breaks — a visible, reviewable act rather
than an `-vf tonemap` appearing quietly in spec E. Scope creep should have to argue with the compiler.

## 4. What is deliberately NOT modelled here

- **HDR tone-mapping (epic §8.3).** Out of scope. HDR direct-plays to capable devices; an SDR target
  is `Unplayable{HdrRequiresToneMapping}` by default, or an opt-in `degraded: true` transcode that
  admits the colours will be wrong. Both branches tested (MDEC-06). Never a silent washed-out picture.
- **Curation policy** — what to permanently re-encode, verify-and-swap, recycle bin, kill-switch.
  Foundry's, permanently (§1).
- **Argv construction.** `plan.rs` builds an argv because it plans an encode; this spec deliberately
  does not, and that absence is what proves it cannot mutate anything. Spec D/E turn a plan into ffmpeg.
- **Whether the plan is executable** — ffmpeg presence, GPU leases, encoder support. Spec D/E/F.
- **ABR ladders.** One `network_ceiling_bps` is honoured; multi-rendition selection is spec E's.
- **Library, metadata, watch state.** Muse's, permanently (epic §2).

## 5. Pre-flight

- [ ] Prefix `MDEC` registered (epic §11, 2026-08-01); `plane_prefix_promote` still outstanding
- [ ] `cargo test` green on `main` at `e8499aa`; record the count as the regression baseline
- [ ] **Spec A's promotion has landed, or is explicitly deferred.** C consumes `media::probe`. If A has
      not moved `foundry::probe` yet, C consumes it behind a `src/media/probe.rs` re-export and A
      completes the move — C never copies the type. Foundry's tests green before and after, untouched.
- [ ] **Negotiate the `MediaProbe` field extension with spec A** (finding 1): `profile`, `level`,
      `avg_frame_rate`, bit depth, HDR class — all already present in the ffprobe JSON that
      `build_ffprobe_args` requests. Confirm which land in A and which C must treat as `CannotDecide`.
      Add to the same negotiation `file_extension: Option<String>` (finding 2): `MediaProbe` carries no
      path or filename today, and spec A is the only place that holds the path at probe time. It is a
      **tiebreaker of last resort** for the Matroska/WebM ambiguity, never authority — C ships without
      it and is merely `CannotDecide` slightly more often.
- [ ] Confirm `toml` (profile parsing) and `proptest` (dev-only, MDEC-11) can be added; everything else
      is already in the tree. No async, HTTP, or DB dependency in the core.

---

## Items

### MDEC-01: Shared substrate alignment + the `DeviceProfile` model
- **Priority:** Critical
- **Labels:** maestro, decision, model
- **Agent:** claude
- **Estimate:** 7h
- **Description:** Define the typed capability model `plan()` reads, **on top of the existing
  substrate rather than beside it**. Two hard requirements: adding a new device is a config file and
  never a code change; and no type, helper, or constant already living in `foundry/{policy,plan}.rs`
  is duplicated here.

  ## FILES
  - `src/media/mod.rs` — the shared media-core module tree (`probe` is spec A's, `decision` is this
    spec's, `codec` is new here)
  - `src/media/codec.rs` — codec-name normalisation (`VideoCodec`, `AudioCodec`, `normalize_*`)
  - `src/media/decision/mod.rs` — module wiring and re-exports
  - `src/media/decision/profile.rs` — `DeviceProfile` and the capability types
  - `src/media/decision/registry.rs` — seed loading, operator overrides, baseline fallback
  - `src/foundry/policy.rs` — **extend `Container` with `WebM`**, behaviour-preservingly for Foundry
  - `src/config.rs` — `maestro_device_profile_dir()` (`MAESTRO_DEVICE_PROFILE_DIR`)
  - `.env.example`, `README.md`

  ## APPROACH
  1. `DeviceProfile { id, display_name, source: ProfileSource, containers: Vec<Container>,
     video: Vec<VideoCapability>, audio: Vec<AudioCapability>, subtitles: SubtitleCapability,
     hdr: HdrCapability, network_ceiling_bps: Option<u64>, adaptive: AdaptiveSupport,
     hdr_on_sdr_policy: HdrOnSdrPolicy, transcode_policy: DeviceTranscodePolicy }`.
     - `containers` uses **`foundry::policy::Container`**, not a new enum.
     - `source: { Seed | OperatorOverride | ClientDeclared }` — provenance travels with the profile so
       `/why` answers "where did this table come from?" without a second lookup (MDEC-10 sets it).
     - `transcode_policy: { Allow | DirectAndRemuxOnly }` (default `Allow`) — a per-device kill switch
       turning would-be transcodes into `Unplayable{TranscodeDisabledByPolicy}`. Named distinctly from
       Foundry's `TranscodePolicy` because they are unrelated objects (§1).
  2. `VideoCapability { codec: VideoCodec, max_profile, max_level, max_width, max_height,
     max_framerate_milli, max_bitrate_bps, max_bit_depth }` — one entry per codec, because "HEVC yes"
     is precisely the imprecision that black-screens a 10-bit file. `max_profile`/`max_level` are
     codec-scoped **ordered** types (`H264: Baseline<Main<High<High10`, `Hevc: Main<Main10`,
     `Vp9: P0<P2`) so "exceeds" is a comparison, not a string match.
  3. `AudioCapability { codec: AudioCodec, max_channels, decode: bool, passthrough: bool }` — the
     booleans are independent: a device may pass E-AC-3 through to a receiver it cannot itself decode,
     and the plan must say `Passthrough` rather than `Encode` so spec D knows the difference.
  4. `SubtitleCapability { text_formats, image_subs: ImageSubSupport{Native|RequiresBurnIn|Unsupported},
     external_delivery: bool }`; `HdrCapability { hdr10, hlg, dolby_vision, max_bit_depth }`.
  5. **`src/media/codec.rs`** — the normaliser neither module has today, mapping ffprobe names onto
     typed codecs (`hevc`/`h265`/`hvc1`, `h264`/`avc1`, `eac3`/`e-ac-3`, `vp9`/`vp09`, `av1`/`av01`,
     `truehd`, `dts`, …), unknown → `None`, never a guess. **Foundry is not retrofitted this sprint** —
     file it as a follow-up; changing the matching semantics of shipped curation code to remove a
     duplicate is not this sprint's risk to take.
  6. **The `Container::WebM` split — derived from content, not from the container name.** This is the
     one genuinely subtle piece of the item, and getting it wrong stalls implementation, so the
     strategy is fixed here rather than discovered later.

     **The problem.** ffmpeg demuxes both formats with one demuxer, so `format.format_name` is the
     literal string `"matroska,webm"` for a `.mkv` and for a `.webm` alike. No amount of parsing that
     string can separate them — which is why `normalize_container` collapses them, and why
     `policy.rs:365` pins that collapse in a test.

     **What is left untouched.** `normalize_container()` keeps its exact current behaviour and its
     test. The `Container` enum gains a `WebM` variant, which forces exactly two new match arms
     (`ffmpeg_format` → `"webm"`, `extension` → `"webm"`) and nothing else. **Foundry's defaults are
     not changed at all** — not its `acceptable_containers`, not its `output_container`. Because
     `normalize_container` never produces `WebM`, the variant is unreachable from every Foundry code
     path, so curation behaviour is unchanged **by construction rather than by inspection**. (An
     earlier draft of this item said to add `WebM` to Foundry's default accepted set. That would have
     been a real, if inert, semantic change to curation policy for no benefit, and is superseded.)

     **What is added.** `media::decision::container::resolve_playback_container(&MediaProbe,
     name_hint: Option<&str>) -> ContainerResolution`, where

     ```
     ContainerResolution = Resolved(Container)                        // unambiguous demuxer
                         | MatroskaFamily { webm_eligible: bool }     // the ambiguous pair
                         | Ambiguous(Undecidable)                     // cannot prove either way
     ```

     For anything other than the Matroska/WebM pair, `normalize_container`'s answer stands. For the
     pair, the discriminator is **codec-based inference**, which is the honest signal because it is
     the one the device's demuxer actually cares about: a file is `webm_eligible` when every video
     stream is VP8/VP9/AV1, every audio stream is Vorbis/Opus, every subtitle stream is WebVTT, and
     there are no attachments (WebM has no attachment concept). Every input for that judgement is
     already in `MediaProbe`.
  6b. **`MatroskaFamily` avoids a false dichotomy, and that is the point.** The question is not "is
     this file *called* WebM" but "will this device's demuxer accept it", so the acceptance test is:

     > accepted ⟺ `device.containers` contains `Matroska`, **or** (`webm_eligible` ∧
     > `device.containers` contains `WebM`)

     A WebM-eligible file is playable by both MKV-capable and WebM-capable devices, so resolving it to
     a single name would throw away information. This also collapses the hard case almost to nothing:
     the ambiguity only *bites* when a file is Matroska-family, the device accepts **WebM but not
     Matroska** (which is every Cast seed), and eligibility cannot be proven because a codec name was
     missing or unrecognised.
  6c. **The name hint is a tiebreaker for that narrow case only, and it is not available today.**
     `MediaProbe` carries no path or filename — its fields are container, duration, bitrate, size, the
     stream vectors, the counts, and `title`. So a hint has to be threaded in, and the right owner is
     spec A, which holds the path at probe time: it adds `file_extension: Option<String>` when it
     persists the probe. Until then the hint is `None`. It is **a hint and never authority** — this
     library is full of scene releases with arbitrary naming, so it may only ever resolve a case that
     codec inference left unproven, and it may never override codec inference that succeeded.
  6d. **When neither resolves it: `CannotDecide{ContainerAmbiguousMatroskaWebm}`, not a guess.** This
     is a missing fact, not a decided refusal, so it belongs on the `CannotDecide` side of §3 exactly
     as the house discipline requires. Guessing WebM would black-screen a Cast; guessing Matroska
     would order a needless transcode of a file that would have direct-played.
  7. Serde everywhere with `#[serde(deny_unknown_fields)]`, so a typo in an operator's TOML is a
     startup error naming the field rather than a silently ignored line. Seeds are `include_str!`-
     embedded (a stock deploy needs no files on disk); a file in `MAESTRO_DEVICE_PROFILE_DIR`
     **replaces** the seed of the same `id` wholesale, never a deep merge — partial merges of
     capability tables produce profiles nobody can reason about. Unknown id → `safe-baseline`, and the
     caller is told it was substituted.
  8. No secrets in this item — profile config is behavioural, so `config.rs` helpers are correct and
     `SecretManager` does not apply. Say so in the README so nobody "fixes" it later.

  ## TEST PLAN
  - `cargo test media::decision::profile` — serde round-trip; ordered profile/level comparisons;
    `deny_unknown_fields` rejects a typo with the field name
  - `cargo test media::codec` — normalisation table tests including unknown → `None`
  - `cargo test foundry::` — **Foundry's existing suite passes unmodified** after the `Container` split,
    including `policy.rs`'s `container_names_are_matched_as_a_list_not_as_a_whole_string`, which still
    asserts `normalize_container("matroska,webm") == Some(Container::Matroska)`
  - `cargo test media::decision::container` — a VP9/Opus probe is `MatroskaFamily{webm_eligible: true}`;
    an H.264/AC-3 probe in the same demuxer pair is `webm_eligible: false`; a VP9 probe carrying font
    attachments or a PGS track is `webm_eligible: false`
  - Acceptance asymmetry: a `webm_eligible` file is accepted by both a Matroska-accepting and a
    WebM-accepting device; a non-eligible one is accepted only by the Matroska-accepting device
  - An unknown video codec in a Matroska-family file → `Ambiguous`, which becomes
    `CannotDecide{ContainerAmbiguousMatroskaWebm}` **only** against a WebM-but-not-Matroska device, and
    is harmless against a Matroska-accepting one
  - A name hint of `.mkv` on a `webm_eligible` file does **not** make it WebM-ineligible (hint never
    overrides successful inference)
  - `Container::WebM` is unreachable from every Foundry code path (asserted by a test that
    `normalize_container` never returns it for any input in the corpus)
  - Registry: override replaces a seed by id; unknown id adds a profile; malformed file fails startup
    naming the file; unknown id at lookup → `safe-baseline` with the substitution reported
  - Verify no hardcoded IPs, hostnames, or org names in new/modified files
  - `cargo test` green (single crate — `muse`, both bins)

  ## EDGE CASES
  - `MAESTRO_DEVICE_PROFILE_DIR` unset/absent/empty → seeds only, warn once, never fail startup
    (config-gated degradation, epic §7.4)
  - Two override files with the same `id` → hard startup error naming both; last-write-wins would make
    the live capability table depend on readdir order
  - Empty `containers` or `video` list → valid, and every plan against it is
    `Unplayable{DeviceDeclaresNoCapabilities}`; a legitimate way to disable a device
  - A profile claiming HDR10 with `max_bit_depth: 8` → contradictory, rejected at load
  - A Matroska-family file with **no streams the WebM subset forbids but also none it requires** (e.g.
    audio-only Opus) → `webm_eligible: true`; the subset test is "nothing outside the set", not
    "something inside it"
  - A file whose video codec is VP9 but whose audio is AC-3 → not eligible; one disqualifying stream is
    enough, since the device must demux the whole file
  - `name_hint` present but not a recognised extension → ignored, exactly as if absent

- **Acceptance criteria:**
  - [ ] `DeviceProfile` reuses `foundry::policy::Container` and defines no parallel container type
  - [ ] `Container::WebM` exists; `normalize_container` is **unchanged** and **Foundry's tests pass
        unmodified**, including the `"matroska,webm"` assertion
  - [ ] WebM is discriminated by **codec-based content inference**, not by the container name, and
        `MatroskaFamily{webm_eligible}` is modelled so an eligible file satisfies both device kinds
  - [ ] An unprovable Matroska/WebM case is `CannotDecide{ContainerAmbiguousMatroskaWebm}` — never a guess
  - [ ] `Container::WebM` is unreachable from Foundry code paths, so curation is unchanged by construction
  - [ ] `media::codec` normalises ffprobe codec names to typed codecs, unknown → `None`
  - [ ] A new device is added by one TOML file — no code change, no recompilation
  - [ ] An unknown-field typo is a startup error naming the field and file; unknown id → `safe-baseline`
  - [ ] Profile carries `source` provenance and a `transcode_policy` kill switch
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README documents the schema, the override mechanism, and the Foundry-shared substrate
  - [ ] All existing tests still pass

---

### MDEC-02: Seeded device profiles — browser, the Chromecast generations, native mobile
- **Priority:** Critical
- **Labels:** maestro, decision, profiles, data
- **Agent:** claude
- **Estimate:** 6h
- **Description:** Populate the registry with the devices the household owns. The epic's claim that
  this problem is tractable rests on the device matrix being **closed and small**, and this is where
  that claim is cashed in. Cast support is a published per-generation table and must be modelled per
  generation: collapsing the generations is the likeliest source of a wrong plan in the whole spec,
  because the spread between a gen-2 dongle and a Google TV Streamer is enormous.

  ## FILES
  - `data/device_profiles/{browser-constellation-web,cast-gen1-2,cast-gen3,cast-ultra,cast-google-tv,
    cast-nest-hub,cast-google-tv-streamer,native-mobile,safe-baseline}.toml`
  - `src/media/decision/seeds.rs` — `include_str!`, parse once, `builtin_profiles()`
  - `README.md` — the seeded-profile table and its provenance

  ## APPROACH
  1. **Cast, per published generation** (each file carries a comment recording that it is transcribed
     from the published Cast supported-media table, with the transcription date):
     - H.264 on **every** generation (High profile; level/resolution ceilings per generation)
     - HEVC **Main and Main10 up to L5.1** on Ultra and Google TV devices only
     - VP9 **profile 0 and 2** on Ultra, Google TV, and Nest Hub
     - AV1 on the **Google TV Streamer only**
     - Audio everywhere: AAC, MP3, Opus, Vorbis, LPCM (`decode: true`); AC-3 and E-AC-3 as
       `decode: false, passthrough: true`
     - Containers: `Mp4`, `MpegTs`, `WebM` — **and not `Matroska`**, which is exactly why MDEC-01
       splits the variant; adaptive HLS and DASH both true
     - HDR: HDR10 + HLG on the 4K-class generations only
  2. **`browser-constellation-web`** — the conservative end, and the profile spec G hits first: H.264
     High, VP9, AV1; **HEVC off by default** (platform-gated in practice, and a wrong direct-play is
     worse than a right transcode); audio AAC/Opus/Vorbis/MP3/FLAC with `max_channels: 2`,
     `passthrough: false`; subtitles text-only, external, WebVTT, `image_subs: RequiresBurnIn`; HDR all
     false.
  3. **`native-mobile`** — permissive, for the future app: H.264/HEVC/VP9/AV1, 10-bit, HDR10+HLG, up to
     8 channels, MP4/Matroska/MpegTs/WebM, and `image_subs: Native` — so PGS does **not** force a
     burn-in here. That contrast against the browser profile is what MDEC-05 proves.
  4. **`safe-baseline`** — the least capable thing that still plays: H.264 High L4.1 ≤1080p30, AAC ≤2ch,
     MP4, WebVTT external, SDR. Used for any unrecognised device.
  5. No IPs, hostnames, or account identifiers in any profile file (S1) — a profile is a capability
     table, not a device address.
  6. A test asserts every seed parses, ids are unique, and structural sanity holds (an audio entry with
     `decode: false, passthrough: false` is meaningless and rejected).

  ## TEST PLAN
  - `cargo test media::decision::seeds` — all parse; ids unique; sanity checks hold
  - Generation-differentiation asserted individually: HEVC Main10 accepted by `cast-ultra` and
    `cast-google-tv`, rejected by `cast-gen3`; VP9 P2 accepted by `cast-nest-hub`, rejected by
    `cast-gen1-2`; AV1 accepted **only** by `cast-google-tv-streamer`
  - A Cast seed accepts `WebM` and **rejects** `Matroska` (the regression the MDEC-01 split exists for)
  - E-AC-3 resolves to passthrough, not decode, on every Cast seed
  - `browser-constellation-web` is `max_channels == 2` and `image_subs == RequiresBurnIn`
  - Verify no hardcoded IPs or org names in new/modified files

  ## EDGE CASES
  - An ambiguous published entry → seed the **conservative** reading and record the ambiguity in the
    file comment. A wrong "yes" is a black screen; a wrong "no" is a transcode.
  - Nest Hub's small panel → `max_width`/`max_height` set to it, so 4K downscales rather than shipping
    whole to a 7-inch screen
  - A Cast device behind a receiver that cannot take E-AC-3 → out of scope; an operator override is the
    sanctioned way to turn passthrough off

- **Acceptance criteria:**
  - [ ] Nine seeds parse at startup with no external files required
  - [ ] Cast generations modelled **distinctly**, with HEVC/VP9/AV1 differences asserted per generation
  - [ ] Cast seeds accept WebM and reject Matroska
  - [ ] AC-3/E-AC-3 modelled as passthrough-capable, decode-incapable on Cast
  - [ ] `browser-constellation-web` is stereo-max and image-sub-incapable
  - [ ] No hardcoded infrastructure values in any profile file or new code
  - [ ] README lists the seeds and their provenance
  - [ ] All existing tests still pass

---

### MDEC-03: The pure `plan()` — signature, outcomes, source and track selection
- **Priority:** Critical
- **Labels:** maestro, decision, core
- **Agent:** claude
- **Estimate:** 9h
- **Description:** The core. Pure, total, synchronous, allocating only its output. The signature is
  fixed **now**, before golden fixtures exist, because every field added afterwards regenerates every
  fixture — which is precisely why multi-source and track selection are in this item rather than
  deferred to spec G.

  ## FILES
  - `src/media/decision/plan.rs` — `plan()`, `plan_all()`, the tier walk
  - `src/media/decision/types.rs` — `PlaybackPlan`, `PlaybackOutcome`, `VideoPlan`, `AudioPlan`,
    `SubtitlePlan`, `PlaybackTier`, `Refusal`, `PlaybackRequest`
  - `src/media/decision/select.rs` — source selection across several `MediaProbe`s
  - `src/media/decision/purity_tests.rs` — the mechanical guard
  - `README.md`

  ## APPROACH
  1. **Signature, fixed now:**
     ```
     pub fn plan(sources: &[MediaProbe], device: &DeviceProfile, req: &PlaybackRequest) -> PlaybackPlan
     pub struct PlaybackRequest {
         audio_track: TrackSelection,      // Auto | Index(u32)
         subtitle_track: TrackSelection,   // None | Auto | Index(u32)
         start_position_secs: Option<f64>, // reserved for D/E; ignored by the pure walk
     }
     pub struct PlaybackPlan { source_index: usize, outcome: PlaybackOutcome,
                               reasons: Vec<PlaybackReason> }
     pub enum PlaybackOutcome {
         DirectPlay,
         Remux { container: Container },
         Transcode { container: Container, video: VideoPlan, audio: AudioPlan,
                     subs: SubtitlePlan, degraded: bool },
         Unplayable { refusal: Refusal },
         CannotDecide { why: Undecidable },
     }
     pub enum VideoPlan { Copy, Encode { codec, profile, level, bitrate_bps, width, height } }
     pub enum AudioPlan { Copy, Passthrough, Downmix { channels },
                          Encode { codec, channels, bitrate_bps, loudnorm: bool } }
     pub enum SubtitlePlan { None, External { format }, Burn }
     ```
     `AudioPlan::Encode` carries **`loudnorm: bool` from the first commit** even though nothing sets it
     true in S130. Reserving the field costs one line now and saves regenerating every golden fixture
     later, which is the entire argument.
  2. **Multi-source selection** (`&[MediaProbe]`, not one). An item can have several files — a 4K HDR
     remux and a 1080p SDR encode — and choosing between them **is a playback decision**, so it belongs
     here rather than in spec D hardcoding "first file". Rule, deterministic for fixtures: plan each
     source, keep the **cheapest tier**; tie-break on highest resolution within the device cap, then
     highest bitrate within the ceiling, then lowest input index. The chosen `source_index` is on the
     plan, and the reasons record that a cheaper source displaced a more capable one — "why is it
     playing the 1080p?" is a question that will be asked.
  3. **Track selection.** `Auto` audio picks the first stream the device can `Copy`/`Passthrough`, else
     the first it can encode; an explicit `Index` is honoured or, if absent from the file, produces
     `CannotDecide{UnknownAudioTrackIndex}` rather than silently substituting another track — playing
     different audio than asked is worse than an error.
  4. **The walk**, cheapest-first: container accepted ∧ every video constraint met ∧ every audio
     constraint met ∧ no burn-in needed → `DirectPlay`. The container test is
     `resolve_playback_container()` (MDEC-01) rather than a bare equality — accepted ⟺ the device lists
     the resolved container, **or** the file is `webm_eligible` and the device lists `WebM`. Container
     wrong, codecs fine, target container can carry what the source has (`container_holds_attachments`)
     → `Remux`. Video fine, audio not →
     `Transcode{video: Copy}`. Video not fine → `Transcode{video: Encode}` targeting the best option
     the device accepts, capped to its profile/level/resolution/framerate/bit-depth, using
     `scale_to_fit()` for aspect-preserving downscale and bitrate
     `min(source, codec_max, network_ceiling)`. **Never upscale, on any branch.**
  5. **Audio preference order: `Copy` > `Passthrough` > `Downmix` > `Encode`.** Passthrough beats
     re-encoding because it is free and preserves the surround mix; getting this backwards silently
     destroys 5.1 and nobody files a bug for it. Named regression test.
  6. **Undecidables first, exactly as `plan.rs` does it** — checked before any capability comparison, so
     a partial probe can never yield a partial verdict that looks like `DirectPlay`. Reuse
     `Undecidable` verbatim where the variant exists (`UnknownVideoCodec`, `UnknownVideoDimensions`,
     `UnknownAudioCodec`, `UnknownAudioChannels`, `UnrecognizedContainer`, `UnindexedStreams`) and
     extend with playback-only variants (`UnknownVideoProfile`, `UnknownVideoLevel`, `UnknownFrameRate`,
     `UnknownBitDepth`, `UnknownHdrClass`, `UnknownAudioTrackIndex`, `NoSourcesOffered`,
     `ContainerAmbiguousMatroskaWebm`). Per §1b
     finding 1 several of these fire until spec A extends the parser — correct behaviour, not a gap: an
     unmeasured level must not be assumed within the device's ceiling.
  7. **Bitrate via `bitrate_verdict()` verbatim.** `Exceeds` escalates; `WithinCeiling` passes;
     `Unknown` is **not** ignored — if it is the only thing between the file and `DirectPlay`, the
     outcome is `CannotDecide{UnknownVideoBitrate}`, matching `plan.rs` exactly. (An earlier draft said
     to skip the constraint when unknown; that contradicted the house discipline and is superseded.)
  8. **`plan_all(&[MediaProbe], &[&DeviceProfile], &PlaybackRequest) -> Vec<(ProfileId, PlaybackPlan)>`**
     — a pure, order-preserving fan-out for the two non-session consumers: "which of my devices can play
     this?" and spec A's direct-play-fraction backfill (epic §6). No ranking or recommendation — that is
     the caller's policy.
  9. Totality: never panics, never returns `Result`; handles empty `sources`, audio-only, video-only,
     and unknown codecs (which escalate or refuse, never optimistically direct-play).
  10. **Purity guard** (`purity_tests.rs`): a `#[cfg(test)]` module pulling each sibling in via
      `include_str!` — the existing Muse convention (`src/tracker/interpret.rs`,
      `src/watch_together/sync.rs`) and the only way the guard avoids tripping its own check. Test
      modules are excluded by name; a companion assertion fails when a new file appears in the
      directory classified as neither source nor test.

  ## TEST PLAN
  - `cargo test media::decision::plan` — one test per branch, plus one per `Refusal` variant
  - H.264/AAC/MP4 → `browser-constellation-web` = `DirectPlay` (the epic §6 headline case)
  - H.264/AAC in MKV → a Cast seed = `Remux`, streams untouched
  - VP9/Opus in the Matroska/WebM demuxer pair → a Cast seed = `DirectPlay` (WebM-eligible by codec
    inference), while the byte-identical `format_name` with H.264/AC-3 streams = `Remux`
  - The same VP9/Opus file → `native-mobile` (accepts Matroska and WebM) = `DirectPlay` either way
  - A Matroska-family file with an unrecognised video codec → `CannotDecide{ContainerAmbiguousMatroskaWebm}`
    against a Cast seed, and a normal codec-driven outcome against `native-mobile`
  - H.264 + DTS 5.1 → browser = `Transcode{video: Copy, audio: Encode}`
  - HEVC Main10 → `cast-gen3` = `Transcode{video: Encode{h264}}`; → `cast-ultra` = `DirectPlay`
  - E-AC-3 5.1 → Cast = `AudioPlan::Passthrough`, **not** `Encode` (named regression test)
  - 4K → 1080p-max device = `Encode` at exactly the cap via `scale_to_fit`; 720p → 4K device = no upscale
  - Two sources (4K HEVC + 1080p H.264) → browser picks the 1080p with a reason; `cast-ultra` picks the
    4K. Same inputs, different devices, different `source_index`
  - Explicit `audio_track: Index(9)` absent from the file → `CannotDecide{UnknownAudioTrackIndex}`
  - A probe with no profile/level (today's `MediaProbe`) against a profile-constrained device →
    `CannotDecide`, never `DirectPlay`
  - Video bitrate absent, container bitrate above the ceiling, nothing else wrong →
    `CannotDecide{UnknownVideoBitrate}` (parity with `plan.rs`)
  - `plan_all` over nine seeds returns nine entries in order, each equal to the single call
  - `cargo test media::decision::purity` — guard passes clean, proven to fail when I/O is added
  - Verify no hardcoded IPs or org names in new/modified files

  ## EDGE CASES
  - `sources` empty → `CannotDecide{NoSourcesOffered}`, no panic and no index into nothing
  - Audio-only file → `DirectPlay` if container and codec fit. **This deliberately differs from
    `plan_transcode`**, which correctly calls audio-only `Undecidable{NoVideoStream}` because *its*
    policy is written for video files. The divergence is intentional and documented in both directions,
    so a future reader does not "fix" one to match the other.
  - Video-only file → decided on video alone; no audio plan fabricated
  - All sources `Unplayable` → return the **cheapest refusal**, not the first, so the reason shown is
    the most actionable one
  - `transcode_policy: DirectAndRemuxOnly` on a file needing an encode → `Unplayable`, never a silent
    transcode

- **Acceptance criteria:**
  - [ ] `plan(&[MediaProbe], &DeviceProfile, &PlaybackRequest)` — multi-source and track selection in the
        signature from the first commit, with `loudnorm` reserved on `AudioPlan::Encode`
  - [ ] Synchronous, non-async, never panics, performs no I/O; the purity guard proves it
  - [ ] `bitrate_verdict`, `scale_to_fit`, `normalize_container`, `container_holds_attachments` and
        `Undecidable` are **reused from the existing modules**, not reimplemented
  - [ ] `CannotDecide` is returned for every unobserved fact — unknown never resolves to `DirectPlay`
  - [ ] Source selection is deterministic and records why a source was chosen
  - [ ] Audio passthrough is preferred over re-encoding; no upscaling on any branch
  - [ ] `plan_all()` is pure, order-preserving, and agrees with `plan()` case for case
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDEC-04: `PlaybackReason` — following the shipped reason convention
- **Priority:** Critical
- **Labels:** maestro, decision, observability
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Every decision attaches a machine-readable reason. `plan.rs` already established the
  house shape — a payload-carrying enum with a `Display` impl, chosen so tests assert on the *reason*
  rather than on prose that can be reworded without anyone noticing the logic changed underneath. This
  item follows that convention rather than inventing a parallel one, and adds only what playback needs
  that curation did not: a serde representation, because `/why` puts these on the wire.

  ## FILES
  - `src/media/decision/reason.rs` — `PlaybackReason`, `Display`, serde
  - `src/media/decision/plan.rs` — emit at every decision point
  - `README.md` — the reason table

  ## APPROACH
  1. `PlaybackReason` mirrors `TranscodeReason`'s shape exactly — variants carrying observed and
     limiting values as **typed fields**:
     `VideoProfileExceedsMax { found: HevcProfile, max: HevcProfile }`,
     `AudioChannelsExceedMax { stream_index: u32, found: u32, max: u32 }`,
     `ContainerNotSupported { found: Container }`,
     `ContainerAcceptedAsWebmEligible { }` (the file demuxes as Matroska/WebM and was accepted because
     its streams are within the WebM subset — worth its own reason, since "why did this MKV direct-play
     to a Chromecast?" is otherwise a genuinely baffling `/why`), `SourceDisplacedCheaperTier { .. }`,
     `SubtitleImageRequiresBurnIn { stream_index: u32 }`, `BurnInForcesVideoEncode`,
     `TranscodeDisabledByPolicy`, `HdrSourceSdrTarget { found: HdrClass }`,
     `DeviceProfileSubstituted { requested: String }`, `CapabilityClientDeclared { codec: VideoCodec }`,
     `DirectPlaySupported`.
  2. **The one addition over `TranscodeReason`:** `#[derive(Serialize)]` with
     `#[serde(tag = "code", rename_all = "snake_case")]`, so the wire form is a tagged object with typed
     fields — machine-readable for the GUI, and stable enough that a test pins the tag strings (a rename
     is a breaking API change for spec G).
  3. `Display` renders the human sentence, derived from the data. Tests assert on variants, never on
     prose — the rule `plan.rs`'s own comment gives.
  4. A derived `fn subject(&self) -> ReasonSubject { Container | Video | Audio | Subtitle | Hdr | Source
     | Tier }` exists **only** to power the tier-justification invariant (MDEC-07). It is a method over
     the enum, not a field, so it cannot drift from the variant it describes.
  5. Deterministic ordering: subject order, then emission order. The snapshot matrix diffs on it, and a
     `HashMap` iteration would produce phantom diffs forever.
  6. **`DirectPlay` is not reason-free** — it carries `DirectPlaySupported`. "Why did this work?" is as
     real a question as its opposite, and an empty list is indistinguishable from a bug that forgot to
     record one. This mirrors `plan.rs`'s guarantee that a `Transcode` can never carry an empty reason
     list; the same unrepresentability is enforced here for every non-`CannotDecide` outcome.

  ## TEST PLAN
  - `cargo test media::decision::reason` — serde round-trip; tag strings pinned
  - HEVC Main10 → `cast-gen3` emits `VideoProfileExceedsMax { found: Main10, max: Main }`
  - 7.1 audio → stereo device emits `AudioChannelsExceedMax { found: 8, max: 2 }`
  - A plan with an empty reason list fails the invariant test
  - Ordering stable across 100 identical invocations
  - Verify no hardcoded IPs or org names in new/modified files

  ## EDGE CASES
  - Several constraints violated at once (HEVC ∧ 4K ∧ 10-bit → 1080p H.264 device) → **all** recorded;
    a first-violation short-circuit would make `/why` lie by omission
  - `CannotDecide` carries its `Undecidable` and needs no `PlaybackReason` — the two vocabularies stay
    distinct, exactly as `plan.rs` keeps `TranscodeReason` and `Undecidable` distinct
  - A client-declared capability that decided the outcome → `CapabilityClientDeclared` is present, so
    "it works on my TV" is answerable from the plan

- **Acceptance criteria:**
  - [ ] `PlaybackReason` follows `TranscodeReason`'s payload-enum + `Display` shape, adding serde
  - [ ] Wire tags are pinned by a test so a rename cannot silently break the GUI
  - [ ] Every non-`CannotDecide` plan carries a non-empty, deterministically ordered reason list
  - [ ] Multiple simultaneous violations all appear
  - [ ] `subject()` is derived, not stored
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README documents the reason table
  - [ ] All existing tests still pass

---

### MDEC-05: Subtitle consequence modelling — burn-in is a video transcode, and the plan says so
- **Priority:** High
- **Labels:** maestro, decision, subtitles
- **Agent:** claude
- **Estimate:** 5h
- **Description:** Subtitles are where a decision engine most commonly lies to itself. A text track is
  nearly free — externalise it as WebVTT and the video path is untouched. An image track (PGS, VOBSUB)
  on a device that cannot render one can only be burned in, which **forces a full video encode** even
  when the codec was perfectly acceptable. That escalation must be visible at decision time, not
  discovered at ffmpeg time when a direct-play-eligible file suddenly pins a CPU.

  ## FILES
  - `src/media/decision/subtitles.rs`
  - `src/media/decision/plan.rs` — apply before finalising the tier
  - `src/media/decision/reason.rs`
  - `README.md` — "Subtitles and their cost"

  ## APPROACH
  1. Classify from `SubtitleStream.codec` (the shipped field): text (`subrip`, `ass`, `ssa`, `mov_text`,
     `webvtt`) vs image (`hdmv_pgs_subtitle`, `dvd_subtitle`). An unrecognised subtitle codec is
     `CannotDecide{UnknownSubtitleCodec}` **only when a track was actually selected** — an unselected
     mystery track costs nothing and must not block playback of the file.
  2. Text + `external_delivery` → `External{WebVTT}` with `SubtitleTextExternalised`. **Never escalates
     the tier** — a direct-play file with an SRT stays `DirectPlay` and gains a sidecar. Named
     regression test, because the opposite is the classic accidental-transcode bug.
  3. Image + `RequiresBurnIn` + selected → `SubtitlePlan::Burn` **and** force `VideoPlan::Encode`
     regardless of the video verdict, recording **two** reasons: `SubtitleImageRequiresBurnIn` (why the
     burn) and `BurnInForcesVideoEncode` (why the tier moved). Two codes because they answer different
     questions, and MDEC-07's invariant needs the second.
  4. Image + `Native` (the `native-mobile` seed) → passed through, no burn, no escalation. The same file
     producing two different tiers on two devices for a reason that is neither codec nor container is
     the clearest demonstration that the model is complete.
  5. Image + `Unsupported` + selected → the subtitle is **dropped** with a reason; the video plays.
     Failing an entire playback because a subtitle cannot be rendered is a worse outcome than playing
     without it.
  6. Burn-in on a `DirectAndRemuxOnly` device → `Unplayable{BurnInRequiredButTranscodeDisabled}`,
     carrying both the burn-in reason and `TranscodeDisabledByPolicy`. A distinct refusal from the plain
     policy one because the remedy differs — this one is also fixable by deselecting the subtitle, and a
     GUI can offer exactly that.
  7. Selection is an input (`PlaybackRequest.subtitle_track`), never a guess. Nothing selected means no
     subtitle cost even on a file carrying five PGS tracks — which is what stops a library of PGS-laden
     remuxes from transcoding by default.
  8. Remux interaction: `container_holds_attachments()` is consulted — remuxing an MKV carrying subtitle
     fonts into MP4 loses them, so a styled-ASS track plus attachments makes that remux
     `CannotDecide{AttachmentsCannotBeCarried}` rather than a silent downgrade. Reused, not rewritten.

  ## TEST PLAN
  - `cargo test media::decision::subtitles`
  - Direct-play-eligible file + selected SRT → browser = `DirectPlay` + `External{WebVTT}`, tier
    unchanged (named regression test)
  - Same file + selected PGS → browser = `Transcode{video: Encode, subs: Burn}` with both reasons
  - Same file + selected PGS → `native-mobile` = no burn, no encode
  - PGS on `image_subs: Unsupported` → dropped with a reason, video not escalated
  - PGS on a burn-in device with transcoding disabled → `Unplayable{BurnInRequiredButTranscodeDisabled}`;
    deselecting the subtitle in the same fixture returns `DirectPlay`
  - Five PGS tracks, none selected → `SubtitlePlan::None`, `DirectPlay` preserved
  - MKV with font attachments remuxed toward MP4 → `CannotDecide{AttachmentsCannotBeCarried}`
  - Verify no hardcoded IPs or org names in new/modified files

  ## EDGE CASES
  - ASS/SSA with embedded styling → text for tier purposes; WebVTT fidelity loss is noted in the README
    and is spec E's problem, never a silent escalation here
  - Burn-in on a device with no usable encode target → `Unplayable{NoSupportedVideoTarget}` carrying the
    burn reason too
  - An external subtitle file rather than an embedded stream → same path; the text's origin does not
    change its cost

- **Acceptance criteria:**
  - [ ] A selected text subtitle never escalates the tier
  - [ ] A selected image subtitle on a burn-in device forces `VideoPlan::Encode` and records both reasons
  - [ ] The identical file yields different tiers on browser vs `native-mobile` purely from
        image-subtitle capability, asserted
  - [ ] Burn-in under `DirectAndRemuxOnly` is a distinct, deselection-fixable refusal
  - [ ] An unrenderable subtitle is dropped with a reason rather than failing playback
  - [ ] `container_holds_attachments()` is reused for the remux consequence
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README documents the cost of each subtitle kind
  - [ ] All existing tests still pass

---

### MDEC-06: HDR — direct play where capable, honest refusal where not, no tone-mapping
- **Priority:** High
- **Labels:** maestro, decision, hdr
- **Agent:** claude
- **Estimate:** 4h
- **Description:** Epic §8.3 puts tone-mapping out of scope; this item makes that boundary a **tested
  behaviour** rather than a gap. HDR direct-plays to capable devices. An HDR source aimed at an SDR-only
  device is a documented outcome — a clean refusal the GUI can explain, or an explicitly opted-in
  degraded transcode that admits in the plan that the colours will be wrong. What must never happen is
  a silent washed-out picture reported as success.

  ## FILES
  - `src/media/decision/hdr.rs`
  - `src/media/decision/plan.rs`, `src/media/decision/profile.rs`
  - `README.md` — "HDR", the boundary and the policy knob

  ## APPROACH
  1. Match the source HDR class (`Sdr`, `Hdr10`, `Hlg`, `DolbyVision` — **spec A's classification**,
     derived from `color_transfer`/`color_primaries`, which `-show_streams` already returns) against
     `HdrCapability`. Capable and otherwise fine → `DirectPlay`. HDR is never by itself a reason to
     transcode to a capable device.
  2. Bit depth is checked **independently**: 10-bit SDR to an 8-bit device escalates on
     `VideoBitDepthExceedsMax`, a different reason from an HDR mismatch. Conflating them produces a
     confusing `/why`.
  3. HDR → SDR-only, policy `Reject` (default) → `Unplayable{HdrRequiresToneMapping}` carrying
     `HdrSourceSdrTarget{found: Hdr10}`. The refusal variant *is* the type-level record of §8.3:
     removing it is what adding tone-mapping would cost.
  4. Policy `DegradedTranscode` (per-profile operator opt-in) → a normal `Transcode` with
     `degraded: true` and the reason retained, so every consumer knows the output is colour-inaccurate.
  5. Dolby Vision to an HDR10-only device → the same policy path, no base-layer extraction.
  6. **Unknown HDR class against an SDR-only target → `CannotDecide{UnknownHdrClass}`**, not "assume
     SDR". Against an HDR-capable target it does not matter and does not block. (An earlier draft said
     to assume SDR when unknown; that violated the `plan.rs` discipline and is superseded.)
  7. A comment at the top of `hdr.rs` states the out-of-scope decision and cites epic §8.3, so a future
     agent adding a tone-map hits the intent before the code (the v4.6 document-intent-in-code rule).

  ## TEST PLAN
  - `cargo test media::decision::hdr`
  - HDR10 HEVC Main10 → `cast-ultra` = `DirectPlay` (assert **no** transcode)
  - HDR10 → browser (SDR, default `Reject`) = `Unplayable{HdrRequiresToneMapping}`
  - Same pair with `DegradedTranscode` → `Transcode` with `degraded: true`, reason retained
  - 10-bit SDR → 8-bit device escalates on bit depth, **not** on an HDR reason
  - HLG → HLG-capable = `DirectPlay`; HLG → HDR10-only = the policy path
  - Unknown HDR class → SDR target = `CannotDecide{UnknownHdrClass}`; → HDR target = unaffected
  - Verify no hardcoded IPs or org names in new/modified files

  ## EDGE CASES
  - HDR source ∧ SDR target ∧ unsupported codec → both reasons recorded; the HDR policy decides, and
    `Reject` wins over any codec-driven transcode
  - A device claiming HDR10 with `max_bit_depth: 8` → rejected at load (MDEC-01)
  - Spec A not yet classifying HDR → every HDR-vs-SDR pair is `CannotDecide`, which is the honest state
    and is asserted as such rather than worked around

- **Acceptance criteria:**
  - [ ] HDR direct-plays to a capable device with no transcode
  - [ ] HDR → SDR-only is `Unplayable{HdrRequiresToneMapping}` by default
  - [ ] The opt-in degraded path sets `degraded: true` and retains the reason
  - [ ] Bit-depth escalation is a distinct reason from HDR mismatch
  - [ ] Unknown HDR class against an SDR target is `CannotDecide`, never assumed SDR
  - [ ] **No tone-mapping logic, ffmpeg filter, or colour-conversion code is added anywhere**
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] README states the boundary and the knob
  - [ ] All existing tests still pass

---

### MDEC-07: Tier-ordering enforcement — never escalate without a justifying reason
- **Priority:** Critical
- **Labels:** maestro, decision, invariants
- **Agent:** claude
- **Estimate:** 5h
- **Description:** The epic's economic claim — most playback needs no transcoding — is only true if the
  engine is *provably* incapable of over-serving. This turns "prefer the cheapest viable tier" from a
  comment into invariants checked against every matrix case rather than a hand-picked few.

  ## FILES
  - `src/media/decision/types.rs` — `PlaybackTier`, ordering, `PlaybackPlan::tier()`
  - `src/media/decision/invariants.rs` — `justifies_escalation()`, shared by tests and `/why`
  - `src/media/decision/tier_ordering_tests.rs`
  - `README.md`

  ## APPROACH
  1. `PlaybackTier: DirectPlay < Remux < TranscodeAudioOnly < TranscodeVideo < Unplayable`, deriving
     `Ord`. `CannotDecide` is deliberately **outside** the order — it is not a more expensive answer, it
     is the absence of one, and forcing it into the lattice would let an undecidable file look like a
     costly plan.
  2. **Invariant 1 — justification.** Each step above `DirectPlay` needs at least one reason of the
     matching `subject()`: `Remux` a `Container` reason; `TranscodeAudioOnly` an `Audio` reason;
     `TranscodeVideo` a `Video`, `Subtitle`, or `Hdr` reason; `Unplayable` a reason matching its
     `Refusal`. An escalation with no recorded cause fails the suite.
  3. **Invariant 2 — monotonicity under relaxation.** For each matrix case, enumerate single-axis
     relaxations of the profile (add the source codec; raise max profile/level/resolution/framerate/
     bit-depth/channels; add the container; enable image subs; enable HDR; flip `transcode_policy` to
     `Allow`) and assert the tier is **never more expensive** than the original. A more capable device
     producing a costlier plan is by definition a bug, and this catches the whole class without anyone
     predicting the case.
  4. Both run over the **full MDEC-08 matrix**, which is why that item exports its case list rather than
     inlining it — a case added later is automatically checked.
  5. A deliberate counter-example: a hand-built plan escalating without a reason must **fail**
     `justifies_escalation()`, proving the check has teeth rather than passing vacuously.

  ## TEST PLAN
  - `cargo test media::decision::tier_ordering`
  - Invariant 1 over every matrix case; Invariant 2 over every case × every single-axis relaxation
  - The counter-example is detected
  - A remuxable-but-direct-playable pair chooses `DirectPlay` — the cheapest viable tier, not a viable one
  - `CannotDecide` cases are excluded from the ordering assertions and asserted separately
  - Verify no hardcoded IPs or org names in new/modified files

  ## EDGE CASES
  - Relaxing a non-binding axis → tier unchanged; still asserted
  - `Unplayable` relaxed into viability → must drop to a real tier
  - Two axes binding simultaneously → relaxing one may legitimately hold the tier; the assertion is
    "never worse", never "always better"
  - A relaxation that turns `CannotDecide` into a decision → allowed in either direction, since the
    unknown fact was the blocker, not the cost

- **Acceptance criteria:**
  - [ ] `PlaybackTier` is totally ordered and `CannotDecide` is deliberately outside it
  - [ ] Invariant 1 passes over the entire matrix
  - [ ] Invariant 2 passes over the entire matrix × all relaxations
  - [ ] A deliberately unjustified plan fails the check
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

### MDEC-08: The exhaustive decision matrix — the highest-value test asset in the epic
- **Priority:** Critical
- **Labels:** maestro, decision, test-matrix
- **Agent:** claude
- **Estimate:** 8h
- **Description:** A table-driven test over the cross-product of representative `MediaProbe` fixtures ×
  every profile, with expected plans. The epic names this its single highest-value test asset, and it is
  a deliverable in its own right. Its value compounds: when spec E's transcoder misbehaves, this is what
  tells you in seconds whether the *decision* was right — the difference between debugging one component
  and debugging four.

  ## FILES
  - `tests/golden/media/probes/*.json` — the corpus, as **raw ffprobe JSON** parsed through the shipped
    `parse_probe_json`, so the fixtures exercise the real parser rather than a hand-built struct that
    can drift from it
  - `tests/golden/media/decision-matrix.expected.tsv` — the committed snapshot
  - `src/media/decision/matrix_tests.rs` — the runner + `pub fn all_cases()` for MDEC-07 and MDEC-11
  - `README.md`

  ## APPROACH
  1. **Corpus (≥24 fixtures)**, each named for what it represents, spanning the real library rather than
     the theoretical space: H.264 High/AAC/MP4 (the direct-play baseline); the same in MKV; the same in
     WebM (**byte-identical `format_name`, distinguished only by stream codecs — the finding-2 pair**);
     a Matroska-family file with an unrecognised codec (the unprovable-eligibility case); H.264/AC-3 5.1; H.264/DTS-HD 5.1; H.264/TrueHD 7.1; HEVC Main 8-bit; HEVC Main10 HDR10 4K;
     HEVC Main10 HLG; VP9 P0; VP9 P2 10-bit; AV1 8-bit; AV1 10-bit HDR10; MPEG-2/MpegTs; VC-1; 4K60
     high-bitrate; 1080p24 low-bitrate; audio-only; video-only; +SRT; +ASS-with-fonts; +PGS; HEVC+PGS;
     unknown-codec; multi-audio (AAC + E-AC-3); **and the undecidables** — no duration, no dimensions,
     unindexed streams, missing profile/level.
  2. **Cross-product:** ≥24 fixtures × **11 profiles** (the nine seeds plus two in-test synthetic ones:
     a `DirectAndRemuxOnly` profile and a client-declared profile contradicting its seed) = **≥264
     cases**, generated by iteration. Purity is what makes this free to run.
  3. **Multi-source cases** are a second, smaller table (≥6 cases) pairing two or three probes against
     each profile and asserting `source_index` — a cross-product of *sets* would explode, and the
     interesting behaviour is covered by chosen pairs.
  4. **Snapshot form:** one TSV line per case —
     `probe_id · profile_id · tier · source_index · video · audio · subs · degraded · reasons` —
     deterministically sorted and committed, so a behaviour change is a **reviewable diff across every
     affected case**, exactly the artefact a decision-engine change needs. `UPDATE_DECISION_MATRIX=1`
     regenerates; the README says plainly that regenerating without reading the diff defeats the file's
     entire purpose.
  5. **Coverage assertions enforced by the test itself** — what makes it exhaustive rather than merely
     large:
     - every profile appears
     - every `PlaybackOutcome` variant is produced
     - **every `Refusal` variant**, including `HdrRequiresToneMapping` — how the §8.3 boundary stays
       tested rather than decaying into a comment
     - **every `Undecidable` variant reachable from playback**
     - every `VideoPlan`/`AudioPlan`/`SubtitlePlan` variant
     - **every `PlaybackReason` variant is emitted at least once** — an unreachable reason is either
       dead code or a missing fixture, and both are worth failing over
  6. ~20 cases carry an explicit commented `expected_tier` in the test source, so intent survives a
     careless regeneration. Snapshot for breadth, explicit assertions for the cases that matter.
  7. Runtime budget: the whole matrix runs well under a second; a test asserts under 5s so it can never
     quietly become a thing people skip.

  ## TEST PLAN
  - `cargo test media::decision::matrix` — all ≥264 cases against the snapshot
  - Coverage assertions pass, including full reason, refusal, and undecidable coverage
  - Deleting a fixture fails a coverage assertion (proving it is not vacuous)
  - `UPDATE_DECISION_MATRIX=1` regenerates cleanly and is a no-op on unchanged logic
  - Fixtures parse through the shipped `parse_probe_json` — a parser change that breaks them is caught
    here rather than in production
  - Verify no hardcoded IPs or org names in fixtures or test code
  - `cargo test` green (single crate — `muse`, both bins)

  ## EDGE CASES
  - A new seed added later → the matrix grows automatically and the diff shows every plan for the new
    device at once, which is the intended review experience
  - A fixture `Unplayable` on every profile → legitimate (unknown-codec); coverage requires it
  - Line ordering must not depend on `HashMap` iteration — sort explicitly

- **Acceptance criteria:**
  - [ ] ≥24 fixtures × 11 profiles = ≥264 asserted cases, plus ≥6 multi-source cases
  - [ ] Fixtures are raw ffprobe JSON parsed by the **shipped** `parse_probe_json`
  - [ ] A committed, deterministically ordered snapshot that diffs readably
  - [ ] Coverage enforces every outcome, refusal, undecidable, plan variant, and reason
  - [ ] ~20 explicit commented expected tiers independent of the snapshot
  - [ ] MDEC-07 and MDEC-11 consume this exact case list
  - [ ] Full matrix runs in under 5 seconds
  - [ ] No hardcoded infrastructure values in fixtures or test code
  - [ ] README explains how to read and regenerate it

---

### MDEC-09: The `/why` debug endpoint
- **Priority:** High
- **Labels:** maestro, decision, api, debug
- **Agent:** claude
- **Estimate:** 4h
- **Description:** One endpoint answering the only question that matters when playback misbehaves: *what
  did the engine see, and what did it decide?* The epic says it will be used constantly, so it is built
  to be pasteable — its response is a complete, self-contained reproduction case for a pure function.

  ## FILES
  - `src/maestro/http/why.rs` — the handler
  - `src/maestro/http/mod.rs` — route registration on Maestro's router (Muse's `src/http/` is untouched;
    the shared bearer-auth layer in `src/http/auth.rs` is reused, not copied)
  - `src/media/decision/mod.rs` — a `WhyReport` type shared by handler and tests
  - `README.md` — "Debugging a playback decision", with a worked example

  ## APPROACH
  1. Two shapes, because the two questions differ:
     - `GET /api/playback/why?item_id={id}&device={profile_id}` — resolves every `MediaProbe` for the
       item from the persisted probe store (spec A) and runs `plan()`. The everyday path.
     - `POST /api/playback/why` with `{ probes, device_profile | device, request }` — runs on supplied
       data with **no library lookup**, which is what makes a bug report actionable offline: paste
       yesterday's output back in and get the same answer on a dev box with no media present.
  2. Response: `{ probes, device_profile, resolved_profile_id, profile_was_substituted, profile_source,
     declared_at, request, plan: { source_index, outcome, reasons }, tier, refusal, undecidable,
     degraded, engine_version }`. `profile_source` (`seed | operator_override | client_declared`) is the
     single most useful field for the "works on my TV" class of bug (MDEC-10) — it distinguishes "we
     assumed this device could do HEVC" from "the client told us it could, forty minutes ago".
  3. All I/O lives in the **handler**: it resolves inputs, then calls the pure function exactly once. The
     purity guard enforces the boundary mechanically.
  4. Auth and reachability per epic §9 clause 1 and §10b: behind Maestro's bearer auth, reached from the
     GUI only through `proxy_maestro`, with **both** tokens <secret-manager>-materialised and read via
     `SecretManager::get()` (S7) — never `std::env::var`. No token, host, or port literal in this spec or
     in the source.
  5. **Read-only and side-effect-free**: no session created, no transcode started, nothing written.
     `/why` must be safe to hammer while debugging.
  6. Reasons are returned as structured objects **and** rendered strings in the same response — the
     structure for tooling, the strings so a human reading curl output need not consult the code table.

  ## TEST PLAN
  - `cargo test maestro::http::why`
  - `GET` with a known item returns 200 with every documented field populated
  - `POST` with inline probes returns the identical plan to a direct `plan()` call — a round-trip test
    binding the endpoint to the pure function
  - Unknown device → 200 with `profile_was_substituted: true`, `resolved_profile_id: "safe-baseline"`
  - An `Unplayable` **and** a `CannotDecide` outcome each return 200 with the relevant field populated —
    never a 4xx, since the question was answered successfully
  - Unknown item → 404 structured error, not a 500; unauthenticated → 401
  - Two identical `GET`s produce byte-identical responses
  - Verify no hardcoded IPs, hostnames, ports, or org names; token via `SecretManager`

  ## EDGE CASES
  - Item exists but was never probed → 200 with a `not_probed` marker and no plan, never a fabricated
    `MediaProbe`; "I have not looked at this file yet" is the honest answer
  - `POST` body with an unknown field → 400 naming it (`deny_unknown_fields`), so a stale pasted report
    fails loudly instead of half-parsing
  - Many streams → no truncation of reasons, ever; truncation would hide the cause

- **Acceptance criteria:**
  - [ ] `GET` returns probes, profile, plan, and every reason
  - [ ] `POST` runs entirely on supplied data and matches a direct `plan()` call
  - [ ] `profile_source` and substitution are reported explicitly rather than hidden
  - [ ] `Unplayable` and `CannotDecide` are successful responses carrying their cause
  - [ ] No writes, no session creation, deterministic output
  - [ ] All I/O in the handler; `src/media/decision/` stays pure (guard passes)
  - [ ] Secrets via `SecretManager`; no hardcoded infrastructure values
  - [ ] README documents `/why` with a worked example
  - [ ] All existing tests still pass

---

### MDEC-10: Client-declared capabilities — the profile store becomes seed + override, not gospel
- **Priority:** High
- **Labels:** maestro, decision, profiles, api
- **Agent:** claude
- **Estimate:** 5h
- **Description:** A hand-maintained capability table is a *prediction*, and predictions rot: browsers
  ship codecs, TVs get firmware, one household laptop lacks the HEVC hardware its neighbour has. Because
  we own every client (epic §6), we need not predict — **we can ask.** The web player calls
  `MediaSource.isTypeSupported()` for a known probe list at session start and POSTs the results; Maestro
  folds them over the seed. This kills the "works on my TV, not on my laptop" class of bug at the source,
  and it is small precisely because the profile type and the pure function already exist.

  ## FILES
  - `src/media/decision/declared.rs` — `ClientCapabilities`, the RFC 6381 parser, and the pure
    `apply_declared(&DeviceProfile, &ClientCapabilities) -> DeviceProfile`
  - `src/maestro/http/capabilities.rs` — `POST /api/playback/capabilities` + the session store
  - `src/maestro/http/mod.rs`, `src/config.rs` (`MAESTRO_CLIENT_CAPABILITY_TTL_SECS`), `.env.example`,
    `README.md`

  ## APPROACH
  1. Wire format: `{ client_id, base_profile_id, declared: [{ mime, supported }], audio_channels, hdr }`,
     where `mime` is exactly the string passed to `isTypeSupported()` — no client-side interpretation,
     because a browser reporting its own answer verbatim is evidence, whereas a browser's opinion about
     what that means is another prediction.
  2. **Server-side RFC 6381 codec-string parsing** (`avc1.640028`, `hvc1.2.4.L153.B0`, `av01.0.05M.10`,
     `mp4a.40.2`, `ec-3`, `vp09.<profile>.<level>.<bit-depth>`) into typed codec + profile + level + bit
     depth, reusing `media::codec` (MDEC-01). Pure, table-driven, its own unit tests — the one genuinely
     fiddly piece here, and exactly the kind of thing that must not be written twice.
  3. **Trust boundary, stated as a rule.** A declaration is authoritative for **codec/profile/level
     support** — `isTypeSupported()` is the decoder itself, and it beats our table in both directions (it
     may add HEVC we assumed absent or remove one we assumed present). It is **not** authoritative for
     what the API cannot see: bitrate ceilings, panel resolution, real HDR display capability, or speaker
     layout beyond a reported channel count. Those keep their seed values. Precedence: **operator
     override > client declaration (within its competence) > seed**, documented in the README so nobody
     later "simplifies" it into blanket trust.
  4. The merge is a **pure function** in `src/media/decision/`; only the endpoint and session store are
     Maestro-side. Changed entries tag the profile `source: ClientDeclared` and make the decisive reasons
     carry `CapabilityClientDeclared`. Purity end to end: the handler builds a profile value, then calls
     `plan()`.
  5. **Ephemeral by construction.** Declarations live in the session store keyed by `client_id` with a
     TTL (default 24h), are **never** written back into `data/device_profiles/`, and never mutate a seed
     — a device's committed profile stays reviewable config-as-code, and a bad declaration expires on its
     own rather than poisoning the repo.
  6. Same auth path as `/why`, token via `SecretManager` (S7). Bounded: capped list length, capped MIME
     length, and unparseable entries **ignored with a recorded reason** rather than rejecting the whole
     payload — a client reporting one codec string we cannot parse should still get the benefit of the
     other forty.

  ## TEST PLAN
  - `cargo test media::decision::declared` — RFC 6381 table tests per codec family, including malformed
    and truncated strings
  - A declaration adding HEVC Main10 to a seed lacking it flips `Transcode` → `DirectPlay`, and the
    reasons carry `CapabilityClientDeclared`
  - A declaration *removing* H.264 the seed claimed escalates accordingly
  - A declaration claiming a 4K panel or higher bitrate ceiling is **ignored** — outside its competence;
    seed values survive (named test, since this is the rule most likely to erode)
  - `cargo test maestro::http::capabilities` — auth required; oversized payload rejected; one unparseable
    entry does not discard the rest; TTL expiry restores the seed
  - No file under `data/device_profiles/` is mutated (asserted on disk)
  - Verify no hardcoded IPs, hostnames, or ports; token via `SecretManager`

  ## EDGE CASES
  - A client declaring support for **nothing** → rejected as implausible, seed stands; a client must not
    be able to talk us into `Unplayable`
  - Unknown `base_profile_id` → applies over `safe-baseline`, and `/why` reports both
  - Two clients sharing a `client_id` → last declaration wins within the TTL; documented, and the reason
    an operator override outranks it
  - Declarations arriving mid-session → applied to subsequent decisions only; an in-flight session is not
    re-planned underneath the viewer (spec D owns re-planning)

- **Acceptance criteria:**
  - [ ] `POST /api/playback/capabilities` folds `isTypeSupported()` results over the seed for the
        session's lifetime
  - [ ] RFC 6381 strings parse into typed codec/profile/level/bit-depth, with table tests
  - [ ] Declarations are authoritative for codec support and ignored for bitrate/resolution/HDR panel
        claims, per the documented trust boundary
  - [ ] The merge is pure and in the shared core; the endpoint performs the only I/O
  - [ ] Declarations are TTL-bounded and never mutate committed profile files
  - [ ] `/why` reports `profile_source: client_declared`
  - [ ] Secrets via `SecretManager`; no hardcoded infrastructure values
  - [ ] README documents the probe list and the trust boundary
  - [ ] All existing tests still pass

---

### MDEC-11: Property-based tests — the holes a fixture matrix cannot see
- **Priority:** High
- **Labels:** maestro, decision, testing
- **Agent:** claude
- **Estimate:** 5h
- **Description:** The golden matrix proves the cases we thought of. Properties prove the cases we did
  not — and on a pure, total function they are nearly free, because there is no setup, no teardown, and
  no flakiness. This is the cheapest remaining test leverage in the spec, and the natural complement to
  MDEC-07's relaxation invariants: those vary the *profile* around fixed media, these vary the media too.

  ## FILES
  - `src/media/decision/property_tests.rs`
  - `Cargo.toml` — `proptest` under `[dev-dependencies]` only
  - `README.md` — the property list, in English, as documentation of intent

  ## APPROACH
  1. `proptest` as a **dev-dependency only** — it never enters either shipped binary, so it adds no
     runtime surface. (Note for the publish gate: `cargo audit` scans the tree, so a future advisory
     against it is a dev-dep triage, not a deploy blocker.)
  2. Generators produce *structurally valid* inputs: an arbitrary `MediaProbe` (codecs drawn from the
     known set plus an unknown-codec case, plausible dimensions and channel counts, `Option` fields
     genuinely `None` a fair fraction of the time) and an arbitrary `DeviceProfile`. Generating `None`s
     is the point — the undecidable paths are exactly where a fixture corpus is thinnest.
  3. **The properties**, each a named test with a comment stating the bug it would catch:
     - **No gratuitous video encode.** If the source's codec, profile, level, resolution, framerate, bit
       depth, and HDR class are all within the device's caps and no burn-in is required, the outcome's
       `VideoPlan` is never `Encode`. This is the epic's economic claim, stated as a law.
     - **Burn-in implies full video transcode.** `SubtitlePlan::Burn` ⇒ `VideoPlan::Encode`. There is no
       such thing as a burned-in stream copy, and a plan claiming one would execute as garbage.
     - **Direct play implies nothing to do.** `DirectPlay` ⇒ no encode, no downmix, no burn, and a
       container the device accepts.
     - **Totality.** `plan()` never panics for any generated input — including empty streams, zero
       dimensions, and absurd channel counts.
     - **Determinism.** The same inputs always produce the identical plan, reason list included.
     - **Unknown never becomes fine.** If any fact the outcome depended on was `None`, the outcome is not
       `DirectPlay`. The `plan.rs` discipline as an executable law.
     - **No upscale.** Any `Encode` has `width ≤ source width` and `height ≤ source height`.
     - **Reason non-emptiness.** Every non-`CannotDecide` outcome carries ≥1 reason.
     - **Passthrough preference.** If the device can passthrough the source audio codec, the plan never
       chooses `Encode` for it.
  4. Bounded case counts (256 default) and a **fixed seed committed in `proptest.toml`** so a CI failure
     is reproducible; failures are shrunk and the minimal case is added to the golden corpus as a
     permanent regression fixture. That loop is what makes property tests compound: each found bug
     becomes a named case in MDEC-08 rather than living only in a seed file.
  5. Properties also run over MDEC-08's `all_cases()` as concrete inputs, so the same laws guard both the
     generated and the curated corpora.

  ## TEST PLAN
  - `cargo test media::decision::property` — all properties pass at the default case count
  - Each property is proven to have teeth: temporarily inverting the corresponding branch in `plan()`
    makes exactly that property fail (documented in the PR, not committed)
  - A shrunk counter-example is reproducible from the committed seed
  - Runtime stays under 30s so it remains a pre-merge gate rather than a nightly
  - Verify no hardcoded IPs or org names in new/modified files
  - `cargo test` green (single crate — `muse`, both bins)

  ## EDGE CASES
  - A generator producing a self-contradictory profile (HDR10 with 8-bit max) → filtered at generation,
    since MDEC-01 rejects it at load; the suite must not assert on states the loader forbids
  - A property failing only at a high case count → treat as a real bug, never by lowering the count
  - Generated absurdities (99 audio channels, 16K video) → must produce a decision or a refusal, never a
    panic; that is the totality property doing its job

- **Acceptance criteria:**
  - [ ] `proptest` added as a dev-dependency only, with a committed fixed seed
  - [ ] All nine named properties implemented, each with a comment naming the bug it catches
  - [ ] "Never encodes video when everything is within caps" and "burn-in implies full video transcode"
        are both present and passing
  - [ ] Every property is demonstrated to fail when its branch is inverted
  - [ ] Shrunk counter-examples are promoted into the MDEC-08 golden corpus
  - [ ] Suite runs in under 30 seconds
  - [ ] No hardcoded infrastructure values in new/modified code
  - [ ] All existing tests still pass

---

## Deliberately out of scope

| Not here | Where |
|---|---|
| HDR tone-mapping for SDR targets | Nowhere in S130 (epic §8.3); spec F follow-up at most |
| Curation policy, verify-and-swap, recycle bin, mutation kill-switch | **Foundry**, shipped (epic §2b) |
| ffmpeg argv construction | Spec D/E — this spec deliberately emits no argv |
| Executing a plan (remux, range requests, sessions) | Spec D |
| HLS segmenting, seek, throttling, session lifecycle | Spec E |
| Hardware encode selection, GPU arbitration, host tool probing (`capability.rs`) | Spec F |
| ABR ladders / multi-rendition selection | Spec E |
| Player UI, device pickers, track menus, Cast sender | Spec G / K |
| ffprobe invocation, `MediaProbe` field extension, persistence, backfill | Spec A |
| Library, metadata, watch state | **Muse only**, permanently (epic §2) |

## Risks

1. **A reviewer reads this as a duplicate of `plan_transcode`.** Mitigated by §1 being the first thing in
   the document and by acceptance criteria that force reuse of the shipped helpers — the strongest
   evidence that this is not a second implementation is that it *calls* the first one's parts.
2. **`MediaProbe` does not yet carry profile/level/framerate/bit-depth/HDR** (§1b finding 1), so a large
   fraction of early cases are `CannotDecide`. Honest, not broken, and the matrix asserts it — but it
   means C's *useful* output is gated on spec A's field extension, which is why that negotiation is a
   pre-flight item rather than a discovery made mid-sprint.
3. **The `Container::WebM` split touches shipped curation code.** Mitigated structurally rather than by
   care: `normalize_container` is not modified (its `"matroska,webm"` test at `policy.rs:365` is the
   constraint), Foundry's defaults are not modified, and the new variant is therefore unreachable from
   every curation path. If any Foundry test needs editing, the change was not behaviour-preserving and
   must be reworked. The residual risk is the *discriminator* being wrong, not the enum: a file wrongly
   judged WebM-eligible black-screens a Cast, so the inference is a whitelist (every stream inside the
   WebM subset) rather than a blacklist, and an unprovable case is `CannotDecide`.
4. **Profile inaccuracy beats logic correctness.** A perfect engine on a wrong capability table is
   confidently wrong. Hence per-generation Cast modelling, conservative readings of ambiguity, provenance
   comments, and MDEC-10 asking the client rather than guessing.
5. **The matrix becoming a rubber stamp.** A reflexively regenerated snapshot is worse than none.
   Mitigated by ~20 explicitly asserted cases that survive regeneration, by the README saying so, and by
   MDEC-11's properties, which no regeneration can silence.
6. **Purity erosion.** The likeliest future regression is a clock or config lookup inside `plan()`. The
   guard is mechanical, and its failure message explains the constraint rather than merely reporting it.
7. **Both Maestro tokens unprovisioned** (epic §10b — there are two, not one) makes `/why` and
   `/capabilities` 401 and look broken, repeating TERM #549. Pre-flight for spec B; until then both are
   fully exercisable through their `POST` forms and the test suite.
