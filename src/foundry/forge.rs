//! MUSEF-02 — execute a plan: encode to a staging file, **prove** the output is
//! good, and only then replace the original.
//!
//! ## The rule this module exists to enforce
//! An unverified success is a failure. ffmpeg exiting `0` is not evidence that
//! the output is a complete, playable file — a truncated encode (disk full, OOM
//! kill, a source that stopped reading half way) can and does exit cleanly. So
//! nothing is replaced until the *output* has been probed and compared against
//! the source: duration, codecs, and stream counts. If any of that cannot be
//! established, the staged file is deleted and the original is left exactly as
//! it was.
//!
//! ## What the swap does and does not guarantee — read this first
//! **The swap is not atomic and it is not crash-safe.** It is a sequence of
//! filesystem operations with rollback, serialized against other Muse workers
//! by an advisory lock ([`SwapLock`]). Concretely:
//!
//! | Property | Status |
//! |---|---|
//! | Never overwrites an unrelated file | **Guaranteed** — names are claimed with `link(2)`, which fails `EEXIST` atomically |
//! | Never deletes the original | **Guaranteed** — it ends up renamed to a sibling `.muse-superseded` entry |
//! | Never leaves the library path absent | **Guaranteed** — the new entry is always established before the old name is released |
//! | Safe against concurrent *Muse* workers | **Guaranteed** — by the advisory lock |
//! | Safe against an arbitrary *external* writer | **NO** — see below |
//! | Atomic as a whole | **NO** — it is several syscalls |
//!
//! ### The residual race, stated exactly
//! The swap hard-links the original to its backup name, checks that the linked
//! inode is the one that was probed, and then releases the original name. The
//! identity check proves the *link* grabbed the expected inode; it says nothing
//! about the *unlink* that follows. A process that replaces the original in
//! that gap leaves the check passing while the unlink removes the newer file.
//!
//! **This cannot be closed with POSIX primitives.** It would need an atomic
//! "remove this directory entry only if it still points at inode X" —
//! a compare-and-unlink — which POSIX does not provide and which no combination
//! of `link`/`rename`/`unlink` synthesizes. So: the lock is the real mitigation
//! for the concurrency Muse itself creates (the realistic case, since Muse is
//! intended to be the sole writer to the library), and the identity check is
//! best-effort detection of the common external case. Neither is a guarantee
//! against a determined external writer, and this module does not claim to be.
//!
//! ### Recovering from a crash
//! Two claims hold at every crash point, and they are the ones that matter:
//! **the title is always reachable** (the new entry is created before the old
//! name is released), and **no residue is ever the only copy of anything**, so
//! deleting any of them is safe. Recovery is always a deletion — never
//! reconstructing data, never a rename by hand.
//!
//! What is *not* claimed: that every residue is a complete file. One of them
//! may be partial, and it is called out below. (This table is a claim about the
//! code and was wrong once — it asserted completeness for the inflight file
//! that `fs::copy` cannot provide. It is written against the code, not against
//! an intention.)
//!
//! In the library directory:
//! - **`<name>.muse-foundry-inflight-*.part`** — the in-library staging copy.
//!   **This file MAY BE INCOMPLETE.** It is produced by a byte copy from the
//!   work dir, so a crash mid-copy leaves it partial; a crash after the swap
//!   linked it leaves it a complete duplicate. Either way it is safe to delete,
//!   because at that point the bytes still exist in the work dir, at the
//!   destination, or both. Its `.part` extension is deliberately not a media
//!   extension, so the library scanner can never ingest a partial one.
//! - **`<name>.muse-superseded`** — the original, preserved. Always complete
//!   (it is a hard link to the original inode, never a copy). Two situations
//!   produce it, distinguishable with `ls -i` / `stat`:
//!   *the swap completed* — it and the live file are **different** inodes, and
//!   this is the old version awaiting retention; or *the swap crashed after
//!   taking the backup* — it and the live file are the **same** inode, i.e. two
//!   names for the untouched original. Deleting the `.muse-superseded` name is
//!   safe in both.
//! - **both an old- and a new-container file** (e.g. `Movie.avi` *and*
//!   `Movie.mkv`) — the swap died between claiming the destination and
//!   releasing the source. Both are complete; delete the old-container one.
//!
//! In the work dir (outside the library, so never scanned):
//! - **`muse-foundry-<uuid>.<ext>`** — the encode staging file. **May be
//!   incomplete** if the crash happened during the encode. Always safe to
//!   delete; it is only ever a candidate, never a live file.
//! - **`locks/<hash>.lock`** — a zero-byte lockfile, deliberately left behind.
//!   Carries no state: the lock is the `flock`, not the file's existence, so a
//!   leftover never blocks a future run. See [`SwapLock`].
//!
//! ### Why the in-library copy cannot be made atomic
//! The obvious fix — build the file in the work dir and bring it into the
//! library with a single `link`/`rename`, which cannot leave a partial — is
//! **not available here**, and not by choice. Safety rail 3 requires the work
//! dir to be on a *different filesystem* from every library root, and
//! [`crate::foundry::config::FoundryConfig::fatal_errors`] makes a same-device
//! work dir a startup refusal whenever the mutation gate is open. So on any run
//! that can reach this code the two are guaranteed to be different devices, and
//! `link(2)`/`rename(2)` across devices fail with `EXDEV`. A byte copy is the
//! only way across, and a byte copy has a partial window. Hence: the window is
//! documented rather than eliminated, and the file is named so that the partial
//! state is both harmless and obvious.
//!
//! ## The ordering, and what each step protects against
//! 1. Encode into the **work dir**, which safety rail 3 requires to be outside
//!    the library and on a different filesystem. A half-written file never
//!    appears in the library, and a runaway encode consumes scratch space
//!    rather than the library's own free space.
//! 2. Re-probe the staged file and verify it. Fail ⇒ delete staged, stop.
//! 3. Copy the verified file into the library directory under a temporary,
//!    non-media name, and re-probe *that* too. The cross-filesystem copy is
//!    itself a step that can truncate, and the copy is the thing that becomes
//!    the library file — so it is verified, not assumed.
//! 4. Claim the backup name, then establish the new entry, then release the old
//!    name. Never the other way round -- see `swap_verified_output`.
//!
//! The original is never `remove`d while it is the only name for its inode, and
//! never opened for writing. It ends up as a sibling `.muse-superseded` entry,
//! which the library scanner ignores (not a media extension) and which the
//! operator or a later retention sweep can delete once the new file has been
//! watched.
//!
//! ## Why the interesting parts are pure
//! `ffmpeg` and `ffprobe` are **not installed** on <host> or on the dev box
//! (verified 2026-07-31), so nothing in this fleet can currently execute an
//! end-to-end transcode — including its tests. The verification *decision*
//! ([`verify_output`]) and the swap *mechanics* ([`swap_verified_output`]) are
//! therefore split out from the invocation: the first is a pure function of two
//! probes and is tested against hand-built ones, the second moves real files
//! and is tested with ordinary text files. What genuinely cannot be tested here
//! is the single `Command::new(ffmpeg)` call and nothing else.

use std::os::unix::io::AsRawFd;
use std::time::{Duration, SystemTime};
use std::path::{Path, PathBuf};

use crate::media::capability::{self, Capabilities};
use crate::foundry::config::FoundryConfig;
use crate::media::paths::{MutablePath, PathError, PathGuard, ResolvedPath};
use crate::foundry::plan::{
    output_container, plan_transcode, AudioAction, TranscodeDecision, TranscodePlan,
    TranscodeReason, Undecidable, VideoAction,
};
use crate::foundry::policy::TranscodePolicy;
use crate::media::probe::{run_ffprobe, MediaProbe, ProbeError};

/// Extension given to the superseded original.
///
/// Deliberately **not** a media extension: `library::scan` selects files by
/// video extension, so a superseded original cannot be re-ingested as a
/// duplicate of the file that just replaced it. Renaming rather than deleting
/// is the point — the operator's undo.
const SUPERSEDED_EXT: &str = "muse-superseded";

/// Prefix for the in-library staging copy. Leading dot so it is hidden.
const INFLIGHT_PREFIX: &str = ".muse-foundry-inflight";

/// Extension for the in-library staging copy — deliberately **not** a media
/// extension, and this is load-bearing rather than cosmetic.
///
/// The staging copy is produced by `fs::copy`, which writes bytes directly to
/// this path over a period of time. A crash or a storage failure part-way
/// through leaves a **genuinely partial file** sitting in the library
/// directory. Naming it `<name>.mkv` (as an earlier version did, taking the
/// extension from the target container) meant that partial file carried a
/// video extension the library scanner selects on — so a half-written file
/// could be ingested as if it were a real title.
///
/// `.part` closes that: the scanner never selects it, and the name itself
/// states that the file may be incomplete. See the module's recovery notes —
/// this is the one residue that is not guaranteed to be a complete file.
const INFLIGHT_EXT: &str = "part";

// --- Outcome types ---------------------------------------------------------

/// What happened to one file. Exactly one of three things, and every one of
/// them is explicit — there is no variant meaning "nothing happened", because
/// "nothing happened" is always *for a reason* and the reason is the useful
/// part.
#[derive(Debug, Clone, PartialEq)]
pub enum ForgeStatus {
    /// Nothing was done, and here is precisely why.
    Skipped { reason: SkipReason },
    /// A process ran, produced a verified output, and that output replaced the
    /// original. This variant is only ever constructed after
    /// [`verify_output`] returned `Ok` for both the staged file and the
    /// in-library copy — it is the module's one claim of work done, and it is
    /// never made on the strength of an exit code alone.
    Rewritten(RewriteRecord),
    /// Work was attempted and did not complete. The original is untouched.
    Failed { reason: String },
}

/// Why a file was skipped. Enumerated rather than stringly-typed so a caller
/// can count categories, and so no call site can invent a vague one.
#[derive(Debug, Clone, PartialEq)]
pub enum SkipReason {
    /// A required tool is not usable on this host. **The live case on this
    /// fleet.** Reported as a skip with a named tool, never as "already
    /// optimal" and never as a success.
    ToolUnavailable { tool: &'static str, detail: String },
    /// `MUSE_FOUNDRY_ENABLE_MUTATION` is closed. Foundry probed and planned,
    /// and stopped at the gate.
    MutationDisabled,
    /// Mutation is enabled but no work dir is configured, so there is nowhere
    /// outside the library to stage. (Registration refuses this combination —
    /// see `FoundryConfig::fatal_errors` — so this is defence in depth.)
    NoWorkDir,
    /// The encode would run and the deletion gate would then REFUSE, so the
    /// original would be kept and the title would use MORE disk rather than
    /// reclaiming any.
    ///
    /// Opt-in via MUSE_FOUNDRY_SKIP_UNRECLAIMABLE=1, default OFF. Skipping is
    /// not obviously right: the swap still puts a direct-playable file in the
    /// library, which is half of what Path A is for. The other half —
    /// reclaiming the original's space — is impossible for these titles, so
    /// the operator is choosing between "direct play at double disk" and
    /// "leave it alone". Measured on this library: 260 of 16,221 titles
    /// (1.6%), led by TrueHD (114) and DTS (50).
    UnreclaimableOriginal { predicted: Vec<String> },
    /// A probe did not return within its deadline, so the file was never
    /// judged. TRANSIENT and retryable — a stalled filesystem, not a bad file.
    ///
    /// Deliberately a skip rather than a failure: `Failed` reads as "something
    /// is wrong with this file", and a later operator (or a later run) would
    /// treat it as such. Nothing was learned about the file at all.
    ProbeTimedOut { secs: u64 },
    /// The plan says the file already meets policy.
    AlreadyOptimal,
    /// Foundry could not judge the file.
    Undecidable(Undecidable),
    /// The path guard refused the path.
    PathRefused(PathError),
    /// Another Foundry swap already holds the lock covering this title, so a
    /// second worker is already handling it. Not a failure — and reported as
    /// its own reason rather than folded into a generic error, because "someone
    /// else is doing it" and "this file could not be processed" call for
    /// completely different operator responses.
    SwapLockBusy,
    /// The source is a symlink. Replacing the file it points at would silently
    /// change whatever else links to it, and if the container changes the link
    /// is left dangling. Hardlinks are *not* affected by this and are not
    /// skipped: a rename preserves the inode, so an *arr-style hardlinked
    /// download keeps seeding from the superseded file.
    SourceIsSymlink,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolUnavailable { tool, detail } => {
                write!(f, "required tool `{tool}` is not usable on this host: {detail}")
            }
            Self::UnreclaimableOriginal { predicted } => write!(
                f,
                "skipped: this encode would run and the deletion gate would then refuse, \
                 so the original would be KEPT and this title would use more disk rather \
                 than reclaiming any ({}). MUSE_FOUNDRY_SKIP_UNRECLAIMABLE is set",
                predicted.join("; ")
            ),
            Self::ProbeTimedOut { secs } => write!(
                f,
                "the probe did not return within {secs}s, so this file was never judged — \
                 a stalled filesystem, not a bad file; retry it"
            ),
            Self::MutationDisabled => write!(
                f,
                "Foundry's mutation gate is closed (MUSE_FOUNDRY_ENABLE_MUTATION) — \
                 the file was probed and planned but not modified"
            ),
            Self::NoWorkDir => write!(
                f,
                "no MUSE_FOUNDRY_WORK_DIR is configured, so there is nowhere outside \
                 the library to stage output"
            ),
            Self::AlreadyOptimal => write!(f, "the file already meets the transcode policy"),
            Self::Undecidable(u) => write!(f, "{u}"),
            Self::PathRefused(e) => write!(f, "{e}"),
            Self::SwapLockBusy => write!(
                f,
                "another Foundry worker already holds the swap lock for this title — \
                 skipped so the two do not race (and so the same file is not encoded twice)"
            ),
            Self::SourceIsSymlink => write!(
                f,
                "the source is a symlink; replacing its target could affect other \
                 files that link to it"
            ),
        }
    }
}

/// The record of a completed, verified replacement.
#[derive(Debug, Clone, PartialEq)]
pub struct RewriteRecord {
    /// The file that now exists in the library.
    pub final_path: PathBuf,
    /// Where the original was renamed to. Nothing was deleted.
    pub superseded_path: PathBuf,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// True when the rewrite re-encoded nothing (a lossless container change).
    pub remux_only: bool,
    pub reasons: Vec<TranscodeReason>,
}

// --- Verification: pure ----------------------------------------------------

/// What the output of a given plan must look like for the swap to be allowed.
///
/// ## Every field here is a promise the argv actually made
/// An earlier version of this type carried only stream *counts*, which meant a
/// file could be reported `Rewritten` without codecs, languages, dispositions,
/// attachments, chapters or metadata ever having been checked — a success
/// claim covering promises it had not verified. The rule now is one-to-one:
/// if [`crate::foundry::plan::build_transcode_args`] asks ffmpeg for it, it is
/// represented here and checked; if it cannot be checked, it is not promised.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyExpectation {
    pub source_duration_secs: f64,
    /// The `codec_name` the output's video stream must report.
    pub video_codec: String,
    /// Dimensions the output must have, when the plan ordered a downscale.
    pub video_dimensions: Option<(u32, u32)>,
    /// Per-stream audio expectations, **in order**. Order matters: `-map 0:a?`
    /// preserves source order, so a positional comparison catches a
    /// re-ordering that a set comparison would miss — and a re-ordered track
    /// list means the player's default language selection changes.
    pub audio: Vec<ExpectedAudio>,
    /// Per-stream subtitle expectations, in order.
    pub subtitles: Vec<ExpectedSubtitle>,
    /// Attachment filenames, in order. Compared by *name*, not just count, so
    /// a substituted font is caught as well as a dropped one.
    pub attachment_filenames: Vec<Option<String>>,
    /// Chapters the argv promised to carry with `-map_chapters 0`.
    pub chapter_count: usize,
    /// The container `title` tag the argv promised to carry with
    /// `-map_metadata 0`.
    ///
    /// **The limit of the metadata claim, stated plainly.** `-map_metadata 0`
    /// carries the whole global tag dictionary, but only `title` is verified
    /// here. Comparing every tag would reject legitimate outputs — muxers
    /// rewrite `encoder`, and Matroska and MP4 do not spell the same tags the
    /// same way — so a full comparison would be a check that fails on correct
    /// files. `title` is the tag the *arr tools and Plex actually read, so it
    /// is the one worth asserting. The rest of the metadata is *passed* but
    /// not *verified*, and this comment exists so that distinction is never
    /// mistaken for a guarantee.
    pub title: Option<String>,
    /// Absolute duration slack, in seconds. Default 2.0 — container duration
    /// legitimately shifts by a frame or two across a remux, and by up to
    /// about a second when a keyframe-aligned encode rounds the last GOP.
    pub duration_abs_tolerance_secs: f64,
    /// Relative duration slack. Default 0.01 (1%) — a long feature accumulates
    /// more rounding than a short one, so the tolerance scales with it.
    pub duration_rel_tolerance: f64,
}

