//! MUSEF-02 — describe a media file: the `ffprobe` invocation and, separately,
//! the pure parser for its output.
//!
//! ## The split, and why it is not cosmetic
//! `ffprobe` **is not installed on <host>**, the host Muse runs on (verified
//! 2026-07-31), and it is not installed on the dev box either. If parsing lived
//! inside the invocation, none of it could be tested anywhere in this fleet's
//! current shape — the parser would ship unexercised, on the one code path that
//! decides whether a file gets re-encoded. So [`parse_probe_json`] is a pure
//! `&str -> Result` function tested against captured `ffprobe` output, and
//! [`run_ffprobe`] is the thin, untestable-here layer that produces that `&str`.
//!
//! ## What this module refuses to do
//! It never returns a *partial* or *empty* [`MediaProbe`] to paper over a
//! failure. A probe that did not happen, or whose output did not parse, is a
//! [`ProbeError`] — never a `MediaProbe` with empty stream lists, which the
//! planner downstream would read as "this file has no video" and act on. The
//! same rule as the rest of Foundry: an unobserved fact is reported as
//! unobserved, not as a benign default.

use std::time::Duration;
use std::process::Command;

use serde::Deserialize;

use crate::foundry::paths::ResolvedPath;

/// Build the `ffprobe` CLI arguments (everything after the binary name).
///
/// Pure, so the exact argv is asserted in tests on a host with no `ffprobe`
/// — the same posture as [`crate::streaming::ffmpeg::build_args`].
///
/// `-v quiet` plus `-print_format json` means stdout is *only* JSON: any
/// diagnostic noise would otherwise be interleaved into the document we are
/// about to parse. `-show_format` gives the container/duration, `-show_streams`
/// the per-stream detail, `-show_chapters` the chapter list; none is the
/// default and all three are needed.
///
/// `-show_chapters` is not optional decoration: the transcode argv promises
/// `-map_chapters 0`, and a promise that is never checked is the class of
/// false claim this module exists to avoid. Chapters are not streams, so
/// `-show_streams` does not report them.
pub fn build_ffprobe_args(file_path: &str) -> Vec<String> {
    vec![
        "-v".to_string(),
        "quiet".to_string(),
        "-print_format".to_string(),
        "json".to_string(),
        "-show_format".to_string(),
        "-show_streams".to_string(),
        "-show_chapters".to_string(),
        file_path.to_string(),
    ]
}

/// A parsed `ffprobe` description of one media file.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaProbe {
    /// The raw `format.format_name`, e.g. `"matroska,webm"`. Kept raw rather
    /// than normalized because normalization is a *policy* question (see
    /// [`crate::foundry::policy::normalize_container`]) and this type is meant
    /// to be a faithful record of what ffprobe said, not an interpretation.
    pub container: String,
    /// Whole-file duration. `None` when ffprobe reported `N/A` — which happens
    /// for some streamed/damaged containers, and which the planner treats as
    /// "cannot decide", never as zero.
    pub duration_secs: Option<f64>,
    /// Whole-file (container) bitrate.
    pub format_bitrate_bps: Option<u64>,
    pub size_bytes: Option<u64>,
    /// Video streams, **excluding cover art** — see [`VideoStream::attached_pic`].
    pub video: Vec<VideoStream>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
    /// Matroska attachments — **including subtitle fonts**.
    ///
    /// Modelled rather than counted, and that is a correction: an earlier
    /// version of this parser folded attachments into a generic "other" count,
    /// and the transcode argv did not map them at all. For anime and many
    /// foreign releases the attached fonts are what the ASS/SSA subtitle track
    /// is styled with — dropping them makes subtitles render in a fallback
    /// face, mispositioned, or not at all. Silently losing them while
    /// reporting the file "rewritten" is exactly the false-claim class this
    /// module is built to prevent, so they are named, carried, and verified.
    pub attachments: Vec<AttachmentStream>,
    /// Count of `data`-type streams (timecode tracks, mov text metadata).
    ///
    /// Counted rather than modelled because Foundry cannot carry them: most
    /// data codecs have no Matroska mapping, so `-map 0:d` would fail the
    /// encode outright. A nonzero count therefore makes the file undecidable
    /// (see `Undecidable::DataStreamsCannotBeCarried`) rather than being
    /// dropped quietly.
    pub data_stream_count: usize,
    /// Streams ffprobe reported with no usable index.
    ///
    /// An index we did not observe cannot be invented — the argv maps streams
    /// by absolute index, so a guess maps the *wrong* stream. But excluding
    /// them silently would let a file be judged on a partial view of its
    /// contents, so the count is surfaced and the planner refuses to decide
    /// while it is nonzero.
    pub unindexed_stream_count: usize,
    /// Chapters. The argv promises `-map_chapters 0`; this is what makes that
    /// promise checkable.
    pub chapter_count: usize,
    /// The container-level `title` tag, when present. The argv promises
    /// `-map_metadata 0`; verifying this one tag is a bounded, honest check on
    /// that promise (see [`crate::foundry::forge::verify_output`] for exactly
    /// how far the metadata claim is verified, and how far it is not).
    pub title: Option<String>,
    /// Streams of a type Foundry does not model at all. Honest about the file
    /// containing more than we described.
    pub other_stream_count: usize,
}

/// A Matroska attachment (typically a subtitle font).
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentStream {
    pub index: u32,
    /// The attachment's codec name (`ttf`, `otf`, `mimetype`-driven).
    pub codec: String,
    /// `tags.filename` — the font's filename, which is what the subtitle
    /// renderer looks up. Verified across a rewrite by filename, not just by
    /// count, so a *substituted* font is caught as well as a missing one.
    pub filename: Option<String>,
}

