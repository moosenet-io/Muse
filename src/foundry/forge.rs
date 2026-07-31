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
//! 4. `rename` the original aside (same directory, so atomic and instant), then
//!    `rename` the new file into place. If the second rename fails, the first
//!    is rolled back.
//!
//! The original is never `remove`d and never opened for writing. It ends up
//! renamed to a sibling `.muse-superseded` file, which the library scanner
//! ignores (not a media extension) and which the operator or a later retention
//! sweep can delete once the new file has been watched.
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

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::foundry::capability::{self, Capabilities};
use crate::foundry::config::FoundryConfig;
use crate::foundry::paths::{MutablePath, PathError, PathGuard, ResolvedPath};
use crate::foundry::plan::{
    output_container, plan_transcode, AudioAction, TranscodeDecision, TranscodePlan,
    TranscodeReason, Undecidable, VideoAction,
};
use crate::foundry::policy::TranscodePolicy;
use crate::foundry::probe::{run_ffprobe, MediaProbe, ProbeError};

/// Extension given to the superseded original.
///
/// Deliberately **not** a media extension: `library::scan` selects files by
/// video extension, so a superseded original cannot be re-ingested as a
/// duplicate of the file that just replaced it. Renaming rather than deleting
/// is the point — the operator's undo.
const SUPERSEDED_EXT: &str = "muse-superseded";

