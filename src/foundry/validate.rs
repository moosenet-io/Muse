//! FOUNDRY-04: the transcode VALIDATION harness — really encode, rigorously
//! verify, never touch the original.
//!
//! FOUNDRY-02's [`crate::foundry::survey`] plans and stops. [`crate::foundry::forge`]
//! encodes and *replaces*. This module is the step between them, and it exists
//! for one reason: Path A of this feature deletes the operator's originals, and
//! nobody has yet seen this code produce a single real output file.
//!
//! > *"i want to have confidence in this before we make it permanent. we will
//! > need to test optomize a dozen types of files so we can validate the output
//! > works consistently."*
//!
//! So this harness encodes for real, to scratch, and then applies **forge's own
//! verification** — [`expectation_for`] and [`verify_output`], the same
//! functions the destructive swap gates on, not a second implementation that
//! could be more lenient. The output is deleted; the original is opened
//! read-only and is never renamed, replaced, or removed.
//!
//! ## What makes this evidence rather than a smoke test
//!
//! The library is **16,221 files**, and a measured sample of 400 is 193 avi,
//! 151 mkv, 54 mp4, 2 m4v. Twelve randomly-drawn files would be mostly
//! h264/aac mkv, and validating twelve h264/aac files proves almost nothing:
//! that path is a stream copy. The value is entirely in the awkward tail the
//! operator measured — `msmpeg4v2` at 294x240, `mpeg4` + ac3, h264 1918x802
//! with **42 subtitle streams**, h264 1280x688 with **dts + ac3**. So the
//! sample is chosen by *coverage of distinct shapes*
//! ([`select_diverse_sample`]), not by position in a directory walk.
//!
//! ## Why this can never damage the library, structurally
//!
//! 1. It does not call, and cannot reach, [`crate::foundry::forge::optimize_file`].
//!    Nothing in this module renames, copies into the library, or deletes.
//! 2. Every ffmpeg invocation is checked by [`argv_writes_only_to`] *before*
//!    the spawn: the argv's output operand must be the exact scratch path this
//!    module generated, inside the scratch directory, and the source path must
//!    appear exactly once and only as the operand of `-i`. A plan whose argv
//!    fails that check is a **failure**, not an encode.
//! 3. The scratch directory is required to satisfy safety rail 3 — a different
//!    filesystem from every allowed root, and not inside one — checked here
//!    through [`crate::foundry::config::FoundryConfig::scratch_rail3_problems`]
//!    regardless of the mutation gate. (`fatal_errors` only makes rail 3 fatal
//!    when mutation is *enabled*, and it is not; this harness must not inherit
//!    that leniency, because it is the thing that actually writes.)
//! 4. It never reads or requires `MUSE_FOUNDRY_ENABLE_MUTATION`. Turning that
//!    on changes nothing here, and neither does leaving it off.
//!
//! ## Fail closed, everywhere
//!
//! A verification that could not be performed is a **failure**, never a pass:
//! an unreadable output, an unprobeable output, an encode that timed out, a
//! source whose duration is unknown so no output of it could be proven
//! un-truncated. There is no code path in this module on which "we could not
//! check" produces [`ValidationOutcome::Verified`].
//!
//! ## The aggregate does not launder a failure
//!
//! [`ValidationRun::verdict`] checks for failures **first**, and there is
//! deliberately no pass-rate or percentage anywhere in this module. One broken
//! output in twelve is the finding; "92%" is how that finding gets skimmed past.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::foundry::config::FoundryConfig;
use crate::foundry::forge::{expectation_for, verify_output, VerifyExpectation, VerifyFailure};
use crate::foundry::plan::{plan_transcode, TranscodeDecision, TranscodePlan, TranscodeReason};
use crate::foundry::policy::{normalize_container, Container, TranscodePolicy};
use crate::foundry::probe::{run_ffprobe, MediaProbe};
use crate::foundry::Foundry;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Everything that stops a validation run from becoming an incident.
///
/// The numbers are sized for the deployment this runs on: **<host> root has
/// ~13 GB free**, and rail 3 requires the scratch directory to be on a
/// different filesystem from `/srv/media` — so scratch is on that 13 GB root,
/// not on the 27 TB library mount. A single 4K remux can exceed the entire
/// budget, which is why there is a per-file ceiling as well as a total.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationBounds {
    /// Largest input this harness will encode. Anything above it is
    /// [`ValidateSkip::InputTooLarge`] — a stated skip, not a silent omission,
    /// and not an attempt that fills the disk.
    ///
    /// Default **2 GiB**. This deliberately excludes the ~1% of the library
    /// that is 4K/HDR/DV, and the report says so: those are the files the
    /// scratch filesystem physically cannot hold alongside a safety margin.
    pub max_input_bytes: u64,

    /// Cumulative reserved output across the whole run. Default **6 GiB** of a
    /// ~13 GB filesystem, leaving room for the OS, logs, and whatever else
    /// shares that root.
    ///
    /// Note this bounds *cumulative* work. **Peak** disk usage is one file's
    /// output, because each output is deleted as soon as it has been verified
    /// (and by [`ScratchOutput`]'s `Drop` even if the run panics or returns
    /// early).
    pub max_total_output_bytes: u64,

    /// How much output to reserve per input, as a multiple of the input size.
    /// Default **2**.
    ///
    /// Not 1. A remux is roughly 1:1, but a CRF-20 x264 re-encode of a *small,
    /// heavily-compressed* source — a 300 kbps `msmpeg4v2` 320x240, exactly the
    /// kind of file this harness exists to test — can come out **larger** than
    /// its input. Reserving 1:1 would let that overrun the budget it was
    /// admitted under.
    pub output_reserve_factor: u64,

    /// Wall-clock ceiling on one encode. Default **20 minutes**.
    ///
    /// A timeout is reported as a FAILURE (see [`ValidationFailure::Timeout`]),
    /// per the fail-closed rule — but its message says plainly that the usual
    /// cause is a long or slow input rather than a broken encoder, so the
    /// operator reads the right thing into it.
    pub per_encode_timeout: Duration,

    /// Wall-clock ceiling on the whole run. Default **60 minutes**. Files not
    /// reached are [`ValidateSkip::RunDeadlineReached`], never quietly dropped.
    ///
    /// This exists because 12 files x 20 minutes is four hours, and this is
    /// reachable from an HTTP endpoint.
    pub run_deadline: Duration,
}

impl ValidationBounds {
    /// Build bounds from optional operator overrides, each clamped.
    ///
    /// Exists so the >2 GiB tail — the 4K/HDR/DV content, which the default
    /// ceiling excludes entirely — can actually be validated once a scratch
    /// filesystem large enough to hold it is available. Every bound is
    /// clamped rather than trusted: an unbounded per-encode timeout on a
    /// 24-file run is an unbounded run, and a budget larger than the disk is
    /// refused separately by `check_free_space`.
    ///
    /// `None` for any field keeps that field's default, so a caller raising
    /// only the size ceiling does not silently also change the timeouts.
    pub fn from_overrides(
        max_input_mb: Option<u64>,
        budget_mb: Option<u64>,
        encode_timeout_secs: Option<u64>,
        run_deadline_secs: Option<u64>,
    ) -> Self {
        const MIB: u64 = 1024 * 1024;
        let d = Self::default();
        Self {
            max_input_bytes: max_input_mb
                .map(|m| m.clamp(1, 65_536) * MIB)
                .unwrap_or(d.max_input_bytes),
            max_total_output_bytes: budget_mb
                .map(|m| m.clamp(1, 4_194_304) * MIB)
                .unwrap_or(d.max_total_output_bytes),
            per_encode_timeout: encode_timeout_secs
                .map(|s| Duration::from_secs(s.clamp(60, 21_600)))
                .unwrap_or(d.per_encode_timeout),
            run_deadline: run_deadline_secs
                .map(|s| Duration::from_secs(s.clamp(60, 86_400)))
                .unwrap_or(d.run_deadline),
            ..d
        }
    }

    /// Free space this run actually needs, in bytes.
    ///
    /// The MAXIMUM of the cumulative budget and one file's peak reserve, not
    /// the budget alone. Cumulative bounds total work; peak is what has to fit
    /// on the disk at one instant, and with a large `max_input_bytes` the peak
    /// is the larger of the two.
    pub fn required_free_bytes(&self) -> u64 {
        let peak = self
            .max_input_bytes
            .saturating_mul(self.output_reserve_factor);
        self.max_total_output_bytes.max(peak)
    }

    /// The coverage sentence for THIS run, generated from the bound actually
    /// in force.
    ///
    /// The note used to be a hardcoded string naming 2 GiB. Once the ceiling
    /// became configurable that string would have kept claiming 2 GiB whatever
    /// the run used — a report that lies about its own coverage is worse than
    /// one that omits it, because it reads as verified.
    pub fn coverage_note(&self) -> String {
        // EXACT bytes, with the GiB figure as a readability aid only.
        // Formatting to one decimal GiB was lossy in both directions — a
        // 1 MiB ceiling printed "0.0 GiB" and 2050 MiB printed "2.0 GiB" —
        // so the note could misstate the very bound it exists to disclose,
        // and the tests missed it by only ever using round numbers. Codex,
        // FOUNDRY-09 gate.
        let gib = self.max_input_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        format!(
            "files larger than {} bytes (~{gib:.2} GiB) were SKIPPED, never validated — \
             this run does NOT cover them. 4K/HDR/Dolby Vision content is the large tail, \
             so a ceiling below it means that content is unvalidated.",
            self.max_input_bytes
        )
    }
}

impl Default for ValidationBounds {
    fn default() -> Self {
        Self {
            max_input_bytes: 2 * 1024 * 1024 * 1024,
            max_total_output_bytes: 6 * 1024 * 1024 * 1024,
            output_reserve_factor: 2,
            per_encode_timeout: Duration::from_secs(20 * 60),
            run_deadline: Duration::from_secs(60 * 60),
        }
    }
}

// ---------------------------------------------------------------------------
// Diversity: the sampling key
// ---------------------------------------------------------------------------

/// Resolution buckets, banded on **width**.
///
/// Width rather than height because scope releases are letterboxed: 1918x802
/// is a 1080p-class file with an 802-pixel height, and banding on height would
/// file it next to 720p. The boundaries are drawn around the operator's
/// measured sample (320x240 / 294x240; 640x352, 720x480, 640x480; 1280x720,
/// 1280x688; 1920x1080, 1918x802).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionBand {
    /// <= 400px wide. The `msmpeg4v2` 320x240 / 294x240 era.
    Tiny,
    /// <= 800px. DivX/Xvid and NTSC DVD rips.
    Sd,
    /// <= 1300px. 720p.
    Hd720,
    /// <= 2000px. 1080p, including 1918-wide scope.
    Hd1080,
    /// Wider. 4K/UHD.
    Uhd,
    /// ffprobe gave no usable dimensions. **Its own band**, not folded into any
    /// real one — a file we cannot measure is a distinct shape, and the planner
    /// will refuse it, which is itself worth seeing in the report.
    Unknown,
}

/// Subtitle-count buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleBand {
    None,
    /// 1-4. The ordinary case.
    Few,
    /// 5-20.
    Many,
    /// >20. The operator measured a file with **42** subtitle streams; every
    /// one of them is stream-copied and every one is verified positionally, so
    /// this is the shape most likely to expose a mapping bug.
    Extreme,
}

/// The shape of a file, for coverage purposes. Two files with the same key
/// exercise the same code path through plan, argv and verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiversityKey {
    /// `None` when the container is not one Foundry recognizes — which is a
    /// shape in its own right (the planner refuses it) and is kept distinct
    /// from every recognized container rather than being dropped.
    pub container: Option<Container>,
    pub video_codec: String,
    pub resolution: ResolutionBand,
    /// Every distinct audio codec in the file, lowercased, sorted, deduped.
    ///
    /// A **set**, not just the first stream: the measured library contains a
    /// file with `dts + ac3`, and its plan differs from a file with `dts`
    /// alone (one unacceptable codec forces the whole audio side to re-encode,
    /// and both streams then come out `aac`). Keying on the first stream would
    /// make those two look identical.
    pub audio_codecs: Vec<String>,
    pub subtitles: SubtitleBand,
    /// Width or height is not a multiple of 8.
    ///
    /// Part of the KEY, not merely of the score, because 1918x802 and
    /// 1920x1080 land in the same resolution band but are not the same test:
    /// unaligned dimensions are where an encoder's padding, a scale filter's
    /// rounding, and a container's display-aspect handling actually go wrong.
    ///
    /// **8, not 16.** 16 would be the macroblock size, but 1080 is not a
    /// multiple of 16 — so a mod-16 test flags *every standard 1080p file* as
    /// unusual and stops discriminating anything. Every standard resolution in
    /// the operator's measured sample (320x240, 320x256, 640x352, 720x480,
    /// 640x480, 1280x720, 1280x688, 1920x1080) is mod-8 aligned, and exactly
    /// the two genuinely odd ones (294x240 and 1918x802) are not.
    pub unaligned_dimensions: bool,
}

/// Band a width. See [`ResolutionBand`] for why width and not height.
pub fn resolution_band(width: Option<u32>, height: Option<u32>) -> ResolutionBand {
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => match w {
            0..=400 => ResolutionBand::Tiny,
            401..=800 => ResolutionBand::Sd,
            801..=1300 => ResolutionBand::Hd720,
            1301..=2000 => ResolutionBand::Hd1080,
            _ => ResolutionBand::Uhd,
        },
        _ => ResolutionBand::Unknown,
    }
}

/// Band a subtitle count.
pub fn subtitle_band(n: usize) -> SubtitleBand {
    match n {
        0 => SubtitleBand::None,
        1..=4 => SubtitleBand::Few,
        5..=20 => SubtitleBand::Many,
        _ => SubtitleBand::Extreme,
    }
}

/// Derive the shape of a probed file. Pure.
pub fn diversity_key(probe: &MediaProbe) -> DiversityKey {
    let video = probe.primary_video();
    let (w, h) = video.map_or((None, None), |v| (v.width, v.height));

    let mut audio_codecs: Vec<String> = probe
        .audio
        .iter()
        .map(|a| a.codec.trim().to_ascii_lowercase())
        .collect();
    audio_codecs.sort();
    audio_codecs.dedup();

    DiversityKey {
        container: normalize_container(&probe.container),
        video_codec: video
            .map(|v| v.codec.trim().to_ascii_lowercase())
            .unwrap_or_default(),
        resolution: resolution_band(w, h),
        audio_codecs,
        subtitles: subtitle_band(probe.subtitles.len()),
        // `map_or(true, ...)`: a dimension we could not read counts as
        // unaligned. Fail-closed applied to *sampling* — an unmeasurable file is
        // pulled toward being tested, never away from it.
        unaligned_dimensions: w.map_or(true, |w| w % 8 != 0) || h.map_or(true, |h| h % 8 != 0),
    }
}