/// What one audio stream must look like after the rewrite.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedAudio {
    pub codec: String,
    pub channels: u32,
    /// The language tag must survive. A track that loses its language stops
    /// being auto-selectable and looks like a duplicate to the player.
    pub language: Option<String>,
}

/// What one subtitle stream must look like after the rewrite.
///
/// Subtitles are always stream-copied, so every field is expected to come
/// through byte-identical — including the dispositions, which decide whether a
/// forced-narrative track auto-displays.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpectedSubtitle {
    pub codec: String,
    pub language: Option<String>,
    pub forced: bool,
    pub default: bool,
}

/// Derive the expectation from the source probe and the plan.
///
/// `None` when the source duration is unknown — at which point verification is
/// impossible and the caller must refuse to swap. [`plan_transcode`] already
/// declines to plan such a file, so this is a second, independent refusal of
/// the same unverifiable case rather than a single check that could be forgotten.
pub fn expectation_for(
    source: &MediaProbe,
    plan: &TranscodePlan,
    policy: &TranscodePolicy,
) -> Option<VerifyExpectation> {
    let source_duration_secs = source.duration_secs?;
    let source_video = source.primary_video()?;

    let (video_codec, video_dimensions) = match plan.video {
        // A stream copy must come out bit-identical in codec terms; if it did
        // not, ffmpeg silently re-encoded and the "lossless remux" claim is
        // false.
        VideoAction::Copy => (source_video.codec.clone(), None),
        VideoAction::Encode { scale } => (
            policy.encode_video.probe_codec_name().to_string(),
            // Only assert dimensions when a downscale was actually ordered:
            // asserting the source dimensions on a no-scale encode would be a
            // second, redundant claim, and one that a legitimate anamorphic
            // fix-up could trip.
            scale,
        ),
    };

    // Per-stream audio expectations. `None` (refusing to build an expectation
    // at all) when any source channel count is unknown: the encode path
    // downmixes to `min(source, ceiling)`, which is not computable without the
    // source count, and asserting nothing there would be the same silent gap
    // the planner's `UnknownAudioChannels` rule closes.
    let mut audio = Vec::with_capacity(source.audio.len());
    for (i, a) in source.audio.iter().enumerate() {
        let channels = a.channels?;
        audio.push(match &plan.audio {
            AudioAction::Copy => ExpectedAudio {
                codec: a.codec.clone(),
                channels,
                language: a.language.clone(),
            },
            AudioAction::Encode { channels: targets } => ExpectedAudio {
                codec: "aac".to_string(),
                // Read from the PLAN, never recomputed. `-ac` sets the channel
                // count exactly rather than capping it, and when this was
                // derived independently here the argv and the expectation
                // disagreed: the argv upmixed stereo to the 6-channel ceiling
                // while this correctly expected 2.
                //
                // A short vector REFUSES rather than falling back to a local
                // `min(source, ceiling)`. The fallback was unreachable for a
                // plan from `plan_transcode`, but `TranscodePlan`'s fields are
                // public: a hand-built plan could omit an `-ac:a:N` from the
                // argv while this quietly assumed a value, which is the exact
                // two-derivations divergence this item exists to remove.
                // Raised by codex, opus and free at the FOUNDRY-08 gate.
                channels: *targets.get(i)?,
                language: a.language.clone(),
            },
        });
    }

    Some(VerifyExpectation {
        source_duration_secs,
        video_codec,
        video_dimensions,
        audio,
        subtitles: source
            .subtitles
            .iter()
            .map(|s| ExpectedSubtitle {
                codec: s.codec.clone(),
                language: s.language.clone(),
                forced: s.forced,
                default: s.default,
            })
            .collect(),
        attachment_filenames: source
            .attachments
            .iter()
            .map(|a| a.filename.clone())
            .collect(),
        chapter_count: source.chapter_count,
        title: source.title.clone(),
        duration_abs_tolerance_secs: 2.0,
        duration_rel_tolerance: 0.01,
    })
}

/// Why an output was rejected. Every variant means the same thing operationally:
/// **do not replace the original**.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyFailure {
    NoVideoStream,
    /// The output's duration could not be read. Not a pass — an output we
    /// cannot measure is an output we cannot clear.
    UnknownDuration,
    /// The classic truncation signature.
    DurationMismatch {
        source_secs: f64,
        output_secs: f64,
        allowed_secs: f64,
    },
    VideoCodecMismatch {
        expected: String,
        found: String,
    },
    DimensionsMismatch {
        expected: (u32, u32),
        found: (Option<u32>, Option<u32>),
    },
    /// A track was lost. `-map 0:a?` should carry every audio stream across;
    /// if the count dropped, ffmpeg discarded something the operator had.
    AudioStreamCountMismatch {
        expected: usize,
        found: usize,
    },
    SubtitleStreamCountMismatch {
        expected: usize,
        found: usize,
    },
    /// An audio stream survived but came out wrong — wrong codec, wrong
    /// channel count, or a lost language tag.
    AudioStreamMismatch {
        ordinal: usize,
        detail: String,
    },
    /// A subtitle stream survived but came out wrong. Includes a lost `forced`
    /// or `default` disposition, which silently changes whether a
    /// forced-narrative track auto-displays.
    SubtitleStreamMismatch {
        ordinal: usize,
        detail: String,
    },
    /// Attachments were dropped, added or substituted. For anime and many
    /// foreign releases these are the fonts the subtitle track is styled with.
    AttachmentsMismatch {
        expected: Vec<String>,
        found: Vec<String>,
    },
    /// `-map_chapters 0` did not carry the chapters across.
    ChapterCountMismatch {
        expected: usize,
        found: usize,
    },
    /// `-map_metadata 0` did not carry the container title across.
    TitleMismatch {
        expected: Option<String>,
        found: Option<String>,
    },
    /// A zero-byte or unmeasurable output.
    EmptyOutput,
}

impl std::fmt::Display for VerifyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoVideoStream => write!(f, "the output has no video stream"),
            Self::UnknownDuration => write!(
                f,
                "the output's duration could not be read, so it cannot be checked for \
                 truncation"
            ),
            Self::DurationMismatch {
                source_secs,
                output_secs,
                allowed_secs,
            } => write!(
                f,
                "the output is {output_secs:.3}s against a {source_secs:.3}s source \
                 (allowed drift {allowed_secs:.3}s) — it is truncated or padded"
            ),
            Self::VideoCodecMismatch { expected, found } => write!(
                f,
                "the output's video codec is `{found}`, expected `{expected}`"
            ),
            Self::DimensionsMismatch { expected, found } => write!(
                f,
                "the output is {:?}x{:?}, expected {}x{}",
                found.0, found.1, expected.0, expected.1
            ),
            Self::AudioStreamCountMismatch { expected, found } => write!(
                f,
                "the output has {found} audio streams, the source had {expected} — \
                 a track was lost"
            ),
            Self::SubtitleStreamCountMismatch { expected, found } => write!(
                f,
                "the output has {found} subtitle streams, the source had {expected} — \
                 a track was lost"
            ),
            Self::AudioStreamMismatch { ordinal, detail } => {
                write!(f, "output audio stream #{ordinal} is wrong: {detail}")
            }
            Self::SubtitleStreamMismatch { ordinal, detail } => {
                write!(f, "output subtitle stream #{ordinal} is wrong: {detail}")
            }
            Self::AttachmentsMismatch { expected, found } => write!(
                f,
                "attachments changed — expected {expected:?}, found {found:?} \
                 (these are typically the fonts the subtitles are styled with)"
            ),
            Self::ChapterCountMismatch { expected, found } => write!(
                f,
                "the output has {found} chapters, the source had {expected}"
            ),
            Self::TitleMismatch { expected, found } => write!(
                f,
                "the container title changed — expected {expected:?}, found {found:?}"
            ),
            Self::EmptyOutput => write!(f, "the output is empty or its size could not be read"),
        }
    }
}

/// Decide whether a probed output is good enough to replace its source.
///
/// **Pure**, and fail-closed on every axis: anything it could not establish is
/// a rejection, never a pass. This is the function that stands between a
/// truncated encode and an irreplaceable file.
pub fn verify_output(
    expect: &VerifyExpectation,
    output: &MediaProbe,
) -> Result<(), VerifyFailure> {
    match output.size_bytes {
        // `None` is a rejection, not a skip of this check: if we could not read
        // the size we have not observed a non-empty file.
        Some(n) if n > 0 => {}
        _ => return Err(VerifyFailure::EmptyOutput),
    }

    let Some(video) = output.primary_video() else {
        return Err(VerifyFailure::NoVideoStream);
    };

    if !video.codec.eq_ignore_ascii_case(&expect.video_codec) {
        return Err(VerifyFailure::VideoCodecMismatch {
            expected: expect.video_codec.clone(),
            found: video.codec.clone(),
        });
    }

    if let Some((ew, eh)) = expect.video_dimensions {
        if video.width != Some(ew) || video.height != Some(eh) {
            return Err(VerifyFailure::DimensionsMismatch {
                expected: (ew, eh),
                found: (video.width, video.height),
            });
        }
    }

    let Some(out_secs) = output.duration_secs else {
        return Err(VerifyFailure::UnknownDuration);
    };

    // The truncation check. Written as an explicit non-negative comparison
    // rather than `!(diff <= allowed)` so a NaN could not slip through as a
    // pass — though `probe::as_f64` already rejects NaN, this is the last gate
    // before a destructive step and does not rely on that.
    let allowed = f64::max(
        expect.duration_abs_tolerance_secs,
        expect.source_duration_secs * expect.duration_rel_tolerance,
    );
    let diff = (out_secs - expect.source_duration_secs).abs();
    if !(diff.is_finite() && allowed.is_finite() && diff <= allowed) {
        return Err(VerifyFailure::DurationMismatch {
            source_secs: expect.source_duration_secs,
            output_secs: out_secs,
            allowed_secs: allowed,
        });
    }

    // --- audio: count first, then every stream, positionally ---------------
    if output.audio.len() != expect.audio.len() {
        return Err(VerifyFailure::AudioStreamCountMismatch {
            expected: expect.audio.len(),
            found: output.audio.len(),
        });
    }
    for (i, (want, got)) in expect.audio.iter().zip(&output.audio).enumerate() {
        if !got.codec.eq_ignore_ascii_case(&want.codec) {
            return Err(VerifyFailure::AudioStreamMismatch {
                ordinal: i,
                detail: format!("codec is `{}`, expected `{}`", got.codec, want.codec),
            });
        }
        // `None` is a mismatch, not a skip: an output whose channel count we
        // cannot read has not been shown to satisfy the downmix.
        if got.channels != Some(want.channels) {
            return Err(VerifyFailure::AudioStreamMismatch {
                ordinal: i,
                detail: format!(
                    "channel count is {:?}, expected {}",
                    got.channels, want.channels
                ),
            });
        }
        if got.language != want.language {
            return Err(VerifyFailure::AudioStreamMismatch {
                ordinal: i,
                detail: format!(
                    "language is {:?}, expected {:?} — a track that loses its language \
                     stops being auto-selectable",
                    got.language, want.language
                ),
            });
        }
    }

    // --- subtitles: always stream-copied, so every field must survive ------
    if output.subtitles.len() != expect.subtitles.len() {
        return Err(VerifyFailure::SubtitleStreamCountMismatch {
            expected: expect.subtitles.len(),
            found: output.subtitles.len(),
        });
    }
    for (i, (want, got)) in expect.subtitles.iter().zip(&output.subtitles).enumerate() {
        if !got.codec.eq_ignore_ascii_case(&want.codec) {
            return Err(VerifyFailure::SubtitleStreamMismatch {
                ordinal: i,
                detail: format!("codec is `{}`, expected `{}`", got.codec, want.codec),
            });
        }
        if got.language != want.language {
            return Err(VerifyFailure::SubtitleStreamMismatch {
                ordinal: i,
                detail: format!("language is {:?}, expected {:?}", got.language, want.language),
            });
        }
        if got.forced != want.forced || got.default != want.default {
            return Err(VerifyFailure::SubtitleStreamMismatch {
                ordinal: i,
                detail: format!(
                    "disposition is forced={} default={}, expected forced={} default={} \
                     — this decides whether a forced-narrative track auto-displays",
                    got.forced, got.default, want.forced, want.default
                ),
            });
        }
    }

    // --- attachments: by filename, not by count ----------------------------
    // A substituted font is as broken as a missing one, and a count comparison
    // cannot tell them apart.
    let found_attachments: Vec<String> = output
        .attachments
        .iter()
        .map(|a| a.filename.clone().unwrap_or_default())
        .collect();
    let want_attachments: Vec<String> = expect
        .attachment_filenames
        .iter()
        .map(|f| f.clone().unwrap_or_default())
        .collect();
    if found_attachments != want_attachments {
        return Err(VerifyFailure::AttachmentsMismatch {
            expected: want_attachments,
            found: found_attachments,
        });
    }

    if output.chapter_count != expect.chapter_count {
        return Err(VerifyFailure::ChapterCountMismatch {
            expected: expect.chapter_count,
            found: output.chapter_count,
        });
    }

    if output.title != expect.title {
        return Err(VerifyFailure::TitleMismatch {
            expected: expect.title.clone(),
            found: output.title.clone(),
        });
    }

    Ok(())
}

// --- Swap mechanics: real files, no media tooling --------------------------

/// The paths a swap produced.
#[derive(Debug, Clone, PartialEq)]
pub struct SwapRecord {
    pub final_path: PathBuf,
    pub superseded_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SwapError {
    /// The destination name is taken by a *different* file. Refused rather
    /// than overwritten: a `.avi` becoming a `.mkv` must not clobber an
    /// unrelated `.mkv` that was already sitting next to it.
    DestinationOccupied(PathBuf),
    /// The `.muse-superseded` name is taken — a previous run left one behind.
    /// Refused, because overwriting it would destroy the older original, which
    /// is the one thing this design promises not to do.
    SupersededNameOccupied(PathBuf),
    /// The file on disk is no longer the one that was probed and planned
    /// against — something replaced it after the probe. Detected by comparing
    /// the inode we captured at probe time against the inode we actually
    /// linked. Refusing is mandatory: the plan describes a file that is no
    /// longer there, so applying it would move a newer, unrelated file aside
    /// and replace it with an encode of something else.
    SourceChangedUnderUs {
        path: PathBuf,
        expected: FileIdentity,
        found: FileIdentity,
    },
    /// Another swap already holds the lock covering this file. Not an error
    /// condition — the other worker is doing the work — so the caller reports
    /// it as a skip with a reason, not a failure.
    LockBusy(PathBuf),
    /// The filesystem refused to create a hard link, which is the primitive
    /// this swap uses to claim a name atomically. Some filesystems do not
    /// support links at all — in that case the swap **refuses rather than
    /// racing**, see [`swap_verified_output`].
    LinkUnsupported { path: PathBuf, message: String },
    Io { step: &'static str, message: String },
}

impl std::fmt::Display for SwapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DestinationOccupied(p) => write!(
                f,
                "a different file already exists at {} — refusing to overwrite it",
                p.display()
            ),
            Self::SupersededNameOccupied(p) => write!(
                f,
                "{} already exists (a previous run's original) — refusing to destroy it",
                p.display()
            ),
            Self::SourceChangedUnderUs {
                path,
                expected,
                found,
            } => write!(
                f,
                "{} was replaced after it was probed (inode {}:{} became {}:{}) — the plan \
                 describes a file that is no longer there, so it was not applied",
                path.display(),
                expected.dev,
                expected.ino,
                found.dev,
                found.ino
            ),
            Self::LockBusy(p) => write!(
                f,
                "another Foundry swap already holds the lock covering {} — skipping this \
                 file rather than racing the worker that has it",
                p.display()
            ),
            Self::LinkUnsupported { path, message } => write!(
                f,
                "could not create a hard link next to {} ({message}) — Foundry's swap needs \
                 hard links to claim a name without a race, and refuses rather than \
                 falling back to a check-then-rename that could overwrite a file",
                path.display()
            ),
            Self::Io { step, message } => write!(f, "{step}: {message}"),
        }
    }
}

