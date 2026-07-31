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
    output_container, plan_transcode, TranscodeDecision, TranscodePlan, TranscodeReason,
    Undecidable, VideoAction,
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
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyExpectation {
    pub source_duration_secs: f64,
    /// The `codec_name` the output's video stream must report.
    pub video_codec: String,
    /// Dimensions the output must have, when the plan ordered a downscale.
    pub video_dimensions: Option<(u32, u32)>,
    pub audio_stream_count: usize,
    pub subtitle_stream_count: usize,
    /// Absolute duration slack, in seconds. Default 2.0 — container duration
    /// legitimately shifts by a frame or two across a remux, and by up to
    /// about a second when a keyframe-aligned encode rounds the last GOP.
    pub duration_abs_tolerance_secs: f64,
    /// Relative duration slack. Default 0.01 (1%) — a long feature accumulates
    /// more rounding than a short one, so the tolerance scales with it.
    pub duration_rel_tolerance: f64,
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

    Some(VerifyExpectation {
        source_duration_secs,
        video_codec,
        video_dimensions,
        audio_stream_count: source.audio.len(),
        subtitle_stream_count: source.subtitles.len(),
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

    if output.audio.len() != expect.audio_stream_count {
        return Err(VerifyFailure::AudioStreamCountMismatch {
            expected: expect.audio_stream_count,
            found: output.audio.len(),
        });
    }

    if output.subtitles.len() != expect.subtitle_stream_count {
        return Err(VerifyFailure::SubtitleStreamCountMismatch {
            expected: expect.subtitle_stream_count,
            found: output.subtitles.len(),
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
            Self::Io { step, message } => write!(f, "{step}: {message}"),
        }
    }
}

/// Put a **already-verified** file into place, renaming the original aside.
///
/// # Preconditions the caller owns
/// `verified_new` must have passed [`verify_output`]. This function performs no
/// media checks of its own — it is the mechanical half, split out precisely so
/// it can be tested with ordinary files on a host with no ffmpeg.
///
/// # Ordering
/// Both files are in the same directory, so both renames are atomic and
/// instant. The original moves aside *first*: at no instant does the
/// destination name hold a partially-written file, and if the second rename
/// fails the first is rolled back so the original is back where it started.
fn swap_verified_output(
    original: &MutablePath,
    verified_new: &MutablePath,
    final_path: &MutablePath,
) -> Result<SwapRecord, SwapError> {
    let original_p = original.as_path();
    let final_p = final_path.as_path();
    let new_p = verified_new.as_path();

    // Only a *different* existing file blocks the swap. When the container is
    // unchanged the final path IS the original path, which is expected.
    if final_p != original_p && final_p.exists() {
        return Err(SwapError::DestinationOccupied(final_p.to_path_buf()));
    }

    let superseded = with_added_extension(original_p, SUPERSEDED_EXT);
    if superseded.exists() {
        return Err(SwapError::SupersededNameOccupied(superseded));
    }

    std::fs::rename(original_p, &superseded).map_err(|e| SwapError::Io {
        step: "moving the original aside",
        message: e.to_string(),
    })?;

    if let Err(e) = std::fs::rename(new_p, final_p) {
        // Roll back so the original is exactly where it was. If the rollback
        // itself fails there is nothing further we can do but report both —
        // and the original still exists under the superseded name, so nothing
        // has been lost even in that case.
        let rollback = std::fs::rename(&superseded, original_p);
        return Err(SwapError::Io {
            step: "moving the new file into place",
            message: match rollback {
                Ok(()) => format!("{e} (the original was rolled back into place)"),
                Err(re) => format!(
                    "{e} — AND the rollback failed ({re}); the original is at {}",
                    superseded.display()
                ),
            },
        });
    }

    Ok(SwapRecord {
        final_path: final_p.to_path_buf(),
        superseded_path: superseded,
    })
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
    let result = run_encode_and_swap(guard, cfg, policy, &resolved, &source, &plan, &args, &staged);

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

    match swap_verified_output(&original_mut, &inflight, &final_mut) {
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
    use crate::foundry::plan::AudioAction;
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

        let rec = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&original).unwrap(),
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

        let rec = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(t.lib().join("Movie.mkv")).unwrap(),
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

        let err = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&bystander).unwrap(),
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

        let err = swap_verified_output(
            &g.resolve_for_mutation(&original).unwrap(),
            &g.resolve_for_mutation(&staged).unwrap(),
            &g.resolve_new_for_mutation(&original).unwrap(),
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

        let original_mut = g.resolve_for_mutation(&original).unwrap();
        let staged_mut = g.resolve_for_mutation(&staged).unwrap();
        let final_mut = g.resolve_new_for_mutation(&original).unwrap();
        // ...and now it is gone, after resolution and before the swap.
        fs::remove_file(&staged).unwrap();

        let err = swap_verified_output(&original_mut, &staged_mut, &final_mut).unwrap_err();

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