/// How unusual a shape is, measured against the policy that will judge it.
///
/// Scored against `policy` rather than against hardcoded codec names so the
/// scale cannot drift away from what the planner actually accepts: "awkward"
/// means "far from the direct-play baseline this policy defines", and if the
/// policy widens, the score follows it.
///
/// Used only to ORDER the shapes when the sample limit is smaller than the
/// number of distinct shapes available — so a limit of 12 spends its budget on
/// `msmpeg4v2` at 294x240 and the 42-subtitle file, not on twelve flavours of
/// h264/aac.
pub fn awkwardness(key: &DiversityKey, policy: &TranscodePolicy) -> u32 {
    let mut score = 0;

    // A codec outside the accepted set forces a full re-encode — the expensive,
    // lossy, hardest-to-verify path, and the one the operator's oldest files take.
    if key.video_codec.is_empty() || !policy.accepts_video_codec(&key.video_codec) {
        score += 3;
    }

    // Both extremes of the resolution range, plus "we could not measure it".
    if matches!(
        key.resolution,
        ResolutionBand::Tiny | ResolutionBand::Uhd | ResolutionBand::Unknown
    ) {
        score += 2;
    }

    if key.audio_codecs.is_empty() {
        // No audio at all: `-map 0:a?` maps nothing and the expectation is an
        // empty list. An edge case that a library of ordinary films never hits.
        score += 2;
    } else {
        if key
            .audio_codecs
            .iter()
            .any(|c| !policy.accepts_audio_codec(c))
        {
            score += 2;
        }
        // dts + ac3 in one file: mixed acceptability, both re-encoded.
        if key.audio_codecs.len() > 1 {
            score += 1;
        }
    }

    score += match key.subtitles {
        SubtitleBand::None | SubtitleBand::Few => 0,
        SubtitleBand::Many => 2,
        SubtitleBand::Extreme => 3,
    };

    if key.unaligned_dimensions {
        score += 1;
    }

    // avi is 48% of the measured library and is NOT an accepted container, so
    // every one of those files is a container rewrite.
    //
    // An UNRECOGNIZED container scores **nothing**, and that is deliberate
    // rather than an omission — it was a surviving mutant (M47), and chasing it
    // showed the original +3 was wrong. `plan_transcode` refuses an
    // unrecognized container outright (`Undecidable::UnrecognizedContainer`),
    // so such a file can never produce an encode. Ranking it as the *most*
    // interesting shape would spend one of the operator's twelve slots on a
    // guaranteed skip, crowding out a file that would actually have been
    // encoded and verified. Its shape is still distinct, so it is still
    // reachable when there is budget to spare; it just does not outbid a file
    // that can produce evidence.
    match key.container {
        None => {}
        Some(c) if !policy.accepts_container(c) => score += 2,
        Some(_) => {}
    }

    score
}

/// Choose up to `limit` indices spanning as many distinct shapes as possible.
///
/// Pure and deterministic: no RNG, no clock, no filesystem. Given the same
/// candidates it returns the same selection, so a re-run is comparable with the
/// previous one.
///
/// The algorithm is a round-robin across shape groups, with groups ordered by
/// [`awkwardness`] and then by first appearance:
///
/// - **Every distinct shape gets its first file before any shape gets a
///   second.** This is the entire point. `Vec::truncate` on a walk order, or
///   picking N at random, both spend the budget on whichever shape is most
///   common — which in this library is the one that needs the least proving.
/// - When `limit` is smaller than the number of shapes, the awkward shapes are
///   the ones that fit.
/// - When `limit` exceeds the number of shapes, the extra slots go round again,
///   which is still more useful than N files of one shape: a second
///   `msmpeg4v2` file is evidence that the first was not a fluke.
pub fn select_diverse_sample(
    probes: &[MediaProbe],
    policy: &TranscodePolicy,
    limit: usize,
) -> Vec<usize> {
    if limit == 0 || probes.is_empty() {
        return Vec::new();
    }

    // Group by key, preserving first-appearance order within each group.
    let mut groups: Vec<(DiversityKey, Vec<usize>)> = Vec::new();
    for (i, p) in probes.iter().enumerate() {
        let key = diversity_key(p);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, members)) => members.push(i),
            None => groups.push((key, vec![i])),
        }
    }

    // Order the groups: most awkward first, ties broken by first appearance so
    // the result is stable rather than dependent on sort implementation.
    groups.sort_by_key(|(k, members)| {
        (
            std::cmp::Reverse(awkwardness(k, policy)),
            *members.first().unwrap_or(&usize::MAX),
        )
    });

    // Round-robin.
    let mut picked = Vec::with_capacity(limit);
    let mut round = 0usize;
    while picked.len() < limit {
        let mut took_any = false;
        for (_, members) in &groups {
            if picked.len() >= limit {
                break;
            }
            if let Some(&idx) = members.get(round) {
                picked.push(idx);
                took_any = true;
            }
        }
        if !took_any {
            // Every group is exhausted: there are fewer candidates than `limit`.
            break;
        }
        round += 1;
    }
    picked
}

/// Pick `want` indices spread evenly across `len`, for choosing which files to
/// *probe* in the first place.
///
/// Probing all 16,221 files to select 12 is not an option, and probing the
/// first N of a directory walk is worse than useless here: a media library
/// walks in path order, so the first 400 entries are a handful of shows in
/// their entirety — one codec, one encoder, one release group. An even stride
/// across the whole walk is the cheapest way to see the library's actual
/// spread. Pure and deterministic.
pub fn stride_sample(len: usize, want: usize) -> Vec<usize> {
    if len == 0 || want == 0 {
        return Vec::new();
    }
    if want >= len {
        return (0..len).collect();
    }
    // Rational stride, computed per element, so the picks stay evenly spread
    // and the last one lands near the end of the list rather than a `len/want`
    // integer stride drifting short.
    (0..want).map(|i| i * len / want).collect()
}

// ---------------------------------------------------------------------------
// The argv safety rail
// ---------------------------------------------------------------------------

/// Why an argv was refused before it could be spawned.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgvRefusal {
    /// The argv's final operand — ffmpeg's output file — is not the scratch
    /// path this harness generated. **The rail that matters.** If a future
    /// change ever wired a library path into the output slot, this refuses to
    /// spawn instead of overwriting it.
    OutputIsNotTheScratchFile { expected: String, found: String },
    /// The scratch path is not inside the scratch directory.
    OutputEscapesScratchDir { output: String, scratch_dir: String },
    /// The source path does not appear exactly once, as the operand of `-i`.
    /// Anything else means the source is being used as something other than a
    /// read-only input.
    SourceIsNotOnlyAnInput { input: String, occurrences: usize },
    /// The argv contains an option that makes ffmpeg write somewhere other
    /// than its final operand. Codex raised this at the FOUNDRY-04 gate:
    /// checking the output operand and `-i` proves nothing about
    /// `-passlogfile`, `-progress` or any other write-capable flag.
    ///
    /// Fail-CLOSED: this refuses any option not on an allowlist of flags known
    /// to be read-only, rather than blocking a list of known-bad ones. A
    /// denylist is wrong here for the usual reason — it is silent about the
    /// option nobody thought of, which is precisely the one that writes to the
    /// library.
    OptionNotKnownReadOnly { option: String },
    /// An empty argv could not be checked at all, so it is refused.
    EmptyArgv,
}

impl std::fmt::Display for ArgvRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OptionNotKnownReadOnly { option } => write!(
                f,
                "argv contains `{option}`, which is not on the allowlist of options this \
                 harness knows cannot write outside its scratch file — refusing to spawn \
                 rather than assuming it is harmless"
            ),
            Self::OutputIsNotTheScratchFile { expected, found } => write!(
                f,
                "the ffmpeg output operand is `{found}`, not the scratch file `{expected}` — \
                 refusing to spawn"
            ),
            Self::OutputEscapesScratchDir {
                output,
                scratch_dir,
            } => write!(
                f,
                "the output path `{output}` is not inside the scratch directory \
                 `{scratch_dir}` — refusing to spawn"
            ),
            Self::SourceIsNotOnlyAnInput {
                input,
                occurrences,
            } => write!(
                f,
                "the source path `{input}` appears {occurrences} time(s) in the argv and not \
                 solely as the operand of `-i` — refusing to spawn"
            ),
            Self::EmptyArgv => write!(f, "the argv is empty, so it could not be checked"),
        }
    }
}

/// Prove, before spawning, that this command line can only write to the scratch
/// file — and that the source is only ever read.
///
/// **This is the load-bearing safety check of the whole module**, and it is
/// pure so that it is tested rather than trusted. Everything else here is
/// discipline (do not call `forge`, do not rename anything); this is a
/// mechanical check applied to the literal argument list a fraction of a second
/// before it becomes a process.
///
/// Fail closed on every axis: an argv it cannot fully account for is refused.
/// Options this harness accepts in a generated ffmpeg argv.
///
/// An ALLOWLIST, deliberately. The rail's job is to guarantee ffmpeg cannot
/// write anywhere but the scratch file, and a denylist of write-capable flags
/// (`-passlogfile`, `-progress`, `-f segment`, ...) is only ever as complete as
/// the last person's memory. Anything not named here is refused, so extending
/// the encoder means extending this list on purpose.
///
/// Every entry is a flag `plan_transcode`/`build_rendition_args` actually emit.
const READ_ONLY_OPTIONS: &[&str] = &[
    "-hide_banner", "-loglevel", "-nostdin", "-y", "-i",
    "-map", "-map_metadata", "-map_chapters",
    "-c:v", "-c:a", "-c:s", "-c:t", "-c", "-crf", "-preset", "-vf", "-pix_fmt",
    "-maxrate", "-bufsize", "-b:v", "-b:a", "-ac", "-ar", "-profile:v",
    "-level", "-movflags", "-f", "-metadata", "-disposition",
    "-max_muxing_queue_size", "-threads", "-sn", "-an", "-vn", "-dn",
];

pub fn argv_writes_only_to(
    args: &[String],
    input_path: &str,
    scratch_output: &Path,
    scratch_dir: &Path,
) -> Result<(), ArgvRefusal> {
    let Some(last) = args.last() else {
        return Err(ArgvRefusal::EmptyArgv);
    };

    // 1. ffmpeg writes to its final operand. It must be exactly the path we
    //    generated — not a prefix of it, not a path that merely looks similar.
    let expected = scratch_output.to_string_lossy().to_string();
    if *last != expected {
        return Err(ArgvRefusal::OutputIsNotTheScratchFile {
            expected,
            found: last.clone(),
        });
    }

    // 2. ...and that path must genuinely live under the scratch directory, with
    //    at least one component below it (so the scratch dir itself, or a bare
    //    prefix match, cannot pass).
    if !scratch_output.starts_with(scratch_dir) || scratch_output == scratch_dir {
        return Err(ArgvRefusal::OutputEscapesScratchDir {
            output: expected,
            scratch_dir: scratch_dir.to_string_lossy().to_string(),
        });
    }

    // 3. The source appears exactly once, and the token before it is `-i`.
    //    A second occurrence would mean the source is filling some other slot —
    //    which, for the final operand, would mean encoding over the original.
    let occurrences = args.iter().filter(|a| a.as_str() == input_path).count();
    let position = args.iter().position(|a| a.as_str() == input_path);
    let preceded_by_dash_i = match position {
        Some(0) | None => false,
        Some(p) => args[p - 1] == "-i",
    };
    if occurrences != 1 || !preceded_by_dash_i {
        return Err(ArgvRefusal::SourceIsNotOnlyAnInput {
            input: input_path.to_string(),
            occurrences,
        });
    }

    // 4. No option may be one that writes outside the final operand. Checked
    //     against an allowlist so an unrecognised flag refuses rather than
    //     being assumed harmless. Only tokens that LOOK like options are
    //     examined; values (codec names, paths, numbers) are not options, and
    //     a negative number like `-1` is a value, not a flag.
    for a in args.iter() {
        if !a.starts_with('-') || a.len() < 2 {
            continue;
        }
        if a.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
            continue; // a negative number, e.g. "-1"
        }
        // Per-stream specifiers (`-ac:a:0`, `-c:v`, `-b:a:1`) are the same
        // option with a stream selector appended, so the allowlist is checked
        // against the part before the first ':'. Still fail-closed: the BASE
        // option must be listed, and an unknown base is refused whatever
        // selector follows it.
        let base = a.split(':').next().unwrap_or(a.as_str());
        if !READ_ONLY_OPTIONS.contains(&a.as_str()) && !READ_ONLY_OPTIONS.contains(&base) {
            return Err(ArgvRefusal::OptionNotKnownReadOnly { option: a.clone() });
        }
    }


    Ok(())
}

// ---------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------

/// Why a selected file was not encoded. A skip is neither a pass nor a
/// failure — it is a stated gap in the evidence, and it is counted separately
/// so it cannot be read as either.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidateSkip {
    /// The planner says the file already meets policy, so there is no encode to
    /// validate. Not a failure: this is the correct answer for such a file.
    AlreadyOptimal,
    /// The planner declined to judge it. Also the correct answer — and the
    /// reason is carried so a library that is mostly undecidable is visible.
    Undecidable { why: String },
    /// Above [`ValidationBounds::max_input_bytes`]. The 4K/HDR tail lands here.
    InputTooLarge { bytes: u64, ceiling_bytes: u64 },
    /// ffprobe reported no size for the input, so it could not be bounded.
    /// Skipped rather than attempted — an input we cannot measure is an input
    /// we cannot budget for.
    UnknownInputSize,
    /// Encoding it would exceed [`ValidationBounds::max_total_output_bytes`].
    ScratchBudgetExhausted {
        would_reserve_bytes: u64,
        remaining_bytes: u64,
    },
    /// [`ValidationBounds::run_deadline`] passed before this file was reached.
    RunDeadlineReached,
    /// A required tool is not usable on this host.
    ToolUnavailable { tool: &'static str, detail: String },
}

impl std::fmt::Display for ValidateSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyOptimal => {
                write!(f, "the file already meets the policy — there is no encode to validate")
            }
            Self::Undecidable { why } => write!(f, "the planner could not judge the file: {why}"),
            Self::InputTooLarge {
                bytes,
                ceiling_bytes,
            } => write!(
                f,
                "the input is {bytes} bytes, above the {ceiling_bytes}-byte per-file ceiling — \
                 the scratch filesystem cannot hold its output safely"
            ),
            Self::UnknownInputSize => write!(
                f,
                "ffprobe reported no size for the input, so it could not be checked against the \
                 scratch budget"
            ),
            Self::ScratchBudgetExhausted {
                would_reserve_bytes,
                remaining_bytes,
            } => write!(
                f,
                "encoding it would reserve {would_reserve_bytes} bytes against {remaining_bytes} \
                 remaining in the scratch budget"
            ),
            Self::RunDeadlineReached => write!(
                f,
                "the run's wall-clock deadline passed before this file was reached"
            ),
            Self::ToolUnavailable { tool, detail } => {
                write!(f, "required tool `{tool}` is not usable on this host: {detail}")
            }
        }
    }
}

/// Why an encode did not produce a verified output. Every variant means the
/// same thing to the operator's decision: **this file is not evidence that
/// Path A is safe.**
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationFailure {
    /// The argv safety rail refused to spawn. A bug in Foundry, reported as a
    /// failure rather than worked around.
    ArgvRefused { refusal: ArgvRefusal },
    /// The scratch file or directory could not be prepared.
    ScratchUnusable { detail: String },
    /// ffmpeg could not be spawned, or exited non-zero.
    Encode { detail: String },
    /// The encode exceeded [`ValidationBounds::per_encode_timeout`] and was
    /// killed. A failure by the fail-closed rule — an encode we stopped is an
    /// encode we cannot vouch for.
    Timeout { after_secs: u64 },
    /// ffmpeg exited zero but wrote nothing we could re-probe. An output we
    /// cannot read is an output we cannot clear.
    OutputUnprobeable { detail: String },
    /// No expectation could be derived — the source duration is unknown, so no
    /// output of it could ever be proven un-truncated.
    Unverifiable { detail: String },
    /// **forge's own verification** rejected the output. This is the finding
    /// the whole harness exists to surface, and it carries forge's exact
    /// [`VerifyFailure`] so the mismatch is specific.
    Verification { failure: VerifyFailure },
    /// The output container is not the one the plan asked ffmpeg for. Checked
    /// here and not by forge because forge's expectation is derived from the
    /// *source* streams; the container is a promise the argv's `-f` made that
    /// nothing else re-reads.
    ContainerMismatch { expected: String, found: String },
}