/// An advisory `flock(2)` held for the duration of one swap.
///
/// ## What this does and does not protect against — read this before trusting it
///
/// **It serializes Muse's own workers**, which is the realistic contention case:
/// Muse is intended to be the sole writer to the library, so two Foundry passes
/// racing on the same file is the collision that can actually happen here, and
/// this eliminates it.
///
/// **It does not protect against an arbitrary external process.** `flock` is
/// *advisory* — a process that never asks for the lock is not stopped by it.
/// Sonarr moving a file, an operator with `mv`, or a script that knows nothing
/// about Foundry can still replace the file mid-swap, and this lock will not
/// notice.
///
/// **That residual race cannot be closed with POSIX primitives, and this code
/// does not pretend otherwise.** The specific hole, stated exactly: the swap
/// hard-links the original to its backup name, checks that the linked inode is
/// the one that was probed, and then unlinks the original name. The identity
/// check proves the *link* grabbed the right inode; it says nothing about the
/// *unlink* that follows. An external actor replacing the original in that gap
/// leaves the check passing while the unlink removes the newer file. Closing it
/// would need an atomic "remove this directory entry only if it still points at
/// inode X", which POSIX does not provide — there is no compare-and-unlink, and
/// no combination of `link`/`rename`/`unlink` synthesizes one. So the identity
/// check is best-effort detection of the common case, not a guarantee, and the
/// lock is the real mitigation for the concurrency Muse actually creates.
///
/// ## Non-blocking on purpose, and held across the encode
/// Acquired with `LOCK_EX | LOCK_NB`. A background pass that *blocked* on a
/// lock held by a long encode would wedge its whole worker; reporting
/// [`SkipReason::SwapLockBusy`] and moving to the next file is the correct
/// posture, and it is a skip with a reason like every other refusal here.
///
/// Because it never blocks, it is taken *before* the encode rather than just
/// around the swap. That costs a second worker nothing (it skips this title and
/// picks up another) and buys two things: the same file is never encoded twice
/// concurrently only for one result to be discarded, and the window in which
/// another Muse worker could replace the source covers probe-through-swap
/// instead of the swap alone.
pub(in crate::foundry) struct SwapLock {
    /// Held open for the lifetime of the lock: closing the descriptor is what
    /// releases the `flock`, so this field is load-bearing despite never being
    /// read.
    _file: std::fs::File,
}

impl SwapLock {
    /// Take the lock covering every name one swap touches.
    ///
    /// **Keyed on the destination's directory + file stem, not on a full
    /// path.** A single swap can touch three names — `Movie.avi`,
    /// `Movie.avi.muse-superseded` and `Movie.mkv` — and locking only one of
    /// them would let a second worker converting the same title from a
    /// different container proceed concurrently. Keying on the stem makes
    /// `Movie.avi -> Movie.mkv` and `Movie.avi -> Movie.mp4` collide, which is
    /// the case that matters.
    ///
    /// One correction to an earlier version of this comment, which claimed the
    /// stem is "what those names share": it is NOT shared by the
    /// `.muse-superseded` name. `file_stem` strips only the LAST extension, so
    /// `Movie.mkv` has stem `Movie` while `Movie.mkv.muse-superseded` has stem
    /// `Movie.mkv` — a different key entirely. Callers that want to exclude a
    /// swap must therefore key on the DESTINATION path, as this function's own
    /// caller does. `reaper` locks on the replacement (live) path for exactly
    /// this reason; keying on the backup's own path would have been a lock
    /// that excluded nothing. Found by a FOUNDRY-12 test that failed on its
    /// first run.
    ///
    /// The lockfile lives in the **work dir**, not the library: a lockfile in
    /// the library would be scratch state inside the media tree, which safety
    /// rail 3 exists to prevent, and the library may be mounted read-only
    /// (it is on <host> today — `MUSE_LIBRARY_ROOT=/srv/media` is `ro`).
    pub(in crate::foundry) fn acquire(lock_dir: &Path, target: &Path) -> Result<Self, SwapError> {
        use sha2::{Digest, Sha256};
        use std::os::unix::ffi::OsStrExt;

        let mut hasher = Sha256::new();
        if let Some(parent) = target.parent() {
            hasher.update(parent.as_os_str().as_bytes());
        }
        hasher.update(b"\0");
        if let Some(stem) = target.file_stem() {
            hasher.update(stem.as_bytes());
        }
        let key = format!("{:x}", hasher.finalize());

        let dir = lock_dir.join("locks");
        std::fs::create_dir_all(&dir).map_err(|e| SwapError::Io {
            step: "creating the Foundry lock directory",
            message: e.to_string(),
        })?;

        let path = dir.join(format!("{key}.lock"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| SwapError::Io {
                step: "opening the Foundry swap lockfile",
                message: e.to_string(),
            })?;

        // SAFETY: `file` owns a valid open descriptor for the duration of this
        // call, and `flock` only ever inspects it.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(match err.raw_os_error() {
                Some(libc::EWOULDBLOCK) => SwapError::LockBusy(target.to_path_buf()),
                _ => SwapError::Io {
                    step: "taking the Foundry swap lock",
                    message: err.to_string(),
                },
            });
        }

        Ok(Self { _file: file })
    }
}

impl Drop for SwapLock {
    /// Release the lock **explicitly** before the descriptor closes.
    ///
    /// Closing the fd also releases an `flock`, so this is belt-and-braces —
    /// but it makes release an operation this type performs rather than a
    /// side effect it relies on, and it removes any dependence on when the
    /// runtime actually closes the file.
    fn drop(&mut self) {
        // SAFETY: `_file` still owns a valid descriptor here; `Drop` runs
        // before the field itself is dropped.
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

// Release also happens implicitly when the process dies (the kernel drops the
// flock with the descriptor), which is why this is an flock and not a
// lockfile-existence lock: a killed worker leaves nothing to clean up by hand.
//
// The lockfile itself is left on disk on purpose: unlinking it would race
// another process that has already opened it but not yet locked it, which is
// the classic way a delete-on-release lock stops being a lock. A stale
// zero-byte lockfile in the work dir is harmless.
//
// EMPIRICAL NOTE — why the explicit `LOCK_UN` above is not redundant.
// Leaving release to `File`'s close was measurably unreliable in this crate's
// own test binary. A tight acquire / failed-acquire / drop / re-acquire cycle
// sometimes failed to re-acquire on a path no other test touches:
//
//   implicit release only : failures in ~30% of parallel runs under heavy
//                           load, ~8% (1 of 12) under the current suite
//   with explicit LOCK_UN : 0 failures in 32 consecutive full runs
//
// It never reproduced in an isolated 500-iteration loop, nor in an equivalent
// pure-syscall repro on the same tmpfs, so it needs the whole test binary's
// concurrency to appear.
//
// Two honest caveats. The mechanism was NOT identified — this is a
// measurement, not an explanation. And 32 clean runs is evidence of a large
// reduction, not proof of elimination: the failure is probabilistic, so its
// absence over a finite sample cannot prove it is gone. The explicit unlock is
// kept because it measurably helps and costs one syscall — do not "simplify"
// it away on the assumption that close-is-enough.

/// A file's identity on disk: the device and inode number it currently
/// occupies.
///
/// **This pair is only an identity for as long as the inode is still
/// referenced.** It distinguishes two files that exist at the same instant —
/// that is what [`reaper`](crate::foundry::reaper) uses it for, to tell a hard
/// link from a distinct file. It does NOT survive a delete: once an inode's
/// last reference goes away, most filesystems hand its *number* straight back
/// out to the next file created. Comparing a number captured earlier against a
/// number read later is therefore not a replacement check unless something
/// kept the original inode alive in between. See [`SourcePin`], which is that
/// something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Read a path's [`FileIdentity`] without following symlinks.
///
/// Sound for comparing two paths that both exist right now. For "is this still
/// the file I looked at earlier", use [`SourcePin`] instead — see the caveat on
/// [`FileIdentity`].
pub fn identity_of(p: &Path) -> std::io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(p)?;
    Ok(FileIdentity {
        dev: md.dev(),
        ino: md.ino(),
    })
}

/// A hold on the source file that makes its [`FileIdentity`] mean something
/// across time.
///
/// # Why this type exists — a real defect, not a tidiness exercise
///
/// The swap's third TOCTOU guard captures the source's `(dev, ino)` before the
/// probe and re-reads it after the encode, refusing if they differ. That was
/// unsound on its own, and provably so: **an inode number is recycled as soon
/// as the inode is released**. Delete a file and create another in its place
/// and the replacement very often lands on the number the deleted file just
/// vacated, at which point the guard compares equal for a completely different
/// file and the swap proceeds — moving the *newer, unrelated* file aside under
/// a `.muse-superseded` name and putting an encode of the OLD content at the
/// library path. That is the exact corruption the guard was written to refuse.
///
/// Whether recycling happens is filesystem policy, which is what made the
/// resulting test failure look like an order-dependent flake rather than a bug:
///
/// - `tmpfs` hands out inode numbers from a monotonically increasing counter
///   and never reuses them, so on a `/tmp`-backed fixture the guard *appears*
///   to work.
/// - `ext4`, `xfs` and the NFS-exported storage the real library lives on all
///   reuse freed numbers, and whether the just-freed one comes back depends on
///   what else allocated an inode in between — so the same code passes alone
///   and fails under a parallel suite.
///
/// # The fix, mechanically
///
/// Hold an open descriptor on the source for the whole probe → plan → encode →
/// swap window. An inode with a live reference cannot be released, and a
/// number cannot be recycled while its inode is still alive. With the pin held
/// there is no file on that filesystem, anywhere, that can present the pinned
/// number — so `(dev, ino)` stops being probabilistic and becomes a genuine
/// identity. The descriptor is load-bearing despite never being read, exactly
/// as [`SwapLock`]'s is.
///
/// # What it still does not catch
///
/// A rewrite *in place* of the same inode (`dd` over the file) leaves `dev` and
/// `ino` unchanged and is not detected. That was equally true before this type
/// existed; no acquisition path Muse drives writes that way — they all create a
/// new file and rename it, which this does catch.
pub(in crate::foundry) struct SourcePin {
    /// Load-bearing: dropping this releases the inode and re-arms number
    /// recycling. Never "simplify" it away because nothing reads it.
    _file: std::fs::File,
    identity: FileIdentity,
}

impl SourcePin {
    /// Open and pin `p`, reporting the identity of the inode actually pinned.
    ///
    /// The identity is read from the descriptor rather than from the path, so
    /// it cannot describe a different file than the one being held.
    pub(in crate::foundry) fn pin(p: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        // O_NOFOLLOW: pin the named file, never something a symlink points at.
        // Callers hand us an already-canonicalized `ResolvedPath`, so a symlink
        // here means the path changed under the guard — refuse rather than
        // pin the wrong inode. Read-only: the library is mounted `ro` on <host>.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(p)?;
        let md = file.metadata()?;
        Ok(Self {
            _file: file,
            identity: FileIdentity {
                dev: md.dev(),
                ino: md.ino(),
            },
        })
    }