/// Prefix for the in-library staging copy. Leading dot so it is hidden, and a
/// non-media extension for the same reason as above: it exists for at most the
/// few seconds between the copy and the rename, but a crash in that window must
/// not leave something the scanner will pick up.
const INFLIGHT_PREFIX: &str = ".muse-foundry-inflight";

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
    /// The plan says the file already meets policy.
    AlreadyOptimal,
    /// Foundry could not judge the file.
    Undecidable(Undecidable),
    /// The path guard refused the path.
    PathRefused(PathError),
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
    for a in &source.audio {
        let channels = a.channels?;
        audio.push(match plan.audio {
            AudioAction::Copy => ExpectedAudio {
                codec: a.codec.clone(),
                channels,
                language: a.language.clone(),
            },
            AudioAction::Encode => ExpectedAudio {
                codec: "aac".to_string(),
                // `-ac N` sets the output channel count exactly, so a source
                // already at or below the ceiling comes out unchanged and only
                // a wider one is downmixed.
                channels: channels.min(policy.max_audio_channels),
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

/// A file's identity on disk: the pair that survives a rename and changes on a
/// replace. Used to detect a source swapped out from under a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Read a path's [`FileIdentity`] without following symlinks.
pub fn identity_of(p: &Path) -> std::io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(p)?;
    Ok(FileIdentity {
        dev: md.dev(),
        ino: md.ino(),
    })
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

/// Put a **already-verified** file into place, renaming the original aside.
///
/// # Preconditions the caller owns
/// `verified_new` must have passed [`verify_output`]. This function performs no
/// media checks of its own — it is the mechanical half, split out precisely so
/// it can be tested with ordinary files on a host with no ffmpeg.
///
/// # Why this does not use `rename`
/// **`rename(2)` silently overwrites its destination on Unix.** The previous
/// version of this function guarded it with `Path::exists()` checks, which is
/// a textbook check-then-act race: neither check is atomic with the rename
/// that follows, so a concurrent writer could create the destination (or the
/// `.muse-superseded` name) in the window between them and have its file
/// destroyed without a word. That directly contradicted the module's
/// never-clobber claim, so the whole approach is gone.
///
/// The primitive used instead is `link(2)` via [`std::fs::hard_link`], which
/// **fails with `EEXIST` atomically** and never overwrites. Claiming a name is
/// therefore a single indivisible operation, and occupancy is discovered by
/// *failing to claim*, not by looking first. (`renameat2(RENAME_NOREPLACE)`
/// would work equally well but needs a `libc` dependency this crate does not
/// carry; `hard_link` is in `std` and gives the same guarantee.)
///
/// If the filesystem does not support hard links at all, this **refuses**
/// ([`SwapError::LinkUnsupported`]) rather than falling back to a racy
/// rename — a swap that cannot be made safely is not performed.
///
/// # Ordering
/// 1. `link(original -> superseded)` — atomically claims the backup name. The
///    original's inode now has two names, so it cannot be lost by anything
///    that follows.
/// 2. Compare the linked inode against the identity captured at probe time. A
///    mismatch means the file was replaced after we planned against it, so
///    the plan describes something that is no longer there — undo and refuse.
/// 3. `unlink(original)` — the inode survives via the backup name.
/// 4. `link(new -> final)` — atomically claims the destination. `EEXIST` here
///    means a bystander occupies it; roll the original back and refuse,
///    leaving the bystander untouched.
/// 5. `unlink(new)` to drop the staging name, then `fsync` the directory so
///    both entries are durable.
///
/// The only window where the destination name is briefly absent is between
/// steps 3 and 4, and any failure in it is rolled back.
///
/// # Preconditions the caller owns
/// `verified_new` must have passed [`verify_output`]. This function performs
/// no media checks of its own.
fn swap_verified_output(
    original: &MutablePath,
    verified_new: &MutablePath,
    final_path: &MutablePath,
    expected_identity: FileIdentity,
) -> Result<SwapRecord, SwapError> {
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
    // Comparing the inode we just linked (not a fresh stat of the path, which
    // would be another race) against the one captured at probe time.
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

    // --- 3. drop the original name (the inode lives on as the backup) ------
    if let Err(e) = std::fs::remove_file(original_p) {
        let _ = std::fs::remove_file(&superseded);
        return Err(SwapError::Io {
            step: "releasing the original name",
            message: e.to_string(),
        });
    }

    // --- 4. atomically claim the destination -------------------------------
    if let Err(e) = std::fs::hard_link(new_p, final_p) {
        // Roll back: re-link the original inode to its own name, then drop the
        // backup name. If the rollback fails there is still nothing lost — the
        // original remains reachable under `superseded` — and the message says
        // exactly where it is.
        let rollback = std::fs::hard_link(&superseded, original_p)
            .and_then(|()| std::fs::remove_file(&superseded));
        let occupied = e.kind() == std::io::ErrorKind::AlreadyExists;
        let detail = match rollback {
            Ok(()) => "the original was rolled back into place".to_string(),
            Err(re) => format!(
                "AND the rollback failed ({re}); the original is at {}",
                superseded.display()
            ),
        };
        return Err(if occupied && rollback_succeeded(&detail) {
            SwapError::DestinationOccupied(final_p.to_path_buf())
        } else {
            SwapError::Io {
                step: "claiming the destination name",
                message: format!("{e} ({detail})"),
            }
        });
    }

    // --- 5. drop the staging name and make both entries durable ------------
    if let Err(e) = std::fs::remove_file(new_p) {
        // Not fatal: the content is correctly in place under `final_p`. A
        // leftover staging name is untidy, not destructive.
        tracing::warn!(
            error = %e,
            path = %new_p.display(),
            "foundry: could not remove the in-library staging name after a successful swap"
        );
    }

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

/// Whether the rollback detail string reports success. Kept as a named helper
/// so the branch above reads as the intent ("only call it `DestinationOccupied`
/// if the library is actually back to its original state").
fn rollback_succeeded(detail: &str) -> bool {
    detail.starts_with("the original was rolled back")
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

    // Captured BEFORE the probe, so it covers the whole probe-plan-encode
    // window: if anything replaces the file while we are working, the swap
    // compares this against the inode it actually links and refuses.
    let source_identity = match identity_of(resolved.as_path()) {
        Ok(id) => id,
        Err(e) => {
            return ForgeStatus::Failed {
                reason: format!("could not read the source file's identity: {e}"),
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

    let bytes_before = source.size_bytes.unwrap_or(0);
    let result = run_encode_and_swap(
        guard, cfg, policy, &resolved, &source, &plan, &args, &staged, source_identity,
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
    source_identity: FileIdentity,
) -> Result<CompletedSwap, String> {
    // --- 1. encode ---------------------------------------------------------
    // The ONE process spawn on this path. Everything above and below it is
    // pure or is ordinary filesystem work.
    let status = Command::new(&cfg.ffmpeg_bin)
        .args(args)
        .output()
        .map_err(|e| format!("spawning ffmpeg `{}`: {e}", cfg.ffmpeg_bin))?;

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
    let inflight_target = parent.join(format!(
        "{INFLIGHT_PREFIX}-{}.{}",
        uuid::Uuid::new_v4(),
        plan.container.extension()
    ));
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

    match swap_verified_output(&original_mut, &inflight, &final_mut, source_identity) {
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

/// Detect tooling using the configured binary names.
pub(in crate::foundry) fn detect_capabilities(cfg: &FoundryConfig) -> Capabilities {
    capability::detect(&cfg.ffprobe_bin, &cfg.ffmpeg_bin, &cfg.handbrake_bin)
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
            audio: AudioAction::Encode,
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
        let enc = expectation_for(&s, &encode_plan(None), &TranscodePolicy::default()).unwrap();
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
        let expect = expectation_for(&s, &encode_plan(None), &TranscodePolicy::default()).unwrap();
        assert_eq!(expect.audio[0].codec, "aac");
        assert_eq!(expect.audio[0].channels, 6, "8ch must be downmixed to the ceiling");
        assert_eq!(
            expect.audio[1].channels, 2,
            "a stereo track must stay stereo, not be inflated to the ceiling"
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

        let id = identity_of(&original).unwrap();
        let rec = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&original).unwrap(),
            id,
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

        let id = identity_of(&original).unwrap();
        let rec = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(t.lib().join("Movie.mkv")).unwrap(),
            id,
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
    fn the_superseded_file_does_not_look_like_media_to_the_library_scanner() {
        // If it did, the scanner would re-ingest the original as a duplicate
        // of the file that just replaced it.
        assert_ne!(SUPERSEDED_EXT, "mkv");
        assert!(!SUPERSEDED_EXT.is_empty());
        let p = with_added_extension(Path::new("/lib/Movie.mkv"), SUPERSEDED_EXT);
        assert_eq!(p, PathBuf::from("/lib/Movie.mkv.muse-superseded"));
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

        let id = identity_of(&original).unwrap();
        let err = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&bystander).unwrap(),
            id,
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

        let id = identity_of(&original).unwrap();
        let err = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&original).unwrap(),
            id,
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
    fn a_failed_second_rename_rolls_the_original_back_into_place() {
        // The one window where the library is momentarily missing the file:
        // between the two renames. The new file vanishing in that window (a
        // concurrent cleaner, a scratch reaper) makes the second rename fail
        // after the first has already succeeded — and the original MUST come
        // back rather than being left aside under a name nothing plays.
        let t = Tmp::new("swap-rollback");
        let g = t.guard();
        let original = t.lib().join("Movie.mkv");
        let staged = t.lib().join(".inflight.mkv");
        fs::write(&original, b"ORIGINAL").unwrap();
        fs::write(&staged, b"NEW").unwrap();

        let id = identity_of(&original).unwrap();
        let original_mut = g.resolve_for_mutation(&original).unwrap();
        let staged_mut = g.resolve_for_mutation(&staged).unwrap();
        let final_mut = g.resolve_new_for_mutation(&original).unwrap();
        // ...and now it is gone, after resolution and before the swap.
        fs::remove_file(&staged).unwrap();

        let err =
            swap_verified_output(&original_mut, &staged_mut, &final_mut, id).unwrap_err();

        assert!(matches!(err, SwapError::Io { .. }), "got {err:?}");
        assert!(
            err.to_string().contains("rolled back"),
            "the failure must say the original was restored, got {err}"
        );
        assert_eq!(
            fs::read(&original).unwrap(),
            b"ORIGINAL",
            "the original must be rolled back into place, byte-for-byte"
        );
        assert!(
            !t.lib().join("Movie.mkv.muse-superseded").exists(),
            "and the aside-name must not be left behind"
        );
    }

    #[test]
    fn the_swap_refuses_when_the_source_was_replaced_after_it_was_probed() {
        // The third TOCTOU case from review. The plan describes the file we
        // probed; if something replaced it in the meantime, applying that plan
        // would move a NEWER, unrelated file aside and replace it with an
        // encode of something else entirely.
        let t = Tmp::new("swap-changed");
        let g = t.guard();
        let original = t.lib().join("Movie.mkv");
        let staged = t.lib().join(".inflight.mkv");
        fs::write(&original, b"THE FILE WE PROBED").unwrap();
        fs::write(&staged, b"NEW").unwrap();

        let stale_identity = identity_of(&original).unwrap();

        // ...and now someone replaces it with a different inode.
        fs::remove_file(&original).unwrap();
        fs::write(&original, b"A DIFFERENT, NEWER FILE").unwrap();

        let err = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&original).unwrap(),
            stale_identity,
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
        let id = identity_of(&original).unwrap();

        swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&original).unwrap(),
            id,
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
            allowed_roots: vec![t.lib()],
            work_dir: Some(t.work()),
            enable_mutation: true,
            retention_days: 14,
            ffprobe_bin: ffprobe.to_string(),
            ffmpeg_bin: ffmpeg.to_string(),
            handbrake_bin: "muse-foundry-absent-handbrake".to_string(),
        }
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