impl std::fmt::Display for ValidationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ArgvRefused { refusal } => write!(
                f,
                "the command line failed this harness's write-confinement check: {refusal}"
            ),
            Self::ScratchUnusable { detail } => {
                write!(f, "the scratch location could not be used: {detail}")
            }
            Self::Encode { detail } => write!(f, "the encode failed: {detail}"),
            Self::Timeout { after_secs } => write!(
                f,
                "the encode was killed after {after_secs}s. Counted as a FAILURE because an \
                 encode we stopped is one we cannot vouch for — but the usual cause is a long \
                 or slow input, not a broken encoder"
            ),
            Self::OutputUnprobeable { detail } => write!(
                f,
                "ffmpeg reported success but its output could not be re-probed, so nothing \
                 about it could be checked: {detail}"
            ),
            Self::Unverifiable { detail } => write!(f, "the output could not be verified: {detail}"),
            Self::Verification { failure } => write!(f, "{failure}"),
            Self::ContainerMismatch { expected, found } => write!(
                f,
                "the output container is `{found}`, but the plan asked ffmpeg for `{expected}`"
            ),
        }
    }
}

/// What happened to one selected file. Exactly three things, and "we could not
/// tell" is not one of them.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationOutcome {
    /// Encoded for real, re-probed, and cleared by forge's own verification
    /// plus the container check. The only outcome that is evidence *for* Path A.
    Verified,
    /// Encoded (or attempted) and did not clear. The finding.
    Failed { failure: ValidationFailure },
    /// Deliberately not attempted, with a stated reason.
    Skipped { reason: ValidateSkip },
}

impl ValidationOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Failed { .. } => "failed",
            Self::Skipped { .. } => "skipped",
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// The full record of one file, written for an operator making a decision.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedFile {
    pub path: String,
    // --- input characteristics ---
    pub input_container: String,
    pub input_video_codec: String,
    pub input_dimensions: Option<(u32, u32)>,
    pub input_audio_codecs: Vec<String>,
    pub input_subtitle_count: usize,
    pub input_attachment_count: usize,
    pub input_chapter_count: usize,
    pub input_duration_secs: Option<f64>,
    pub input_bytes: Option<u64>,
    // --- what the plan promised ---
    /// `Display` form of each [`TranscodeReason`] — why this file was to be
    /// rewritten at all. Empty when nothing was planned.
    pub plan_reasons: Vec<String>,
    /// `None` when nothing was planned.
    pub plan_summary: Option<String>,
    // --- what came out ---
    pub output_container: Option<String>,
    pub output_video_codec: Option<String>,
    pub output_dimensions: Option<(u32, u32)>,
    pub output_audio_codecs: Vec<String>,
    pub output_subtitle_count: Option<usize>,
    pub output_duration_secs: Option<f64>,
    pub output_bytes: Option<u64>,
    /// Output minus input, in bytes. Negative means the encode saved space.
    pub size_delta_bytes: Option<i64>,
    pub encode_wall_secs: Option<f64>,
    // --- the verdict on this file ---
    pub outcome: ValidationOutcome,
}

/// The whole run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValidationRun {
    /// Candidates whose source probe failed during selection. Their own count:
    /// "ffprobe cannot read these files" and "these files failed validation"
    /// are different problems with different fixes.
    pub source_probe_failures: usize,
    /// How many candidates were probed to choose the sample from.
    pub candidates_probed: usize,
    /// How many distinct shapes ([`DiversityKey`]) the probed candidates
    /// contained. The honest measure of how much of the library's variety the
    /// sample could possibly have covered.
    pub distinct_shapes: usize,
    pub verified: usize,
    pub failed: usize,
    pub skipped: usize,
    pub files: Vec<ValidatedFile>,
    /// Total bytes reserved against the scratch budget.
    pub scratch_bytes_reserved: u64,
    /// True when the run's wall-clock deadline cut it short.
    pub deadline_hit: bool,
}

impl ValidationRun {
    fn record(&mut self, file: ValidatedFile) {
        match &file.outcome {
            ValidationOutcome::Verified => self.verified += 1,
            ValidationOutcome::Failed { .. } => self.failed += 1,
            ValidationOutcome::Skipped { .. } => self.skipped += 1,
        }
        self.files.push(file);
    }

    /// The aggregate. **Failures are checked first and nothing can mask them.**
    ///
    /// There is deliberately no pass rate, no percentage, and no "mostly fine"
    /// state anywhere in this type. The operator is deciding whether to let
    /// this code delete originals; one file in twelve that came out wrong is
    /// the entire answer, and a ratio is how that answer gets rounded away.
    pub fn verdict(&self) -> ValidationVerdict {
        if self.failed > 0 {
            return ValidationVerdict::Failures {
                failed: self.failed,
                verified: self.verified,
            };
        }
        if self.candidates_probed > 0 && self.source_probe_failures == self.candidates_probed {
            // Every probe failed: the tooling is broken, and a zero-failure run
            // on zero encodes must not read as a clean bill of health.
            return ValidationVerdict::ProbeUnusable;
        }
        if self.verified == 0 {
            return ValidationVerdict::NothingValidated;
        }
        ValidationVerdict::AllVerified {
            verified: self.verified,
        }
    }
}

/// A judgement about the RUN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationVerdict {
    /// Nothing was encoded — every selected file was skipped, or there were
    /// none. Says nothing about the encoder either way, and is NOT a pass.
    NothingValidated,
    /// Every source probe failed. The tooling is not working.
    ProbeUnusable,
    /// At least one file did not come out right. **This is the finding**, and
    /// it wins over every other state.
    Failures { failed: usize, verified: usize },
    /// Every file that was encoded came out verified, and at least one was.
    ///
    /// Still not an instruction to enable deletion: it says the encodes in
    /// *this* sample were sound, and the report's skip list says what was not
    /// covered.
    AllVerified { verified: usize },
}

impl ValidationVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NothingValidated => "nothing_validated",
            Self::ProbeUnusable => "probe_unusable",
            Self::Failures { .. } => "failures",
            Self::AllVerified { .. } => "all_verified",
        }
    }
}

// ---------------------------------------------------------------------------
// Budget: pure
// ---------------------------------------------------------------------------

/// Decide whether one input may be encoded within the bounds, and how much of
/// the budget it reserves.
///
/// Pure, fail-closed: an input whose size is unknown is refused rather than
/// admitted, because an unmeasured input cannot be reserved for.
pub fn budget_admits(
    input_bytes: Option<u64>,
    already_reserved: u64,
    bounds: &ValidationBounds,
) -> Result<u64, ValidateSkip> {
    let Some(bytes) = input_bytes else {
        return Err(ValidateSkip::UnknownInputSize);
    };
    if bytes > bounds.max_input_bytes {
        return Err(ValidateSkip::InputTooLarge {
            bytes,
            ceiling_bytes: bounds.max_input_bytes,
        });
    }
    let reserve = bytes.saturating_mul(bounds.output_reserve_factor);
    let remaining = bounds
        .max_total_output_bytes
        .saturating_sub(already_reserved);
    if reserve > remaining {
        return Err(ValidateSkip::ScratchBudgetExhausted {
            would_reserve_bytes: reserve,
            remaining_bytes: remaining,
        });
    }
    Ok(reserve)
}

/// Confirm the output's container is the one the plan's `-f` asked for. Pure.
///
/// Fail-closed: an output container ffprobe named but Foundry does not
/// recognize is a mismatch, not a shrug.
pub fn check_output_container(
    expected: Container,
    output_format_name: &str,
) -> Result<(), ValidationFailure> {
    match normalize_container(output_format_name) {
        Some(found) if found == expected => Ok(()),
        other => Err(ValidationFailure::ContainerMismatch {
            expected: expected.ffmpeg_format().to_string(),
            found: other.map_or_else(
                || format!("unrecognized ({output_format_name})"),
                |c| c.ffmpeg_format().to_string(),
            ),
        }),
    }
}

/// Turn an observed output into an outcome. **Pure**, and the reason it is
/// pure is that this is where a bug would be silent.
///
/// The gating order — verification, then container, then and only then
/// `Verified` — lives here rather than inline in the driver so it can be
/// exercised without ffmpeg. Inline, every one of these rules was reachable
/// only on a host with a working encoder and a real media file, which is the
/// "covered only by tests that skip" pattern that has repeatedly let
/// safety-critical mutations survive in this codebase.
///
/// `output` is `None` when the file could not be re-probed at all — which is a
/// FAILURE, not an absence of evidence to shrug at.
pub fn judge_output(
    expectation: &VerifyExpectation,
    expected_container: Container,
    output: Option<&MediaProbe>,
) -> ValidationOutcome {
    let Some(out) = output else {
        return ValidationOutcome::Failed {
            failure: ValidationFailure::OutputUnprobeable {
                detail: "the output could not be re-probed".to_string(),
            },
        };
    };
    if let Err(failure) = verify_output(expectation, out) {
        return ValidationOutcome::Failed {
            failure: ValidationFailure::Verification { failure },
        };
    }
    if let Err(failure) = check_output_container(expected_container, &out.container) {
        return ValidationOutcome::Failed { failure };
    }
    ValidationOutcome::Verified
}

/// Map how the encode process ended onto a failure, or `None` when it
/// succeeded. Pure, so the timeout-is-a-failure rule is testable without
/// waiting twenty minutes for a real one.
fn encode_failure(run: EncodeRun) -> Option<ValidationFailure> {
    match run {
        EncodeRun::Ok => None,
        EncodeRun::NonZero { detail } | EncodeRun::SpawnFailed { detail } => {
            Some(ValidationFailure::Encode { detail })
        }
        EncodeRun::TimedOut { after } => Some(ValidationFailure::Timeout {
            after_secs: after.as_secs(),
        }),
    }
}

/// A one-line description of what the plan will do, for the report.
pub fn describe_plan(plan: &TranscodePlan) -> String {
    use crate::foundry::plan::{AudioAction, VideoAction};
    let video = match plan.video {
        VideoAction::Copy => "video: copy".to_string(),
        VideoAction::Encode { scale: None } => "video: re-encode".to_string(),
        VideoAction::Encode {
            scale: Some((w, h)),
        } => format!("video: re-encode + scale to {w}x{h}"),
    };
    let audio = match plan.audio {
        AudioAction::Copy => "audio: copy",
        AudioAction::Encode { .. } => "audio: re-encode to aac",
    };
    format!(
        "{video}; {audio}; container: {}{}",
        plan.container.ffmpeg_format(),
        if plan.is_remux_only() {
            " (remux only — nothing re-encoded)"
        } else {
            ""
        }
    )
}

// ---------------------------------------------------------------------------
// Scratch: RAII
// ---------------------------------------------------------------------------

/// A scratch output file (plus its captured stderr), removed on drop.
///
/// `Drop` rather than an explicit cleanup call at the end of each iteration,
/// because the brief is specific about it: *"clean up each scratch output after
/// verifying, or the run dies partway through."* Every early return in
/// [`validate_one`] — and a panic — still removes the file. With a 6 GiB budget
/// and 12 files, one leaked 1.5 GiB output is a quarter of the run.
struct ScratchOutput {
    output: PathBuf,
    stderr: PathBuf,
}

impl ScratchOutput {
    fn new(dir: &Path, extension: &str) -> Self {
        let job = uuid::Uuid::new_v4();
        Self {
            output: dir.join(format!("muse-foundry-validate-{job}.{extension}")),
            stderr: dir.join(format!("muse-foundry-validate-{job}.stderr")),
        }
    }
}

impl Drop for ScratchOutput {
    fn drop(&mut self) {
        for p in [&self.output, &self.stderr] {
            if let Err(e) = std::fs::remove_file(p) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        error = %e,
                        path = %p.display(),
                        "foundry/validate: could not remove a scratch file"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The one impure layer: running ffmpeg with a timeout
// ---------------------------------------------------------------------------

/// How an encode process ended.
#[derive(Debug)]
enum EncodeRun {
    Ok,
    NonZero { detail: String },
    TimedOut { after: Duration },
    SpawnFailed { detail: String },
}

/// Poll interval while waiting for ffmpeg. Long enough to be free, short enough
/// that the timeout is honoured to within a second.
const ENCODE_POLL: Duration = Duration::from_millis(500);

/// Run ffmpeg with a hard wall-clock ceiling.
///
/// `Command::output()` — which [`crate::foundry::forge`] uses — has no timeout,
/// so one pathological file could wedge an unattended run for ever. This
/// spawns, polls, and kills.
///
/// stderr goes to a **file**, not a pipe. A pipe that is not drained while the
/// child runs fills its 64 KiB buffer and blocks ffmpeg for ever — which would
/// present as a timeout on a file that was actually fine, and would be a
/// self-inflicted false failure in the one report that must not have any.
fn run_encode_with_timeout(
    ffmpeg_bin: &str,
    args: &[String],
    stderr_path: &Path,
    timeout: Duration,
) -> EncodeRun {
    let stderr_file = match std::fs::File::create(stderr_path) {
        Ok(f) => f,
        Err(e) => {
            return EncodeRun::SpawnFailed {
                detail: format!("creating the stderr capture file: {e}"),
            }
        }
    };

    let mut child = match Command::new(ffmpeg_bin)
        .args(args)
        // `-nostdin` is already in the argv; null stdin as well, so an
        // inherited terminal cannot be consumed even if that flag ever moves.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return EncodeRun::SpawnFailed {
                detail: format!("spawning `{ffmpeg_bin}`: {e}"),
            }
        }
    };

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return EncodeRun::Ok;
                }
                let tail = read_stderr_tail(stderr_path);
                return EncodeRun::NonZero {
                    detail: format!(
                        "ffmpeg exited with {}: {tail}",
                        status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "a signal".into())
                    ),
                };
            }
            Ok(None) => {
                let elapsed = start.elapsed();
                if elapsed >= timeout {
                    let _ = child.kill();
                    // `try_wait`, NEVER `wait`. A child wedged in
                    // uninterruptible D-state on a stalled NFS read ignores
                    // SIGKILL until its I/O returns, so a blocking `wait()`
                    // here hangs forever — the timeout path would itself
                    // become the hang it exists to prevent. Seen live in the
                    // probe path (FOUNDRY-12); this is the same defect in the
                    // encoder.
                    //
                    // Not reaping leaves a zombie, bounded by the number of
                    // STALLS rather than of files. A few zombies are
                    // survivable; a hang is not.
                    let _ = child.try_wait();
                    return EncodeRun::TimedOut { after: elapsed };
                }
                std::thread::sleep(ENCODE_POLL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.try_wait();
                return EncodeRun::SpawnFailed {
                    detail: format!("waiting on ffmpeg: {e}"),
                };
            }
        }
    }
}