    /// The identity of the pinned inode. Valid — in the sense of being
    /// unforgeable by any other file — for as long as `self` is alive.
    pub(in crate::foundry) fn identity(&self) -> FileIdentity {
        self.identity
    }
}

/// `fsync` a directory so a rename/link in it survives a power loss.
///
/// On Linux, opening a directory read-only and calling `fsync` is the
/// documented way to make its *entries* durable. Without it the new file can
/// be fully written and the directory entry still lost on a crash, leaving the
/// library path simply absent — the original is recoverable from the
/// `.muse-superseded` entry only if that entry is durable too, which is why
/// this is called once after both links are in place.
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Put an **already-verified** file into place, moving the original aside.
///
/// # Preconditions the caller owns
/// `verified_new` must have passed [`verify_output`]. This function performs no
/// media checks of its own — it is the mechanical half, split out precisely so
/// it can be tested with ordinary files on a host with no ffmpeg.
///
/// The `_lock` parameter is not read. It exists so that holding the swap lock
/// is a *type-level* precondition rather than a comment someone has to
/// remember — the same trick [`MutablePath`] plays for the mutation gate.
///
/// # What is and is not guaranteed — stated plainly
///
/// **This swap is not atomic and it is not crash-safe.** It is a sequence of
/// filesystem operations with rollback, serialized against other Muse workers
/// by an advisory lock. Specifically:
///
/// - **Never clobbers a bystander.** Claiming a name uses `link(2)`, which
///   fails `EEXIST` atomically and never overwrites, so occupancy is discovered
///   by *failing to claim* rather than by an `exists()` check that a concurrent
///   writer could invalidate. The one place `rename(2)` is used is the case
///   where the destination *is* the original — the file we have already
///   backed up and are deliberately replacing — so its overwriting behaviour
///   destroys nothing that is not already safe under the backup name.
/// - **Never deletes the original.** It ends up renamed to a sibling
///   `.muse-superseded` entry; nothing here calls `remove_file` on a path that
///   is not either a staging file or a name whose inode is already reachable
///   under another entry.
/// - **Does NOT protect against an arbitrary external writer.** See
///   [`SwapLock`] for the exact residual race (replace-between-link-and-unlink)
///   and why POSIX cannot close it.
///
/// # Ordering
/// 1. `link(original -> superseded)` — atomically claims the backup name. From
///    here the original's inode has two names and cannot be lost.
/// 2. Compare the linked inode against the [`SourcePin`] taken before the
///    probe. The pin is what makes this comparison mean anything: it keeps the
///    probed inode alive, so its number cannot have been recycled by a
///    replacement (see [`SourcePin`]). Still not a guarantee against an
///    arbitrary external writer — see [`SwapLock`] for the residual race.
/// 3. Put the new file at the destination, whichever of the two shapes applies:
///    - **destination == original** (container unchanged): `rename(new ->
///      final)`, which replaces the entry in one step. The original is already
///      safe under the backup name, so there is no moment when the library
///      path is missing.
///    - **destination != original** (e.g. `.avi` becoming `.mkv`):
///      `link(new -> final)` — atomic, refuses `EEXIST` so an unrelated
///      `.mkv` sitting alongside is never destroyed — and only then
///      `unlink(original)`. Both names exist briefly; neither is ever absent.
/// 4. Drop the staging name, then `fsync` the directory.
///
/// **The absent-name window is gone.** An earlier version unlinked the original
/// *before* claiming the destination, leaving the library path missing in
/// between; a crash there left the bytes recoverable only by hand. Both shapes
/// above now establish the new entry before removing the old one, so no crash
/// point leaves the title unreachable — the worst case is a leftover
/// `.muse-superseded` entry, or briefly both an `.avi` and an `.mkv`.
fn swap_verified_output(
    original: &MutablePath,
    verified_new: &MutablePath,
    final_path: &MutablePath,
    source_pin: &SourcePin,
    _lock: &SwapLock,
) -> Result<SwapRecord, SwapError> {
    let expected_identity = source_pin.identity();
    let original_p = original.as_path();
    let final_p = final_path.as_path();
    let new_p = verified_new.as_path();

    let superseded = with_added_extension(original_p, SUPERSEDED_EXT);

    // --- 1. atomically claim the backup name -------------------------------
    if let Err(e) = std::fs::hard_link(original_p, &superseded) {
        return Err(match e.kind() {
            std::io::ErrorKind::AlreadyExists => SwapError::SupersededNameOccupied(superseded),
            // EPERM/EMLINK/ENOSYS: the filesystem will not give us the atomic
            // primitive. Refuse rather than race.
            _ => SwapError::LinkUnsupported {
                path: original_p.to_path_buf(),
                message: e.to_string(),
            },
        });
    }

    // --- 2. is this still the file we planned against? ---------------------
    match identity_of(&superseded) {
        Ok(found) if found == expected_identity => {}
        Ok(found) => {
            let _ = std::fs::remove_file(&superseded);
            return Err(SwapError::SourceChangedUnderUs {
                path: original_p.to_path_buf(),
                expected: expected_identity,
                found,
            });
        }
        Err(e) => {
            let _ = std::fs::remove_file(&superseded);
            return Err(SwapError::Io {
                step: "confirming the original had not been replaced",
                message: e.to_string(),
            });
        }
    }

    // --- 3. establish the new entry BEFORE removing the old one ------------
    if final_p == original_p {
        // Same name: replace it in one step. `rename` overwrites, which is
        // exactly what is wanted here and is safe precisely because the thing
        // being overwritten is the original we just backed up.
        if let Err(e) = std::fs::rename(new_p, final_p) {
            let _ = std::fs::remove_file(&superseded);
            return Err(SwapError::Io {
                step: "moving the new file into place",
                message: format!("{e} (the original was left untouched)"),
            });
        }
    } else {
        // Different name: claim it atomically so an unrelated file already
        // sitting there is never destroyed...
        if let Err(e) = std::fs::hard_link(new_p, final_p) {
            let occupied = e.kind() == std::io::ErrorKind::AlreadyExists;
            let _ = std::fs::remove_file(&superseded);
            return Err(if occupied {
                SwapError::DestinationOccupied(final_p.to_path_buf())
            } else {
                SwapError::Io {
                    step: "claiming the destination name",
                    message: format!("{e} (the original was left untouched)"),
                }
            });
        }
        // ...and only now release the old name. The inode is reachable under
        // the backup name, so this cannot lose it.
        if let Err(e) = std::fs::remove_file(original_p) {
            // Roll back the destination we just claimed, so a partial swap
            // does not leave two live copies of the title.
            let _ = std::fs::remove_file(final_p);
            let _ = std::fs::remove_file(&superseded);
            return Err(SwapError::Io {
                step: "releasing the original name",
                message: format!("{e} (the destination link was rolled back)"),
            });
        }
        // The staging name still points at the same inode as `final_p`.
        if let Err(e) = std::fs::remove_file(new_p) {
            tracing::warn!(
                error = %e,
                path = %new_p.display(),
                "foundry: could not remove the in-library staging name after a successful swap"
            );
        }
    }

    // --- 4. make the directory entries durable -----------------------------
    if let Some(dir) = final_p.parent() {
        if let Err(e) = fsync_dir(dir) {
            // The swap itself succeeded and the library is correct right now;
            // only crash-durability is unconfirmed. Reporting failure here
            // would be wrong (the work IS done), so this is a warning that
            // names the specific residual risk.
            tracing::warn!(
                error = %e,
                dir = %dir.display(),
                "foundry: swap completed but the directory could not be fsynced — the \
                 new name may not survive a power loss"
            );
        }
    }

    Ok(SwapRecord {
        final_path: final_p.to_path_buf(),
        superseded_path: superseded,
    })
}

/// The name of the in-library staging copy for one job.
///
/// A free function purely so a test can assert on the name production code
/// actually builds. An earlier test checked only the `INFLIGHT_EXT` constant
/// and then assembled a name of its own — so a mutation that left the constant
/// alone and re-introduced `plan.container.extension()` at the call site
/// survived untouched. The test was asserting on a string it had built itself,
/// which is the definition of a decorative test.
///
/// `.part`, never the target container's extension: see [`INFLIGHT_EXT`].
fn inflight_file_name(job: uuid::Uuid) -> String {
    format!("{INFLIGHT_PREFIX}-{job}.{INFLIGHT_EXT}")
}

/// Append an extension, keeping the existing one (`a.mkv` -> `a.mkv.superseded`).
///
/// `Path::set_extension` would *replace* it, turning `a.mkv` into
/// `a.superseded` — which loses the information needed to restore the file and
/// makes two originals with different containers collide on one name.
fn with_added_extension(p: &Path, ext: &str) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// The path a rewrite of `original` into `extension` should end up at:
/// same directory, same stem, possibly a new extension.
fn final_path_for(original: &Path, extension: &str) -> PathBuf {
    let mut out = original.to_path_buf();
    out.set_extension(extension);
    out
}

// --- The impure driver -----------------------------------------------------

/// Probe, plan, and (if the gate is open and the tools exist) rewrite one file.
///
/// Never returns `Err` and never panics — the same posture as
/// [`crate::maintenance::run_trending_pass`] and every other unattended pass in
/// this crate. Every outcome, including every refusal, is a value the caller
/// can count and report.
pub(in crate::foundry) fn optimize_file(
    guard: &PathGuard,
    cfg: &FoundryConfig,
    policy: &TranscodePolicy,
    path: &Path,
) -> ForgeStatus {
    // A symlink check must happen on the path as given, before the guard
    // canonicalizes it away.
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            return ForgeStatus::Skipped {
                reason: SkipReason::SourceIsSymlink,
            }
        }
        // A missing/unreadable path is the guard's error to report, with its
        // own better message — fall through rather than duplicating it here.
        _ => {}
    }

    let resolved = match guard.resolve(path) {
        Ok(r) => r,
        Err(e) => {
            return ForgeStatus::Skipped {
                reason: SkipReason::PathRefused(e),
            }
        }
    };

    let caps = detect_capabilities(cfg);
    if !caps.can_probe() {
        return ForgeStatus::Skipped {
            reason: SkipReason::ToolUnavailable {
                tool: "ffprobe",
                detail: caps.ffprobe.summary(),
            },
        };
    }

    // Taken BEFORE the probe and held for the whole probe-plan-encode window:
    // if anything replaces the file while we are working, the swap compares
    // this against the inode it actually links and refuses. The *pin* (an open
    // descriptor, not just a stat) is what makes that comparison sound — see
    // `SourcePin`: without it the replacement can inherit the inode number and
    // the check silently passes.
    let source_pin = match SourcePin::pin(resolved.as_path()) {
        Ok(p) => p,
        Err(e) => {
            return ForgeStatus::Failed {
                reason: format!("could not pin the source file's identity: {e}"),
            }
        }
    };

    let source = match run_ffprobe(&cfg.ffprobe_bin, &resolved) {
        Ok(p) => p,
        // A tool that vanished between the capability check and the call is
        // still an absent tool, not a bad file.
        Err(ProbeError::ToolMissing { binary }) => {
            return ForgeStatus::Skipped {
                reason: SkipReason::ToolUnavailable {
                    tool: "ffprobe",
                    detail: format!("`{binary}` is not installed"),
                },
            }
        }
        // A stall says nothing about the file, so it must not be reported as
        // the file having failed. FOUNDRY-10.
        Err(ProbeError::Timeout { secs }) => {
            return ForgeStatus::Skipped {
                reason: SkipReason::ProbeTimedOut { secs },
            }
        }
        Err(e) => return ForgeStatus::Failed { reason: e.to_string() },
    };

    // The staging filename's extension has to match the container the plan
    // will ask ffmpeg for, so the container is resolved first through the
    // helper both sides share.
    let Some(container) = output_container(&source, policy) else {
        return ForgeStatus::Skipped {
            reason: SkipReason::Undecidable(Undecidable::UnrecognizedContainer {
                found: source.container.clone(),
            }),
        };
    };

    let Some(work_dir) = cfg.work_dir.clone() else {
        // Checked before planning so the operator is told about the
        // misconfiguration rather than about the file.
        return ForgeStatus::Skipped {
            reason: SkipReason::NoWorkDir,
        };
    };

    let job = uuid::Uuid::new_v4();
    let staged_name = format!("muse-foundry-{job}.{}", container.extension());
    let staged_target = work_dir.join(&staged_name);

    let decision = plan_transcode(
        &source,
        policy,
        &resolved.as_path().to_string_lossy(),
        &staged_target.to_string_lossy(),
    );

    let (plan, args, reasons) = match decision {
        TranscodeDecision::AlreadyOptimal => {
            return ForgeStatus::Skipped {
                reason: SkipReason::AlreadyOptimal,
            }
        }
        TranscodeDecision::CannotDecide { why } => {
            return ForgeStatus::Skipped {
                reason: SkipReason::Undecidable(why),
            }
        }
        TranscodeDecision::Transcode { plan, args, reasons } => (plan, args, reasons),
    };

    // Would this encode reclaim anything? FOUNDRY-22 can tell from the source
    // and the plan, before spending the CPU.
    //
    // Default OFF. The skip is a genuine trade, not a strict improvement: the
    // swap still leaves a direct-playable file in the library, which is half
    // of Path A's purpose. Only the other half — reclaiming the original's
    // space — is impossible here. So the operator chooses between "direct play
    // at double disk" and "leave it alone", and that choice is theirs.
    if std::env::var("MUSE_FOUNDRY_SKIP_UNRECLAIMABLE")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
    {
        let predicted =
            crate::foundry::directplay::predicted_deletion_refusals(&source, &plan);
        if !predicted.is_empty() {
            return ForgeStatus::Skipped {
                reason: SkipReason::UnreclaimableOriginal { predicted },
            };
        }
    }

    // Both tools, not just the encoder: without ffprobe the result could be
    // produced but never verified, and an unverified success is a failure.
    if !caps.can_transcode() {
        return ForgeStatus::Skipped {
            reason: SkipReason::ToolUnavailable {
                tool: "ffmpeg",
                detail: caps.ffmpeg.summary(),
            },
        };
    }

    // Gate check before spending an encode we would not be allowed to use.
    if let Err(e) = guard.require_mutation() {
        return ForgeStatus::Skipped {
            reason: match e {
                PathError::MutationDisabled => SkipReason::MutationDisabled,
                other => SkipReason::PathRefused(other),
            },
        };
    }

    let staged = match guard.resolve_new_for_mutation(&staged_target) {
        Ok(p) => p,
        Err(e) => {
            return ForgeStatus::Skipped {
                reason: SkipReason::PathRefused(e),
            }
        }
    };

    // Take the swap lock BEFORE the encode, not just around the swap. Holding
    // it across the encode costs nothing (it is non-blocking, so a second
    // worker skips this title and moves to another) and buys two things: two
    // workers never burn CPU encoding the same file only for one result to be
    // thrown away, and the window in which another Muse worker could replace
    // the source now covers probe-through-swap rather than the swap alone.
    let lock = match SwapLock::acquire(&work_dir, &final_path_for(
        resolved.as_path(),
        plan.container.extension(),
    )) {
        Ok(l) => l,
        Err(e) => {
            return match skip_for_lock_error(&e) {
                Some(reason) => ForgeStatus::Skipped { reason },
                None => ForgeStatus::Failed { reason: e.to_string() },
            }
        }
    };

    let bytes_before = source.size_bytes.unwrap_or(0);
    let result = run_encode_and_swap(
        guard,
        cfg,
        policy,
        &resolved,
        &source,
        &plan,
        &args,
        &staged,
        &source_pin,
        &lock,
    );

    // Unconditionally, on BOTH paths: the work-dir encode has served its
    // purpose either way (on success it was copied into the library, on
    // failure it is worthless), and a staging file left behind on the success
    // path is the version of this leak that nobody notices — it accumulates
    // one full-size encode per optimized file until the scratch filesystem
    // fills, which then presents as unrelated encode failures.
    discard_staged(&staged);

    match result {
        Ok(record) => ForgeStatus::Rewritten(RewriteRecord {
            final_path: record.final_path,
            superseded_path: record.superseded_path,
            bytes_before,
            bytes_after: record.bytes_after,
            remux_only: plan.is_remux_only(),
            reasons,
        }),
        Err(reason) => ForgeStatus::Failed { reason },
    }
}

struct CompletedSwap {
    final_path: PathBuf,
    superseded_path: PathBuf,
    bytes_after: u64,
}

/// Encode, verify, copy in, verify again, swap. Returns a human-readable
/// reason string on any failure — at which point the caller discards the
/// staged file and the original has not been touched.
#[allow(clippy::too_many_arguments)]
fn run_encode_and_swap(
    guard: &PathGuard,
    cfg: &FoundryConfig,
    policy: &TranscodePolicy,
    original: &ResolvedPath,
    source: &MediaProbe,
    plan: &TranscodePlan,
    args: &[String],
    staged: &MutablePath,
    source_pin: &SourcePin,
    lock: &SwapLock,
) -> Result<CompletedSwap, String> {
    // --- 1. encode ---------------------------------------------------------
    // The ONE process spawn on this path. Everything above and below it is
    // pure or is ordinary filesystem work.
    // Bounded, because `Command::output()` is not.
    //
    // This is the PRODUCTION encode — the one that rewrites library files —
    // and it runs while holding the swap lock for the title. An ffmpeg wedged
    // on a stalled NFS read therefore blocked this swap forever AND kept the
    // title locked against every future pass, with no way to tell that from
    // "still encoding". The probe path had exactly this defect (FOUNDRY-10/12)
    // and it was observed live; there is no reason to think the encoder is
    // immune when it reads from the same mount.
    //
    // The ceiling is deliberately generous — a 4K feature is legitimately
    // hours on a CPU — so it never fires on honest work, only on a wedge.
    let status = crate::media::probe::spawn_with_timeout(
        &cfg.ffmpeg_bin,
        args,
        cfg.encode_timeout,
    )
    .map_err(|e| match e {
        crate::media::probe::ProbeError::Timeout { secs } => format!(
            "ffmpeg did not finish within {secs}s and was abandoned — the encode is \
             incomplete and NOTHING was swapped; the original is untouched"
        ),
        other => format!("spawning ffmpeg `{}`: {other}", cfg.ffmpeg_bin),
    })?;

    if !status.status.success() {
        return Err(format!(
            "ffmpeg exited with {}: {}",
            status
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "a signal".into()),
            String::from_utf8_lossy(&status.stderr).trim().chars().take(400).collect::<String>()
        ));
    }

    // --- 2. verify the staged output --------------------------------------
    let expectation = expectation_for(source, plan, policy)
        .ok_or_else(|| "the source duration is unknown, so no output of it could be verified".to_string())?;

    let staged_probe = run_ffprobe(&cfg.ffprobe_bin, staged.as_resolved())
        .map_err(|e| format!("re-probing the staged output: {e}"))?;
    verify_output(&expectation, &staged_probe)
        .map_err(|e| format!("the staged output failed verification: {e}"))?;

    // --- 3. copy into the library and verify the copy ----------------------
    // Cross-filesystem by design (rail 3), so this is a real copy, and a copy
    // is itself something that can truncate. The copy is what becomes the
    // library file, so it is verified rather than assumed.
    let original_p = original.as_path();
    let parent = original_p
        .parent()
        .ok_or_else(|| "the source has no parent directory".to_string())?;
    let inflight_target = parent.join(inflight_file_name(uuid::Uuid::new_v4()));
    let inflight = guard
        .resolve_new_for_mutation(&inflight_target)
        .map_err(|e| format!("resolving the in-library staging path: {e}"))?;

    let copy_result = std::fs::copy(staged.as_path(), inflight.as_path())
        .map_err(|e| format!("copying the verified output into the library: {e}"));
    let bytes_after = match copy_result {
        Ok(n) => n,
        Err(e) => {
            discard_staged(&inflight);
            return Err(e);
        }
    };

    // Flush the copy to disk before verifying it. `fs::copy` leaves the bytes
    // in the page cache; verifying a file that exists only in cache and then
    // losing power leaves a library entry pointing at a partial file that our
    // own verification vouched for.
    if let Err(e) = std::fs::File::open(inflight.as_path()).and_then(|fh| fh.sync_all()) {
        discard_staged(&inflight);
        return Err(format!("flushing the in-library copy to disk: {e}"));
    }

    let inflight_probe = match run_ffprobe(&cfg.ffprobe_bin, inflight.as_resolved()) {
        Ok(p) => p,
        Err(e) => {
            discard_staged(&inflight);
            return Err(format!("re-probing the in-library copy: {e}"));
        }
    };
    if let Err(e) = verify_output(&expectation, &inflight_probe) {
        discard_staged(&inflight);
        return Err(format!(
            "the in-library copy failed verification (the original was NOT replaced): {e}"
        ));
    }

    // --- 4. swap -----------------------------------------------------------
    let final_target = final_path_for(original_p, plan.container.extension());
    let original_mut = guard
        .resolve_for_mutation(original_p)
        .map_err(|e| format!("resolving the original for mutation: {e}"))?;
    let final_mut = match guard.resolve_new_for_mutation(&final_target) {
        Ok(p) => p,
        Err(e) => {
            discard_staged(&inflight);
            return Err(format!("resolving the destination path: {e}"));
        }
    };

    // The lock was taken by `optimize_file` before the encode and is held for
    // this whole call. See `SwapLock` for what it covers and what it does not.
    match swap_verified_output(&original_mut, &inflight, &final_mut, source_pin, lock) {
        Ok(record) => Ok(CompletedSwap {
            final_path: record.final_path,
            superseded_path: record.superseded_path,
            bytes_after,
        }),
        Err(e) => {
            discard_staged(&inflight);
            Err(format!("{e}"))
        }
    }
}

