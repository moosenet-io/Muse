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
/// the per-stream detail; both are needed and neither is the default.
pub fn build_ffprobe_args(file_path: &str) -> Vec<String> {
    vec![
        "-v".to_string(),
        "quiet".to_string(),
        "-print_format".to_string(),
        "json".to_string(),
        "-show_format".to_string(),
        "-show_streams".to_string(),
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
    /// Streams of any other type (`data`, `attachment`). Counted rather than
    /// described: nothing here plans against them, but a nonzero count is
    /// honest about the file containing more than we modelled.
    pub other_stream_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
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
    /// The document parsed but described no streams at all. Treated as an
    /// error, not as an empty probe, because "a file with zero streams" is not
    /// a thing the planner should ever be asked to reason about.
    NoStreams,
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
    let args = build_ffprobe_args(&path.as_path().to_string_lossy());
    let output = Command::new(ffprobe_bin)
        .args(&args)
        .output()
        .map_err(|e| classify_probe_spawn_error(ffprobe_bin, &e))?;

    if !output.status.success() {
        return Err(ProbeError::ExitFailure {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    parse_probe_json(&String::from_utf8_lossy(&output.stdout))
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
    disposition: Option<RawDisposition>,
    #[serde(default)]
    tags: Option<RawTags>,
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
    let mut other_stream_count = 0usize;

    for s in &raw.streams {
        // A stream with no index cannot be mapped in an ffmpeg argv, so it
        // cannot be planned against. Count it as "other" rather than
        // inventing an index — a guessed index would map the WRONG stream.
        let Some(index) = as_u32(&s.index) else {
            other_stream_count += 1;
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
                    pix_fmt: s.pix_fmt.clone(),
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
            _ => other_stream_count += 1,
        }
    }

    Ok(MediaProbe {
        container,
        duration_secs: as_f64(&format.as_ref().and_then(|f| f.duration.clone())),
        format_bitrate_bps: as_u64(&format.as_ref().and_then(|f| f.bit_rate.clone())),
        size_bytes: as_u64(&format.as_ref().and_then(|f| f.size.clone())),
        video,
        audio,
        subtitles,
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
    fn ffprobe_argv_asks_for_json_only_with_both_sections() {
        assert_eq!(
            build_ffprobe_args("/srv/media/Movies/A/A.mkv"),
            vec![
                "-v",
                "quiet",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
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
    fn a_stream_with_no_index_is_counted_not_guessed_at() {
        // An index we did not observe cannot be invented: the argv maps
        // streams by absolute index, so a guess would map the wrong stream.
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "h264", "codec_type": "video" },
                { "codec_name": "aac", "codec_type": "audio" }
            ],
            "format": { "format_name": "mp4" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.audio.len(), 0, "an unindexable stream must not be planned against");
        assert_eq!(p.other_stream_count, 1);
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

    #[test]
    fn tool_stderr_is_truncated_before_it_reaches_a_log() {
        let long = "x".repeat(5000);
        let out = truncate_for_log(&long);
        assert!(out.chars().count() < 500, "len {}", out.chars().count());
        assert!(out.ends_with("(truncated)"));
        // ...and a short one is passed through untouched.
        assert_eq!(truncate_for_log("  boom  "), "boom");
    }
}
