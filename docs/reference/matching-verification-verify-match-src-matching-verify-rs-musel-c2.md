## Matching-verification: `verify_match` (`src/matching/verify.rs`, MUSEL-C2)

MUSEL-C2 is THE critical piece: given an identified file's observed properties, its
`ProviderMetadata` (MUSEL-A1), and a set of sample stills (MUSEL-C1, above), `verify_match`
proves — or disproves — that the file really is the title Muse identified it as, rather than
trusting a provider-ID match blindly. It combines three independently optional/graceful
signals into a single `MatchVerdict`, strongest signal first:

1. **Local VLM via Chord (strongest).** `matching::vision::ChordVisionVerifier` asks a
   vision-capable model, over the existing `taste_model::chord_client::ChordClient` seam
   (`ChordClient::chat_completion_with_image` — the ONE Chord HTTP transport in this crate;
   `vision.rs` only builds the prompt and parses the reply, never a second direct client
   against a model URL), whether a sample frame is consistent with the claimed title
   (era/genre/setting — not a black frame, test pattern, or slate). `from_config` returns
   `None` when Chord isn't configured (`CHORD_URL` unset) — same graceful-degrade posture as
   every other optional integration in this crate — and `verify_match` simply skips the
   signal, never fails the pipeline. The model name defaults to `qwen2.5-vl:7b`
   (`vision::DEFAULT_VISION_MODEL`), overridable via `MUSE_VISION_MODEL`.
2. **Still-liveness.** `matching::liveness::check_liveness` decodes each sample still to real
   luma (grayscale) pixels via the `image` crate (`jpeg` feature only) and runs mean/variance +
   a dominant-pixel-ratio over the actual decoded pixels (rejecting near-uniform/black/slate
   stills), plus a cross-still comparison (rejecting a set of stills that all look identical —
   a stuck/frozen source). A still whose bytes don't decode as a JPEG at all (corrupt/truncated
   capture) is treated as maximally uniform — "decodes to garbage" fails liveness, per the spec's
   edge cases, never a panic. **This module's first version deliberately avoided the decode
   dependency**, instead running the same mean/variance/dominant-ratio statistics directly over
   the still's *compressed* JPEG bytes (the theory: a solid-color frame's DCT blocks are almost
   all "zero AC coefficients", so the entropy coder should emit a visibly more repetitive
   compressed byte stream). **That was measured against a real encoded JPEG and proven wrong**
   (review finding: codex) — JPEG's entropy coding makes the *compressed* byte stream look
   statistically noisy regardless of image content (that's the point of entropy coding), and it
   swamped the signal: a real solid-black 64x64 JPEG's compressed bytes measured variance
   ≈6560, a real varied/textured 64x64 JPEG measured ≈6070 — statistically indistinguishable.
   `liveness.rs`'s tests now decode REAL JPEG fixtures (`liveness::fixtures`, built with
   `image`'s encoder) rather than raw synthetic byte arrays, and
   `analyze_still_genuinely_discriminates_real_black_from_real_varied_jpeg` is the direct proof
   the pixel-decode approach actually works where the byte-stream proxy didn't. See the module
   doc comment for the full account.
3. **Metadata consistency.** Compares the file's *observed* runtime (e.g. from an ffprobe
   duration, via `verify::FileObservation`) against the provider's *stated* runtime
   (`ProviderMetadata::runtime_minutes`, added by this item — TheTVDB v4 client doesn't parse
   this yet, so it's `None` from that provider today; the field exists for `verify_match` and
   any future provider/richer TVDB parse to populate). A disagreement beyond ~50% of the
   provider's runtime (and at least 5 minutes absolute) is a hard contradiction; missing data
   on either side is reported but never itself treated as a mismatch.

`verify_match(file, metadata, stills, vision) -> MatchVerdict` combines the three: a hard
liveness failure (all sampled stills near-uniform or all identical) drives `Inconsistent`
regardless of what the VLM says — dead/stuck content can't confirm any title. Otherwise, when
the VLM is present, its answer is the primary discriminator (a "no" is trusted directly; a
"yes" is only `Consistent` when metadata doesn't hard-contradict it). When the VLM is absent,
the verdict comes from liveness + metadata alone — a weaker `Consistent` when nothing
contradicts (never the same confidence a vision-backed match gets), `Inconclusive` when there
is genuinely nothing to judge, and still a clear `Inconsistent` on a hard metadata
contradiction — a mislabeled file doesn't get a free pass just because no vision model happens
to be configured. **This last point holds even when no stills are extractable at all**
(`LivenessOutcome::Empty`, e.g. ffmpeg unavailable): a fixed review finding (codex) is that
`combine` originally checked "no stills" *before* the metadata-contradiction check, so a file
with a grossly wrong runtime but no extractable stills came back `Inconclusive` — independent
hard evidence was silently suppressed. `combine` now checks metadata contradiction first for
the empty-stills case too: empty stills + a hard metadata contradiction → `Inconsistent`;
empty stills with nothing else wrong → `Inconclusive`, unchanged.

**Verdict-only, always.** `verify_match` takes every input by shared reference and returns a
plain `MatchVerdict { outcome, confidence, reasons }` value — it has no write path to the
file, the library, or the metadata. An `Inconsistent` verdict FLAGS the match for operator
review in a scan report; it never auto-deletes or re-tags anything. That decision belongs to
whatever calls `verify_match`, not to `verify_match` itself.

**The mismatch-detection harness** (`src/matching/verify.rs`'s test module) is the acceptance
spine the operator asked for: it proves the check genuinely *discriminates*, not just that it
runs.
- `positive_correct_match_with_vision_yes_is_consistent` — correct metadata + live/varied
  stills + a mock vision "yes" → `Consistent`, high confidence.
- `mismatch_vision_says_no_and_wrong_runtime_is_inconsistent` — a mock vision "no" *and* a
  grossly wrong observed runtime → `Inconsistent`.
- `all_black_stills_are_inconsistent_regardless_of_a_vision_yes` — all-black/uniform stills →
  never `Consistent`, even when the (implausible) mocked vision signal says "yes".
- `gross_runtime_disagreement_flags_even_without_vision` — a runtime that grossly disagrees
  with the provider's is flagged `Inconsistent` even with no VLM configured.
- `gross_runtime_disagreement_flags_even_with_no_stills_at_all` — the same gross runtime
  mismatch flags `Inconsistent` even when there are NO stills at all (regression test for the
  fixed "empty stills suppress hard metadata evidence" review finding, above).
- `vlm_absent_never_fabricates_a_vision_backed_consistent` — with `vision: None`, the verdict
  never claims vision-backed reasoning and never reaches the same high-confidence `Consistent`
  a real vision "yes" would produce.
- `vlm_absent_with_no_stills_is_inconclusive_not_a_crash` — no stills AND no metadata
  contradiction → `Inconclusive`, not a panic or a false `Consistent`.
- `verify_match_leaves_every_input_unchanged` — the verdict-only contract, checked
  behaviorally on top of the `&`-reference-only signature.

All of the above (except the vision-mocked cases, which only need a still to hand to the mock)
build their stills from REAL encoded JPEG fixtures (`liveness::fixtures::real_varied_jpeg`/
`real_black_jpeg`), so the mismatch harness exercises the real pixel-decode liveness path, not
a synthetic byte stand-in.

`matching::vision::VisionVerifier` is a trait (not the concrete `ChordVisionVerifier`)
precisely so these tests supply a deterministic mock instead of a live Chord endpoint —
mirrors `metadata::MockMetadataProvider`'s relationship to `MetadataProvider` elsewhere in
this crate.