/// Delete a staging file, logging rather than propagating a failure: the caller
/// is already on an error path and a leaked scratch file must not mask the real
/// reason.
fn discard_staged(p: &MutablePath) {
    if let Err(e) = std::fs::remove_file(p.as_path()) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                error = %e,
                path = %p.display(),
                "foundry: could not remove a staging file"
            );
        }
    }
}

/// Classify a lock-acquisition failure: contention is a **skip**, anything else
/// is a real failure.
///
/// Extracted from [`optimize_file`] purely so this rule is reachable by a test.
/// Inline, it was unreachable on every host in this fleet — the lock is taken
/// after the probe, and with no ffprobe installed `optimize_file` returns
/// `ToolUnavailable` long before it. A mutation that turned the skip into a
/// failure therefore survived, which is exactly the "test is decorative"
/// signal. As a free function it is directly testable.
///
/// The distinction is not cosmetic: "another worker is already handling this
/// title" needs no operator action at all, while a failure means something is
/// wrong with the lock directory or the filesystem. Reporting the first as the
/// second turns a healthy parallel run into a page of spurious errors.
fn skip_for_lock_error(e: &SwapError) -> Option<SkipReason> {
    match e {
        SwapError::LockBusy(_) => Some(SkipReason::SwapLockBusy),
        _ => None,
    }
}

/// Remove staging files left behind by a process that died mid-encode.
///
/// Nothing cleans these up today. `discard_staged` runs after
/// `run_encode_and_swap` returns — which never happens if the process is killed
/// first. A deploy, a crash, an OOM, or an operator cancelling a run all leave
/// a full-size partial encode in the work dir, permanently.
///
/// At 16,000 items, interruptions are not hypothetical: this session alone
/// cancelled several runs by hand, each leaving ~20 GB behind that had to be
/// removed manually. The accumulation eventually fills the scratch filesystem,
/// which then presents as unrelated encode failures.
///
/// ## Why age, and why THIS age
///
/// A staging file cannot be distinguished from a live one by inspection — a
/// concurrent encode's file looks identical. But a live encode is bounded by
/// `encode_timeout`, so anything older than that ceiling **plus a margin**
/// provably does not belong to a running encode: the encode that created it
/// would already have been killed by its own deadline.
///
/// That makes the rule safe without needing to know what else is running, and
/// it is why the threshold is derived from the timeout rather than being an
/// arbitrary "24 hours".
///
/// Never touches anything outside the work dir, and only files matching the
/// staging name shapes — a stray operator file in the work dir is left alone.
pub fn sweep_orphaned_staging(work_dir: &Path, encode_timeout: Duration) -> SweepReport {
    let mut report = SweepReport::default();
    // Double the ceiling: generous enough that clock skew or a slow unlink
    // cannot make a live encode look orphaned.
    let min_age = encode_timeout.saturating_mul(2);

    let Ok(entries) = std::fs::read_dir(work_dir) else {
        report.unreadable = true;
        return report;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Only the shapes forge itself creates.
        let is_staging = name.starts_with("muse-foundry-") || name.starts_with(INFLIGHT_PREFIX);
        if !is_staging || !path.is_file() {
            continue;
        }
        report.examined += 1;
        let age = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        match age {
            Some(a) if a >= min_age => {
                let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        report.removed += 1;
                        report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(bytes);
                        tracing::info!(
                            path = %path.display(),
                            age_secs = a.as_secs(),
                            "foundry: removed an orphaned staging file from a dead encode"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e,
                            "foundry: could not remove an orphaned staging file");
                        report.failed += 1;
                    }
                }
            }
            // Too young, or an unreadable mtime. Both are KEPT: an unknown age
            // cannot be shown to be orphaned, and deleting a live encode's
            // staging file would corrupt a running swap.
            _ => report.kept_live_or_unknown += 1,
        }
    }
    report
}

/// What one orphan sweep did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SweepReport {
    pub examined: usize,
    pub removed: usize,
    pub failed: usize,
    /// Kept because they are younger than the ceiling, or their age could not
    /// be read. An unknown age is never treated as old.
    pub kept_live_or_unknown: usize,
    pub bytes_reclaimed: u64,
    /// The work dir itself could not be listed, so this sweep established
    /// nothing.
    pub unreadable: bool,
}