/// The last 400 characters of the captured stderr, for the report.
fn read_stderr_tail(path: &Path) -> String {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let trimmed = raw.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    let start = chars.len().saturating_sub(400);
    chars[start..].iter().collect()
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// Build the report skeleton for a file from its source probe alone, so every
/// outcome — including a skip — still carries the input characteristics the
/// operator needs to interpret it.
fn describe_input(path: &Path, probe: &MediaProbe) -> ValidatedFile {
    let video = probe.primary_video();
    ValidatedFile {
        path: path.display().to_string(),
        input_container: probe.container.clone(),
        input_video_codec: video.map(|v| v.codec.clone()).unwrap_or_default(),
        input_dimensions: video.and_then(|v| Some((v.width?, v.height?))),
        input_audio_codecs: probe.audio.iter().map(|a| a.codec.clone()).collect(),
        input_subtitle_count: probe.subtitles.len(),
        input_attachment_count: probe.attachments.len(),
        input_chapter_count: probe.chapter_count,
        input_duration_secs: probe.duration_secs,
        input_bytes: probe.size_bytes,
        plan_reasons: Vec::new(),
        plan_summary: None,
        output_container: None,
        output_video_codec: None,
        output_dimensions: None,
        output_audio_codecs: Vec::new(),
        output_subtitle_count: None,
        output_duration_secs: None,
        output_bytes: None,
        size_delta_bytes: None,
        encode_wall_secs: None,
        outcome: ValidationOutcome::Skipped {
            reason: ValidateSkip::AlreadyOptimal,
        },
    }
}

fn fill_output(file: &mut ValidatedFile, out: &MediaProbe) {
    let video = out.primary_video();
    file.output_container = Some(out.container.clone());
    file.output_video_codec = video.map(|v| v.codec.clone());
    file.output_dimensions = video.and_then(|v| Some((v.width?, v.height?)));
    file.output_audio_codecs = out.audio.iter().map(|a| a.codec.clone()).collect();
    file.output_subtitle_count = Some(out.subtitles.len());
    file.output_duration_secs = out.duration_secs;
    file.output_bytes = out.size_bytes;
    file.size_delta_bytes = match (out.size_bytes, file.input_bytes) {
        // Saturating, not `as i64`. These sizes come from ffprobe on an
        // arbitrary file; a value past i64::MAX wraps to a negative delta in
        // release and panics in debug. Codex, FOUNDRY-04 gate.
        (Some(o), Some(i)) => Some((o as i128 - i as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64),
        _ => None,
    };
}

/// Probe, plan, ENCODE TO SCRATCH, re-probe, verify, delete. Never touches the
/// original.
///
/// The source is passed as an already-resolved, guard-approved path and is used
/// only as ffmpeg's `-i` operand — checked mechanically by
/// [`argv_writes_only_to`] immediately before the spawn.
#[allow(clippy::too_many_arguments)]
fn validate_one(
    cfg: &FoundryConfig,
    policy: &TranscodePolicy,
    scratch_dir: &Path,
    source_path: &Path,
    resolved_input: &str,
    probe: &MediaProbe,
    bounds: &ValidationBounds,
    already_reserved: u64,
    // Wall-clock left in the whole run, if a deadline applies. The encode's
    // own ceiling is reduced to this so one encode cannot outlive the run.
    deadline_remaining: Option<Duration>,
) -> (ValidatedFile, u64) {
    let mut file = describe_input(source_path, probe);

    // --- plan (pure) -------------------------------------------------------
    // The output path is decided first because the argv embeds it, and the
    // extension has to match the container the plan will ask ffmpeg for.
    let Some(container) = crate::foundry::plan::output_container(probe, policy) else {
        file.outcome = ValidationOutcome::Skipped {
            reason: ValidateSkip::Undecidable {
                why: format!("unrecognized container `{}`", probe.container),
            },
        };
        return (file, 0);
    };
    let scratch = ScratchOutput::new(scratch_dir, container.extension());

    let decision = plan_transcode(
        probe,
        policy,
        resolved_input,
        &scratch.output.to_string_lossy(),
    );
    let (plan, args, reasons) = match decision {
        TranscodeDecision::AlreadyOptimal => {
            file.outcome = ValidationOutcome::Skipped {
                reason: ValidateSkip::AlreadyOptimal,
            };
            return (file, 0);
        }
        TranscodeDecision::CannotDecide { why } => {
            file.outcome = ValidationOutcome::Skipped {
                reason: ValidateSkip::Undecidable {
                    why: why.to_string(),
                },
            };
            return (file, 0);
        }
        TranscodeDecision::Transcode { plan, args, reasons } => (plan, args, reasons),
    };
    file.plan_reasons = reasons.iter().map(TranscodeReason::to_string).collect();
    file.plan_summary = Some(describe_plan(&plan));

    // --- bounds ------------------------------------------------------------
    let reserve = match budget_admits(probe.size_bytes, already_reserved, bounds) {
        Ok(r) => r,
        Err(reason) => {
            file.outcome = ValidationOutcome::Skipped { reason };
            return (file, 0);
        }
    };

    // --- the expectation, derived BEFORE the encode ------------------------
    // Derived from the source and the plan, so it cannot be influenced by what
    // the encode happened to produce. If no expectation can be derived, nothing
    // is encoded at all — an unverifiable encode is wasted CPU and a result
    // nobody could act on.
    let expectation: VerifyExpectation = match expectation_for(probe, &plan, policy) {
        Some(e) => e,
        None => {
            file.outcome = ValidationOutcome::Failed {
                failure: ValidationFailure::Unverifiable {
                    detail: "the source duration or an audio channel count is unknown, so no \
                             output of this file could be proven complete"
                        .to_string(),
                },
            };
            return (file, 0);
        }
    };

    // --- the write-confinement rail ----------------------------------------
    if let Err(refusal) = argv_writes_only_to(&args, resolved_input, &scratch.output, scratch_dir) {
        tracing::error!(
            refusal = %refusal,
            "foundry/validate: refusing to spawn a command line that is not confined to scratch"
        );
        file.outcome = ValidationOutcome::Failed {
            failure: ValidationFailure::ArgvRefused { refusal },
        };
        return (file, 0);
    }

    // --- encode ------------------------------------------------------------
    let started = Instant::now();
    // Capped by the time the RUN has left, not just the per-encode ceiling.
    // The two clamps are independent, so `run_deadline_secs=60` with
    // `encode_timeout_secs=21600` was accepted and an in-flight encode could
    // run six hours past a stated one-minute deadline — the deadline was only
    // consulted BETWEEN files. Codex, FOUNDRY-09 gate.
    let encode_timeout = match deadline_remaining {
        Some(left) => bounds.per_encode_timeout.min(left),
        None => bounds.per_encode_timeout,
    };
    let run = run_encode_with_timeout(&cfg.ffmpeg_bin, &args, &scratch.stderr, encode_timeout);
    file.encode_wall_secs = Some(started.elapsed().as_secs_f64());

    if let Some(failure) = encode_failure(run) {
        file.outcome = ValidationOutcome::Failed { failure };
        return (file, reserve);
    }

    // --- re-probe the output -----------------------------------------------
    // Deliberately NOT through the PathGuard: the guard confines paths to the
    // library's allowed roots, and the scratch dir is required to be outside
    // every one of them. `run_ffprobe` takes a `ResolvedPath`, so the scratch
    // path is minted through the guard-free constructor below.
    let out_probe = match probe_scratch_file(&cfg.ffprobe_bin, &scratch.output) {
        Ok(p) => Some(p),
        Err(detail) => {
            tracing::warn!(detail, "foundry/validate: an encoded output could not be re-probed");
            None
        }
    };
    if let Some(p) = &out_probe {
        fill_output(&mut file, p);
    }

    // --- verify: forge's own rules, applied to scratch ---------------------
    file.outcome = judge_output(&expectation, plan.container, out_probe.as_ref());
    (file, reserve)
    // `scratch` drops here: the output and its stderr capture are removed.
}

/// ffprobe a scratch file.
///
/// Split out with a named reason: [`run_ffprobe`] takes a
/// [`crate::foundry::ResolvedPath`], whose whole purpose is "this path was
/// approved by the library's PathGuard". The scratch file is deliberately
/// *outside* every allowed root (rail 3), so the guard would — correctly —
/// refuse it. Minting a resolved path here is therefore not a bypass of the
/// guard's job; it is reading a file this process just created, at a path this
/// process generated, in a directory it verified. Nothing about it is
/// attacker-influenced or derived from the library.
fn probe_scratch_file(ffprobe_bin: &str, path: &Path) -> Result<MediaProbe, String> {
    let resolved = crate::foundry::paths::ResolvedPath::for_process_owned_scratch(path);
    run_ffprobe(ffprobe_bin, &resolved).map_err(|e| e.to_string())
}

/// Why a run could not start at all. Distinct from an empty run: "the harness
/// is not set up" and "the library needs nothing" are different facts.
#[derive(Debug, Clone, PartialEq)]
pub enum ValidationRefusal {
    /// `MUSE_FOUNDRY_WORK_DIR` is unset, so there is no scratch location.
    NoScratchDir,
    /// The scratch location violates safety rail 3.
    ScratchViolatesRail3 { problems: Vec<String> },
    /// The scratch directory could not be created.
    ScratchUnusable { detail: String },
    /// ffprobe or ffmpeg is not usable on this host.
    ToolUnavailable { tool: &'static str, detail: String },
    /// The scratch filesystem does not have room for the run's budget.
    ///
    /// Raised by codex and free at the FOUNDRY-04 gate: the 6 GiB budget was
    /// reservation ACCOUNTING, not a statement about the disk. On a host with
    /// ~13 GB free and a root filesystem shared with everything else, a run
    /// that admits work it cannot store fills the disk — and per
    /// `pvf1_vgscratch`, a full/failing filesystem here presents as bogus
    /// compiler gates and systemctl EIO, not as an obvious disk error.
    NotEnoughFreeSpace { available_bytes: u64, needed_bytes: u64 },
}

impl std::fmt::Display for ValidationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoScratchDir => write!(
                f,
                "MUSE_FOUNDRY_WORK_DIR is not set, so there is nowhere outside the library to \
                 encode to — validation needs a scratch directory on a different filesystem"
            ),
            Self::NotEnoughFreeSpace { available_bytes, needed_bytes } => write!(
                f,
                "the scratch filesystem has {available_bytes} bytes free but this run's budget \
                 needs {needed_bytes} — refusing to start rather than filling the disk partway \
                 through and leaving the host in a state that looks like unrelated failures"
            ),
            Self::ScratchViolatesRail3 { problems } => write!(
                f,
                "the scratch directory violates safety rail 3 (never in place): {}",
                problems.join("; ")
            ),
            Self::ScratchUnusable { detail } => {
                write!(f, "the scratch directory could not be created: {detail}")
            }
            Self::ToolUnavailable { tool, detail } => write!(
                f,
                "required tool `{tool}` is not usable on this host: {detail}"
            ),
        }
    }
}

/// Resolve and prepare the scratch directory, refusing anything that would put
/// output near the library.
///
/// Rail 3 is re-checked here, unconditionally, and that is the point: the
/// config only escalates rail-3 problems to `fatal_errors` when
/// `MUSE_FOUNDRY_ENABLE_MUTATION` is enabled — and it is *not* enabled on this
/// deployment. This harness writes real files regardless of that flag, so it
/// must not inherit the leniency the flag buys.
pub fn prepare_scratch_dir(foundry: &Foundry) -> Result<PathBuf, ValidationRefusal> {
    // Takes the `Foundry`, not its config: `FoundryConfig` carries the raw
    // allowed roots, so a public function accepting one would need a public
    // accessor for it — the exact leak `Foundry::config()` was narrowed to
    // prevent (S128 MUSEF-01 review, round 6).
    let cfg = foundry.config();
    let Some(base) = cfg.work_dir_for_validation() else {
        return Err(ValidationRefusal::NoScratchDir);
    };
    // Its own subdirectory, so a validation output can never be confused with
    // a forge staging file, and so cleanup is scoped.
    let dir = base.join("validate");

    let problems = cfg.scratch_rail3_problems(&dir);
    if !problems.is_empty() {
        return Err(ValidationRefusal::ScratchViolatesRail3 { problems });
    }

    std::fs::create_dir_all(&dir).map_err(|e| ValidationRefusal::ScratchUnusable {
        detail: format!("{}: {e}", dir.display()),
    })?;
    Ok(dir)
}

/// Bytes free on the filesystem holding `dir`, or `None` if it cannot be read.
///
/// Shells out to `df` because Muse has no `libc` dependency and `std` exposes
/// no `statvfs`. `None` means "could not determine", which the caller treats as
/// a refusal rather than as "plenty" — the whole point is not to guess.
pub fn free_bytes_for(dir: &Path) -> Option<u64> {
    let out = std::process::Command::new("df")
        .arg("--output=avail")
        .arg("-B1")
        .arg(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Refuse a run the scratch filesystem cannot actually hold.
///
/// Pass [`ValidationBounds::required_free_bytes`], which is the MAXIMUM of the
/// cumulative budget and one file's peak reserve. Checking the budget alone was
/// wrong in the direction that matters once the size ceiling became
/// configurable: with a 64 GiB `max_input_bytes` a single encode reserves
/// 128 GiB, which a 6 GiB budget check would happily admit onto a filesystem
/// that cannot hold it. Raised by opus and free at the FOUNDRY-09 gate.
pub fn check_free_space(dir: &Path, needed: u64) -> Result<(), ValidationRefusal> {
    let Some(available) = free_bytes_for(dir) else {
        // Unreadable is not free. Refuse and say so.
        return Err(ValidationRefusal::NotEnoughFreeSpace {
            available_bytes: 0,
            needed_bytes: needed,
        });
    };
    if available < needed {
        return Err(ValidationRefusal::NotEnoughFreeSpace {
            available_bytes: available,
            needed_bytes: needed,
        });
    }
    Ok(())
}

/// Probe up to `probe_budget` candidates, spread across the whole list.
///
/// Returns the probes that succeeded (with their paths) and a count of the
/// ones that did not. The probe failures are counted rather than dropped: a run
/// where nothing could be probed must not report as a clean, empty validation.
/// Why a probe walk stopped early, if it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStop {
    /// Enough matches were collected. The normal, complete outcome.
    BudgetFilled,
    /// The probe phase ran out of wall clock. The sample is SHORTER than
    /// requested, which is a materially different fact and must not be
    /// reported as a complete one.
    DeadlineReached,
}

/// Should the probe walk stop? Pure, so the composite rule is testable —
/// `probe_candidates` needs a live `Foundry`, and when the deadline check was
/// inline its mutation survived every test.
///
/// The deadline exists because a TARGETED walk visits every candidate: with a
/// 120s probe timeout, ~25,000 entries is a worst case of hundreds of hours,
/// and the run deadline was only consulted in the encode loop. A targeted run
/// could therefore spend longer than its entire deadline before a single
/// encode began.
pub fn probe_stop_reason(
    probed: usize,
    budget: usize,
    elapsed: Duration,
    deadline: Duration,
) -> Option<ProbeStop> {
    if probed >= budget {
        return Some(ProbeStop::BudgetFilled);
    }
    if elapsed >= deadline {
        return Some(ProbeStop::DeadlineReached);
    }
    None
}

/// Which candidate indices to probe, and in what order.
///
/// Named and separate so the CHOICE is testable — `probe_candidates` needs a
/// live `Foundry`, and when this was inline a mutation reverting a targeted run
/// to stride sampling survived every test.
///
/// - **Unrestricted:** an even stride, so the sample spans the whole library.
/// - **Targeted:** EVERY candidate, in order. A stride over ~25,000 entries
///   looking for the ~1% that is 4K/HDR would find almost none of it, which is
///   the failure the filter exists to fix. The probe budget then bounds how
///   many MATCHES are collected, not how many files are looked at.
pub fn probe_order(len: usize, probe_budget: usize, filter: &CandidateFilter) -> Vec<usize> {
    if filter.is_unrestricted() {
        stride_sample(len, probe_budget)
    } else {
        (0..len).collect()
    }
}

pub fn probe_candidates(
    foundry: &Foundry,
    candidates: &[PathBuf],
    probe_budget: usize,
    filter: &CandidateFilter,
    // Wall-clock ceiling for the PROBE phase. A targeted walk visits every
    // candidate, and with a 120s probe timeout ~25,000 entries is a worst case
    // of hundreds of hours — the run deadline was only consulted in the encode
    // loop, so a targeted run could spend longer than its whole deadline
    // before a single encode began. Raised by opus and free at the FOUNDRY-16
    // gate.
    probe_deadline: Duration,
) -> (Vec<(PathBuf, MediaProbe)>, usize) {
    let started = Instant::now();
    let mut probed = Vec::new();
    let mut failures = 0;
    // A restricted run walks the WHOLE candidate list rather than a stride
    // sample. A stride over 25,000 entries looking for the ~1% that is 4K/HDR
    // would find almost none of it, which is the failure this filter exists to
    // fix — so the probe budget bounds how many MATCHES are collected, not how
    // many files are looked at.
    let order: Vec<usize> = probe_order(candidates.len(), probe_budget, filter);
    for i in order {
        if let Some(stop) = probe_stop_reason(
            probed.len(),
            probe_budget,
            started.elapsed(),
            probe_deadline,
        ) {
            if stop == ProbeStop::DeadlineReached {
                tracing::warn!(
                    probed = probed.len(),
                    "foundry/validate: probe phase hit its deadline; the sample is \
                     SHORTER than requested and the run reports what it actually examined"
                );
            }
            break;
        }
        let path = &candidates[i];
        match foundry.probe_file(path) {
            Ok(p) => {
                if filter.admits(&p) {
                    probed.push((path.clone(), p));
                }
            }
            Err(e) => {
                failures += 1;
                tracing::debug!(path = %path.display(), error = %e, "foundry/validate: probe failed");
            }
        }
    }
    (probed, failures)
}

/// Which files a run is allowed to consider, before diversity sampling.
///
/// The diversity sampler picks for SHAPE COVERAGE, which is right for "does the
/// encoder handle the range of things in this library" and exactly wrong for
/// "does it handle the dangerous 1%". 4K/HDR/Dolby Vision is roughly 1% of this
/// library, so a diverse sample of 16 files will essentially never contain one
/// — raising the size ceiling made that content ELIGIBLE without making it
/// REACHABLE.
///
/// That matters because the 1% is precisely the content whose misclassification
/// is unrecoverable: Path A deletes the original.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CandidateFilter {
    /// Only consider files at least this large. Targets the large tail, which
    /// is where 4K lives.
    pub min_input_bytes: Option<u64>,
    /// Only consider files whose video carries an HDR transfer or a Dolby
    /// Vision signal.
    pub hdr_only: bool,
}