/// One entry of ffprobe's per-stream `side_data_list`.
///
/// FOUNDRY-03 added this, and it is the *only* place Dolby Vision is visible.
/// A DV file's `codec_name` is plain `hevc` and its `profile` is plain
/// `Main 10` — nothing in the ordinary stream fields distinguishes it from an
/// HDR10 file. The DV profile number lives here, in the `DOVI configuration
/// record` side-data entry, and without it a planner cannot tell a
/// transcodable profile 8 from a profile 5 that renders green and purple when
/// its RPU is dropped.
///
/// `kind` is kept as the raw, un-normalized `side_data_type` string because it
/// is ffprobe's own vocabulary and it has drifted across versions; matching is
/// done case-insensitively by the consumers rather than by mapping to a closed
/// enum here, so a side-data type this fleet has never seen is carried through
/// and reported rather than silently discarded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamSideData {
    /// ffprobe's `side_data_type`, e.g. `"DOVI configuration record"`,
    /// `"Mastering display metadata"`, `"Content light level metadata"`.
    pub kind: String,
    /// `dv_profile` from a DOVI configuration record. 5 is the dangerous one.
    pub dv_profile: Option<u32>,
    /// `dv_bl_signal_compatibility_id`: what the *base layer* is on its own.
    /// 0 = nothing (profile 5's "there is no fallback"), 1 = HDR10, 2 = SDR,
    /// 4 = HLG. This is the field that decides whether a tone-map is possible,
    /// so it is carried separately rather than inferred from the profile.
    pub dv_bl_signal_compatibility_id: Option<u32>,
    /// Whether an RPU (the per-frame Dolby metadata) is present.
    pub rpu_present: Option<bool>,
    /// Whether a base layer is present.
    pub bl_present: Option<bool>,
    /// Whether an enhancement layer is present (profile 7's second layer).
    pub el_present: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoStream {
    /// The stream's absolute index within the file, as ffprobe reported it.
    /// Absolute (not the `v:0` relative form) because the transcode argv maps
    /// streams by absolute index — see
    /// [`crate::foundry::plan::build_transcode_args`].
    pub index: u32,
    pub codec: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bitrate_bps: Option<u64>,
    pub pix_fmt: Option<String>,
    /// ffprobe's `profile`, lowercased — e.g. `"main 10"`, `"high"`.
    ///
    /// Load-bearing for bit depth when `pix_fmt` is absent, and *not*
    /// load-bearing for Dolby Vision: a DV file reports an ordinary HEVC
    /// profile here. Anyone reaching for this to detect DV is reaching for the
    /// wrong field — see [`StreamSideData`].
    pub profile: Option<String>,
    /// ffprobe's `codec_tag_string`, lowercased — e.g. `"hvc1"`, `"dvh1"`,
    /// `"dvhe"`.
    ///
    /// The `dvh1`/`dvhe` tags are a *second*, independent DV signal, and they
    /// matter because they can be present when the DOVI side-data record is
    /// not (an MP4 whose `dvcC` box ffprobe did not surface as side data).
    /// A tag with no record tells us the file is Dolby Vision but not which
    /// profile — which is exactly the state that must fail closed rather than
    /// be assumed benign.
    pub codec_tag: Option<String>,
    /// `color_transfer`, lowercased — `"smpte2084"` (PQ/HDR10),
    /// `"arib-std-b67"` (HLG), `"bt709"` (SDR), or absent.
    ///
    /// This is the primary HDR signal. Absent is *common* and does not mean
    /// SDR — see [`crate::foundry::hdr::classify_hdr`] for the inference that
    /// is drawn from it, and the one that deliberately is not.
    pub color_transfer: Option<String>,
    /// `color_primaries`, lowercased — `"bt2020"` for wide gamut.
    pub color_primaries: Option<String>,
    /// `color_space`, lowercased — e.g. `"bt2020nc"`.
    pub color_space: Option<String>,
    /// ffprobe's `side_data_list` for this stream. Empty when ffprobe reported
    /// none — which, importantly, is not proof that the file carries none.
    pub side_data: Vec<StreamSideData>,
    /// True for embedded cover art (`disposition.attached_pic`).
    ///
    /// This flag is load-bearing and is why [`MediaProbe::video`] is filtered.
    /// A Matroska or MP4 file with a poster embedded carries that poster as a
    /// *video stream* (typically `mjpeg`/`png`, 600x900). Treated as real
    /// video it poisons every downstream judgement: the codec is not in the
    /// acceptable list, so the planner would order a full re-encode of a file
    /// that is already fine, and `-map 0:v:0` might select the artwork instead
    /// of the feature. Filtering it out here — once, in the parser — is the
    /// only place it can be got right for every consumer.
    pub attached_pic: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioStream {
    pub index: u32,
    pub codec: String,
    pub channels: Option<u32>,
    /// ISO-639 language from `tags.language`, lowercased. `None` when the
    /// muxer wrote no tag, which is common and is not an error.
    pub language: Option<String>,
    pub bitrate_bps: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleStream {
    pub index: u32,
    pub codec: String,
    pub language: Option<String>,
    pub forced: bool,
    pub default: bool,
}

impl MediaProbe {
    /// The stream the planner judges: the first non-cover-art video stream.
    ///
    /// `None` for an audio-only file, or for a file whose only "video" was
    /// cover art. Both are cases the planner must decline to act on rather
    /// than guess at.
    pub fn primary_video(&self) -> Option<&VideoStream> {
        self.video.first()
    }
}

/// Why a probe did not produce a [`MediaProbe`].
///
/// [`ProbeError::ToolMissing`] is deliberately its own variant rather than
/// being folded into a generic spawn failure. It is the expected state on this
/// fleet today (ffprobe is absent on <host>), and the pipeline must be able to
/// report "the tool is not installed" distinctly from "the tool ran and the
/// file is broken" — reporting the former as the latter would blame the
/// operator's media for a deployment gap.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeError {
    /// The configured ffprobe binary does not exist on this host.
    ToolMissing { binary: String },
    /// The binary exists but could not be spawned (permissions, resource
    /// limits, ...). Distinct from `ToolMissing` for the same reason
    /// [`crate::streaming::ffmpeg::classify_spawn_error`] draws the line.
    Spawn { binary: String, message: String },
    /// ffprobe ran and exited non-zero — the file is unreadable or not media.
    ExitFailure { code: Option<i32>, stderr: String },
    /// ffprobe produced output that is not the JSON document we asked for.
    MalformedOutput { message: String },
    /// ffprobe did not finish within the allowed time and was killed.
    ///
    /// The case this exists for is not a slow file, it is a WEDGED one: a
    /// transient NFS stall leaves ffprobe in uninterruptible D-state, and with
    /// no timeout the caller waits on it forever. Observed live — a single
    /// stalled episode blocked an entire validation run for over 40 minutes,
    /// and would have blocked it indefinitely. Across ~16,000 items a stall is
    /// not an edge case, it is a certainty.
    Timeout { secs: u64 },
    /// The document parsed but described no streams at all. Treated as an
    /// error, not as an empty probe, because "a file with zero streams" is not
    /// a thing the planner should ever be asked to reason about.
    NoStreams,
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout { secs } => write!(
                f,
                "ffprobe did not finish within {secs}s and was killed — the file is \
                 unreadable, pathologically slow, or the filesystem stalled. This is NOT \
                 a statement about the file's contents"
            ),
            Self::ToolMissing { binary } => write!(
                f,
                "ffprobe binary `{binary}` is not installed on this host — Foundry \
                 cannot describe media files (set MUSE_FOUNDRY_FFPROBE_BIN, or \
                 install ffmpeg)"
            ),
            Self::Spawn { binary, message } => {
                write!(f, "could not spawn ffprobe binary `{binary}`: {message}")
            }
            Self::ExitFailure { code, stderr } => write!(
                f,
                "ffprobe exited with {} — the file is unreadable or is not media: {}",
                code.map(|c| c.to_string()).unwrap_or_else(|| "a signal".into()),
                truncate_for_log(stderr)
            ),
            Self::MalformedOutput { message } => {
                write!(f, "ffprobe output could not be parsed: {message}")
            }
            Self::NoStreams => write!(
                f,
                "ffprobe reported a file with no streams at all — refusing to \
                 treat this as a describable media file"
            ),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Cap an external tool's stderr before it reaches a log line or an error
/// message. ffprobe/ffmpeg can emit kilobytes on a damaged file, and an
/// unbounded splice into a structured log field is how a worker loop turns one
/// bad file into an unreadable log.
fn truncate_for_log(s: &str) -> String {
    const MAX: usize = 400;
    let trimmed = s.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(MAX).collect();
    format!("{head}… (truncated)")
}

// --- The impure edge -------------------------------------------------------

/// Actually invoke `ffprobe` on a guard-resolved path and parse the result.
///
/// This is the **only** function in Foundry that spawns a probe process; every
/// other probe-shaped operation goes through it. It takes a [`ResolvedPath`],
/// not a `&Path`, so "I forgot to validate this path" is a compile error rather
/// than a review catch (see [`crate::foundry::paths`]).
///
/// Read-only by construction: a `ResolvedPath` carries no mutation capability,
/// and ffprobe with these arguments writes nothing.
pub fn run_ffprobe(ffprobe_bin: &str, path: &ResolvedPath) -> Result<MediaProbe, ProbeError> {
    run_ffprobe_with_timeout(ffprobe_bin, path, PROBE_TIMEOUT)
}

/// How long a single ffprobe may take before it is killed.
///
/// **Two minutes.** A local probe is milliseconds and an NFS probe of a large
/// MKV is a few seconds, so this is orders of magnitude above legitimate use
/// and only ever fires on a genuine stall.
///
/// Not unbounded, which is what it was. `Command::output()` waits forever, so
/// one ffprobe wedged in uninterruptible D-state on a stalled NFS read blocked
/// a whole validation run indefinitely — seen live, and unavoidable at
/// 16,000-item scale.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// [`run_ffprobe`] with an explicit deadline.
///
/// Spawns, polls, and kills rather than using `Command::output()`, for the same
/// reason the encoder does: `output()` has no timeout at all.
///
/// A kill does NOT guarantee the process dies — a task in uninterruptible
/// D-state ignores SIGKILL until its I/O returns. What the kill DOES guarantee
/// is that this function stops waiting, so one wedged file costs a stated
/// timeout instead of the entire run. The reaped child is left to the OS.
pub fn run_ffprobe_with_timeout(
    ffprobe_bin: &str,
    path: &ResolvedPath,
    timeout: Duration,
) -> Result<MediaProbe, ProbeError> {
    let args = build_ffprobe_args(&path.as_path().to_string_lossy());
    let output = spawn_with_timeout(ffprobe_bin, &args, timeout)?;

    if !output.status.success() {
        return Err(ProbeError::ExitFailure {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    parse_probe_json(&String::from_utf8_lossy(&output.stdout))
}

/// Most output this will hold from one child, per stream.
///
/// **8 MiB.** ffprobe's JSON tops out in the tens of KB (measured: the largest
/// in this library is ~71 KB), and ffmpeg at `-loglevel error` is near-silent,
/// so honest output never approaches this.
///
/// It exists because "near-silent" is not "silent": a pathological input can
/// make ffmpeg emit a decode error per frame, and a 6-hour encode has millions
/// of frames. Reading that to completion in memory is unbounded growth driven
/// by the contents of an untrusted media file — across 16,000 items, one such
/// file is enough.
const MAX_CAPTURED_BYTES: usize = 8 * 1024 * 1024;

/// Drain a pipe to EOF, keeping at most [`MAX_CAPTURED_BYTES`].
///
/// Keeps draining after the cap rather than stopping, but NOT for the reason
/// it first appears. Stopping early does not deadlock — returning from this
/// function drops the pipe, which closes the read end, and the child then dies
/// of SIGPIPE. (An earlier version of this comment claimed deadlock; that was
/// wrong, and mutation testing showed it: the "stop at the cap" mutant did not
/// hang, it changed how the child DIED.)
///
/// The actual reason is that a child killed by SIGPIPE reports a signal exit
/// rather than its real status, so a merely-verbose encode would be
/// indistinguishable from a genuinely failed one. Draining to EOF lets the
/// child finish and be judged on what it actually did.
///
/// Keeps the HEAD, not the tail: ffmpeg's first error is the one that explains
/// the failure, and the ten-thousandth is a consequence of it.
fn drain_capped(pipe: Option<&mut impl std::io::Read>) -> Vec<u8> {
    let mut kept: Vec<u8> = Vec::new();
    let Some(p) = pipe else { return kept };
    let mut chunk = [0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        match p.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                // `room` alone enforces the cap — saturating so it is 0 once
                // full. A guarding `if` around this was redundant, which
                // mutation testing correctly reported as an equivalent mutant.
                let room = MAX_CAPTURED_BYTES.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
                if n > room {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    if truncated {
        kept.extend_from_slice(
            b"\n[muse: output truncated at 8 MiB; the child kept writing and was still drained]\n",
        );
    }
    kept
}

/// Spawn a process, wait for it with a deadline, and kill it if the deadline
/// passes. Returns its captured output.
///
/// Separate from [`run_ffprobe_with_timeout`] so the deadline behaviour is
/// testable with an ordinary command — `ResolvedPath` is deliberately
/// unconstructible outside the guard, and weakening that to make a test
/// possible would trade a real safety property for a testing convenience.
///
/// A kill does NOT guarantee the process dies: a task in uninterruptible
/// D-state ignores SIGKILL until its I/O returns. What it guarantees is that
/// THIS function stops waiting, so one wedged file costs a stated timeout
/// instead of the whole run.
pub fn spawn_with_timeout(
    bin: &str,
    args: &[String],
    timeout: Duration,
) -> Result<std::process::Output, ProbeError> {
    use std::io::Read;

    let mut child = Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| classify_probe_spawn_error(bin, &e))?;

    // Drain both pipes on their OWN threads, starting immediately.
    //
    // This is not tidiness, it is the difference between working and
    // deadlocking. A pipe holds ~64 KB; once full, the child BLOCKS on write
    // until someone reads. Waiting for exit before reading therefore hangs on
    // any child whose output exceeds the buffer — and ffprobe with
    // `-show_chapters` on a large MKV comfortably does. That hang would then
    // be reported as a timeout, i.e. a perfectly good file failing because of
    // how we read it. `Command::output()` drains concurrently for exactly this
    // reason; a hand-rolled poll loop has to do it too. Raised by codex at the
    // FOUNDRY-10 gate.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_thread = std::thread::spawn(move || drain_capped(out_pipe.as_mut()));
    let err_thread = std::thread::spawn(move || drain_capped(err_pipe.as_mut()));

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    // One last look before giving up: the child can exit
                    // between the `try_wait` above and this check, and
                    // reporting a completed probe as a timeout would discard a
                    // good result. Codex and opus both flagged the race.
                    //
                    // DELIBERATELY UNTESTED: the window is a few microseconds
                    // wide and cannot be hit deterministically, so a test for
                    // it would be flaky rather than informative — and mutation
                    // testing confirms removing this line kills nothing. It is
                    // kept as a cheap safety net, not as covered behaviour.
                    // Its absence costs a good probe being reported as a
                    // timeout, which downstream treats as a SKIP, never as a
                    // verdict about the file.
                    if let Ok(Some(status)) = child.try_wait() {
                        break status;
                    }
                    let _ = child.kill();
                    // `try_wait`, NEVER `wait`. A child in uninterruptible
                    // D-state ignores SIGKILL until its I/O returns, so a
                    // blocking `wait()` here hangs forever — which would make
                    // the timeout path itself hang and defeat the entire
                    // purpose of this function. An earlier revision of this
                    // code did exactly that.
                    //
                    // The consequence of not reaping is a zombie for the run's
                    // lifetime, bounded by the number of STALLS rather than by
                    // the number of files. That is the correct trade: a
                    // handful of zombies is survivable, a hang is not.
                    let _ = child.try_wait();
                    // The reader threads are deliberately abandoned too. They
                    // are blocked in `read` on the wedged child's pipes and
                    // cannot be interrupted; joining them would block for the
                    // same reason. They exit on their own if the child's I/O
                    // ever returns.
                    return Err(ProbeError::Timeout { secs: timeout.as_secs() });
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return Err(ProbeError::Spawn {
                    binary: bin.to_string(),
                    message: format!("waiting for `{bin}`: {e}"),
                })
            }
        }
    };

    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(std::process::Output { status, stdout, stderr })
}

/// Classify a spawn failure. Split out and pure so the missing-binary
/// distinction — the case that is actually live on this fleet — is unit-tested
/// without needing a host that lacks (or has) ffprobe.
pub fn classify_probe_spawn_error(binary: &str, err: &std::io::Error) -> ProbeError {
    if err.kind() == std::io::ErrorKind::NotFound {
        ProbeError::ToolMissing {
            binary: binary.to_string(),
        }
    } else {
        ProbeError::Spawn {
            binary: binary.to_string(),
            message: err.to_string(),
        }
    }
}

// --- The pure parser -------------------------------------------------------

/// ffprobe's JSON document, as literally as serde can express it.
///
/// Every numeric field that ffprobe may render as a *string* is typed
/// `Option<serde_json::Value>` and read through [`as_u64`]/[`as_f64`] rather
/// than being given a concrete numeric type. This is not defensive
/// over-engineering: ffprobe emits `"bit_rate": "5000000"` (a string) for
/// stream and format bitrates while emitting `"width": 1920` (a number) in the
/// same document, the exact rendering has drifted between ffmpeg major
/// versions, and it emits the literal string `"N/A"` for values it could not
/// determine. A concrete `u64` field would make the whole document fail to
/// deserialize the first time any of those three cases showed up — turning a
/// perfectly probeable file into a `MalformedOutput` error.
#[derive(Debug, Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    #[serde(default)]
    format: Option<RawFormat>,
    #[serde(default)]
    chapters: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    #[serde(default)]
    format_name: Option<String>,
    #[serde(default)]
    duration: Option<serde_json::Value>,
    #[serde(default)]
    bit_rate: Option<serde_json::Value>,
    #[serde(default)]
    size: Option<serde_json::Value>,
    #[serde(default)]
    tags: Option<RawTags>,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    #[serde(default)]
    index: Option<serde_json::Value>,
    #[serde(default)]
    codec_name: Option<String>,
    #[serde(default)]
    codec_type: Option<String>,
    #[serde(default)]
    width: Option<serde_json::Value>,
    #[serde(default)]
    height: Option<serde_json::Value>,
    #[serde(default)]
    channels: Option<serde_json::Value>,
    #[serde(default)]
    bit_rate: Option<serde_json::Value>,
    #[serde(default)]
    pix_fmt: Option<String>,
    #[serde(default)]
    profile: Option<serde_json::Value>,
    #[serde(default)]
    codec_tag_string: Option<String>,
    #[serde(default)]
    color_transfer: Option<String>,
    #[serde(default)]
    color_primaries: Option<String>,
    #[serde(default)]
    color_space: Option<String>,
    #[serde(default)]
    side_data_list: Vec<RawSideData>,
    #[serde(default)]
    disposition: Option<RawDisposition>,
    #[serde(default)]
    tags: Option<RawTags>,
}

/// One `side_data_list` entry.
///
/// Every DV field is `Option<serde_json::Value>` for the same reason the
/// bitrates are: ffprobe renders `dv_profile` as a bare number in some builds
/// and as a string in others, and a concrete `u32` here would make an entire
/// probe fail to deserialize — turning a Dolby Vision file into
/// `MalformedOutput`, i.e. into a file we know nothing about, which is the one
/// state where a naive planner is most likely to do damage.
#[derive(Debug, Deserialize)]
struct RawSideData {
    #[serde(default)]
    side_data_type: Option<String>,
    #[serde(default)]
    dv_profile: Option<serde_json::Value>,
    #[serde(default)]
    dv_bl_signal_compatibility_id: Option<serde_json::Value>,
    #[serde(default)]
    rpu_present_flag: Option<serde_json::Value>,
    #[serde(default)]
    bl_present_flag: Option<serde_json::Value>,
    #[serde(default)]
    el_present_flag: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct RawDisposition {
    #[serde(default)]
    default: Option<i64>,
    #[serde(default)]
    forced: Option<i64>,
    #[serde(default)]
    attached_pic: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RawTags {
    #[serde(default)]
    language: Option<String>,
    /// Some muxers write `LANGUAGE` rather than `language`. Matroska in
    /// particular is case-inconsistent across tools, and missing the tag means
    /// a subtitle/audio track silently loses its language.
    #[serde(default, rename = "LANGUAGE")]
    language_upper: Option<String>,
    /// Attachment filename (the font file name a subtitle renderer looks up).
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "TITLE")]
    title_upper: Option<String>,
}

/// Read a JSON value that may be a number **or** a numeric string, returning
/// `None` for `"N/A"`, an empty string, a negative value, or anything else
/// unparseable.
///
/// `None` here means "ffprobe did not tell us", which every caller must handle
/// as an unknown rather than as a zero. Negatives are folded into `None` for
/// the same reason: a negative bitrate is not a fact, it is a malformed one,
/// and `0` would be a *claim* that the stream has no bitrate.
fn as_u64(v: &Option<serde_json::Value>) -> Option<u64> {
    match v.as_ref()? {
        serde_json::Value::Number(n) => n.as_u64().or_else(|| {
            // A float-rendered integer (e.g. 5000000.0) is still a usable
            // value; a negative or NaN one is not.
            let f = n.as_f64()?;
            (f.is_finite() && f >= 0.0).then_some(f as u64)
        }),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// As [`as_u64`], for fractional values (durations).
fn as_f64(v: &Option<serde_json::Value>) -> Option<f64> {
    let parsed = match v.as_ref()? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    // Reject NaN/inf/negative: a duration that is not a finite non-negative
    // number is an unknown, and letting NaN through would make every later
    // comparison silently false (NaN compares false against everything),
    // which is precisely how a truncation check stops catching truncation.
    (parsed.is_finite() && parsed >= 0.0).then_some(parsed)
}

fn as_u32(v: &Option<serde_json::Value>) -> Option<u32> {
    as_u64(v).and_then(|n| u32::try_from(n).ok())
}

/// Read an ffprobe boolean-ish flag: `1`/`0`, or the strings `"1"`/`"0"`.
///
/// `None` when the flag was absent or unreadable, and every caller must treat
/// that as "we do not know" rather than as `false`. A missing `rpu_present_flag`
/// read as `false` would turn a Dolby Vision stream into an ordinary HDR10 one.
fn as_flag(v: &Option<serde_json::Value>) -> Option<bool> {
    match v.as_ref()? {
        serde_json::Value::Bool(b) => Some(*b),
        _ => as_u64(v).map(|n| n != 0),
    }
}

/// Normalize a descriptive ffprobe string field: trim, lowercase, and fold
/// ffprobe's several spellings of "I could not determine this" into `None`.
///
/// `"unknown"` is the one that matters. ffprobe writes it into `color_transfer`
/// and `color_primaries` for most SDR H.264 files, and a consumer that compared
/// it against a known-SDR list would find no match and treat the file as having
/// an *unrecognized* transfer — which is a different, more alarming state than
/// "not stated". Both are unknowns and both must land in the same place.
fn normalize_descriptor(v: &Option<String>) -> Option<String> {
    let s = v.as_ref()?.trim().to_ascii_lowercase();
    if s.is_empty() || s == "unknown" || s == "n/a" || s == "reserved" {
        return None;
    }
    Some(s)
}

/// Parse `ffprobe -print_format json -show_format -show_streams` output.
///
/// Pure and total: it never panics and never spawns anything, so it is fully
/// testable on a host with no ffmpeg at all — which is every host in this
/// fleet today.
pub fn parse_probe_json(stdout: &str) -> Result<MediaProbe, ProbeError> {
    let raw: RawProbe =
        serde_json::from_str(stdout).map_err(|e| ProbeError::MalformedOutput {
            message: e.to_string(),
        })?;

    if raw.streams.is_empty() {
        return Err(ProbeError::NoStreams);
    }

    let format = raw.format;
    let container = format
        .as_ref()
        .and_then(|f| f.format_name.clone())
        .unwrap_or_default();

    let mut video = Vec::new();
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    let mut attachments = Vec::new();
    let mut data_stream_count = 0usize;
    let mut unindexed_stream_count = 0usize;
    let mut other_stream_count = 0usize;

    for s in &raw.streams {
        // A stream with no index cannot be mapped in an ffmpeg argv, so it
        // cannot be planned against — a guessed index would map the WRONG
        // stream. It is counted in its OWN field rather than folded into
        // `other_stream_count`, because the two mean different things: "a
        // stream type we chose not to model" is benign, while "a stream we
        // could not address at all" means our view of the file is incomplete
        // and the planner must refuse to judge it.
        let Some(index) = as_u32(&s.index) else {
            unindexed_stream_count += 1;
            continue;
        };
        let codec = s.codec_name.clone().unwrap_or_default();
        let disposition = s.disposition.as_ref();
        let language = s
            .tags
            .as_ref()
            .and_then(|t| t.language.clone().or_else(|| t.language_upper.clone()))
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|l| !l.is_empty() && l != "und");

        match s.codec_type.as_deref() {
            Some("video") => {
                let attached_pic = disposition.and_then(|d| d.attached_pic).unwrap_or(0) != 0;
                // Cover art is dropped here rather than flagged-and-kept:
                // every consumer wants the feature stream, and a `video` list
                // that can contain artwork is a list every consumer has to
                // remember to filter. `other_stream_count` still records that
                // the file contained it, so nothing is silently vanished.
                if attached_pic {
                    other_stream_count += 1;
                    continue;
                }
                video.push(VideoStream {
                    index,
                    codec,
                    width: as_u32(&s.width),
                    height: as_u32(&s.height),
                    bitrate_bps: as_u64(&s.bit_rate),
                    pix_fmt: normalize_descriptor(&s.pix_fmt),
                    profile: normalize_descriptor(&match &s.profile {
                        // ffprobe renders `profile` as a string for most
                        // codecs but as a bare integer for a few (and for
                        // unrecognized ones). Both are read; neither is
                        // allowed to fail the whole document.
                        Some(serde_json::Value::String(p)) => Some(p.clone()),
                        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
                        _ => None,
                    }),
                    codec_tag: normalize_descriptor(&s.codec_tag_string),
                    color_transfer: normalize_descriptor(&s.color_transfer),
                    color_primaries: normalize_descriptor(&s.color_primaries),
                    color_space: normalize_descriptor(&s.color_space),
                    side_data: s
                        .side_data_list
                        .iter()
                        .map(|d| StreamSideData {
                            kind: d.side_data_type.clone().unwrap_or_default(),
                            dv_profile: as_u32(&d.dv_profile),
                            dv_bl_signal_compatibility_id: as_u32(
                                &d.dv_bl_signal_compatibility_id,
                            ),
                            rpu_present: as_flag(&d.rpu_present_flag),
                            bl_present: as_flag(&d.bl_present_flag),
                            el_present: as_flag(&d.el_present_flag),
                        })
                        .collect(),
                    attached_pic,
                });
            }
            Some("audio") => audio.push(AudioStream {
                index,
                codec,
                channels: as_u32(&s.channels),
                language,
                bitrate_bps: as_u64(&s.bit_rate),
            }),
            Some("subtitle") => subtitles.push(SubtitleStream {
                index,
                codec,
                language,
                forced: disposition.and_then(|d| d.forced).unwrap_or(0) != 0,
                default: disposition.and_then(|d| d.default).unwrap_or(0) != 0,
            }),
            Some("attachment") => attachments.push(AttachmentStream {
                index,
                codec,
                filename: s
                    .tags
                    .as_ref()
                    .and_then(|t| t.filename.clone())
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty()),
            }),
            Some("data") => data_stream_count += 1,
            _ => other_stream_count += 1,
        }
    }

    let title = format
        .as_ref()
        .and_then(|f| f.tags.as_ref())
        .and_then(|t| t.title.clone().or_else(|| t.title_upper.clone()))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    Ok(MediaProbe {
        container,
        duration_secs: as_f64(&format.as_ref().and_then(|f| f.duration.clone())),
        format_bitrate_bps: as_u64(&format.as_ref().and_then(|f| f.bit_rate.clone())),
        size_bytes: as_u64(&format.as_ref().and_then(|f| f.size.clone())),
        video,
        audio,
        subtitles,
        attachments,
        data_stream_count,
        unindexed_stream_count,
        chapter_count: raw.chapters.len(),
        title,
        other_stream_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a real `ffprobe -v quiet -print_format json -show_format
    /// -show_streams` run on a 1080p H.264 Matroska file. Kept verbatim
    /// (including the string-rendered numerics and the `N/A`-free happy path)
    /// so the parser is exercised against ffprobe's actual output shape rather
    /// than a shape invented to match the parser.
    const H264_MKV: &str = r#"{
        "streams": [
            {
                "index": 0,
                "codec_name": "h264",
                "codec_type": "video",
                "width": 1920,
                "height": 1080,
                "pix_fmt": "yuv420p",
                "bit_rate": "5000000",
                "disposition": { "default": 1, "forced": 0, "attached_pic": 0 }
            },
            {
                "index": 1,
                "codec_name": "eac3",
                "codec_type": "audio",
                "channels": 6,
                "bit_rate": "640000",
                "disposition": { "default": 1, "forced": 0 },
                "tags": { "language": "eng" }
            },
            {
                "index": 2,
                "codec_name": "subrip",
                "codec_type": "subtitle",
                "disposition": { "default": 0, "forced": 1 },
                "tags": { "language": "eng" }
            }
        ],
        "format": {
            "format_name": "matroska,webm",
            "duration": "5400.048000",
            "size": "3400000000",
            "bit_rate": "5037037"
        }
    }"#;

    fn h264_mkv() -> MediaProbe {
        parse_probe_json(H264_MKV).expect("the captured fixture must parse")
    }

    #[test]
    fn ffprobe_argv_asks_for_json_only_with_every_section() {
        assert_eq!(
            build_ffprobe_args("/srv/media/Movies/A/A.mkv"),
            vec![
                "-v",
                "quiet",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
                "-show_chapters",
                "/srv/media/Movies/A/A.mkv",
            ]
        );
    }

    #[test]
    fn ffprobe_argv_puts_the_path_last_and_never_quotes_it() {
        // The path is passed as its own argv element, never interpolated into
        // a shell string — a filename with a space or a quote in it (common in
        // a real library) must not be able to change the command.
        let args = build_ffprobe_args("/srv/media/Movies/It's a Wonderful Life (1946).mkv");
        assert_eq!(
            args.last().unwrap(),
            "/srv/media/Movies/It's a Wonderful Life (1946).mkv"
        );
    }

    #[test]
    fn parses_container_duration_and_size_from_string_rendered_numerics() {
        let p = h264_mkv();
        assert_eq!(p.container, "matroska,webm");
        assert_eq!(p.duration_secs, Some(5400.048));
        assert_eq!(p.format_bitrate_bps, Some(5_037_037));
        assert_eq!(p.size_bytes, Some(3_400_000_000));
    }

    #[test]
    fn parses_the_video_stream_with_its_absolute_index() {
        let p = h264_mkv();
        let v = p.primary_video().expect("fixture has a video stream");
        assert_eq!(v.index, 0);
        assert_eq!(v.codec, "h264");
        assert_eq!(v.width, Some(1920));
        assert_eq!(v.height, Some(1080));
        assert_eq!(v.bitrate_bps, Some(5_000_000));
        assert_eq!(v.pix_fmt.as_deref(), Some("yuv420p"));
        assert!(!v.attached_pic);
    }

    #[test]
    fn parses_audio_and_subtitle_streams_with_language_and_disposition() {
        let p = h264_mkv();
        assert_eq!(p.audio.len(), 1);
        assert_eq!(p.audio[0].index, 1);
        assert_eq!(p.audio[0].codec, "eac3");
        assert_eq!(p.audio[0].channels, Some(6));
        assert_eq!(p.audio[0].language.as_deref(), Some("eng"));

        assert_eq!(p.subtitles.len(), 1);
        assert_eq!(p.subtitles[0].index, 2);
        assert_eq!(p.subtitles[0].codec, "subrip");
        assert!(p.subtitles[0].forced, "forced disposition must survive parsing");
        assert!(!p.subtitles[0].default);
    }

    #[test]
    fn cover_art_is_not_reported_as_a_video_stream() {
        // The regression this whole `attached_pic` flag exists for. Treated as
        // real video, this mjpeg poster is a codec the policy rejects, and the
        // planner would order a full re-encode of an already-optimal file.
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "h264", "codec_type": "video",
                  "width": 1920, "height": 1080,
                  "disposition": { "attached_pic": 0 } },
                { "index": 1, "codec_name": "mjpeg", "codec_type": "video",
                  "width": 600, "height": 900,
                  "disposition": { "attached_pic": 1 } }
            ],
            "format": { "format_name": "matroska,webm", "duration": "60.0" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.video.len(), 1, "cover art must not appear in `video`");
        assert_eq!(p.primary_video().unwrap().codec, "h264");
        assert_eq!(
            p.other_stream_count, 1,
            "but it must still be counted, not silently vanished"
        );
    }

    #[test]
    fn an_audio_only_file_has_no_primary_video() {
        let json = r#"{
            "streams": [ { "index": 0, "codec_name": "flac", "codec_type": "audio", "channels": 2 } ],
            "format": { "format_name": "flac", "duration": "180.0" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert!(p.primary_video().is_none());
        assert_eq!(p.audio.len(), 1);
    }

    #[test]
    fn na_values_parse_as_unknown_not_as_zero() {
        // THE honesty case. ffprobe writes "N/A" when it could not determine a
        // value. Reading that as 0 would tell the planner "this file has a
        // zero-second duration / a zero bitrate", both of which are claims we
        // never observed — and a 0 duration would make the truncation check in
        // `verify_output` pass for any output at all.
        let json = r#"{
            "streams": [ { "index": 0, "codec_name": "h264", "codec_type": "video",
                           "width": 1920, "height": 1080, "bit_rate": "N/A" } ],
            "format": { "format_name": "matroska,webm", "duration": "N/A", "bit_rate": "N/A" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.duration_secs, None);
        assert_eq!(p.format_bitrate_bps, None);
        assert_eq!(p.primary_video().unwrap().bitrate_bps, None);
    }

    #[test]
    fn numeric_fields_parse_whether_rendered_as_string_or_number() {
        // ffmpeg major versions have differed on this within one document.
        let as_string = r#"{
            "streams": [ { "index": 0, "codec_name": "h264", "codec_type": "video",
                           "width": "1920", "height": "1080", "bit_rate": "5000000" } ],
            "format": { "format_name": "mp4", "duration": "60.5", "bit_rate": "5000000" }
        }"#;
        let as_number = r#"{
            "streams": [ { "index": 0, "codec_name": "h264", "codec_type": "video",
                           "width": 1920, "height": 1080, "bit_rate": 5000000 } ],
            "format": { "format_name": "mp4", "duration": 60.5, "bit_rate": 5000000 }
        }"#;
        let a = parse_probe_json(as_string).unwrap();
        let b = parse_probe_json(as_number).unwrap();
        assert_eq!(a, b, "string- and number-rendered probes must agree");
        assert_eq!(a.duration_secs, Some(60.5));
        assert_eq!(a.primary_video().unwrap().width, Some(1920));
    }

    #[test]
    fn a_negative_or_nan_duration_is_unknown_not_a_value() {
        // NaN compares false against everything, so a NaN duration that got
        // through would make the truncation comparison in `verify_output`
        // silently non-firing — an unverified output would look verified.
        let json = r#"{
            "streams": [ { "index": 0, "codec_name": "h264", "codec_type": "video" } ],
            "format": { "format_name": "mp4", "duration": "-1", "bit_rate": "-5" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.duration_secs, None);
        assert_eq!(p.format_bitrate_bps, None);
    }

    #[test]
    fn a_stream_with_no_index_is_counted_separately_not_guessed_at() {
        // An index we did not observe cannot be invented: the argv maps
        // streams by absolute index, so a guess would map the wrong stream.
        // It lands in its OWN counter, not `other_stream_count` — the planner
        // refuses to judge a file it cannot fully address, and folding the two
        // together would hide that.
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "h264", "codec_type": "video" },
                { "codec_name": "aac", "codec_type": "audio" }
            ],
            "format": { "format_name": "mp4" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.audio.len(), 0, "an unindexable stream must not be planned against");
        assert_eq!(p.unindexed_stream_count, 1);
        assert_eq!(p.other_stream_count, 0, "it is not merely an unmodelled type");
    }

    #[test]
    fn matroska_font_attachments_are_modelled_by_filename() {
        // Dropping these makes styled ASS/SSA subtitles render in a fallback
        // face or not at all. They are named (not just counted) so a
        // SUBSTITUTED font is caught across a rewrite as well as a missing one.
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "h264", "codec_type": "video" },
                { "index": 1, "codec_name": "ass", "codec_type": "subtitle",
                  "tags": { "language": "eng" } },
                { "index": 2, "codec_name": "ttf", "codec_type": "attachment",
                  "tags": { "filename": "Gandhi Sans Bold.ttf" } },
                { "index": 3, "codec_name": "otf", "codec_type": "attachment",
                  "tags": { "filename": "Trebuchet MS.otf" } }
            ],
            "format": { "format_name": "matroska,webm", "duration": "1420.0" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.attachments.len(), 2);
        assert_eq!(p.attachments[0].index, 2);
        assert_eq!(p.attachments[0].codec, "ttf");
        assert_eq!(
            p.attachments[0].filename.as_deref(),
            Some("Gandhi Sans Bold.ttf")
        );
        assert_eq!(p.attachments[1].filename.as_deref(), Some("Trebuchet MS.otf"));
        assert_eq!(
            p.other_stream_count, 0,
            "attachments are modelled now, not lumped into an opaque count"
        );
    }

    #[test]
    fn data_streams_are_counted_distinctly_from_unmodelled_types() {
        // Foundry cannot carry data streams into Matroska, so the planner has
        // to know they exist in order to refuse rather than drop them.
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "h264", "codec_type": "video" },
                { "index": 1, "codec_name": "bin_data", "codec_type": "data" },
                { "index": 2, "codec_name": "smpte_2038", "codec_type": "data" }
            ],
            "format": { "format_name": "mpegts", "duration": "60.0" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.data_stream_count, 2);
        assert_eq!(p.other_stream_count, 0);
        assert_eq!(p.unindexed_stream_count, 0);
    }

    #[test]
    fn chapters_and_the_container_title_are_parsed() {
        // The argv promises `-map_chapters 0` and `-map_metadata 0`; these are
        // what make those promises checkable rather than decorative.
        let json = r#"{
            "streams": [ { "index": 0, "codec_name": "h264", "codec_type": "video" } ],
            "format": {
                "format_name": "matroska,webm", "duration": "5400.0",
                "tags": { "title": "The Thing (1982)" }
            },
            "chapters": [
                { "id": 0, "start_time": "0.000", "end_time": "600.000" },
                { "id": 1, "start_time": "600.000", "end_time": "1200.000" },
                { "id": 2, "start_time": "1200.000", "end_time": "5400.000" }
            ]
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.chapter_count, 3);
        assert_eq!(p.title.as_deref(), Some("The Thing (1982)"));
    }

    #[test]
    fn a_file_with_no_chapters_or_title_reports_zero_and_none() {
        let p = h264_mkv();
        assert_eq!(p.chapter_count, 0);
        assert_eq!(p.title, None);
        assert!(p.attachments.is_empty());
        assert_eq!(p.data_stream_count, 0);
        assert_eq!(p.unindexed_stream_count, 0);
    }

    #[test]
    fn the_ffprobe_argv_asks_for_chapters() {
        // Without this flag `chapter_count` is always 0 and the
        // `-map_chapters 0` promise silently verifies against nothing.
        assert!(build_ffprobe_args("/x.mkv").contains(&"-show_chapters".to_string()));
    }

    #[test]
    fn undetermined_language_tags_are_none_not_the_literal_und() {
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "h264", "codec_type": "video" },
                { "index": 1, "codec_name": "aac", "codec_type": "audio",
                  "tags": { "language": "und" } },
                { "index": 2, "codec_name": "aac", "codec_type": "audio",
                  "tags": { "LANGUAGE": "FRA" } }
            ],
            "format": { "format_name": "mp4" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.audio[0].language, None);
        assert_eq!(
            p.audio[1].language.as_deref(),
            Some("fra"),
            "the uppercase Matroska spelling must be read, and normalized"
        );
    }

    // --- FOUNDRY-03: colour, bit depth and Dolby Vision ---------------------

    #[test]
    fn an_sdr_h264_file_reports_no_hdr_or_dv_signal_at_all() {
        // The 99% case in this library. Every FOUNDRY-03 field must come back
        // empty rather than defaulted to something that reads as a signal.
        let p = h264_mkv();
        let v = p.primary_video().unwrap();
        assert_eq!(v.color_transfer, None);
        assert_eq!(v.color_primaries, None);
        assert_eq!(v.codec_tag, None);
        assert_eq!(v.profile, None);
        assert!(v.side_data.is_empty());
    }

    #[test]
    fn hdr10_colour_tags_are_parsed_and_lowercased() {
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "hevc", "codec_type": "video",
                  "profile": "Main 10", "codec_tag_string": "hvc1",
                  "width": 3840, "height": 2160, "pix_fmt": "yuv420p10le",
                  "color_space": "bt2020nc", "color_transfer": "smpte2084",
                  "color_primaries": "bt2020" }
            ],
            "format": { "format_name": "matroska,webm", "duration": "7200.0" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        let v = p.primary_video().unwrap();
        assert_eq!(v.color_transfer.as_deref(), Some("smpte2084"));
        assert_eq!(v.color_primaries.as_deref(), Some("bt2020"));
        assert_eq!(v.color_space.as_deref(), Some("bt2020nc"));
        assert_eq!(v.profile.as_deref(), Some("main 10"), "normalized for matching");
        assert_eq!(v.pix_fmt.as_deref(), Some("yuv420p10le"));
        assert!(v.side_data.is_empty(), "HDR10 alone carries no DOVI record");
    }

    #[test]
    fn ffprobes_literal_unknown_is_an_absent_descriptor_not_an_unrecognized_one() {
        // ffprobe writes "unknown" into the colour fields for most SDR H.264.
        // Carried through verbatim it would fail to match any known-SDR name
        // and read as an unrecognized transfer — a scarier state than "not
        // stated", and a different one, for the same underlying fact.
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "h264", "codec_type": "video",
                  "color_transfer": "unknown", "color_primaries": "unknown",
                  "color_space": "unknown", "codec_tag_string": "" }
            ],
            "format": { "format_name": "mp4" }
        }"#;
        let v = parse_probe_json(json).unwrap().video.remove(0);
        assert_eq!(v.color_transfer, None);
        assert_eq!(v.color_primaries, None);
        assert_eq!(v.color_space, None);
        assert_eq!(v.codec_tag, None);
    }

    #[test]
    fn a_dolby_vision_profile_5_stream_is_visible_only_in_its_side_data() {
        // THE fixture this module was extended for. Note what the ordinary
        // fields say: codec `hevc`, profile `Main 10` — identical to an HDR10
        // file. The only thing that distinguishes it is the DOVI record, and
        // `dv_bl_signal_compatibility_id: 0` is the field that says "this base
        // layer is not viewable on its own".
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "hevc", "codec_type": "video",
                  "profile": "Main 10", "codec_tag_string": "dvh1",
                  "width": 3840, "height": 2160, "pix_fmt": "yuv420p10le",
                  "side_data_list": [
                    { "side_data_type": "DOVI configuration record",
                      "dv_version_major": 1, "dv_version_minor": 0,
                      "dv_profile": 5, "dv_level": 6,
                      "rpu_present_flag": 1, "bl_present_flag": 1,
                      "el_present_flag": 0,
                      "dv_bl_signal_compatibility_id": 0 }
                  ] }
            ],
            "format": { "format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "7200.0" }
        }"#;
        let v = parse_probe_json(json).unwrap().video.remove(0);
        assert_eq!(v.codec, "hevc", "the codec name gives nothing away");
        assert_eq!(v.profile.as_deref(), Some("main 10"), "nor does the profile");
        assert_eq!(v.codec_tag.as_deref(), Some("dvh1"));
        assert_eq!(v.side_data.len(), 1);
        let d = &v.side_data[0];
        assert_eq!(d.kind, "DOVI configuration record");
        assert_eq!(d.dv_profile, Some(5));
        assert_eq!(d.dv_bl_signal_compatibility_id, Some(0));
        assert_eq!(d.rpu_present, Some(true));
        assert_eq!(d.el_present, Some(false));
    }

    #[test]
    fn dv_side_data_parses_whether_its_numbers_are_rendered_as_strings() {
        // ffprobe builds differ on this within one document, and a hard u32
        // field would turn a Dolby Vision file into MalformedOutput — i.e.
        // into a file we know nothing at all about, which is the worst state
        // to be in for exactly this content.
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "hevc", "codec_type": "video",
                  "side_data_list": [
                    { "side_data_type": "DOVI configuration record",
                      "dv_profile": "8", "dv_bl_signal_compatibility_id": "1",
                      "rpu_present_flag": "1", "el_present_flag": "0" }
                  ] }
            ],
            "format": { "format_name": "matroska,webm" }
        }"#;
        let v = parse_probe_json(json).unwrap().video.remove(0);
        assert_eq!(v.side_data[0].dv_profile, Some(8));
        assert_eq!(v.side_data[0].dv_bl_signal_compatibility_id, Some(1));
        assert_eq!(v.side_data[0].rpu_present, Some(true));
        assert_eq!(v.side_data[0].el_present, Some(false));
    }

    #[test]
    fn an_absent_dv_flag_is_unknown_not_false() {
        // A missing `rpu_present_flag` read as `false` would demote a Dolby
        // Vision stream to an ordinary HDR10 one.
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "hevc", "codec_type": "video",
                  "side_data_list": [ { "side_data_type": "DOVI configuration record" } ] }
            ],
            "format": { "format_name": "matroska,webm" }
        }"#;
        let v = parse_probe_json(json).unwrap().video.remove(0);
        assert_eq!(v.side_data[0].rpu_present, None);
        assert_eq!(v.side_data[0].dv_profile, None);
        assert_eq!(v.side_data[0].dv_bl_signal_compatibility_id, None);
    }

    #[test]
    fn side_data_types_we_do_not_model_are_carried_not_discarded() {
        // HDR10 static metadata arrives this way on some builds. Foundry has
        // no rule for it yet, and dropping unrecognized side data would mean a
        // future rule could never see it.
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "hevc", "codec_type": "video",
                  "side_data_list": [
                    { "side_data_type": "Mastering display metadata" },
                    { "side_data_type": "Content light level metadata" }
                  ] }
            ],
            "format": { "format_name": "matroska,webm" }
        }"#;
        let v = parse_probe_json(json).unwrap().video.remove(0);
        assert_eq!(v.side_data.len(), 2);
        assert_eq!(v.side_data[0].kind, "Mastering display metadata");
        assert_eq!(v.side_data[1].kind, "Content light level metadata");
        assert_eq!(v.side_data[0].dv_profile, None);
    }

    #[test]
    fn a_numeric_profile_field_does_not_fail_the_document() {
        // ffprobe emits a bare integer here for codecs whose profile names it
        // does not know.
        let json = r#"{
            "streams": [ { "index": 0, "codec_name": "vp9", "codec_type": "video", "profile": 2 } ],
            "format": { "format_name": "matroska,webm" }
        }"#;
        let v = parse_probe_json(json).unwrap().video.remove(0);
        assert_eq!(v.profile.as_deref(), Some("2"));
    }

    #[test]
    fn malformed_output_is_an_error_never_an_empty_probe() {
        // The core honesty rule: a probe that did not parse must not become a
        // MediaProbe with empty stream lists, which the planner would read as
        // "this file has no video".
        let e = parse_probe_json("this is not json").unwrap_err();
        assert!(matches!(e, ProbeError::MalformedOutput { .. }), "got {e:?}");

        // Not even for the plausible-looking empty cases.
        assert!(matches!(parse_probe_json("").unwrap_err(), ProbeError::MalformedOutput { .. }));
        assert!(matches!(parse_probe_json("{}").unwrap_err(), ProbeError::NoStreams));
        assert!(matches!(
            parse_probe_json(r#"{"streams":[],"format":{"format_name":"mp4"}}"#).unwrap_err(),
            ProbeError::NoStreams
        ));
    }

    #[test]
    fn a_missing_format_section_still_parses_the_streams() {
        // `-show_format` can come back empty for an unusual input; the streams
        // are still real observations and must not be thrown away.
        let json = r#"{ "streams": [ { "index": 0, "codec_name": "h264", "codec_type": "video" } ] }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.container, "");
        assert_eq!(p.duration_secs, None);
        assert_eq!(p.video.len(), 1);
    }

    #[test]
    fn a_missing_binary_is_classified_distinctly_from_any_other_spawn_failure() {
        // This is the live case on this fleet: ffprobe is not installed on
        // <host>. It must be reportable as "not installed", never as a broken
        // file or a transient error.
        let missing = std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory");
        assert_eq!(
            classify_probe_spawn_error("ffprobe", &missing),
            ProbeError::ToolMissing { binary: "ffprobe".into() }
        );

        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied");
        assert!(matches!(
            classify_probe_spawn_error("ffprobe", &denied),
            ProbeError::Spawn { .. }
        ));
    }

    #[test]
    fn the_tool_missing_message_names_the_binary_and_the_remedy() {
        let msg = ProbeError::ToolMissing { binary: "ffprobe".into() }.to_string();
        assert!(msg.contains("ffprobe"), "got {msg}");
        assert!(msg.contains("not installed"), "got {msg}");
    }

    /// The bug this fixes, reproduced with a stand-in for a wedged probe.
    ///
    /// `Command::output()` waits forever. Live, a single ffprobe stuck in
    /// uninterruptible D-state on a stalled NFS read held an entire validation
    /// run for over 40 minutes and would have held it indefinitely; the run
    /// only advanced once the process was killed by hand. Across ~16,000 items
    /// a stall is a certainty, not an edge case.
    ///
    /// `sleep` stands in for the wedged probe: the point under test is that
    /// this function stops waiting on a deadline, not anything about ffprobe.
    #[test]
    fn a_probe_that_never_returns_is_abandoned_rather_than_waited_on_forever() {
        let start = std::time::Instant::now();
        let got = spawn_with_timeout("sleep", &["30".to_string()], Duration::from_millis(300));
        let waited = start.elapsed();

        assert!(
            matches!(got, Err(ProbeError::Timeout { .. })),
            "expected a timeout, got {got:?}"
        );
        // Tight bound. Codex: `< 10s` would pass with a materially broken
        // multi-second deadline, so it did not test the number it names.
        assert!(
            waited < Duration::from_secs(3),
            "must stop waiting NEAR the 300ms deadline, waited {waited:?}"
        );
        assert!(
            waited >= Duration::from_millis(250),
            "...and must actually wait for it rather than returning at once: {waited:?}"
        );
        // And the message must not read as a verdict about the file.
        let msg = got.unwrap_err().to_string();
        assert!(msg.contains("NOT a statement about the file"), "{msg}");
    }

    /// The off state: a probe that finishes inside the deadline is unaffected.
    /// Without this, the timeout could be satisfied by always timing out.
    #[test]
    fn a_probe_that_finishes_in_time_is_not_affected_by_the_deadline() {
        let got = spawn_with_timeout("true", &[], Duration::from_secs(30));
        assert!(
            got.is_ok(),
            "a process that finishes immediately must not be affected: {got:?}"
        );
    }

    /// The cap bounds MEMORY without breaking the DRAIN.
    ///
    /// Those two requirements pull against each other: stopping the read at
    /// the cap would bound memory and immediately reintroduce the pipe
    /// deadlock, because the child blocks once the pipe fills and nobody is
    /// reading. So the drain must continue and only the RETENTION stops.
    /// Raised as "technically unbounded" by opus and free at the FOUNDRY-13
    /// gate.
    #[test]
    fn a_child_that_floods_its_output_is_capped_but_still_drained() {
        // 20 MiB, well past the 8 MiB cap and vastly past the 64 KiB pipe.
        let args = vec![
            "-c".to_string(),
            "head -c 20971520 /dev/zero | tr '\\0' 'x'".to_string(),
        ];
        let start = std::time::Instant::now();
        let got = spawn_with_timeout("sh", &args, Duration::from_secs(60));
        let waited = start.elapsed();

        let out = got.expect("a flooding child must COMPLETE, not deadlock");
        assert!(
            out.stdout.len() <= MAX_CAPTURED_BYTES + 200,
            "memory must be bounded, kept {} bytes",
            out.stdout.len()
        );
        assert!(
            out.stdout.len() >= MAX_CAPTURED_BYTES,
            "...but the cap's worth must actually be retained: {}",
            out.stdout.len()
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("output truncated"),
            "truncation must be STATED, not silent — otherwise a caller reads a partial \
             capture as the whole output"
        );
        assert!(
            waited < Duration::from_secs(30),
            "the child must not have blocked: {waited:?}"
        );
        // The property that actually distinguishes draining-to-EOF from
        // stopping at the cap: a child whose pipe is closed early dies of
        // SIGPIPE and reports a SIGNAL exit, so a merely-verbose encode would
        // be indistinguishable from a failed one. Draining lets it finish and
        // be judged on what it did.
        assert!(
            out.status.success(),
            "the child must exit normally, not be killed by SIGPIPE from an early stop: {:?}",
            out.status
        );
    }

    /// The head is kept, not the tail: ffmpeg's FIRST error explains the
    /// failure; the ten-thousandth is a consequence of it.
    #[test]
    fn the_retained_capture_is_the_head_of_the_stream() {
        let args = vec![
            "-c".to_string(),
            "printf 'FIRSTLINE\\n'; head -c 12582912 /dev/zero | tr '\\0' 'z'".to_string(),
        ];
        let out = spawn_with_timeout("sh", &args, Duration::from_secs(60)).expect("completes");
        assert!(
            out.stdout.starts_with(b"FIRSTLINE"),
            "the beginning of the output must survive truncation"
        );
    }

    /// The regression codex caught: piping without draining DEADLOCKS.
    ///
    /// A pipe holds ~64 KB. If the reader waits for exit before reading, any
    /// child producing more than that blocks on write and never exits — and
    /// the poll loop then reports a TIMEOUT for a perfectly good file. ffprobe
    /// with `-show_chapters` on a large MKV comfortably exceeds 64 KB, so this
    /// would have misreported real library files as stalled.
    ///
    /// 2 MiB is far enough past the buffer that a non-draining implementation
    /// cannot pass by luck, and the generous deadline means a failure here is
    /// the deadlock rather than slowness.
    #[test]
    fn a_child_that_writes_more_than_a_pipe_buffer_does_not_deadlock() {
        let args = vec![
            "-c".to_string(),
            // 2 MiB on stdout and a chunk on stderr, so both pipes are exercised.
            "head -c 2097152 /dev/zero | tr '\\0' 'x'; head -c 200000 /dev/zero | tr '\\0' 'e' >&2"
                .to_string(),
        ];
        let start = std::time::Instant::now();
        let got = spawn_with_timeout("sh", &args, Duration::from_secs(60));
        let waited = start.elapsed();

        let out = got.expect("a large-output child must complete, not deadlock");
        assert_eq!(out.stdout.len(), 2_097_152, "all of stdout must be captured");
        assert_eq!(out.stderr.len(), 200_000, "all of stderr must be captured");
        assert!(
            waited < Duration::from_secs(30),
            "completing this should take moments, not approach the deadline: {waited:?}"
        );
    }

    #[test]
    fn tool_stderr_is_truncated_before_it_reaches_a_log() {
        let long = "x".repeat(5000);
        let out = truncate_for_log(&long);
        assert!(out.chars().count() < 500, "len {}", out.chars().count());
        assert!(out.ends_with("(truncated)"));
        // ...and a short one is passed through untouched.
        assert_eq!(truncate_for_log("  boom  "), "boom");
    }

    /// Captured VERBATIM from `ffprobe 5.1.9-0+deb12u1` on the deployment host
    /// (<host>), running `-show_streams -select_streams v:0` against a real
    /// Dolby Vision title in the library. Only irrelevant keys were dropped;
    /// every value below is exactly what the deployment host emits.
    ///
    /// This exists because every other DV test in this file uses a fixture
    /// someone WROTE. That proves the classifier is self-consistent, not that
    /// it matches reality — and the whole DV refusal rests on ffprobe actually
    /// emitting `side_data_list` under `-show_streams`, which is a real
    /// build-dependent question and not a thing the code can assert about
    /// itself. It does emit it. If a host upgrade ever stops, this test is
    /// what notices, rather than a profile 5 file being silently tone-mapped.
    const LIVE_DOLBY_VISION_STREAM: &str = r#"{
      "streams": [{
        "index": 0,
        "codec_name": "hevc",
        "codec_long_name": "H.265 / HEVC (High Efficiency Video Coding)",
        "profile": "Main 10",
        "codec_type": "video",
        "codec_tag_string": "[0][0][0][0]",
        "codec_tag": "0x0000",
        "width": 3832, "height": 2068,
        "coded_width": 3832, "coded_height": 2072,
        "has_b_frames": 4,
        "sample_aspect_ratio": "1:1",
        "display_aspect_ratio": "958:517",
        "pix_fmt": "yuv420p10le",
        "level": 150,
        "color_range": "tv",
        "color_space": "bt2020nc",
        "color_transfer": "smpte2084",
        "color_primaries": "bt2020",
        "chroma_location": "topleft",
        "refs": 1,
        "side_data_list": [{
          "side_data_type": "DOVI configuration record",
          "dv_version_major": 1, "dv_version_minor": 0,
          "dv_profile": 8, "dv_level": 6,
          "rpu_present_flag": 1, "el_present_flag": 0, "bl_present_flag": 1,
          "dv_bl_signal_compatibility_id": 1
        }]
      }]
    }"#;

    #[test]
    fn a_real_dolby_vision_stream_from_the_deployment_host_parses_end_to_end() {
        let p = parse_probe_json(LIVE_DOLBY_VISION_STREAM).expect("live capture must parse");
        let v = p.primary_video().expect("a video stream");

        // The DOVI record survives the round trip. If `side_data` came back
        // empty here, DV detection would fall through to the codec tag —
        // which, as the next assertion shows, is not available on this file.
        let d = v
            .side_data
            .iter()
            .find(|d| d.kind == "DOVI configuration record")
            .expect("the DOVI record ffprobe actually emitted");
        assert_eq!(d.dv_profile, Some(8));
        assert_eq!(d.dv_bl_signal_compatibility_id, Some(1));

        // MKV renders an absent codec tag as this literal, NOT as an empty
        // string. So on this file the tag fallback carries no DV signal, and
        // detection rests entirely on the side-data record above. It must also
        // not be mistaken FOR a tag that means something.
        assert_ne!(
            v.codec_tag.as_deref(),
            Some("dvh1"),
            "the placeholder tag must not be read as a real DV tag"
        );

        // The colour metadata is genuine HDR10 PQ/BT.2020.
        assert_eq!(v.color_transfer.as_deref(), Some("smpte2084"));
        assert_eq!(v.color_primaries.as_deref(), Some("bt2020"));

        // Real 4K releases are cropped, not 3840x2160. A resolution rule that
        // assumes the nominal frame size misfiles this one.
        assert_eq!((v.width, v.height), (Some(3832), Some(2068)));
    }
}