/// Detect tooling using the configured binary names.
pub(in crate::foundry) fn detect_capabilities(cfg: &FoundryConfig) -> Capabilities {
    capability::detect(
        &cfg.ffprobe_bin,
        &cfg.ffmpeg_bin,
        &cfg.handbrake_bin,
        cfg.capability_timeout,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::foundry::policy::Container;
    use crate::foundry::probe::{AudioStream, SubtitleStream, VideoStream};
    use std::fs;

    // --- fixtures ----------------------------------------------------------

    fn probe_of(
        duration: Option<f64>,
        video_codec: &str,
        dims: (u32, u32),
        audio: usize,
        subs: usize,
        size: Option<u64>,
    ) -> MediaProbe {
        MediaProbe {
            container: "matroska,webm".to_string(),
            duration_secs: duration,
            format_bitrate_bps: Some(5_000_000),
            size_bytes: size,
            video: vec![VideoStream {
                index: 0,
                codec: video_codec.to_string(),
                width: Some(dims.0),
                height: Some(dims.1),
                bitrate_bps: Some(5_000_000),
                pix_fmt: Some("yuv420p".to_string()),
                attached_pic: false,
                ..VideoStream::default()
            }],
            audio: (0..audio)
                .map(|i| AudioStream {
                    index: 1 + i as u32,
                    codec: "aac".to_string(),
                    channels: Some(2),
                    language: Some("eng".to_string()),
                    bitrate_bps: Some(128_000),
                })
                .collect(),
            subtitles: (0..subs)
                .map(|i| SubtitleStream {
                    index: 10 + i as u32,
                    codec: "subrip".to_string(),
                    language: Some("eng".to_string()),
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

    fn source() -> MediaProbe {
        probe_of(Some(5400.0), "h264", (1920, 1080), 1, 1, Some(4_000_000_000))
    }

    fn encode_plan(scale: Option<(u32, u32)>) -> TranscodePlan {
        TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale },
            audio: AudioAction::Encode { channels: vec![2] },
            container: Container::Matroska,
        }
    }

    fn remux_plan() -> TranscodePlan {
        TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Copy,
            audio: AudioAction::Copy,
            container: Container::Matroska,
        }
    }

    fn expect_for(plan: &TranscodePlan) -> VerifyExpectation {
        expectation_for(&source(), plan, &TranscodePolicy::default())
            .expect("the source fixture has a duration")
    }

    // --- verify_output -----------------------------------------------------

    #[test]
    fn a_faithful_output_verifies() {
        let expect = expect_for(&encode_plan(None));
        let out = probe_of(Some(5400.0), "h264", (1920, 1080), 1, 1, Some(2_000_000_000));
        assert_eq!(verify_output(&expect, &out), Ok(()));
    }

    #[test]
    fn a_truncated_output_is_rejected() {
        // THE case this whole module exists for: ffmpeg exits 0 having written
        // half the film. Nothing else about the file is wrong.
        let expect = expect_for(&encode_plan(None));
        let out = probe_of(Some(2700.0), "h264", (1920, 1080), 1, 1, Some(1_000_000_000));
        assert!(
            matches!(
                verify_output(&expect, &out),
                Err(VerifyFailure::DurationMismatch { .. })
            ),
            "a half-length output must never be accepted"
        );
    }

    #[test]
    fn a_barely_truncated_output_is_still_rejected() {
        // 5400s source, 1% relative tolerance = 54s. 100s short must fail.
        let expect = expect_for(&encode_plan(None));
        let out = probe_of(Some(5300.0), "h264", (1920, 1080), 1, 1, Some(3_000_000_000));
        assert!(matches!(
            verify_output(&expect, &out),
            Err(VerifyFailure::DurationMismatch { .. })
        ));
    }

    #[test]
    fn normal_container_rounding_is_tolerated() {
        // A remux legitimately shifts the container duration by a frame or
        // two; a check that rejected that would reject every good output.
        let expect = expect_for(&remux_plan());
        for secs in [5400.0, 5400.04, 5399.96, 5401.5, 5398.5, 5440.0, 5360.0] {
            let out = probe_of(Some(secs), "h264", (1920, 1080), 1, 1, Some(3_000_000_000));
            assert_eq!(verify_output(&expect, &out), Ok(()), "rejected {secs}s");
        }
    }

    #[test]
    fn an_output_whose_duration_is_unknown_is_rejected_not_waved_through() {
        // Fail closed: an output we cannot measure is an output we cannot
        // clear for a destructive swap.
        let expect = expect_for(&encode_plan(None));
        let out = probe_of(None, "h264", (1920, 1080), 1, 1, Some(2_000_000_000));
        assert_eq!(verify_output(&expect, &out), Err(VerifyFailure::UnknownDuration));
    }

    #[test]
    fn an_empty_or_unmeasurable_output_is_rejected() {
        let expect = expect_for(&encode_plan(None));
        for size in [Some(0), None] {
            let out = probe_of(Some(5400.0), "h264", (1920, 1080), 1, 1, size);
            assert_eq!(
                verify_output(&expect, &out),
                Err(VerifyFailure::EmptyOutput),
                "size {size:?} must be rejected"
            );
        }
    }

    #[test]
    fn an_output_with_no_video_is_rejected() {
        let expect = expect_for(&encode_plan(None));
        let mut out = probe_of(Some(5400.0), "h264", (1920, 1080), 1, 1, Some(2_000_000_000));
        out.video.clear();
        assert_eq!(verify_output(&expect, &out), Err(VerifyFailure::NoVideoStream));
    }

    #[test]
    fn an_encode_that_produced_the_wrong_codec_is_rejected() {
        // Proves the encoder did the thing that was asked, rather than
        // inferring it from a zero exit code.
        let expect = expect_for(&encode_plan(None));
        assert_eq!(expect.video_codec, "h264");
        let out = probe_of(Some(5400.0), "mpeg4", (1920, 1080), 1, 1, Some(2_000_000_000));
        assert!(matches!(
            verify_output(&expect, &out),
            Err(VerifyFailure::VideoCodecMismatch { .. })
        ));
    }

    #[test]
    fn a_remux_that_silently_re_encoded_is_rejected() {
        // A stream copy must come out bit-identical in codec terms. If the
        // codec changed, the "lossless remux" claim in the report is false.
        let expect = expect_for(&remux_plan());
        assert_eq!(expect.video_codec, "h264", "a copy expects the SOURCE codec");
        let out = probe_of(Some(5400.0), "hevc", (1920, 1080), 1, 1, Some(2_000_000_000));
        assert!(matches!(
            verify_output(&expect, &out),
            Err(VerifyFailure::VideoCodecMismatch { .. })
        ));
    }

    #[test]
    fn a_remux_expects_the_sources_own_codec_not_the_encoders() {
        // Added because a mutation SURVIVED the test above: it used an H.264
        // source, whose codec happens to equal the encoder's own output codec
        // (`h264`), so swapping `source_video.codec` for
        // `policy.encode_video.probe_codec_name()` made no observable
        // difference and the test could not tell the two rules apart.
        //
        // An HEVC source separates them. A stream copy of HEVC must expect
        // HEVC out; expecting the encoder's codec would reject every correct
        // HEVC remux, and would accept an HEVC source that ffmpeg silently
        // re-encoded to H.264 — exactly backwards.
        let mut s = source();
        s.video[0].codec = "hevc".to_string();
        let expect = expectation_for(&s, &remux_plan(), &TranscodePolicy::default())
            .expect("the fixture has a duration");
        assert_eq!(
            expect.video_codec, "hevc",
            "a copy must expect the SOURCE codec, not the encoder's"
        );

        let faithful = probe_of(Some(5400.0), "hevc", (1920, 1080), 1, 1, Some(2_000_000_000));
        assert_eq!(
            verify_output(&expect, &faithful),
            Ok(()),
            "a correct HEVC remux must not be rejected"
        );

        let silently_reencoded =
            probe_of(Some(5400.0), "h264", (1920, 1080), 1, 1, Some(2_000_000_000));
        assert!(
            matches!(
                verify_output(&expect, &silently_reencoded),
                Err(VerifyFailure::VideoCodecMismatch { .. })
            ),
            "an HEVC source that came out as H.264 was re-encoded, not copied"
        );
    }

    #[test]
    fn a_downscale_that_did_not_happen_is_rejected() {
        let expect = expect_for(&encode_plan(Some((1920, 1080))));
        let out = probe_of(Some(5400.0), "h264", (3840, 2160), 1, 1, Some(2_000_000_000));
        assert!(matches!(
            verify_output(&expect, &out),
            Err(VerifyFailure::DimensionsMismatch { .. })
        ));
    }

    #[test]
    fn dimensions_are_only_asserted_when_a_downscale_was_ordered() {
        // Asserting the source dimensions on a no-scale encode would be a
        // second, redundant claim that a legitimate anamorphic fix-up trips.
        let expect = expect_for(&encode_plan(None));
        assert_eq!(expect.video_dimensions, None);
        let out = probe_of(Some(5400.0), "h264", (1918, 1080), 1, 1, Some(2_000_000_000));
        assert_eq!(verify_output(&expect, &out), Ok(()));
    }

    #[test]
    fn a_lost_audio_or_subtitle_track_is_rejected() {
        let expect = expect_for(&encode_plan(None));
        let out = probe_of(Some(5400.0), "h264", (1920, 1080), 0, 1, Some(2_000_000_000));
        assert_eq!(
            verify_output(&expect, &out),
            Err(VerifyFailure::AudioStreamCountMismatch { expected: 1, found: 0 })
        );

        let out = probe_of(Some(5400.0), "h264", (1920, 1080), 1, 0, Some(2_000_000_000));
        assert_eq!(
            verify_output(&expect, &out),
            Err(VerifyFailure::SubtitleStreamCountMismatch { expected: 1, found: 0 })
        );
    }

    // --- verification now covers what the argv actually promised ----------

    /// A source with the full range of things the argv promises to carry:
    /// languages, dispositions, fonts, chapters and a title.
    fn rich_source() -> MediaProbe {
        let mut p = probe_of(Some(1420.0), "h264", (1920, 1080), 2, 2, Some(4_000_000_000));
        p.audio[0].language = Some("jpn".to_string());
        p.audio[1].language = Some("eng".to_string());
        p.subtitles[0].language = Some("eng".to_string());
        p.subtitles[0].forced = true;
        p.subtitles[1].language = Some("eng".to_string());
        p.subtitles[1].default = true;
        p.attachments = vec![
            crate::foundry::probe::AttachmentStream {
                index: 20,
                codec: "ttf".into(),
                filename: Some("Gandhi Sans Bold.ttf".into()),
            },
            crate::foundry::probe::AttachmentStream {
                index: 21,
                codec: "otf".into(),
                filename: Some("Trebuchet MS.otf".into()),
            },
        ];
        p.chapter_count = 4;
        p.title = Some("Cowboy Bebop - 01".to_string());
        p
    }

    fn rich_expect() -> VerifyExpectation {
        expectation_for(&rich_source(), &remux_plan(), &TranscodePolicy::default())
            .expect("the fixture has a duration and channel counts")
    }

    #[test]
    fn a_faithful_rich_output_verifies() {
        assert_eq!(verify_output(&rich_expect(), &rich_source()), Ok(()));
    }

    #[test]
    fn dropped_subtitle_fonts_are_rejected() {
        // The silent-media-loss regression. For anime and many foreign
        // releases these attachments ARE the subtitle styling; losing them
        // while reporting the file "rewritten" is a false success claim.
        let mut out = rich_source();
        out.attachments.clear();
        assert!(
            matches!(
                verify_output(&rich_expect(), &out),
                Err(VerifyFailure::AttachmentsMismatch { .. })
            ),
            "a rewrite that dropped every font must not be accepted"
        );
    }

    #[test]
    fn a_substituted_font_is_rejected_even_though_the_count_matches() {
        // Precisely what a count-only check cannot see.
        let mut out = rich_source();
        out.attachments[1].filename = Some("Comic Sans MS.ttf".into());
        assert_eq!(out.attachments.len(), rich_expect().attachment_filenames.len());
        assert!(matches!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::AttachmentsMismatch { .. })
        ));
    }

    #[test]
    fn lost_chapters_are_rejected() {
        // `-map_chapters 0` is a promise; this is what makes it checkable.
        let mut out = rich_source();
        out.chapter_count = 0;
        assert_eq!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::ChapterCountMismatch { expected: 4, found: 0 })
        );
    }

    #[test]
    fn a_lost_container_title_is_rejected() {
        let mut out = rich_source();
        out.title = None;
        assert!(matches!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::TitleMismatch { .. })
        ));
    }

    #[test]
    fn an_audio_track_that_lost_its_language_is_rejected() {
        // A track without a language stops being auto-selectable and looks
        // like a duplicate to the player — a real regression a count check
        // cannot see.
        let mut out = rich_source();
        out.audio[0].language = None;
        assert!(matches!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::AudioStreamMismatch { ordinal: 0, .. })
        ));
    }

    #[test]
    fn an_audio_track_that_came_out_in_the_wrong_codec_is_rejected() {
        // Added because a mutation SURVIVED: deleting the output-side audio
        // codec check changed nothing observable. The test that was supposed
        // to cover it only exercised `expectation_for` (what we *intend*),
        // never `verify_output` (what we *got*) — so the rule that an audio
        // stream must actually come out in the expected codec was untested.
        //
        // This matters on both plan shapes: a stream copy that silently
        // re-encoded, and an encode that produced something other than AAC.
        let expect = rich_expect();
        assert_eq!(expect.audio[0].codec, "aac", "the remux fixture copies AAC");

        let mut out = rich_source();
        out.audio[0].codec = "mp3".to_string();
        assert!(
            matches!(
                verify_output(&expect, &out),
                Err(VerifyFailure::AudioStreamMismatch { ordinal: 0, .. })
            ),
            "an audio stream that came out in the wrong codec must be rejected"
        );

        // And on the encode path, where the expected codec is the encoder's.
        let mut s = rich_source();
        s.audio[0].codec = "truehd".to_string();
        // `rich_source` has TWO audio streams, so the plan must state a target
        // for both — a short vector now refuses rather than filling the gap.
        let enc_plan = TranscodePlan {
            audio: AudioAction::Encode { channels: vec![6, 2] },
            ..encode_plan(None)
        };
        let enc = expectation_for(&s, &enc_plan, &TranscodePolicy::default()).unwrap();
        assert_eq!(enc.audio[0].codec, "aac");
        let mut out = rich_source();
        out.audio[0].codec = "ac3".to_string();
        assert!(
            matches!(
                verify_output(&enc, &out),
                Err(VerifyFailure::AudioStreamMismatch { ordinal: 0, .. })
            ),
            "an encode that produced AC3 instead of AAC must be rejected"
        );
    }

    #[test]
    fn re_ordered_audio_tracks_are_rejected() {
        // Same set, different order: the default track the player picks
        // changes. A set comparison would pass this; a positional one catches it.
        let mut out = rich_source();
        out.audio.swap(0, 1);
        assert!(matches!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::AudioStreamMismatch { .. })
        ));
    }

    #[test]
    fn an_audio_track_with_the_wrong_channel_count_is_rejected() {
        let mut out = rich_source();
        out.audio[0].channels = Some(1);
        assert!(matches!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::AudioStreamMismatch { ordinal: 0, .. })
        ));

        // And an unreadable count is a rejection, not a skipped check.
        let mut out = rich_source();
        out.audio[0].channels = None;
        assert!(matches!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::AudioStreamMismatch { ordinal: 0, .. })
        ));
    }

    #[test]
    fn a_subtitle_that_lost_its_forced_disposition_is_rejected() {
        // This decides whether a forced-narrative track auto-displays. Losing
        // it is invisible in a count and very visible on screen.
        let mut out = rich_source();
        out.subtitles[0].forced = false;
        assert!(matches!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::SubtitleStreamMismatch { ordinal: 0, .. })
        ));

        let mut out = rich_source();
        out.subtitles[1].default = false;
        assert!(matches!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::SubtitleStreamMismatch { ordinal: 1, .. })
        ));
    }

    #[test]
    fn a_subtitle_that_changed_codec_or_language_is_rejected() {
        let mut out = rich_source();
        out.subtitles[0].codec = "ass".to_string();
        assert!(matches!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::SubtitleStreamMismatch { ordinal: 0, .. })
        ));

        let mut out = rich_source();
        out.subtitles[1].language = Some("fra".to_string());
        assert!(matches!(
            verify_output(&rich_expect(), &out),
            Err(VerifyFailure::SubtitleStreamMismatch { ordinal: 1, .. })
        ));
    }

    #[test]
    fn an_audio_re_encode_expects_the_downmix_it_actually_ordered() {
        // The encode path promises `-c:a aac -ac N`; the expectation must
        // follow that, and must NOT "downmix" a source already below the
        // ceiling (`-ac` sets the count exactly).
        let mut s = rich_source();
        s.audio[0].channels = Some(8);
        s.audio[1].channels = Some(2);
        // The plan CARRIES the per-stream targets, already clamped by
        // `plan_transcode`: 8ch -> the 6ch ceiling, 2ch stays 2ch. The
        // expectation reads them rather than deriving its own, which is the
        // whole point — when the two derived independently, the argv upmixed
        // stereo to 6 while this expected 2.
        let plan = TranscodePlan {
            audio: AudioAction::Encode { channels: vec![6, 2] },
            ..encode_plan(None)
        };
        let expect = expectation_for(&s, &plan, &TranscodePolicy::default()).unwrap();
        assert_eq!(expect.audio[0].codec, "aac");
        assert_eq!(expect.audio[0].channels, 6, "8ch must be downmixed to the ceiling");
        assert_eq!(
            expect.audio[1].channels, 2,
            "a stereo track must stay stereo, not be inflated to the ceiling"
        );

        // Distinguishes "reads the plan" from "recomputes min(source, ceiling)".
        // With source [8,2] and plan [6,2] both readings agree, so the
        // assertions above cannot tell them apart — opus and free both said so
        // at the FOUNDRY-08 gate, and they were right. A plan value that could
        // NOT arise from min(source, ceiling) forces the distinction.
        let odd = TranscodePlan {
            audio: AudioAction::Encode { channels: vec![4, 1] },
            ..encode_plan(None)
        };
        let e2 = expectation_for(&s, &odd, &TranscodePolicy::default()).unwrap();
        assert_eq!(
            (e2.audio[0].channels, e2.audio[1].channels),
            (4, 1),
            "the expectation must take the PLAN's numbers, not recompute its own"
        );

        // And a plan whose vector is shorter than the audio stream list
        // REFUSES rather than filling the gap with a locally-derived value.
        let short = TranscodePlan {
            audio: AudioAction::Encode { channels: vec![6] },
            ..encode_plan(None)
        };
        assert!(
            expectation_for(&s, &short, &TranscodePolicy::default()).is_none(),
            "a plan that does not state a target for every audio stream must refuse"
        );
        assert_eq!(
            expect.audio[0].language.as_deref(),
            Some("jpn"),
            "a re-encode must still carry the language"
        );
    }

    #[test]
    fn a_source_with_an_unknown_channel_count_yields_no_expectation_at_all() {
        // Consistent with the duration rule: the downmix target is not
        // computable, so no expectation can be built and no swap authorized.
        let mut s = rich_source();
        s.audio[0].channels = None;
        assert_eq!(
            expectation_for(&s, &encode_plan(None), &TranscodePolicy::default()),
            None
        );
    }

    #[test]
    fn a_source_with_no_duration_yields_no_expectation_at_all() {
        // The second, independent refusal of the unverifiable case: even if
        // the planner were changed to plan such a file, no expectation could
        // be built for it, so no swap could be authorized.
        let mut s = source();
        s.duration_secs = None;
        assert_eq!(
            expectation_for(&s, &encode_plan(None), &TranscodePolicy::default()),
            None
        );
    }

    // --- swap mechanics (no media tooling involved) ------------------------

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "muse-forge-{tag}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(p.join("lib")).unwrap();
            fs::create_dir_all(p.join("work")).unwrap();
            Self(fs::canonicalize(&p).unwrap())
        }
        fn lib(&self) -> PathBuf {
            self.0.join("lib")
        }
        fn work(&self) -> PathBuf {
            self.0.join("work")
        }
        fn guard(&self) -> PathGuard {
            PathGuard::new([self.lib(), self.work()], true)
        }
        /// A real `SwapLock` over this fixture's work dir, so the swap tests
        /// exercise the same locked path production takes.
        fn lock(&self, target: &Path) -> SwapLock {
            SwapLock::acquire(&self.work(), target).expect("uncontended lock must be available")
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_swap_puts_the_new_file_in_place_and_keeps_the_original() {
        let t = Tmp::new("swap-ok");
        let g = t.guard();
        let original = t.lib().join("Movie.mkv");
        let staged = t.lib().join(".muse-foundry-inflight-1.mkv");
        fs::write(&original, b"ORIGINAL").unwrap();
        fs::write(&staged, b"NEW-VERIFIED").unwrap();

        let id = SourcePin::pin(&original).unwrap();
        let rec = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&original).unwrap(),
            &id,
            &t.lock(&original),
        )
        .expect("the swap must succeed");

        assert_eq!(fs::read(&original).unwrap(), b"NEW-VERIFIED");
        assert_eq!(
            fs::read(&rec.superseded_path).unwrap(),
            b"ORIGINAL",
            "the original must still exist, byte-for-byte"
        );
        assert!(!staged.exists(), "the staging file was moved, not copied");
    }

    #[test]
    fn the_superseded_name_keeps_the_original_extension() {
        // `set_extension` would turn Movie.mkv into Movie.muse-superseded,
        // losing what it was and colliding with a superseded Movie.avi.
        let t = Tmp::new("swap-ext");
        let g = t.guard();
        let original = t.lib().join("Movie.avi");
        let staged = t.lib().join(".inflight.mkv");
        fs::write(&original, b"O").unwrap();
        fs::write(&staged, b"N").unwrap();

        let id = SourcePin::pin(&original).unwrap();
        let rec = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(t.lib().join("Movie.mkv")).unwrap(),
            &id,
            &t.lock(&t.lib().join("Movie.mkv")),
        )
        .unwrap();

        assert_eq!(
            rec.superseded_path.file_name().unwrap(),
            "Movie.avi.muse-superseded"
        );
        assert_eq!(rec.final_path.file_name().unwrap(), "Movie.mkv");
        assert!(!original.exists(), "the .avi name is gone");
    }

    #[test]
    fn the_inflight_staging_copy_never_carries_a_media_extension() {
        // The in-library staging copy is written by a byte copy, so it is
        // genuinely partial for the duration. An earlier version named it with
        // the TARGET CONTAINER's extension (`.mkv`), which put a half-written
        // file in the library under an extension the scanner selects on — a
        // partial file ingestible as a real title.
        //
        // `.part` is what stops that, so it is pinned here rather than left as
        // a constant nobody checks.
        assert_eq!(INFLIGHT_EXT, "part");
        for media in ["mkv", "mp4", "avi", "ts", "asf", "flv", "m4v", "mov", "webm"] {
            assert_ne!(
                INFLIGHT_EXT, media,
                "the staging copy must not be selectable by the library scanner"
            );
        }
        // And the name PRODUCTION builds really uses it. Asserting on a name
        // the test assembles itself proves nothing about the call site.
        let name = inflight_file_name(uuid::Uuid::new_v4());
        assert!(name.starts_with(".muse-foundry-inflight-"), "got {name}");
        assert!(name.ends_with(".part"), "got {name}");
        for media in ["mkv", "mp4", "avi", "ts", "asf", "flv"] {
            assert!(
                !name.ends_with(&format!(".{media}")),
                "the staging name must never carry a media extension, got {name}"
            );
        }
    }

    #[test]
    fn every_container_extension_differs_from_the_inflight_extension() {
        // Guards the same rule from the other direction: adding a container
        // whose extension happened to be `part` would silently re-open it.
        for c in [
            Container::Matroska,
            Container::Mp4,
            Container::Avi,
            Container::MpegTs,
            Container::Asf,
            Container::Flv,
        ] {
            assert_ne!(c.extension(), INFLIGHT_EXT, "{c:?} collides with the staging name");
        }
    }

    #[test]
    fn the_superseded_file_does_not_look_like_media_to_the_library_scanner() {
        // If it did, the scanner would re-ingest the original as a duplicate
        // of the file that just replaced it.
        assert_ne!(SUPERSEDED_EXT, "mkv");
        assert!(!SUPERSEDED_EXT.is_empty());
        let p = with_added_extension(Path::new("/lib/Movie.mkv"), SUPERSEDED_EXT);
        assert_eq!(p, PathBuf::from("/lib/Movie.mkv.muse-superseded"));
    }

    #[test]
    fn a_container_changing_swap_also_leaves_no_staging_file() {
        // Added because a mutation SURVIVED. The swap has two shapes, and they
        // dispose of the staging name differently: when the destination name
        // is unchanged, `rename` consumes it; when the container changes, the
        // staging name is a separate link that must be explicitly removed.
        // The existing "exactly two names" test only covered the first shape,
        // so deleting the explicit removal in the second was invisible — it
        // would have leaked a full-size hard link into the library directory on
        // every container conversion.
        let t = Tmp::new("swap-tidy-convert");
        let g = t.guard();
        let original = t.lib().join("Movie.avi");
        let staged = t.lib().join(".muse-foundry-inflight-x.mkv");
        fs::write(&original, b"ORIGINAL").unwrap();
        fs::write(&staged, b"NEW-VERIFIED").unwrap();
        let id = SourcePin::pin(&original).unwrap();

        swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(t.lib().join("Movie.mkv")).unwrap(),
            &id,
            &t.lock(&t.lib().join("Movie.mkv")),
        )
        .expect("the conversion swap must succeed");

        assert!(!staged.exists(), "the staging name must not survive the swap");
        let mut names: Vec<String> = fs::read_dir(t.lib())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["Movie.avi.muse-superseded", "Movie.mkv"],
            "exactly the new file and the backup, nothing else"
        );
        assert_eq!(fs::read(t.lib().join("Movie.mkv")).unwrap(), b"NEW-VERIFIED");
    }

    #[test]
    fn the_swap_refuses_to_clobber_a_different_existing_file() {
        // An .avi becoming an .mkv must not destroy an unrelated .mkv that was
        // already sitting next to it.
        let t = Tmp::new("swap-occupied");
        let g = t.guard();
        let original = t.lib().join("Movie.avi");
        let bystander = t.lib().join("Movie.mkv");
        let staged = t.lib().join(".inflight.mkv");
        fs::write(&original, b"ORIGINAL").unwrap();
        fs::write(&bystander, b"SOMEONE-ELSES-FILE").unwrap();
        fs::write(&staged, b"NEW").unwrap();

        let id = SourcePin::pin(&original).unwrap();
        let err = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&bystander).unwrap(),
            &id,
            &t.lock(&bystander),
        )
        .unwrap_err();

        assert!(matches!(err, SwapError::DestinationOccupied(_)), "got {err:?}");
        assert_eq!(fs::read(&bystander).unwrap(), b"SOMEONE-ELSES-FILE");
        assert_eq!(fs::read(&original).unwrap(), b"ORIGINAL", "and nothing moved");
    }

    #[test]
    fn the_swap_refuses_to_destroy_a_previous_runs_original() {
        let t = Tmp::new("swap-super-occupied");
        let g = t.guard();
        let original = t.lib().join("Movie.mkv");
        let staged = t.lib().join(".inflight.mkv");
        fs::write(&original, b"CURRENT").unwrap();
        fs::write(t.lib().join("Movie.mkv.muse-superseded"), b"OLDER-ORIGINAL").unwrap();
        fs::write(&staged, b"NEW").unwrap();

        let id = SourcePin::pin(&original).unwrap();
        let err = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&original).unwrap(),
            &id,
            &t.lock(&original),
        )
        .unwrap_err();

        assert!(matches!(err, SwapError::SupersededNameOccupied(_)), "got {err:?}");
        assert_eq!(
            fs::read(t.lib().join("Movie.mkv.muse-superseded")).unwrap(),
            b"OLDER-ORIGINAL"
        );
        assert_eq!(fs::read(&original).unwrap(), b"CURRENT");
    }

    #[test]
    fn a_failed_swap_leaves_the_original_exactly_where_it_was() {
        // The new file vanishing mid-swap (a concurrent cleaner, a scratch
        // reaper) must leave the library exactly as it was. Note what this
        // asserts NOW versus before: the swap no longer moves the original
        // aside first, so there is no rollback to perform — the original was
        // never moved at all. That is the stronger property, and it is why
        // there is no longer a crash point that leaves the title unreachable.
        let t = Tmp::new("swap-rollback");
        let g = t.guard();
        let original = t.lib().join("Movie.mkv");
        let staged = t.lib().join(".inflight.mkv");
        fs::write(&original, b"ORIGINAL").unwrap();
        fs::write(&staged, b"NEW").unwrap();

        let id = SourcePin::pin(&original).unwrap();
        let original_mut = g.resolve_for_mutation(&original).unwrap();
        let staged_mut = g.resolve_for_mutation(&staged).unwrap();
        let final_mut = g.resolve_new_for_mutation(&original).unwrap();
        // ...and now it is gone, after resolution and before the swap.
        fs::remove_file(&staged).unwrap();

        let err =
            swap_verified_output(&original_mut, &staged_mut, &final_mut, &id, &t.lock(&original))
                .unwrap_err();

        assert!(matches!(err, SwapError::Io { .. }), "got {err:?}");
        assert!(
            err.to_string().contains("left untouched"),
            "the failure must say the original was never moved, got {err}"
        );
        assert_eq!(
            fs::read(&original).unwrap(),
            b"ORIGINAL",
            "the original must still be in place, byte-for-byte"
        );
        assert!(
            !t.lib().join("Movie.mkv.muse-superseded").exists(),
            "and the backup name must not be left behind"
        );
    }

    // --- the swap lock -----------------------------------------------------

    #[test]
    fn lock_contention_is_a_skip_but_a_broken_lock_is_a_failure() {
        // Added because a mutation SURVIVED: turning the busy-lock skip into a
        // hard failure was invisible, since nothing could reach that code path
        // on a host with no ffprobe (the lock is taken after the probe). The
        // rule now lives in a free function so it is actually reachable.
        //
        // The distinction matters operationally: "another worker has this
        // title" needs no response, while a lock that cannot be created at all
        // is a real fault. Collapsing them turns a healthy parallel run into a
        // page of spurious errors.
        assert_eq!(
            skip_for_lock_error(&SwapError::LockBusy(PathBuf::from("/lib/Movie.mkv"))),
            Some(SkipReason::SwapLockBusy),
            "contention must be reported as a skip"
        );

        for e in [
            SwapError::Io { step: "creating the Foundry lock directory", message: "EROFS".into() },
            SwapError::LinkUnsupported { path: PathBuf::from("/lib/a"), message: "EPERM".into() },
            SwapError::DestinationOccupied(PathBuf::from("/lib/b")),
        ] {
            assert_eq!(
                skip_for_lock_error(&e),
                None,
                "a genuine lock fault must not be silently downgraded to a skip: {e:?}"
            );
        }
    }

    #[test]
    fn a_second_swap_on_the_same_file_cannot_take_the_lock() {
        // The property the lock exists for: two Muse workers cannot be inside
        // the swap for the same title at once.
        let t = Tmp::new("lock-excl");
        let target = t.lib().join("Movie.mkv");

        let held = SwapLock::acquire(&t.work(), &target).expect("first acquire");
        let second = SwapLock::acquire(&t.work(), &target);
        assert!(
            matches!(second, Err(SwapError::LockBusy(_))),
            "a second holder must be refused"
        );

        // ...and it is non-blocking: the refusal came back rather than hanging.
        drop(held);
        // Report the ACTUAL error rather than asserting `is_ok()`. A bare
        // `is_ok()` here blames the release path for any failure, including an
        // `open()` that failed for unrelated reasons under parallel test load —
        // an assertion message that misattributes its own cause is the same
        // class of false claim this module is built to avoid.
        if let Err(e) = SwapLock::acquire(&t.work(), &target) {
            panic!("the lock must be released when the holder is dropped, but got: {e}");
        }
    }

    #[test]
    fn the_lock_covers_every_name_one_swap_touches() {
        // A swap converting Movie.avi -> Movie.mkv touches Movie.avi,
        // Movie.avi.muse-superseded and Movie.mkv. Keying on the full path
        // would let a second worker converting the same title from a different
        // container run concurrently, which is exactly the collision the lock
        // is for — so the key is the directory + stem.
        let t = Tmp::new("lock-key");
        let _held = SwapLock::acquire(&t.work(), &t.lib().join("Movie.mkv")).unwrap();
        assert!(
            matches!(
                SwapLock::acquire(&t.work(), &t.lib().join("Movie.avi")),
                Err(SwapError::LockBusy(_))
            ),
            "the same title in a different container must share the lock"
        );
    }

    #[test]
    fn the_lock_is_reliably_released_under_concurrent_load() {
        // Guards the explicit `LOCK_UN` in `SwapLock::drop`. Leaving release to
        // the descriptor close was measurably unreliable under parallel load
        // (see the note beside that impl), and a single-threaded
        // acquire/drop/re-acquire check does NOT detect it — it passed 500
        // iterations with the bug present. Concurrency is what surfaces it, so
        // the guard has to be concurrent too, or it is decorative.
        let t = Tmp::new("lock-release-load");
        let work = t.work();
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let work = work.clone();
                let target = t.lib().join(format!("Title {n}.mkv"));
                std::thread::spawn(move || {
                    for i in 0..200 {
                        let held = SwapLock::acquire(&work, &target)
                            .unwrap_or_else(|e| panic!("thread {n} iter {i}: first acquire: {e}"));
                        // The failed second acquire matters: it opens and closes
                        // a second descriptor to the same file, which is the
                        // shape that made implicit release unreliable.
                        assert!(
                            SwapLock::acquire(&work, &target).is_err(),
                            "thread {n} iter {i}: exclusion must hold"
                        );
                        drop(held);
                        if let Err(e) = SwapLock::acquire(&work, &target) {
                            panic!("thread {n} iter {i}: lock not released after drop: {e}");
                        }
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().expect("no thread may fail");
        }
    }

    #[test]
    fn the_same_stem_in_different_directories_does_not_contend() {
        // Added because a mutation SURVIVED: `different_titles_do_not_contend`
        // compares two stems in the SAME directory, so dropping the directory
        // from the lock key changed nothing there.
        //
        // This is the case that exposes it, and it is not exotic — conventional
        // episode naming means half the library shares a stem. Without the
        // directory in the key, every show's `S01E01.mkv` would serialize
        // against every other show's, so the whole library would process one
        // episode at a time.
        let t = Tmp::new("lock-dirs");
        let a = t.lib().join("Show A").join("Season 01").join("S01E01.mkv");
        let b = t.lib().join("Show B").join("Season 01").join("S01E01.mkv");

        let _held = SwapLock::acquire(&t.work(), &a).expect("first acquire");
        assert!(
            SwapLock::acquire(&t.work(), &b).is_ok(),
            "the same stem under a different directory is an unrelated title \
             and must not contend"
        );
    }

    #[test]
    fn different_titles_do_not_contend() {
        // The flip side: the lock must not serialize the whole library.
        let t = Tmp::new("lock-indep");
        let _a = SwapLock::acquire(&t.work(), &t.lib().join("A.mkv")).unwrap();
        assert!(
            SwapLock::acquire(&t.work(), &t.lib().join("B.mkv")).is_ok(),
            "unrelated titles must proceed in parallel"
        );
    }

    #[test]
    fn the_lockfile_lives_outside_the_library() {
        // Two reasons, both real on <host> today: scratch state inside the media
        // tree is what safety rail 3 forbids, and the library is mounted
        // READ-ONLY there (MUSE_LIBRARY_ROOT=/srv/media is ro), so a lockfile
        // in the library could not even be created.
        let t = Tmp::new("lock-location");
        let _held = SwapLock::acquire(&t.work(), &t.lib().join("Movie.mkv")).unwrap();

        let locks: Vec<_> = fs::read_dir(t.work().join("locks")).unwrap().collect();
        assert_eq!(locks.len(), 1, "the lockfile belongs in the work dir");
        assert!(
            fs::read_dir(t.lib()).unwrap().next().is_none(),
            "and nothing may be written into the library to take a lock"
        );
    }

    #[test]
    fn a_stale_lockfile_from_a_dead_process_does_not_block_forever() {
        // The reason this is flock and not a create_new() lockfile: an
        // existence-based lock left behind by a killed process blocks every
        // future run until someone deletes it by hand. flock is released by the
        // kernel when the descriptor closes, including on SIGKILL.
        let t = Tmp::new("lock-stale");
        let target = t.lib().join("Movie.mkv");
        drop(SwapLock::acquire(&t.work(), &target).unwrap());

        let locks: Vec<_> = fs::read_dir(t.work().join("locks")).unwrap().collect();
        assert_eq!(locks.len(), 1, "the lockfile is deliberately left on disk");
        assert!(
            SwapLock::acquire(&t.work(), &target).is_ok(),
            "a leftover lockfile must not be mistaken for a held lock"
        );
    }

    #[test]
    fn the_swap_refuses_when_the_source_was_replaced_after_it_was_probed() {
        // The third TOCTOU case from review. The plan describes the file we
        // probed; if something replaced it in the meantime, applying that plan
        // would move a NEWER, unrelated file aside and replace it with an
        // encode of something else entirely.
        //
        // HISTORY — MUSE #148. This test was red in full-suite runs for the
        // whole Foundry epic and was written off as "pre-existing" three times.
        // It was not flaky and it was not wrong: it was correctly reporting
        // that the guard did not work, and the *visibility* of that was what
        // varied. The guard compared a bare `(dev, ino)` captured before the
        // probe, and an inode NUMBER is recycled the moment its inode is
        // released — so the replacement written on the next line could inherit
        // the deleted file's number and compare equal. Whether it did depended
        // on the filesystem under $TMPDIR (tmpfs never reuses numbers, ext4 and
        // the real NFS library do) and on what else in a parallel suite
        // allocated an inode in between. `SourcePin` fixes it at the root by
        // holding the probed inode open, which makes recycling impossible. Do
        // not weaken this back to `identity_of`.
        let t = Tmp::new("swap-changed");
        let g = t.guard();
        let original = t.lib().join("Movie.mkv");
        let staged = t.lib().join(".inflight.mkv");
        fs::write(&original, b"THE FILE WE PROBED").unwrap();
        fs::write(&staged, b"NEW").unwrap();

        let stale_pin = SourcePin::pin(&original).unwrap();

        // ...and now someone replaces it with a different inode.
        fs::remove_file(&original).unwrap();
        fs::write(&original, b"A DIFFERENT, NEWER FILE").unwrap();

        let err = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&original).unwrap(),
            &stale_pin,
            &t.lock(&original),
        )
        .unwrap_err();

        assert!(
            matches!(err, SwapError::SourceChangedUnderUs { .. }),
            "got {err:?}"
        );
        assert_eq!(
            fs::read(&original).unwrap(),
            b"A DIFFERENT, NEWER FILE",
            "the newer file must be left exactly as it was"
        );
        assert!(
            !t.lib().join("Movie.mkv.muse-superseded").exists(),
            "and no backup name may be left behind after a refusal"
        );
    }

    #[test]
    fn the_source_pin_keeps_the_probed_inode_alive_after_it_is_unlinked() {
        // The mechanism the test above depends on, asserted directly — and
        // deliberately in a way that does not depend on the filesystem under
        // $TMPDIR. Whether ext4 hands a freed inode number straight back is
        // policy we cannot force from a test; that an inode with a live
        // descriptor is NOT released is a kernel guarantee we can.
        //
        // So: the pin must still be reading the bytes of the file that was
        // probed, after that file's last directory entry is gone. If it is,
        // the inode has not been released, and a number that has not been
        // released cannot have been recycled — which is the entire reason
        // `swap_verified_output` may compare `(dev, ino)` across an encode.
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;

        let t = Tmp::new("pin-holds");
        let probed = t.lib().join("Movie.mkv");
        fs::write(&probed, b"THE FILE WE PROBED").unwrap();

        let pin = SourcePin::pin(&probed).unwrap();
        let pinned = pin.identity();

        fs::remove_file(&probed).unwrap();

        let mut held = pin._file.try_clone().unwrap();
        let md = held.metadata().unwrap();
        assert_eq!(
            md.nlink(),
            0,
            "the probed file must really be unlinked — otherwise this proves nothing"
        );
        assert_eq!(
            (md.dev(), md.ino()),
            (pinned.dev, pinned.ino),
            "the pin must still refer to the inode it reported"
        );

        let mut got = Vec::new();
        held.read_to_end(&mut got).unwrap();
        assert_eq!(
            got, b"THE FILE WE PROBED",
            "the pin must hold the probed inode open, not merely remember its number"
        );
    }

    #[test]
    fn claiming_a_name_never_overwrites_even_though_rename_would_have() {
        // The core of the TOCTOU fix, asserted on the primitive itself rather
        // than only through the swap: `fs::rename` silently destroys its
        // destination, `fs::hard_link` refuses atomically. If this ever
        // changes, the swap's whole safety argument is void.
        let t = Tmp::new("primitive");
        let a = t.lib().join("a");
        let b = t.lib().join("b");
        fs::write(&a, b"AAA").unwrap();
        fs::write(&b, b"BBB").unwrap();

        let e = fs::hard_link(&a, &b).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&b).unwrap(), b"BBB", "hard_link must not clobber");

        // The primitive the old code used, for contrast.
        fs::rename(&a, &b).unwrap();
        assert_eq!(
            fs::read(&b).unwrap(),
            b"AAA",
            "rename DOES clobber — which is why it is not used to claim a name"
        );
    }

    #[test]
    fn a_successful_swap_leaves_exactly_two_names_and_no_staging_file() {
        let t = Tmp::new("swap-tidy");
        let g = t.guard();
        let original = t.lib().join("Movie.mkv");
        let staged = t.lib().join(".inflight.mkv");
        fs::write(&original, b"ORIGINAL").unwrap();
        fs::write(&staged, b"NEW-VERIFIED").unwrap();
        let id = SourcePin::pin(&original).unwrap();

        swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&original).unwrap(),
            &id,
            &t.lock(&original),
        )
        .unwrap();

        assert!(!staged.exists(), "the staging name must be gone");
        let mut names: Vec<String> = fs::read_dir(t.lib())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["Movie.mkv", "Movie.mkv.muse-superseded"]);
    }

    // --- the driver, on a host with no media tooling -----------------------

    fn cfg_for(t: &Tmp, ffprobe: &str, ffmpeg: &str) -> FoundryConfig {
        FoundryConfig {
            encode_timeout: std::time::Duration::from_secs(6 * 60 * 60),
            allowed_roots: vec![t.lib()],
            work_dir: Some(t.work()),
            enable_mutation: true,
            retention_days: 14,
            ffprobe_bin: ffprobe.to_string(),
            ffmpeg_bin: ffmpeg.to_string(),
            handbrake_bin: "muse-foundry-absent-handbrake".to_string(),
            capability_timeout: std::time::Duration::from_secs(
                crate::media::capability::DEFAULT_CAPABILITY_TIMEOUT_SECS,
            ),
        }
    }

    /// The skip is OPT-IN and must stay that way.
    ///
    /// It is a genuine trade, not a strict improvement: the swap still leaves
    /// a direct-playable file in the library, which is half of Path A's
    /// purpose. Only the reclaim half is impossible for these titles. Turning
    /// it on by default would silently decide that for the operator.
    ///
    /// Measured: 260 of 16,221 titles (1.6%), led by TrueHD (114), DTS (50).
    #[test]
    fn skipping_unreclaimable_titles_is_opt_in_not_the_default() {
        let body = include_str!("forge.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("a non-test body");
        assert!(
            body.contains("MUSE_FOUNDRY_SKIP_UNRECLAIMABLE"),
            "the behaviour must be reachable"
        );
        assert!(
            body.contains(".unwrap_or(false)"),
            "it must default to OFF — the current behaviour is preserved unless the \
             operator opts in"
        );
        // ...and it must consult the PREDICTION rather than re-deriving a rule.
        assert!(
            body.contains("predicted_deletion_refusals(&source, &plan)"),
            "the skip must use the same prediction the survey reports, or the two \
             would disagree about which titles are affected"
        );
    }

    /// The reason must say what was traded away, not just that it skipped.
    #[test]
    fn the_unreclaimable_skip_explains_the_tradeoff() {
        let r = SkipReason::UnreclaimableOriginal {
            predicted: vec!["audio stream 1 is `truehd`, which may carry Dolby Atmos".into()],
        };
        let msg = r.to_string();
        assert!(msg.contains("more disk"), "{msg}");
        assert!(msg.contains("truehd"), "it must carry the specific reason: {msg}");
        assert!(
            msg.contains("MUSE_FOUNDRY_SKIP_UNRECLAIMABLE"),
            "and name the setting that caused it: {msg}"
        );
    }

    /// A stalled probe must be a SKIP, never a failure.
    ///
    /// `Failed` reads as "something is wrong with this file", and a later run
    /// or operator would treat it that way — but a probe timeout means nothing
    /// was learned about the file at all. The distinction matters at
    /// 16,000-item scale, where transient stalls are certain and a pile of
    /// "failures" would obscure the real ones.
    /// Path A's trigger must never become a sweep.
    ///
    /// `optimize_file` had NO production caller until FOUNDRY-27, so the full
    /// chain — probe, plan, encode, verify, swap — had never executed. The
    /// three existing optimize_file tests cover only refusal paths, which
    /// means the most destructive operation in Muse was also the least
    /// exercised.
    ///
    /// The trigger is therefore the narrowest one that can prove the chain
    /// works: explicit paths, bounded count, both gates. If a future change
    /// makes it enumerate anything, the blast radius stops being "what was
    /// typed".
    #[test]
    fn the_optimize_endpoint_is_explicit_paths_only_and_never_a_sweep() {
        let dash = include_str!("../web/dashboard.rs");
        let start = dash
            .find("pub async fn foundry_optimize")
            .expect("the Path A trigger exists");
        let body = &dash[start..start + 3000];

        assert!(
            !body.contains("walk_media_files") && !body.contains("library_root"),
            "the trigger must not be able to enumerate the library — a sweep would make \
             one request able to rewrite 16,000 files"
        );
        assert!(
            body.contains("body.paths.len() > 8"),
            "it must be bounded per request"
        );
        assert!(
            body.contains("foundry.mutation_enabled()"),
            "it must consult the GLOBAL gate, not only forge's internal check"
        );
        assert!(
            body.contains("confirm"),
            "it must require the operator to restate what is being rewritten"
        );
        assert!(
            body.contains("direct_play_normalization()"),
            "it must use Path A's real policy, not the default"
        );
    }

    /// The production encode must be bounded.
    ///
    /// It rewrites library files AND holds the title's swap lock while it
    /// runs, so an ffmpeg wedged on a stalled NFS read blocked that swap
    /// forever and kept the title locked against every future pass —
    /// indistinguishable from "still encoding". `Command::output()` has no
    /// timeout; this was found by auditing for the same defect after it was
    /// observed live in the probe path.
    #[test]
    fn the_production_encode_is_bounded_by_a_configured_ceiling() {
        let src = include_str!("forge.rs");
        let body = src.split("#[cfg(test)]").next().expect("a non-test body");
        assert!(
            !body.contains(".output()"),
            "the production encode must not use Command::output(), which cannot time out"
        );
        assert!(
            body.contains("spawn_with_timeout"),
            "it must go through the bounded spawner"
        );
        assert!(
            body.contains("cfg.encode_timeout"),
            "the ceiling must come from configuration, not be hardcoded at the call site"
        );
    }

    /// A timeout must say that nothing was swapped. An operator reading
    /// "ffmpeg timed out" needs to know the library is untouched, not wonder
    /// whether a half-written file replaced their title.
    /// The staged file must be discarded on EVERY path, timeout included.
    ///
    /// Both reviewers raised this at the gate, reading only the encode call
    /// site — where it does look unhandled. It is not: `discard_staged` runs
    /// unconditionally after `run_encode_and_swap` returns, on success and on
    /// every error. Pinned here so it stays that way, because the leak it
    /// prevents is the kind nobody notices: one full-size encode per file
    /// until the scratch filesystem fills, which then presents as unrelated
    /// encode failures.
    #[test]
    fn the_staged_file_is_discarded_on_every_path_including_a_timeout() {
        let body = include_str!("forge.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("a non-test body");
        let call = body
            .find("discard_staged(&staged);")
            .expect("the staged file must be discarded");
        let matched = body
            .find("match result {")
            .expect("the result is matched after the encode");
        assert!(
            call < matched,
            "discard_staged must run BEFORE the result is matched, so it happens on the \
             error paths too — not only on success"
        );
    }

    #[test]
    fn an_encode_timeout_says_the_original_is_untouched() {
        // The NON-TEST body only. Searching the whole file matches this
        // assertion's own literal, so the test would pass by finding itself —
        // caught by a mutation that removed the production string and left
        // this test green.
        let body = include_str!("forge.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("a non-test body");
        assert!(
            body.contains("NOTHING was swapped; the original is untouched"),
            "the timeout message must state that the library was not modified"
        );
    }

    /// Default 6h, clamped. Generous enough that honest 4K work never trips
    /// it, bounded enough that a wedge cannot last a day.
    #[test]
    fn the_encode_ceiling_defaults_generously_and_is_clamped() {
        let cfg = crate::config::Config::default();
        let f = FoundryConfig::from_config(&cfg);
        assert_eq!(f.encode_timeout, std::time::Duration::from_secs(6 * 60 * 60));

        let mut absurd = crate::config::Config::default();
        absurd.foundry_encode_timeout_secs = Some(u64::MAX);
        assert_eq!(
            FoundryConfig::from_config(&absurd).encode_timeout,
            std::time::Duration::from_secs(48 * 60 * 60),
            "clamped to 48h — CPU-only 4K HDR can legitimately run past a day"
        );

        let mut zero = crate::config::Config::default();
        zero.foundry_encode_timeout_secs = Some(0);
        assert_eq!(
            FoundryConfig::from_config(&zero).encode_timeout,
            std::time::Duration::from_secs(60),
            "zero clamps UP so the ceiling cannot be disabled"
        );
    }

    /// Orphaned staging files must be removed, and live ones must not be.
    ///
    /// Nothing cleaned these up. `discard_staged` runs after the encode
    /// returns, which never happens if the process is killed first — so a
    /// deploy, crash, OOM, or cancelled run leaves a full-size partial encode
    /// behind permanently. This session cancelled several runs by hand, each
    /// leaving ~20 GB that had to be removed manually; at 16,000 items that
    /// fills the scratch filesystem and then presents as unrelated encode
    /// failures.
    #[test]
    fn an_orphaned_staging_file_is_swept_but_a_live_one_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("muse-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let ceiling = Duration::from_secs(60);
        let old = SystemTime::now() - Duration::from_secs(10_000); // > 2x ceiling

        // An orphan: a staging file older than twice the encode ceiling, so it
        // provably cannot belong to a running encode.
        let orphan = dir.join("muse-foundry-deadjob.mkv");
        fs::write(&orphan, vec![b'x'; 4096]).unwrap();
        let ft = fs::FileTimes::new().set_modified(old).set_accessed(old);
        fs::File::options().write(true).open(&orphan).unwrap().set_times(ft).unwrap();

        // A LIVE one: same shape, but recent.
        let live = dir.join("muse-foundry-runningjob.mkv");
        fs::write(&live, b"y").unwrap();

        // ...and an inflight-prefixed orphan, the other shape forge creates.
        let inflight = dir.join(format!("{INFLIGHT_PREFIX}-dead.part"));
        fs::write(&inflight, b"z").unwrap();
        fs::File::options().write(true).open(&inflight).unwrap().set_times(ft).unwrap();

        // A file forge did NOT create must be left completely alone, however old.
        let operators_file = dir.join("operator-notes.txt");
        fs::write(&operators_file, b"do not delete").unwrap();
        fs::File::options().write(true).open(&operators_file).unwrap().set_times(ft).unwrap();
        let _ = fs::set_permissions(&operators_file, fs::Permissions::from_mode(0o644));

        let report = sweep_orphaned_staging(&dir, ceiling);

        assert!(!orphan.exists(), "the orphan must be removed");
        assert!(!inflight.exists(), "the inflight orphan must be removed too");
        assert!(live.exists(), "a LIVE encode's staging file must survive");
        assert!(
            operators_file.exists(),
            "a file forge did not create must never be touched, however old"
        );
        assert_eq!(report.removed, 2);
        assert_eq!(report.kept_live_or_unknown, 1, "the live one");
        assert_eq!(report.examined, 3, "only forge-shaped files are examined");
        assert!(report.bytes_reclaimed >= 4096);
        assert!(!report.unreadable);

        let _ = fs::remove_dir_all(&dir);
    }

    /// An unreadable work dir establishes nothing and must say so, rather than
    /// reporting a clean sweep of zero files.
    #[test]
    fn a_work_dir_that_cannot_be_listed_reports_that_it_established_nothing() {
        let report = sweep_orphaned_staging(
            Path::new("/nonexistent-muse-sweep-dir"),
            Duration::from_secs(60),
        );
        assert!(report.unreadable, "an unlistable work dir must be reported");
        assert_eq!(report.removed, 0);
        assert_eq!(report.examined, 0);
    }

    #[test]
    fn a_probe_timeout_is_a_retryable_skip_not_a_file_failure() {
        let reason = SkipReason::ProbeTimedOut { secs: 120 };
        let msg = reason.to_string();
        assert!(msg.contains("120s"), "{msg}");
        assert!(
            msg.contains("not a bad file"),
            "the message must not read as a verdict about the file: {msg}"
        );
        assert!(msg.contains("retry"), "it must say it is retryable: {msg}");
    }

    #[test]
    fn a_missing_ffprobe_is_reported_as_a_missing_tool_never_as_no_work_needed() {
        // THE honesty requirement, and the state this fleet is actually in:
        // ffprobe is not installed on <host>. The file must come back as
        // "skipped: tool unavailable", naming the tool — never as
        // AlreadyOptimal, and never as a success.
        let t = Tmp::new("no-tools");
        let file = t.lib().join("Movie.mkv");
        fs::write(&file, b"not really media").unwrap();

        let status = optimize_file(
            &t.guard(),
            &cfg_for(&t, "muse-foundry-absent-ffprobe", "muse-foundry-absent-ffmpeg"),
            &TranscodePolicy::default(),
            &file,
        );

        match status {
            ForgeStatus::Skipped {
                reason: SkipReason::ToolUnavailable { tool, .. },
            } => assert_eq!(tool, "ffprobe"),
            other => panic!("expected a named tool-unavailable skip, got {other:?}"),
        }
        assert_eq!(
            fs::read(&file).unwrap(),
            b"not really media",
            "and the file must be untouched"
        );
    }

    #[test]
    fn a_skip_is_never_silent_every_reason_renders_a_sentence() {
        // "No silent skips": each variant must produce something an operator
        // can act on, not an empty or Debug-shaped string.
        for r in [
            SkipReason::ToolUnavailable { tool: "ffmpeg", detail: "not installed".into() },
            SkipReason::MutationDisabled,
            SkipReason::NoWorkDir,
            SkipReason::AlreadyOptimal,
            SkipReason::Undecidable(Undecidable::UnknownDuration),
            SkipReason::PathRefused(PathError::NoAllowedRoots),
            SkipReason::SourceIsSymlink,
        ] {
            let s = r.to_string();
            assert!(s.len() > 20, "reason too thin to act on: {s:?}");
            assert!(!s.contains('{'), "a Debug dump is not an explanation: {s:?}");
        }
    }

    #[test]
    fn a_symlinked_source_is_skipped_before_anything_else_happens() {
        let t = Tmp::new("symlink");
        let real = t.lib().join("Real.mkv");
        let link = t.lib().join("Link.mkv");
        fs::write(&real, b"x").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let status = optimize_file(
            &t.guard(),
            &cfg_for(&t, "muse-foundry-absent-ffprobe", "muse-foundry-absent-ffmpeg"),
            &TranscodePolicy::default(),
            &link,
        );
        assert_eq!(
            status,
            ForgeStatus::Skipped { reason: SkipReason::SourceIsSymlink }
        );
    }

    #[test]
    fn a_path_outside_the_allowed_roots_is_refused_by_the_guard() {
        let t = Tmp::new("outside");
        let outside = t.0.join("elsewhere.mkv");
        fs::write(&outside, b"x").unwrap();

        let status = optimize_file(
            &t.guard(),
            &cfg_for(&t, "ffprobe", "ffmpeg"),
            &TranscodePolicy::default(),
            &outside,
        );
        assert!(
            matches!(
                status,
                ForgeStatus::Skipped {
                    reason: SkipReason::PathRefused(PathError::OutsideAllowedRoots { .. })
                }
            ),
            "got {status:?}"
        );
    }

    #[test]
    fn capability_detection_reports_this_hosts_real_state() {
        // Documents what Foundry says on <host>/<host> as they stand today.
        // Uses deliberately absent names so the assertion is about the
        // reporting, not about what happens to be installed.
        let t = Tmp::new("caps");
        let caps = detect_capabilities(&cfg_for(
            &t,
            "muse-foundry-absent-ffprobe",
            "muse-foundry-absent-ffmpeg",
        ));
        assert!(!caps.can_probe());
        assert!(!caps.can_transcode());
        assert_eq!(caps.unavailable(), vec!["ffprobe", "ffmpeg", "HandBrakeCLI"]);
    }

    #[test]
    fn the_final_path_keeps_the_directory_and_stem_and_changes_only_the_extension() {
        assert_eq!(
            final_path_for(Path::new("/lib/Movies/A Film (1999)/A Film (1999).avi"), "mkv"),
            PathBuf::from("/lib/Movies/A Film (1999)/A Film (1999).mkv")
        );
        // A stem containing dots must not be mangled.
        assert_eq!(
            final_path_for(Path::new("/lib/S01E01.1080p.WEB-DL.mkv"), "mkv"),
            PathBuf::from("/lib/S01E01.1080p.WEB-DL.mkv")
        );
    }
}