impl CandidateFilter {
    pub fn is_unrestricted(&self) -> bool {
        self.min_input_bytes.is_none() && !self.hdr_only
    }

    /// Whether a probed file passes. Pure, so the targeting rule is testable
    /// without a filesystem.
    pub fn admits(&self, probe: &MediaProbe) -> bool {
        if let Some(min) = self.min_input_bytes {
            match probe.size_bytes {
                Some(sz) if sz >= min => {}
                // Unknown size cannot be shown to meet the floor, so it does
                // not — the filter exists to GUARANTEE the risky content is
                // reached, and admitting unknowns would dilute that.
                _ => return false,
            }
        }
        if self.hdr_only {
            let Some(v) = probe.primary_video() else {
                return false;
            };
            let hdr = matches!(
                crate::foundry::hdr::classify_hdr(v),
                crate::foundry::hdr::HdrVerdict::Hdr { .. }
            );
            let dv = crate::foundry::hdr::classify_dolby_vision(v).is_present();
            // `Unknown` dynamic range is deliberately INCLUDED: an untagged
            // 10-bit file is exactly the ambiguous case worth validating, and
            // excluding it would hide the files most likely to be misjudged.
            let unknown = matches!(
                crate::foundry::hdr::classify_hdr(v),
                crate::foundry::hdr::HdrVerdict::Unknown { .. }
            );
            if !(hdr || dv || unknown) {
                return false;
            }
        }
        true
    }
}

/// Encode and verify a diverse sample. **Never writes outside `scratch_dir`,
/// never calls [`crate::foundry::forge::optimize_file`], never renames,
/// replaces or deletes anything in the library.**
pub fn validate_sample(
    foundry: &Foundry,
    policy: &TranscodePolicy,
    scratch_dir: &Path,
    probed: &[(PathBuf, MediaProbe)],
    source_probe_failures: usize,
    limit: usize,
    bounds: &ValidationBounds,
) -> ValidationRun {
    let cfg = foundry.config();
    let probes: Vec<MediaProbe> = probed.iter().map(|(_, p)| p.clone()).collect();

    let mut run = ValidationRun {
        source_probe_failures,
        candidates_probed: probed.len() + source_probe_failures,
        distinct_shapes: {
            let mut keys: Vec<DiversityKey> = Vec::new();
            for p in &probes {
                let k = diversity_key(p);
                if !keys.contains(&k) {
                    keys.push(k);
                }
            }
            keys.len()
        },
        ..Default::default()
    };

    let selected = select_diverse_sample(&probes, policy, limit);
    let started = Instant::now();

    for idx in selected {
        let (path, probe) = &probed[idx];

        if started.elapsed() >= bounds.run_deadline {
            run.deadline_hit = true;
            let mut file = describe_input(path, probe);
            file.outcome = ValidationOutcome::Skipped {
                reason: ValidateSkip::RunDeadlineReached,
            };
            run.record(file);
            continue;
        }

        // Re-resolve through the guard for the argv. The probe already went
        // through it, but the *canonical* path is what must appear after `-i`,
        // and it is what the confinement check is asserted against.
        let resolved = match foundry.guard().resolve(path) {
            Ok(r) => r.into_path_buf(),
            Err(e) => {
                let mut file = describe_input(path, probe);
                file.outcome = ValidationOutcome::Failed {
                    failure: ValidationFailure::ScratchUnusable {
                        detail: format!("the source path was refused by the guard: {e}"),
                    },
                };
                run.record(file);
                continue;
            }
        };

        let (file, reserved) = validate_one(
            cfg,
            policy,
            scratch_dir,
            path,
            &resolved.to_string_lossy(),
            probe,
            bounds,
            run.scratch_bytes_reserved,
            bounds.run_deadline.checked_sub(started.elapsed()),
        );
        run.scratch_bytes_reserved += reserved;
        run.record(file);
    }

    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundry::probe::{AudioStream, SubtitleStream, VideoStream};

    // --- fixtures ----------------------------------------------------------

    fn probe(
        container: &str,
        vcodec: &str,
        w: u32,
        h: u32,
        acodecs: &[&str],
        subs: usize,
    ) -> MediaProbe {
        MediaProbe {
            container: container.to_string(),
            duration_secs: Some(1200.0),
            format_bitrate_bps: Some(3_000_000),
            size_bytes: Some(500 * 1024 * 1024),
            video: vec![VideoStream {
                index: 0,
                codec: vcodec.to_string(),
                width: Some(w),
                height: Some(h),
                bitrate_bps: Some(2_500_000),
                pix_fmt: Some("yuv420p".into()),
                attached_pic: false,
                // FOUNDRY-03 added colour/DV fields. Defaulted here rather
                // than enumerated: this fixture exercises sampling and the
                // encode gate, and an SDR untagged stream is the ordinary
                // shape for that. The colour paths have their own tests.
                ..VideoStream::default()
            }],
            audio: acodecs
                .iter()
                .enumerate()
                .map(|(i, c)| AudioStream {
                    index: 1 + i as u32,
                    codec: c.to_string(),
                    channels: Some(2),
                    language: Some("eng".into()),
                    bitrate_bps: Some(192_000),
                })
                .collect(),
            subtitles: (0..subs)
                .map(|i| SubtitleStream {
                    index: 100 + i as u32,
                    codec: "subrip".into(),
                    language: Some("eng".into()),
                    forced: false,
                    default: false,
                })
                .collect(),
            attachments: Vec::new(),
            data_stream_count: 0,
            unindexed_stream_count: 0,
            chapter_count: 0,
            title: None,
            other_stream_count: 0,
        }
    }

    /// The operator's twelve measured files, as close as fixtures get.
    fn measured_library() -> Vec<MediaProbe> {
        vec![
            probe("avi", "msmpeg4v2", 320, 240, &["mp3"], 0),
            probe("avi", "msmpeg4v2", 320, 256, &["mp3"], 0),
            probe("avi", "msmpeg4v2", 294, 240, &["mp3"], 0),
            probe("avi", "mpeg4", 640, 352, &["ac3"], 0),
            probe("avi", "mpeg4", 640, 352, &["ac3"], 0),
            probe("matroska,webm", "h264", 720, 480, &["aac"], 2),
            probe("matroska,webm", "h264", 720, 480, &["aac"], 2),
            probe("mov,mp4,m4a,3gp,3g2,mj2", "h264", 640, 480, &["aac"], 0),
            probe("matroska,webm", "h264", 1280, 720, &["aac"], 2),
            probe("matroska,webm", "h264", 1920, 1080, &["aac"], 3),
            probe("matroska,webm", "h264", 1918, 802, &["eac3"], 42),
            probe("matroska,webm", "h264", 1280, 688, &["dts", "ac3"], 4),
        ]
    }

    fn policy() -> TranscodePolicy {
        TranscodePolicy::default()
    }

    // --- banding -----------------------------------------------------------

    #[test]
    fn resolution_is_banded_on_width_so_scope_releases_are_not_filed_as_720p() {
        // 1918x802 is a 1080p-class scope release. Banding on HEIGHT would put
        // it next to 1280x720 and the sampler would treat them as one shape.
        assert_eq!(
            resolution_band(Some(1918), Some(802)),
            ResolutionBand::Hd1080
        );
        assert_eq!(
            resolution_band(Some(1280), Some(720)),
            ResolutionBand::Hd720
        );
        assert_eq!(resolution_band(Some(320), Some(240)), ResolutionBand::Tiny);
        assert_eq!(resolution_band(Some(640), Some(352)), ResolutionBand::Sd);
        assert_eq!(resolution_band(Some(3840), Some(2160)), ResolutionBand::Uhd);
    }

    #[test]
    fn unmeasurable_dimensions_get_their_own_band_rather_than_a_default() {
        // Folding these into any real band would make an unprobeable file look
        // like an ordinary one and let the sampler skip it.
        assert_eq!(resolution_band(None, Some(240)), ResolutionBand::Unknown);
        assert_eq!(resolution_band(Some(320), None), ResolutionBand::Unknown);
        assert_eq!(resolution_band(Some(0), Some(0)), ResolutionBand::Unknown);
    }

    #[test]
    fn the_forty_two_subtitle_file_lands_in_its_own_band() {
        assert_eq!(subtitle_band(0), SubtitleBand::None);
        assert_eq!(subtitle_band(4), SubtitleBand::Few);
        assert_eq!(subtitle_band(5), SubtitleBand::Many);
        assert_eq!(subtitle_band(20), SubtitleBand::Many);
        assert_eq!(subtitle_band(42), SubtitleBand::Extreme);
    }

    // --- the key -----------------------------------------------------------

    #[test]
    fn audio_codecs_are_a_sorted_set_so_dts_plus_ac3_is_its_own_shape() {
        // The measured library has a `dts + ac3` file. Keying on the first
        // audio stream would make it indistinguishable from a plain `dts` file,
        // whose plan is different.
        let mixed = diversity_key(&probe("matroska", "h264", 1280, 688, &["dts", "ac3"], 4));
        let single = diversity_key(&probe("matroska", "h264", 1280, 688, &["dts"], 4));
        assert_eq!(mixed.audio_codecs, vec!["ac3", "dts"]);
        assert_ne!(mixed, single);
        // Order in the file must not matter.
        let reordered = diversity_key(&probe("matroska", "h264", 1280, 688, &["ac3", "dts"], 4));
        assert_eq!(mixed, reordered);
    }

    #[test]
    fn unaligned_dimensions_are_part_of_the_key_not_just_the_score() {
        // 1918x802 and 1920x1080 share a resolution band. They are NOT the same
        // test: unaligned dimensions are where scaling, padding and
        // display-aspect handling go wrong.
        let scope = diversity_key(&probe("matroska", "h264", 1918, 802, &["eac3"], 42));
        let flat = diversity_key(&probe("matroska", "h264", 1920, 1080, &["eac3"], 42));
        assert!(scope.unaligned_dimensions);
        assert!(!flat.unaligned_dimensions);
        assert_ne!(scope, flat);
    }

    #[test]
    fn every_standard_resolution_in_the_measured_library_reads_as_aligned() {
        // The trap a mod-16 test falls into: 1080 is not a multiple of 16, so
        // mod-16 flags EVERY standard 1080p file as unusual and the signal
        // stops discriminating. Only the two genuinely odd shapes may trip it.
        for (w, h) in [(320, 240), (320, 256), (640, 352), (720, 480), (640, 480),
                       (1280, 720), (1280, 688), (1920, 1080)] {
            let k = diversity_key(&probe("matroska", "h264", w, h, &["aac"], 0));
            assert!(!k.unaligned_dimensions, "{w}x{h} should read as aligned");
        }
        for (w, h) in [(294, 240), (1918, 802)] {
            let k = diversity_key(&probe("matroska", "h264", w, h, &["aac"], 0));
            assert!(k.unaligned_dimensions, "{w}x{h} should read as unaligned");
        }
    }

    #[test]
    fn an_unrecognized_container_is_a_distinct_shape_not_a_dropped_one() {
        let weird = diversity_key(&probe("bink,smacker", "h264", 640, 480, &["aac"], 0));
        assert_eq!(weird.container, None);
        let mp4 = diversity_key(&probe("mov,mp4,m4a,3gp,3g2,mj2", "h264", 640, 480, &["aac"], 0));
        assert_eq!(mp4.container, Some(Container::Mp4));
        assert_ne!(weird, mp4);
    }

    // --- sampling ----------------------------------------------------------

    #[test]
    fn sampling_covers_distinct_shapes_before_repeating_any_of_them() {
        // THE point of the harness. A `truncate(3)` on the walk order, or three
        // random picks, would take three msmpeg4v2 avi files here and prove
        // nothing about the other nine.
        let lib = measured_library();
        let picked = select_diverse_sample(&lib, &policy(), 3);
        assert_eq!(picked.len(), 3);
        let keys: Vec<DiversityKey> = picked.iter().map(|&i| diversity_key(&lib[i])).collect();
        for i in 0..keys.len() {
            for j in (i + 1)..keys.len() {
                assert_ne!(keys[i], keys[j], "picked two files of the same shape: {picked:?}");
            }
        }
    }

    #[test]
    fn twelve_h264_aac_files_do_not_crowd_out_the_awkward_ones() {
        // The failure this whole module exists to avoid: the library is 16,221
        // files and the common shape is the one that needs the least proving.
        let mut lib = vec![probe("matroska,webm", "h264", 1920, 1080, &["aac"], 2); 200];
        lib.push(probe("avi", "msmpeg4v2", 294, 240, &["mp3"], 0));
        lib.push(probe("matroska,webm", "h264", 1918, 802, &["eac3"], 42));
        lib.push(probe("matroska,webm", "h264", 1280, 688, &["dts", "ac3"], 4));

        let picked = select_diverse_sample(&lib, &policy(), 4);
        assert!(picked.contains(&200), "the msmpeg4v2 294x240 file must be sampled");
        assert!(picked.contains(&201), "the 42-subtitle scope file must be sampled");
        assert!(picked.contains(&202), "the dts+ac3 file must be sampled");
    }

    #[test]
    fn a_limit_below_the_shape_count_spends_itself_on_the_awkward_shapes() {
        let lib = measured_library();
        // One slot. It must go to the most awkward shape available, which is an
        // avi msmpeg4v2 at 294x240 (unaccepted codec, unaccepted container,
        // tiny, odd dimensions, unaccepted audio) — not to an h264/aac mkv.
        let picked = select_diverse_sample(&lib, &policy(), 1);
        assert_eq!(picked.len(), 1);
        let k = diversity_key(&lib[picked[0]]);
        assert_eq!(k.video_codec, "msmpeg4v2");
        assert_eq!(k.resolution, ResolutionBand::Tiny);
        assert!(k.unaligned_dimensions, "294 is not a multiple of 8");
    }

    #[test]
    fn awkwardness_ranks_the_measured_library_the_way_the_operator_described_it() {
        let p = policy();
        let ancient = awkwardness(
            &diversity_key(&probe("avi", "msmpeg4v2", 294, 240, &["mp3"], 0)),
            &p,
        );
        let modern = awkwardness(
            &diversity_key(&probe("matroska,webm", "h264", 1920, 1080, &["aac"], 2)),
            &p,
        );
        let forty_two_subs = awkwardness(
            &diversity_key(&probe("matroska,webm", "h264", 1918, 802, &["eac3"], 42)),
            &p,
        );
        assert!(ancient > modern, "{ancient} vs {modern}");
        assert!(forty_two_subs > modern, "{forty_two_subs} vs {modern}");
        // A plain, modern, conforming file is the baseline: nothing about it is
        // awkward, so it scores zero and sorts last.
        assert_eq!(modern, 0);
    }

    #[test]
    fn each_awkwardness_axis_on_its_own_decides_which_shape_is_sampled() {
        // MUTATION SURVIVORS (M33, M34, M35), fixed. The earlier awkwardness
        // test compared files that differed on SEVERAL axes at once, so
        // deleting any single term left the ordering intact and the mutants
        // lived. Each pair below differs on exactly ONE axis, and the ORDINARY
        // file is listed FIRST — so if that axis stopped contributing, the tie
        // would break on first appearance and the wrong file would be sampled.
        //
        // This matters because the score is what decides which shapes fit when
        // the operator asks for twelve files out of a 16,221-file library.
        let ordinary = || probe("matroska,webm", "h264", 1920, 1080, &["aac"], 2);
        let cases: Vec<(&str, MediaProbe)> = vec![
            // codec two decades old — the msmpeg4v2 files
            ("video codec", probe("matroska,webm", "msmpeg4v2", 1920, 1080, &["aac"], 2)),
            // the 42-subtitle file
            ("subtitle count", probe("matroska,webm", "h264", 1920, 1080, &["aac"], 42)),
            // dts forces a re-encode of the audio side
            ("audio codec", probe("matroska,webm", "h264", 1920, 1080, &["dts"], 2)),
            // avi is 48% of the measured library and is a container rewrite
            ("container", probe("avi", "h264", 1920, 1080, &["aac"], 2)),
            ("resolution band", probe("matroska,webm", "h264", 3840, 2160, &["aac"], 2)),
            // 1918-wide scope
            ("dimension alignment", probe("matroska,webm", "h264", 1918, 1080, &["aac"], 2)),
            ("no audio at all", probe("matroska,webm", "h264", 1920, 1080, &[], 2)),
            ("mixed audio codecs", probe("matroska,webm", "h264", 1920, 1080, &["aac", "eac3"], 2)),
        ];
        for (axis, awkward) in cases {
            let p = policy();
            assert!(
                awkwardness(&diversity_key(&awkward), &p)
                    > awkwardness(&diversity_key(&ordinary()), &p),
                "the `{axis}` axis contributes nothing to the score"
            );
            let lib = vec![ordinary(), awkward];
            assert_eq!(
                select_diverse_sample(&lib, &p, 1),
                vec![1],
                "with one slot, the `{axis}` axis failed to decide which shape is sampled"
            );
        }
    }

    #[test]
    fn a_shape_that_can_never_be_encoded_does_not_outbid_one_that_can() {
        // MUTATION SURVIVOR (M47), and chasing it found a real bug rather than
        // a missing assertion. An unrecognized container is refused by
        // `plan_transcode`, so such a file can only ever be a skip. Scoring it
        // as the most awkward shape would burn one of twelve slots producing no
        // evidence at all, while a msmpeg4v2 avi — which really encodes, and is
        // the shape the operator most wants proven — waited behind it.
        let lib = vec![
            probe("bink,smacker", "msmpeg4v2", 294, 240, &["mp3"], 0),
            probe("avi", "msmpeg4v2", 294, 240, &["mp3"], 0),
        ];
        let p = policy();
        assert_eq!(diversity_key(&lib[0]).container, None, "precondition");
        assert!(
            awkwardness(&diversity_key(&lib[1]), &p)
                > awkwardness(&diversity_key(&lib[0]), &p),
            "an encodable shape must outrank one that is guaranteed to be skipped"
        );
        assert_eq!(select_diverse_sample(&lib, &p, 1), vec![1]);
    }

    #[test]
    fn a_limit_above_the_shape_count_goes_round_again_rather_than_stopping_short() {
        // A second file of an awkward shape is evidence the first was not a
        // fluke, so the extra budget is spent rather than returned.
        let lib = vec![
            probe("avi", "msmpeg4v2", 320, 240, &["mp3"], 0),
            probe("avi", "msmpeg4v2", 320, 240, &["mp3"], 0),
            probe("matroska,webm", "h264", 1920, 1080, &["aac"], 2),
        ];
        let picked = select_diverse_sample(&lib, &policy(), 3);
        assert_eq!(picked.len(), 3);
        let mut sorted = picked.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2]);
    }

    #[test]
    fn sampling_never_returns_more_than_the_limit_or_a_duplicate_index() {
        let lib = measured_library();
        for limit in [0usize, 1, 5, 12, 40] {
            let picked = select_diverse_sample(&lib, &policy(), limit);
            assert!(picked.len() <= limit, "limit {limit} produced {picked:?}");
            assert!(picked.len() <= lib.len());
            let mut seen = picked.clone();
            seen.sort();
            seen.dedup();
            assert_eq!(seen.len(), picked.len(), "duplicate index at limit {limit}");
        }
    }

    #[test]
    fn sampling_is_deterministic_so_two_runs_are_comparable() {
        let lib = measured_library();
        assert_eq!(
            select_diverse_sample(&lib, &policy(), 6),
            select_diverse_sample(&lib, &policy(), 6)
        );
    }

    #[test]
    fn stride_sampling_spans_the_whole_walk_rather_than_its_first_entries() {
        // A media library walks in path order, so the first 400 entries are a
        // few shows in their entirety — one codec, one release group.
        let picks = stride_sample(16_221, 400);
        assert_eq!(picks.len(), 400);
        assert_eq!(picks[0], 0);
        assert!(
            *picks.last().unwrap() > 16_000,
            "the last pick was {:?}, so the tail of the library was never seen",
            picks.last()
        );
        assert!(picks.windows(2).all(|w| w[0] < w[1]), "picks must be ascending");
    }

    #[test]
    fn stride_sampling_degenerates_safely() {
        assert!(stride_sample(0, 10).is_empty());
        assert!(stride_sample(10, 0).is_empty());
        assert_eq!(stride_sample(3, 10), vec![0, 1, 2]);
        assert_eq!(stride_sample(3, 3), vec![0, 1, 2]);
    }

    // --- the argv confinement rail ----------------------------------------

    fn scratch_paths() -> (PathBuf, PathBuf) {
        let dir = PathBuf::from("/var/tmp/muse-foundry/validate");
        let out = dir.join("muse-foundry-validate-abc.mkv");
        (dir, out)
    }

    fn real_argv(input: &str, output: &Path) -> Vec<String> {
        let p = policy();
        let pr = probe("avi", "msmpeg4v2", 320, 240, &["mp3"], 0);
        match plan_transcode(&pr, &p, input, &output.to_string_lossy()) {
            TranscodeDecision::Transcode { args, .. } => args,
            other => panic!("expected a transcode plan, got {other:?}"),
        }
    }

    #[test]
    fn overrides_are_clamped_and_absent_ones_keep_their_default() {
        const MIB: u64 = 1024 * 1024;
        let d = ValidationBounds::default();

        // Absent fields must not be disturbed: raising only the size ceiling
        // must not silently also change the timeouts.
        let only_size = ValidationBounds::from_overrides(Some(8192), None, None, None);
        assert_eq!(only_size.max_input_bytes, 8192 * MIB);
        assert_eq!(only_size.max_total_output_bytes, d.max_total_output_bytes);
        assert_eq!(only_size.per_encode_timeout, d.per_encode_timeout);
        assert_eq!(only_size.run_deadline, d.run_deadline);

        // Absurd values clamp rather than being honoured.
        let huge = ValidationBounds::from_overrides(
            Some(u64::MAX),
            Some(u64::MAX),
            Some(u64::MAX),
            Some(u64::MAX),
        );
        assert_eq!(huge.max_input_bytes, 65_536 * MIB, "input ceiling clamps to 64 GiB");
        assert_eq!(huge.max_total_output_bytes, 4_194_304 * MIB, "budget clamps to 4 TiB");
        assert_eq!(huge.per_encode_timeout, Duration::from_secs(21_600));
        assert_eq!(huge.run_deadline, Duration::from_secs(86_400));

        // ...and zero clamps up, so a caller cannot disable a bound.
        let zero = ValidationBounds::from_overrides(Some(0), Some(0), Some(0), Some(0));
        assert_eq!(zero.max_input_bytes, MIB);
        assert_eq!(zero.per_encode_timeout, Duration::from_secs(60));
        assert_eq!(zero.run_deadline, Duration::from_secs(60));
    }

    /// The coverage sentence must describe THIS run.
    ///
    /// It used to be a fixed string naming 2 GiB. Once the ceiling became an
    /// override, that string would have kept claiming 2 GiB whatever the run
    /// actually used — a report that misstates its own coverage reads as
    /// verified when it is not, which is worse than omitting the note.
    #[test]
    fn the_coverage_note_states_the_ceiling_actually_in_force() {
        const MIB: u64 = 1024 * 1024;
        for mb in [2048u64, 32768, 1, 2050, 65_536] {
            let b = ValidationBounds::from_overrides(Some(mb), None, None, None);
            let note = b.coverage_note();
            // The EXACT byte count, so the note cannot round away the real
            // ceiling. Codex caught the first version at the gate: it printed
            // one decimal GiB, so a 1 MiB ceiling read as "0.0 GiB" and 2050
            // MiB read as "2.0 GiB" — misstating the very bound the note
            // exists to disclose. The round-number-only fixtures missed it,
            // which is why 1 and 2050 are in this list.
            assert!(
                note.contains(&format!("{} bytes", mb * MIB)),
                "note must state the exact ceiling for {mb} MiB: {note}"
            );
            // And it must keep naming what the exclusion COSTS, not just a number.
            assert!(note.to_lowercase().contains("4k"), "{note}");
        }

        // Two ceilings that round to the same GiB string must still produce
        // DIFFERENT notes — the property the rounding bug violated.
        let a = ValidationBounds::from_overrides(Some(2048), None, None, None).coverage_note();
        let b = ValidationBounds::from_overrides(Some(2050), None, None, None).coverage_note();
        assert_ne!(a, b, "2048 and 2050 MiB must not produce the same coverage claim");
    }

    /// Peak, not just cumulative. Raised by opus and free at the gate: with a
    /// 64 GiB size ceiling one encode reserves 128 GiB, which a 6 GiB budget
    /// check would admit onto a filesystem that cannot hold it.
    #[test]
    fn the_required_free_space_covers_one_files_peak_not_only_the_budget() {
        const MIB: u64 = 1024 * 1024;
        // Large ceiling, small budget: the PEAK dominates.
        let big = ValidationBounds::from_overrides(Some(65_536), Some(6144), None, None);
        assert_eq!(
            big.required_free_bytes(),
            65_536 * MIB * big.output_reserve_factor,
            "one file's reserve must dominate when the ceiling is large"
        );
        assert!(big.required_free_bytes() > big.max_total_output_bytes);

        // Small ceiling, large budget: the CUMULATIVE budget dominates.
        let many = ValidationBounds::from_overrides(Some(512), Some(102_400), None, None);
        assert_eq!(many.required_free_bytes(), many.max_total_output_bytes);
    }

    /// One encode must not outlive the run it belongs to. The two clamps are
    /// independent, so a 60s deadline with a 6h encode ceiling was accepted
    /// and the deadline was only consulted between files.
    #[test]
    fn an_encodes_ceiling_is_reduced_to_the_time_the_run_has_left() {
        let b = ValidationBounds::from_overrides(None, None, Some(21_600), Some(60));
        assert_eq!(b.per_encode_timeout, Duration::from_secs(21_600));
        assert_eq!(b.run_deadline, Duration::from_secs(60));
        // The effective ceiling is the smaller of the two.
        let remaining = Duration::from_secs(5);
        assert_eq!(
            b.per_encode_timeout.min(remaining),
            remaining,
            "an encode may not run past the run's own deadline"
        );
    }

    fn probe_with(size: Option<u64>, transfer: Option<&str>, pix: &str) -> MediaProbe {
        // Built on the module's existing fixture so it stays in step with the
        // struct rather than duplicating every field.
        let mut p = probe("matroska,webm", "hevc", 3840, 2160, &["aac"], 0);
        p.size_bytes = size;
        if let Some(v) = p.video.first_mut() {
            v.pix_fmt = Some(pix.to_string());
            v.color_transfer = transfer.map(str::to_string);
        }
        p
    }

    /// The gap this filter closes.
    ///
    /// The diversity sampler picks for SHAPE coverage, so a 16-file sample of a
    /// library that is ~1% 4K/HDR essentially never contains one. Raising
    /// `max_input_mb` (FOUNDRY-09) made that content ELIGIBLE without making it
    /// REACHABLE — and it is precisely the content whose misclassification is
    /// unrecoverable, because Path A deletes the original.
    #[test]
    fn the_size_floor_admits_the_large_tail_and_excludes_the_rest() {
        let f = CandidateFilter {
            min_input_bytes: Some(8 * 1024 * 1024 * 1024),
            hdr_only: false,
        };
        assert!(f.admits(&probe_with(Some(20 * 1024 * 1024 * 1024), None, "yuv420p")));
        assert!(!f.admits(&probe_with(Some(700 * 1024 * 1024), None, "yuv420p")));
        // An UNKNOWN size cannot be shown to meet the floor, so it does not.
        // The filter exists to GUARANTEE the risky content is reached;
        // admitting unknowns would dilute exactly that.
        assert!(!f.admits(&probe_with(None, None, "yuv420p")));
    }

    #[test]
    fn hdr_only_admits_hdr_and_dolby_vision() {
        let f = CandidateFilter { min_input_bytes: None, hdr_only: true };
        // PQ HDR10.
        assert!(f.admits(&probe_with(Some(1), Some("smpte2084"), "yuv420p10le")));
        // HLG.
        assert!(f.admits(&probe_with(Some(1), Some("arib-std-b67"), "yuv420p10le")));
        // Ordinary SDR is excluded — that is the point of the filter.
        assert!(!f.admits(&probe_with(Some(1), Some("bt709"), "yuv420p")));

        // DOLBY VISION, which the test's own name claimed and did not check.
        // Opus caught it at the FOUNDRY-16 gate: the assertions above are all
        // transfer-based, so DV admission could break entirely and this test
        // would stay green. A DV file whose TRANSFER is ordinary bt709 is the
        // case that isolates it — admitted only via the DOVI signal.
        let mut dv = probe_with(Some(1), Some("bt709"), "yuv420p");
        dv.video[0].side_data = vec![crate::foundry::probe::StreamSideData {
            kind: "DOVI configuration record".into(),
            dv_profile: Some(5),
            dv_bl_signal_compatibility_id: Some(0),
            rpu_present: Some(true),
            bl_present: Some(true),
            el_present: Some(false),
        }];
        assert!(
            crate::foundry::hdr::classify_dolby_vision(dv.primary_video().unwrap())
                .is_present(),
            "fixture must actually carry a DV signal"
        );
        assert!(
            f.admits(&dv),
            "a Dolby Vision file must be admitted even when its transfer looks SDR — \
             profile 5 is the single most dangerous input in the library"
        );
    }

    /// UNDETERMINED dynamic range is INCLUDED, deliberately.
    ///
    /// An untagged 10-bit file is the ambiguous case most likely to be
    /// misjudged, so excluding it would hide exactly the files worth looking
    /// at — the filter would then validate only the content the classifier
    /// already finds easy.
    #[test]
    fn an_undetermined_dynamic_range_is_included_not_filtered_out() {
        let f = CandidateFilter { min_input_bytes: None, hdr_only: true };
        let untagged_10bit = probe_with(Some(1), None, "yuv420p10le");
        assert_eq!(
            crate::foundry::hdr::classify_hdr(untagged_10bit.primary_video().unwrap()),
            crate::foundry::hdr::HdrVerdict::Unknown {
                why: crate::foundry::hdr::DynamicRangeUnknown::NoTransferTagAndUnknownBitDepth {
                    pix_fmt: Some("yuv420p10le".to_string())
                }
            },
            "fixture must actually be the undetermined case"
        );
        assert!(
            f.admits(&untagged_10bit),
            "the ambiguous case is the one most worth validating"
        );
        // ...while untagged 8-bit is confidently SDR and is excluded.
        assert!(!f.admits(&probe_with(Some(1), None, "yuv420p")));
    }

    /// The probe walk must stop on EITHER limit, and the two are different
    /// facts.
    ///
    /// A targeted walk visits every candidate; at a 120s probe timeout,
    /// ~25,000 entries is hundreds of hours, and the run deadline was only
    /// consulted in the ENCODE loop — so a targeted run could burn its whole
    /// deadline before a single encode began. Raised by opus and free.
    #[test]
    fn the_probe_walk_stops_on_the_budget_or_the_deadline() {
        let short = Duration::from_secs(10);
        let long = Duration::from_secs(10_000);

        // Neither limit reached: keep going.
        assert_eq!(probe_stop_reason(5, 250, Duration::from_secs(1), long), None);

        // Budget filled — the normal, complete outcome.
        assert_eq!(
            probe_stop_reason(250, 250, Duration::from_secs(1), long),
            Some(ProbeStop::BudgetFilled)
        );

        // Deadline reached with the budget UNFILLED: a short sample, and a
        // materially different fact from a complete one.
        assert_eq!(
            probe_stop_reason(3, 250, Duration::from_secs(11), short),
            Some(ProbeStop::DeadlineReached)
        );

        // The budget takes precedence when both are hit, so a completed walk
        // is never reported as truncated.
        assert_eq!(
            probe_stop_reason(250, 250, Duration::from_secs(11), short),
            Some(ProbeStop::BudgetFilled)
        );
    }

    /// A targeted run must walk EVERYTHING, not a stride.
    ///
    /// A stride of 250 over ~25,000 entries samples 1%, and the 4K/HDR tail is
    /// itself ~1% — so a strided targeted run would find essentially none of
    /// the content it was asked to target, while still reporting "all
    /// verified". The budget must bound MATCHES, not files examined.
    #[test]
    fn a_targeted_run_examines_every_candidate_rather_than_a_stride() {
        let targeted = CandidateFilter {
            min_input_bytes: Some(8 * 1024 * 1024 * 1024),
            hdr_only: false,
        };
        let order = probe_order(25_000, 250, &targeted);
        assert_eq!(
            order.len(),
            25_000,
            "a targeted run must look at every candidate; the budget bounds MATCHES"
        );
        assert_eq!(order.first(), Some(&0));
        assert_eq!(order.last(), Some(&24_999));

        // Unrestricted keeps the stride, which is right for shape coverage.
        let open = CandidateFilter::default();
        let strided = probe_order(25_000, 250, &open);
        assert_eq!(strided.len(), 250, "an unrestricted run samples");
        assert!(
            strided.last().unwrap() > &20_000,
            "the stride must still span the library: {:?}",
            strided.last()
        );
    }

    #[test]
    fn an_unrestricted_filter_admits_everything_and_says_so() {
        let f = CandidateFilter::default();
        assert!(f.is_unrestricted());
        assert!(f.admits(&probe_with(None, None, "yuv420p")));
        assert!(f.admits(&probe_with(Some(1), Some("bt709"), "yuv420p")));
    }

    #[test]
    fn free_space_is_read_from_the_real_filesystem() {
        // A sanity check that the df parse works at all on this host — a
        // silently-failing parse would make every run refuse, which is safe
        // but useless, or (if it defaulted the other way) unsafe.
        let got = free_bytes_for(Path::new("."));
        assert!(got.is_some(), "df --output=avail must be parseable here");
        assert!(got.unwrap() > 0, "the working filesystem reports no free space");
    }

    #[test]
    fn a_budget_larger_than_the_disk_is_refused_before_anything_is_encoded() {
        let huge = u64::MAX / 2;
        let got = check_free_space(Path::new("."), huge);
        assert!(
            matches!(got, Err(ValidationRefusal::NotEnoughFreeSpace { .. })),
            "got {got:?}"
        );
    }

    #[test]
    fn an_unreadable_filesystem_refuses_rather_than_assuming_room() {
        // "Could not determine" is not "plenty". A path df cannot stat must
        // refuse, or the check is worse than nothing: it would pass exactly in
        // the situation where the disk state is unknown.
        let got = check_free_space(Path::new("/nonexistent-muse-validate-probe"), 1024);
        assert!(
            matches!(
                got,
                Err(ValidationRefusal::NotEnoughFreeSpace {
                    available_bytes: 0,
                    ..
                })
            ),
            "got {got:?}"
        );
    }

    #[test]
    fn a_budget_that_fits_is_allowed() {
        // The off state: without this the two tests above would pass against a
        // function that refused unconditionally.
        assert!(check_free_space(Path::new("."), 1).is_ok());
    }

    /// Codex, FOUNDRY-04 gate: checking the output operand and `-i` proves
    /// nothing about an option that writes on its own account.
    /// `-passlogfile` and `-progress` both take a path and both write to it.
    #[test]
    fn an_option_that_writes_on_its_own_account_is_refused() {
        let scratch = Path::new("/scratch/v/out.mkv");
        let dir = Path::new("/scratch/v");
        for bad in [
            vec!["-i", "/lib/a.mkv", "-passlogfile", "/lib/a", "/scratch/v/out.mkv"],
            vec!["-i", "/lib/a.mkv", "-progress", "/lib/progress.txt", "/scratch/v/out.mkv"],
            vec!["-i", "/lib/a.mkv", "-report", "/scratch/v/out.mkv"],
        ] {
            let argv: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            let got = argv_writes_only_to(&argv, "/lib/a.mkv", scratch, dir);
            assert!(
                matches!(got, Err(ArgvRefusal::OptionNotKnownReadOnly { .. })),
                "{bad:?} must be refused, got {got:?}"
            );
        }
    }

    /// The allowlist must not be so broad it accepts anything, nor so narrow
    /// it rejects the argv the encoder actually produces (covered by the test
    /// below). A value that merely looks like a flag — a negative number — is
    /// a value, not an option.
    #[test]
    fn a_negative_number_is_a_value_not_an_unknown_option() {
        let scratch = Path::new("/scratch/v/out.mkv");
        let dir = Path::new("/scratch/v");
        let argv: Vec<String> = ["-i", "/lib/a.mkv", "-crf", "-1", "/scratch/v/out.mkv"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(argv_writes_only_to(&argv, "/lib/a.mkv", scratch, dir).is_ok());
    }

    #[test]
    fn the_real_argv_for_a_real_plan_passes_the_confinement_check() {
        let (dir, out) = scratch_paths();
        let input = "/srv/media/Show/ep.avi";
        let args = real_argv(input, &out);
        assert_eq!(argv_writes_only_to(&args, input, &out, &dir), Ok(()));
    }

    #[test]
    fn an_argv_whose_output_operand_is_not_the_scratch_file_is_refused() {
        // The catastrophic case: ffmpeg writes to its LAST operand. If that
        // were ever the library path, the original would be encoded over.
        let (dir, out) = scratch_paths();
        let input = "/srv/media/Show/ep.avi";
        let mut args = real_argv(input, &out);
        *args.last_mut().unwrap() = "/srv/media/Show/ep.avi".to_string();
        assert!(matches!(
            argv_writes_only_to(&args, input, &out, &dir),
            Err(ArgvRefusal::OutputIsNotTheScratchFile { .. })
        ));
    }

    #[test]
    fn an_output_that_escapes_the_scratch_dir_is_refused() {
        let dir = PathBuf::from("/var/tmp/muse-foundry/validate");
        let out = PathBuf::from("/srv/media/Show/sneaky.mkv");
        let input = "/srv/media/Show/ep.avi";
        let args = real_argv(input, &out);
        assert!(matches!(
            argv_writes_only_to(&args, input, &out, &dir),
            Err(ArgvRefusal::OutputEscapesScratchDir { .. })
        ));
    }

    #[test]
    fn the_scratch_dir_itself_is_not_an_acceptable_output_path() {
        // `starts_with` alone would accept the directory, and a bare prefix
        // match would accept `/var/tmp/muse-foundry/validate-elsewhere`.
        let dir = PathBuf::from("/var/tmp/muse-foundry/validate");
        let args = vec!["-i".into(), "/x".into(), dir.to_string_lossy().to_string()];
        assert!(matches!(
            argv_writes_only_to(&args, "/x", &dir, &dir),
            Err(ArgvRefusal::OutputEscapesScratchDir { .. })
        ));

        let sibling = PathBuf::from("/var/tmp/muse-foundry/validate-elsewhere/out.mkv");
        let args = vec![
            "-i".into(),
            "/x".into(),
            sibling.to_string_lossy().to_string(),
        ];
        assert!(matches!(
            argv_writes_only_to(&args, "/x", &sibling, &dir),
            Err(ArgvRefusal::OutputEscapesScratchDir { .. })
        ));
    }

    #[test]
    fn a_source_path_appearing_anywhere_but_after_dash_i_is_refused() {
        let (dir, out) = scratch_paths();
        let input = "/srv/media/Show/ep.avi";
        let mut args = real_argv(input, &out);
        // A second occurrence — e.g. smuggled in as a filter argument.
        args.insert(2, input.to_string());
        assert!(matches!(
            argv_writes_only_to(&args, input, &out, &dir),
            Err(ArgvRefusal::SourceIsNotOnlyAnInput { occurrences: 2, .. })
        ));
    }

    #[test]
    fn a_source_path_that_appears_twice_is_refused_even_when_the_first_is_the_input() {
        // MUTATION SURVIVOR (M5), fixed. The existing "appears anywhere but
        // after -i" test inserted the duplicate BEFORE the `-i`, so the
        // `preceded_by_dash_i` half of the rule caught it on its own and
        // dropping `occurrences != 1` changed nothing. A second occurrence
        // AFTER a legitimate `-i INPUT` pair is the case only the count check
        // catches — and it is the dangerous one, because a source path in a
        // second operand slot is a source path ffmpeg might write to.
        let (dir, out) = scratch_paths();
        let input = "/srv/media/Show/ep.avi";
        let mut args = real_argv(input, &out);
        let after_input = args.iter().position(|a| a == input).unwrap() + 1;
        args.insert(after_input, input.to_string());
        // Precondition: the first occurrence is still a legitimate `-i` operand,
        // so only the count rule can reject this.
        let first = args.iter().position(|a| a == input).unwrap();
        assert_eq!(args[first - 1], "-i", "precondition: the first occurrence is the input");
        assert!(matches!(
            argv_writes_only_to(&args, input, &out, &dir),
            Err(ArgvRefusal::SourceIsNotOnlyAnInput { occurrences: 2, .. })
        ));
    }

    #[test]
    fn a_source_path_not_preceded_by_dash_i_is_refused() {
        let dir = PathBuf::from("/var/tmp/muse-foundry/validate");
        let out = dir.join("o.mkv");
        // Present exactly once, but as the operand of something else.
        let args = vec![
            "-f".into(),
            "/srv/media/x.avi".into(),
            out.to_string_lossy().to_string(),
        ];
        assert!(matches!(
            argv_writes_only_to(&args, "/srv/media/x.avi", &out, &dir),
            Err(ArgvRefusal::SourceIsNotOnlyAnInput { .. })
        ));
    }

    #[test]
    fn an_empty_argv_is_refused_rather_than_passed() {
        let (dir, out) = scratch_paths();
        assert_eq!(
            argv_writes_only_to(&[], "/x", &out, &dir),
            Err(ArgvRefusal::EmptyArgv)
        );
    }

    // --- budget ------------------------------------------------------------

    #[test]
    fn an_input_above_the_per_file_ceiling_is_skipped_not_attempted() {
        // <host> root has ~13 GB free. A single 4K remux can exceed the entire
        // scratch budget, and "the harness filled the disk" is a worse outcome
        // than "this file was not validated".
        let b = ValidationBounds::default();
        let huge = b.max_input_bytes + 1;
        assert_eq!(
            budget_admits(Some(huge), 0, &b),
            Err(ValidateSkip::InputTooLarge {
                bytes: huge,
                ceiling_bytes: b.max_input_bytes
            })
        );
    }

    #[test]
    fn an_unmeasurable_input_is_refused_rather_than_admitted() {
        // Fail closed: an input whose size ffprobe never reported cannot be
        // reserved for, so admitting it would put an unbounded write on a
        // 13 GB filesystem.
        let b = ValidationBounds::default();
        assert_eq!(
            budget_admits(None, 0, &b),
            Err(ValidateSkip::UnknownInputSize)
        );
    }

    #[test]
    fn the_total_budget_stops_the_run_before_the_disk_does() {
        let b = ValidationBounds {
            max_input_bytes: 1000,
            max_total_output_bytes: 1000,
            output_reserve_factor: 2,
            ..Default::default()
        };
        // 400 bytes reserves 800 of the 1000.
        assert_eq!(budget_admits(Some(400), 0, &b), Ok(800));
        // A second such file would need 800 more against 200 remaining.
        assert_eq!(
            budget_admits(Some(400), 800, &b),
            Err(ValidateSkip::ScratchBudgetExhausted {
                would_reserve_bytes: 800,
                remaining_bytes: 200
            })
        );
    }

    #[test]
    fn the_reserve_is_larger_than_the_input_because_an_encode_can_grow_a_file() {
        // A CRF-20 x264 re-encode of a 300 kbps msmpeg4v2 320x240 — exactly the
        // kind of file this harness targets — can come out BIGGER than its
        // input. A 1:1 reserve would let that overrun the budget it was
        // admitted under.
        let b = ValidationBounds::default();
        assert!(
            b.output_reserve_factor >= 2,
            "reserve factor {} does not cover an encode that grows the file",
            b.output_reserve_factor
        );
        assert_eq!(budget_admits(Some(1_000), 0, &b), Ok(2_000));
    }

    #[test]
    fn the_default_budget_fits_the_thirteen_gigabyte_scratch_filesystem() {
        // Sized for <host>'s root, which is where rail 3 forces scratch to live.
        let b = ValidationBounds::default();
        assert!(
            b.max_total_output_bytes <= 8 * 1024 * 1024 * 1024,
            "the budget must leave headroom on a ~13 GB filesystem"
        );
        assert!(
            b.max_input_bytes.saturating_mul(b.output_reserve_factor)
                <= b.max_total_output_bytes,
            "a single admitted file must never be able to exceed the whole budget"
        );
    }

    // --- container check ---------------------------------------------------

    #[test]
    fn the_output_container_must_be_the_one_the_plan_asked_for() {
        assert_eq!(
            check_output_container(Container::Matroska, "matroska,webm"),
            Ok(())
        );
        assert!(matches!(
            check_output_container(Container::Matroska, "avi"),
            Err(ValidationFailure::ContainerMismatch { .. })
        ));
    }

    #[test]
    fn an_unrecognizable_output_container_is_a_mismatch_not_a_pass() {
        // Fail closed: forge's `verify_output` never looks at the container, so
        // if this shrugged, an output written in a format nothing can play
        // would be reported Verified.
        let r = check_output_container(Container::Matroska, "bink,smacker");
        match r {
            Err(ValidationFailure::ContainerMismatch { found, .. }) => {
                assert!(found.contains("unrecognized"), "{found}")
            }
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    // --- the gate between an encode and a "Verified" ----------------------

    /// A source probe, its plan, and the expectation forge derives from them.
    fn expectation_of(src: &MediaProbe) -> (VerifyExpectation, Container) {
        let p = policy();
        let plan = match plan_transcode(src, &p, "/srv/media/in.avi", "/tmp/out.mkv") {
            TranscodeDecision::Transcode { plan, .. } => plan,
            other => panic!("expected a transcode plan, got {other:?}"),
        };
        let e = expectation_for(src, &plan, &p).expect("the fixture has a duration");
        (e, plan.container)
    }

    /// What a correct encode of `src` would look like coming back out.
    fn good_output_for(src: &MediaProbe) -> MediaProbe {
        let mut out = src.clone();
        out.container = "matroska,webm".to_string();
        out.video[0].codec = "h264".to_string();
        for a in &mut out.audio {
            a.codec = "aac".to_string();
        }
        out.size_bytes = Some(200 * 1024 * 1024);
        out
    }

    #[test]
    fn a_correct_output_is_the_only_thing_that_reads_as_verified() {
        let src = probe("avi", "msmpeg4v2", 320, 240, &["mp3"], 0);
        let (e, container) = expectation_of(&src);
        let out = good_output_for(&src);
        assert_eq!(
            judge_output(&e, container, Some(&out)),
            ValidationOutcome::Verified
        );
    }

    #[test]
    fn an_output_that_cannot_be_re_probed_is_a_failure_never_a_pass() {
        // Fail closed. ffmpeg exiting zero is not evidence: an output we cannot
        // read is an output nothing about which has been checked.
        let src = probe("avi", "msmpeg4v2", 320, 240, &["mp3"], 0);
        let (e, container) = expectation_of(&src);
        match judge_output(&e, container, None) {
            ValidationOutcome::Failed {
                failure: ValidationFailure::OutputUnprobeable { .. },
            } => {}
            other => panic!("expected OutputUnprobeable, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_output_is_caught_by_forges_own_verification() {
        // The single most important thing this harness can find: Path A deletes
        // originals, and a truncated encode is the classic silent loss.
        let src = probe("avi", "msmpeg4v2", 320, 240, &["mp3"], 0);
        let (e, container) = expectation_of(&src);
        let mut out = good_output_for(&src);
        out.duration_secs = Some(600.0); // half of the 1200s source
        match judge_output(&e, container, Some(&out)) {
            ValidationOutcome::Failed {
                failure: ValidationFailure::Verification {
                    failure: VerifyFailure::DurationMismatch { .. },
                },
            } => {}
            other => panic!("expected a DurationMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_dropped_subtitle_track_is_caught() {
        // The 42-subtitle file is in the sample precisely because this is where
        // a mapping bug shows up.
        let src = probe("matroska,webm", "h264", 1918, 802, &["dts"], 42);
        let (e, container) = expectation_of(&src);
        let mut out = good_output_for(&src);
        out.subtitles.truncate(41);
        match judge_output(&e, container, Some(&out)) {
            ValidationOutcome::Failed {
                failure: ValidationFailure::Verification {
                    failure: VerifyFailure::SubtitleStreamCountMismatch { expected: 42, found: 41 },
                },
            } => {}
            other => panic!("expected a subtitle count mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_wrong_output_container_is_caught_even_though_forge_never_looks_at_it() {
        // `verify_output` compares streams, not the container. Without this
        // check, an output written in a format nothing can play would pass.
        let src = probe("avi", "msmpeg4v2", 320, 240, &["mp3"], 0);
        let (e, container) = expectation_of(&src);
        assert_eq!(container, Container::Matroska);
        let mut out = good_output_for(&src);
        out.container = "avi".to_string();
        match judge_output(&e, container, Some(&out)) {
            ValidationOutcome::Failed {
                failure: ValidationFailure::ContainerMismatch { .. },
            } => {}
            other => panic!("expected a ContainerMismatch, got {other:?}"),
        }
    }

    #[test]
    fn an_encode_that_timed_out_is_reported_as_a_failure() {
        // Stated by the brief. An encode we killed is one we cannot vouch for,
        // so it must not shrink the denominator as a skip.
        match encode_failure(EncodeRun::TimedOut {
            after: Duration::from_secs(1200),
        }) {
            Some(ValidationFailure::Timeout { after_secs: 1200 }) => {}
            other => panic!("expected a Timeout failure, got {other:?}"),
        }
    }

    #[test]
    fn a_nonzero_exit_and_a_failed_spawn_are_both_failures() {
        assert!(matches!(
            encode_failure(EncodeRun::NonZero { detail: "x".into() }),
            Some(ValidationFailure::Encode { .. })
        ));
        assert!(matches!(
            encode_failure(EncodeRun::SpawnFailed { detail: "x".into() }),
            Some(ValidationFailure::Encode { .. })
        ));
        assert_eq!(encode_failure(EncodeRun::Ok), None);
    }

    // --- the aggregate -----------------------------------------------------

    fn run_of(outcomes: Vec<ValidationOutcome>) -> ValidationRun {
        let mut r = ValidationRun {
            candidates_probed: outcomes.len(),
            ..Default::default()
        };
        for (i, o) in outcomes.into_iter().enumerate() {
            let mut f = describe_input(
                Path::new(&format!("/srv/media/f{i}.mkv")),
                &probe("matroska", "h264", 1920, 1080, &["aac"], 0),
            );
            f.outcome = o;
            r.record(f);
        }
        r
    }

    fn failed() -> ValidationOutcome {
        ValidationOutcome::Failed {
            failure: ValidationFailure::Verification {
                failure: VerifyFailure::UnknownDuration,
            },
        }
    }

    #[test]
    fn one_failure_in_twelve_is_the_verdict_not_a_footnote() {
        // The whole point of the aggregate. Eleven good files must not be able
        // to average away the one that came out wrong — the operator is
        // deciding whether to let this delete originals.
        let mut outcomes = vec![ValidationOutcome::Verified; 11];
        outcomes.push(failed());
        let r = run_of(outcomes);
        assert_eq!(r.verified, 11);
        assert_eq!(
            r.verdict(),
            ValidationVerdict::Failures {
                failed: 1,
                verified: 11
            }
        );
    }

    #[test]
    fn a_failure_outranks_every_other_verdict_state() {
        // Including the ones checked earlier in a naive ordering: an all-probes-
        // failed run that also produced a failure must still report the failure.
        let mut r = run_of(vec![failed()]);
        r.source_probe_failures = r.candidates_probed;
        assert!(matches!(r.verdict(), ValidationVerdict::Failures { .. }));
    }

    #[test]
    fn a_run_that_encoded_nothing_is_not_a_pass() {
        // Twelve skips and zero failures would otherwise read as a clean result
        // — it is actually "no evidence was produced".
        let r = run_of(vec![
            ValidationOutcome::Skipped {
                reason: ValidateSkip::AlreadyOptimal,
            },
            ValidationOutcome::Skipped {
                reason: ValidateSkip::InputTooLarge {
                    bytes: 9,
                    ceiling_bytes: 8,
                },
            },
        ]);
        assert_eq!(r.failed, 0);
        assert_eq!(r.verdict(), ValidationVerdict::NothingValidated);
    }

    #[test]
    fn a_run_where_every_probe_failed_is_not_a_clean_run() {
        let mut r = ValidationRun {
            candidates_probed: 20,
            source_probe_failures: 20,
            ..Default::default()
        };
        assert_eq!(r.verdict(), ValidationVerdict::ProbeUnusable);
        // ...and it stays distinct from "nothing needed validating".
        r.source_probe_failures = 0;
        assert_eq!(r.verdict(), ValidationVerdict::NothingValidated);
    }

    #[test]
    fn all_verified_requires_at_least_one_real_encode() {
        let r = run_of(vec![ValidationOutcome::Verified]);
        assert_eq!(r.verdict(), ValidationVerdict::AllVerified { verified: 1 });
        assert_eq!(run_of(vec![]).verdict(), ValidationVerdict::NothingValidated);
    }

    #[test]
    fn the_run_reports_no_pass_rate_anywhere() {
        // Deliberate absence, pinned: a percentage is how "one output in twelve
        // is broken" becomes "92%, ship it". If a field like this is ever
        // added, this test is the conversation about it.
        let r = run_of(vec![ValidationOutcome::Verified, failed()]);
        let rendered = format!("{r:?}");
        for banned in ["pass_rate", "success_rate", "percent"] {
            assert!(!rendered.contains(banned), "found `{banned}` in {rendered}");
        }
    }

    #[test]
    fn every_outcome_lands_in_exactly_one_bucket() {
        let r = run_of(vec![
            ValidationOutcome::Verified,
            failed(),
            ValidationOutcome::Skipped {
                reason: ValidateSkip::AlreadyOptimal,
            },
        ]);
        assert_eq!((r.verified, r.failed, r.skipped), (1, 1, 1));
        assert_eq!(r.files.len(), 3);
    }

    #[test]
    fn a_timeout_is_a_failure_and_not_a_skip() {
        // Fail closed, stated by the brief: an encode we stopped is one we
        // cannot vouch for. Reporting it as a skip would quietly shrink the
        // denominator instead of raising a finding.
        let o = ValidationOutcome::Failed {
            failure: ValidationFailure::Timeout { after_secs: 1200 },
        };
        assert!(o.is_failure());
        assert_eq!(run_of(vec![o]).verdict().as_str(), "failures");
    }

    #[test]
    fn an_unprobeable_output_is_a_failure_and_not_a_pass() {
        let o = ValidationOutcome::Failed {
            failure: ValidationFailure::OutputUnprobeable {
                detail: "ffprobe: invalid data".into(),
            },
        };
        assert!(o.is_failure());
    }

    #[test]
    fn the_outcome_and_verdict_labels_are_distinct() {
        // They end up in an operator-facing report; two states sharing a word
        // is how a broken run gets mistaken for a clean one.
        let labels = [
            ValidationOutcome::Verified.as_str(),
            failed().as_str(),
            ValidationOutcome::Skipped {
                reason: ValidateSkip::AlreadyOptimal,
            }
            .as_str(),
            ValidationVerdict::NothingValidated.as_str(),
            ValidationVerdict::ProbeUnusable.as_str(),
            ValidationVerdict::Failures {
                failed: 1,
                verified: 0,
            }
            .as_str(),
            ValidationVerdict::AllVerified { verified: 1 }.as_str(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }

    // --- report completeness ----------------------------------------------

    #[test]
    fn a_skipped_file_still_carries_its_input_characteristics() {
        // Otherwise the skip list is a list of paths, and the operator cannot
        // tell "we skipped the 4K tail" from "we skipped a dozen ordinary files".
        let p = probe("matroska,webm", "h264", 1918, 802, &["eac3"], 42);
        let f = describe_input(Path::new("/srv/media/x.mkv"), &p);
        assert_eq!(f.input_video_codec, "h264");
        assert_eq!(f.input_dimensions, Some((1918, 802)));
        assert_eq!(f.input_subtitle_count, 42);
        assert_eq!(f.input_audio_codecs, vec!["eac3"]);
        assert_eq!(f.input_bytes, Some(500 * 1024 * 1024));
    }

    #[test]
    fn the_size_delta_is_signed_so_a_growing_encode_is_visible() {
        let mut f = describe_input(
            Path::new("/x.mkv"),
            &probe("matroska", "h264", 1920, 1080, &["aac"], 0),
        );
        let mut out = probe("matroska", "h264", 1920, 1080, &["aac"], 0);
        out.size_bytes = Some(600 * 1024 * 1024);
        fill_output(&mut f, &out);
        assert_eq!(f.size_delta_bytes, Some(100 * 1024 * 1024));

        out.size_bytes = Some(100 * 1024 * 1024);
        fill_output(&mut f, &out);
        assert!(f.size_delta_bytes.unwrap() < 0, "a saving must read as negative");
    }

    #[test]
    fn the_plan_description_says_whether_anything_was_re_encoded() {
        use crate::foundry::plan::{AudioAction, VideoAction};
        let remux = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Copy,
            audio: AudioAction::Copy,
            container: Container::Matroska,
        };
        assert!(describe_plan(&remux).contains("remux only"));

        let encode = TranscodePlan {
            video: VideoAction::Encode {
                scale: Some((1920, 1080)),
            },
            audio: AudioAction::Encode { channels: vec![2] },
            ..remux
        };
        let d = describe_plan(&encode);
        assert!(d.contains("1920x1080"), "{d}");
        assert!(!d.contains("remux only"), "{d}");
    }

    // --- the destructive path is not reachable from here -------------------

    #[test]
    fn this_module_never_names_the_destructive_entry_point() {
        // A source-level guard on the one rule that cannot be expressed in the
        // type system: `forge::optimize_file` replaces a library file, and
        // nothing here may call it. Also catches the mutation gate being read,
        // which would imply behaviour that changes when it is switched on.
        let src = include_str!("validate.rs");
        // Strip this test's own body so its literals do not match themselves.
        let code = src.split("fn this_module_never_names").next().unwrap();
        for banned in [
            "optimize_file(",
            "swap_verified_output",
            "resolve_for_mutation",
            "require_mutation",
            "fs::rename",
            "fs::remove_dir_all",
        ] {
            assert!(
                !code.contains(banned),
                "validate.rs must never use `{banned}` — it is the destructive path"
            );
        }
    }

    #[test]
    fn the_only_removals_this_module_performs_are_of_its_own_scratch_files() {
        // `ScratchOutput::drop` is the sole `remove_file` call site, and both
        // paths it removes were generated by this module inside the scratch dir.
        let src = include_str!("validate.rs");
        let code = src.split("fn the_only_removals_this_module_performs").next().unwrap();
        assert_eq!(
            code.matches("remove_file").count(),
            1,
            "validate.rs must contain exactly one remove_file call site (ScratchOutput::drop)"
        );
    }

    #[test]
    fn scratch_names_are_unique_per_job_so_two_files_cannot_collide() {
        let dir = Path::new("/var/tmp/scratch");
        let a = ScratchOutput::new(dir, "mkv");
        let b = ScratchOutput::new(dir, "mkv");
        assert_ne!(a.output, b.output);
        assert_ne!(a.stderr, b.stderr);
        assert!(a.output.starts_with(dir) && a.stderr.starts_with(dir));
        // Nothing is created on disk by the constructor, so the Drop that runs
        // at the end of this test finds nothing and must not complain.
    }

    #[test]
    fn a_scratch_output_removes_itself_even_on_an_early_return() {
        // The leak this guards: with a 6 GiB budget, one leaked 1.5 GiB output
        // is a quarter of the run, and the failure presents as unrelated
        // "budget exhausted" skips several files later.
        let dir = std::env::temp_dir().join(format!("muse-validate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (output, stderr) = {
            let s = ScratchOutput::new(&dir, "mkv");
            std::fs::write(&s.output, b"encoded bytes").unwrap();
            std::fs::write(&s.stderr, b"ffmpeg said things").unwrap();
            assert!(s.output.exists() && s.stderr.exists());
            (s.output.clone(), s.stderr.clone())
        };
        assert!(!output.exists(), "the scratch output outlived its guard");
        assert!(!stderr.exists(), "the stderr capture outlived its guard");
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn the_stderr_tail_is_bounded_so_one_noisy_file_cannot_flood_the_report() {
        let dir = std::env::temp_dir().join(format!("muse-validate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("err");
        std::fs::write(&p, "x".repeat(100_000)).unwrap();
        assert_eq!(read_stderr_tail(&p).chars().count(), 400);
        // A missing capture file is empty, not a panic.
        assert_eq!(read_stderr_tail(&dir.join("absent")), "");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }
}
