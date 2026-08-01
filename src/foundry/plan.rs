//! MUSEF-02 — the pure decision: given a [`MediaProbe`] and a
//! [`TranscodePolicy`], what (if anything) should be done to this file?
//!
//! ## Nothing here touches the filesystem or spawns anything
//! [`plan_transcode`] is a total function of its two inputs. That is what makes
//! the interesting cases testable on a host with no ffmpeg — which is every
//! host in this fleet today — and it is why the argv is built here rather than
//! at the call site that spawns it: an argv built next to a `Command` is an
//! argv nobody can assert on.
//!
//! ## The three-way decision, and why there are three
//! A two-way `bool` ("does this need transcoding?") cannot express the case
//! that actually matters: *we do not know*. A file whose duration ffprobe could
//! not determine, or whose container Foundry does not recognize, is not
//! "already optimal" — that would be a claim about a file we failed to
//! understand, and it would leave the file silently un-optimized with no
//! record. So [`TranscodeDecision::CannotDecide`] is a first-class outcome
//! carrying the specific reason, and the executor reports it rather than
//! swallowing it.
//!
//! Symmetrically, [`TranscodeDecision::AlreadyOptimal`] is only ever returned
//! when every dimension of the policy was actually *checked and passed*. It is
//! never a fallthrough.

use crate::foundry::policy::{
    normalize_container, scale_to_fit, Container, TranscodePolicy,
};
use crate::foundry::probe::MediaProbe;

/// What Foundry decided about one file.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscodeDecision {
    /// Every policy dimension was checked and passed. Nothing to do.
    AlreadyOptimal,
    /// The file should be rewritten. Carries both the machine-readable plan and
    /// the exact argv, plus every reason that contributed — a transcode with an
    /// empty reason list is unrepresentable by construction (see
    /// [`plan_transcode`]).
    Transcode {
        plan: TranscodePlan,
        args: Vec<String>,
        reasons: Vec<TranscodeReason>,
    },
    /// Foundry could not judge this file. The file is left alone and the
    /// reason is reported; this is never silently folded into "optimal".
    CannotDecide { why: Undecidable },
}

/// The mechanical plan: which streams, encoded how, into what container.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscodePlan {
    /// Absolute index of the video stream to keep (cover art already excluded
    /// by the probe parser).
    pub video_stream_index: u32,
    pub video: VideoAction,
    pub audio: AudioAction,
    pub container: Container,
}

impl TranscodePlan {
    /// True when this plan re-encodes nothing — a pure container rewrite.
    ///
    /// Worth distinguishing in reports and logs: a remux is lossless and cheap,
    /// a re-encode is neither, and an operator reading "Foundry rewrote 400
    /// files" deserves to know which kind.
    pub fn is_remux_only(&self) -> bool {
        self.video == VideoAction::Copy && self.audio == AudioAction::Copy
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoAction {
    /// Stream-copy the video: no quality loss, no CPU.
    Copy,
    /// Re-encode. `scale` is `Some` only when a downscale was ordered.
    Encode { scale: Option<(u32, u32)> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioAction {
    Copy,
    /// Re-encode every audio stream to the policy's audio target.
    Encode,
}

/// A specific, checked policy violation. An enum rather than a `String` so a
/// test asserts on the *reason*, not on prose that can be reworded without
/// anyone noticing the logic changed underneath it.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscodeReason {
    VideoCodecNotAccepted {
        found: String,
    },
    ResolutionAboveCeiling {
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    },
    VideoBitrateAboveCeiling {
        found_bps: u64,
        ceiling_bps: u64,
    },
    AudioCodecNotAccepted {
        stream_index: u32,
        found: String,
    },
    AudioChannelsAboveCeiling {
        stream_index: u32,
        found: u32,
        max: u32,
    },
    ContainerNotAccepted {
        found: Container,
    },
}

impl std::fmt::Display for TranscodeReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VideoCodecNotAccepted { found } => {
                write!(f, "video codec `{found}` is not in the accepted set")
            }
            Self::ResolutionAboveCeiling {
                width,
                height,
                max_width,
                max_height,
            } => write!(
                f,
                "resolution {width}x{height} is above the {max_width}x{max_height} ceiling"
            ),
            Self::VideoBitrateAboveCeiling {
                found_bps,
                ceiling_bps,
            } => write!(
                f,
                "video bitrate {found_bps} bps is above the {ceiling_bps} bps ceiling \
                 (policy maximum plus tolerance)"
            ),
            Self::AudioCodecNotAccepted { stream_index, found } => write!(
                f,
                "audio stream {stream_index} codec `{found}` is not in the accepted set"
            ),
            Self::AudioChannelsAboveCeiling {
                stream_index,
                found,
                max,
            } => write!(
                f,
                "audio stream {stream_index} has {found} channels, above the {max} ceiling"
            ),
            Self::ContainerNotAccepted { found } => {
                write!(f, "container `{}` is not in the accepted set", found.ffmpeg_format())
            }
        }
    }
}

/// Why Foundry declined to judge a file.
///
/// Every variant is a fact ffprobe did *not* give us. None of them are
/// failures of the file — they are limits of what we observed, and they are
/// reported as such.
#[derive(Debug, Clone, PartialEq)]
pub enum Undecidable {
    /// No video stream at all (or only cover art). Foundry's policy is written
    /// for video files; an audio-only file is out of scope, not "optimal".
    NoVideoStream,
    /// ffprobe named no codec for the video stream.
    UnknownVideoCodec,
    /// Width or height missing, or zero. Without dimensions the resolution
    /// ceiling cannot be checked and a scale filter cannot be computed.
    UnknownVideoDimensions,
    /// ffprobe named no codec for an audio stream. Re-encoding a stream we
    /// cannot identify, or passing it through as "acceptable", are both claims
    /// we have no basis for.
    UnknownAudioCodec { stream_index: u32 },
    /// ffprobe reported no channel count for an audio stream.
    ///
    /// The same reasoning as [`Undecidable::UnknownDuration`], applied
    /// uniformly (review finding 3). An earlier version skipped the
    /// channel-ceiling check when the count was absent, which meant
    /// `AlreadyOptimal` could be returned for a file whose channel count was
    /// never checked — while `AlreadyOptimal` is documented as "every
    /// dimension checked and passed". Unknown must not resolve to "fine".
    UnknownAudioChannels { stream_index: u32 },
    /// ffprobe reported streams Foundry could not address by index, so its
    /// view of the file is incomplete. Judging a file on a partial view is how
    /// a stream gets silently dropped.
    UnindexedStreams { count: usize },
    /// The file carries `data` streams, which Foundry cannot carry across a
    /// rewrite: most data codecs have no Matroska mapping, so `-map 0:d` fails
    /// the encode outright. Refusing is the fail-closed choice — the
    /// alternative is dropping them and calling the file "rewritten".
    ///
    /// Deliberately conservative. If a future item establishes which data
    /// codecs remux safely, this can narrow to those; it must not widen to
    /// "drop them quietly".
    DataStreamsCannotBeCarried { count: usize },
    /// The file has attachments (typically subtitle fonts) but the target
    /// container cannot hold them. Only Matroska can, so this arises when an
    /// MP4 source with attachments would stay MP4. Dropping the fonts would
    /// break subtitle rendering silently.
    AttachmentsCannotBeCarried { count: usize },
    /// The container is not one Foundry recognizes, so it cannot know whether
    /// the streams survive a remux.
    UnrecognizedContainer { found: String },
    /// No duration. This one is not about the *plan* — it is about the
    /// *verification*: the post-encode check proves an output is not truncated
    /// by comparing its duration to the source's. With no source duration there
    /// is nothing to compare against, so a completed encode could never be
    /// proven complete. Refusing to plan is the fail-closed choice, because the
    /// alternative is a destructive swap backed by an unverifiable success.
    UnknownDuration,
    /// The video bitrate could not be determined and could not be bounded from
    /// the container bitrate either — and nothing *else* was wrong with the
    /// file. Calling it optimal would assert a bitrate we never observed.
    UnknownVideoBitrate,
}

impl std::fmt::Display for Undecidable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoVideoStream => write!(
                f,
                "the file has no video stream (audio-only, or only cover art) — \
                 Foundry's transcode policy does not apply to it"
            ),
            Self::UnknownVideoCodec => {
                write!(f, "ffprobe reported no codec name for the video stream")
            }
            Self::UnknownVideoDimensions => write!(
                f,
                "ffprobe reported no usable width/height for the video stream, so the \
                 resolution ceiling cannot be checked"
            ),
            Self::UnknownAudioCodec { stream_index } => write!(
                f,
                "ffprobe reported no codec name for audio stream {stream_index}"
            ),
            Self::UnknownAudioChannels { stream_index } => write!(
                f,
                "ffprobe reported no channel count for audio stream {stream_index}, so the \
                 channel ceiling cannot be checked and the file cannot be declared \
                 within policy"
            ),
            Self::UnindexedStreams { count } => write!(
                f,
                "ffprobe reported {count} stream(s) with no usable index, so Foundry's \
                 view of this file is incomplete — refusing to judge it rather than \
                 risk silently dropping a stream"
            ),
            Self::DataStreamsCannotBeCarried { count } => write!(
                f,
                "the file carries {count} data stream(s), which Foundry cannot carry \
                 across a rewrite — refusing rather than dropping them silently"
            ),
            Self::AttachmentsCannotBeCarried { count } => write!(
                f,
                "the file carries {count} attachment(s) (typically subtitle fonts) that \
                 the target container cannot hold — refusing rather than breaking \
                 subtitle rendering silently"
            ),
            Self::UnrecognizedContainer { found } => write!(
                f,
                "container `{found}` is not one Foundry recognizes, so it cannot know \
                 whether the streams survive a rewrite"
            ),
            Self::UnknownDuration => write!(
                f,
                "ffprobe reported no duration, so a transcode of this file could never \
                 be verified as complete — refusing to plan a swap that could not be \
                 checked for truncation"
            ),
            Self::UnknownVideoBitrate => write!(
                f,
                "the video bitrate could not be determined or bounded, so this file \
                 cannot be declared within policy"
            ),
        }
    }
}

/// The bitrate check's three outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitrateVerdict {
    /// Proven at or below the ceiling.
    WithinCeiling,
    /// Proven above it.
    Exceeds { found_bps: u64 },
    /// Neither could be proven.
    Unknown,
}

/// Judge a video stream's bitrate against a ceiling, using the container
/// bitrate as a **bound** when the per-stream figure is missing.
///
/// The fallback is the interesting part, and it is deliberately one-sided.
/// Matroska very often carries no per-stream `bit_rate` at all, so a planner
/// that simply gave up whenever it was absent would declare most of the
/// library undecidable and do nothing useful. But substituting the *container*
/// bitrate for the video bitrate would be wrong in the other direction: the
/// container figure includes audio and subtitles, so it overstates video and
/// would order irreversible re-encodes of files that are actually within
/// policy.
///
/// The sound half of that inference is used and the unsound half is not.
/// Video bitrate is necessarily ≤ container bitrate, so:
/// - container bitrate ≤ ceiling ⇒ video bitrate ≤ ceiling. **Proven within.**
/// - container bitrate > ceiling ⇒ nothing follows about the video stream.
///   **Unknown**, never "exceeds".
pub fn bitrate_verdict(
    video_bitrate_bps: Option<u64>,
    format_bitrate_bps: Option<u64>,
    ceiling_bps: u64,
) -> BitrateVerdict {
    if let Some(v) = video_bitrate_bps {
        return if v > ceiling_bps {
            BitrateVerdict::Exceeds { found_bps: v }
        } else {
            BitrateVerdict::WithinCeiling
        };
    }
    match format_bitrate_bps {
        Some(total) if total <= ceiling_bps => BitrateVerdict::WithinCeiling,
        _ => BitrateVerdict::Unknown,
    }
}

/// Whether a container can carry attachment streams (subtitle fonts).
///
/// Only Matroska can, in practice. MP4 has no attachment concept at all, and
/// the rest of the recognized set are legacy containers with none either. This
/// is why an MP4 with fonts is refused rather than rewritten: there is nowhere
/// for the fonts to go, and losing them breaks styled subtitle rendering.
pub fn container_holds_attachments(container: Container) -> bool {
    matches!(container, Container::Matroska)
}

/// The container a rewrite of this file would be written into, or `None` when
/// the source container is not one Foundry recognizes.
///
/// Extracted as its own function because [`crate::foundry::forge`] needs the
/// answer *before* it can plan: the staging filename's extension depends on it,
/// and [`plan_transcode`] needs the staging path to build the argv. Duplicating
/// the rule at the call site would let the staged file's extension drift out of
/// step with the `-f` the argv asks for — an mkv named `.mp4`, which is exactly
/// the kind of silent mismatch that becomes an unplayable library file.
///
/// The rule: keep an already-acceptable container, otherwise use the policy's
/// output container. Keeping it matters — rewriting a conforming MP4 to MKV
/// purely because MKV is the default would churn files that are fine and move
/// paths the *arr tools already track.
pub fn output_container(probe: &MediaProbe, policy: &TranscodePolicy) -> Option<Container> {
    let source = normalize_container(&probe.container)?;
    Some(if policy.accepts_container(source) {
        source
    } else {
        policy.output_container
    })
}

/// Decide what to do with one probed file.
///
/// Pure: no filesystem access, no process spawning, no clock. `input_path` and
/// `output_path` are used only to build the argv.
///
/// The returned [`TranscodeDecision::Transcode`] always carries a non-empty
/// `reasons` list — the only path that constructs it is the one guarded by
/// `reasons.is_empty()` below, so "we rewrote this file but cannot say why" is
/// not a state this function can produce.
pub fn plan_transcode(
    probe: &MediaProbe,
    policy: &TranscodePolicy,
    input_path: &str,
    output_path: &str,
) -> TranscodeDecision {
    // --- Undecidables first ------------------------------------------------
    // Each of these is a fact we needed and did not get. Checked before any
    // policy comparison so a partial probe can never produce a partial verdict
    // that happens to look like "optimal".

    // An incomplete view of the file comes first: every judgement below is
    // made on the streams we could see, so if there are streams we could not
    // address at all, none of those judgements is trustworthy.
    if probe.unindexed_stream_count > 0 {
        return TranscodeDecision::CannotDecide {
            why: Undecidable::UnindexedStreams {
                count: probe.unindexed_stream_count,
            },
        };
    }

    if probe.data_stream_count > 0 {
        return TranscodeDecision::CannotDecide {
            why: Undecidable::DataStreamsCannotBeCarried {
                count: probe.data_stream_count,
            },
        };
    }

    let Some(container) = normalize_container(&probe.container) else {
        return TranscodeDecision::CannotDecide {
            why: Undecidable::UnrecognizedContainer {
                found: probe.container.clone(),
            },
        };
    };

    let Some(video) = probe.primary_video() else {
        return TranscodeDecision::CannotDecide {
            why: Undecidable::NoVideoStream,
        };
    };

    if video.codec.trim().is_empty() {
        return TranscodeDecision::CannotDecide {
            why: Undecidable::UnknownVideoCodec,
        };
    }

    let (width, height) = match (video.width, video.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => {
            return TranscodeDecision::CannotDecide {
                why: Undecidable::UnknownVideoDimensions,
            }
        }
    };

    // Duration gates the *verification*, not the plan — see
    // `Undecidable::UnknownDuration`. Without it, no output of this transcode
    // could ever be proven un-truncated, and the swap is destructive.
    if probe.duration_secs.is_none() {
        return TranscodeDecision::CannotDecide {
            why: Undecidable::UnknownDuration,
        };
    }

    for a in &probe.audio {
        if a.codec.trim().is_empty() {
            return TranscodeDecision::CannotDecide {
                why: Undecidable::UnknownAudioCodec {
                    stream_index: a.index,
                },
            };
        }
        // Review finding 3: an absent channel count used to skip the channel
        // ceiling silently, so `AlreadyOptimal` could be returned for a file
        // that was never checked on that axis. Same rule as duration —
        // unknown is undecidable, not "fine".
        if a.channels.map_or(true, |c| c == 0) {
            return TranscodeDecision::CannotDecide {
                why: Undecidable::UnknownAudioChannels {
                    stream_index: a.index,
                },
            };
        }
    }

    // The container a rewrite would produce, needed here (before the policy
    // comparison) because whether attachments can survive depends on it.
    let target_container = if policy.accepts_container(container) {
        container
    } else {
        policy.output_container
    };
    if !probe.attachments.is_empty() && !container_holds_attachments(target_container) {
        return TranscodeDecision::CannotDecide {
            why: Undecidable::AttachmentsCannotBeCarried {
                count: probe.attachments.len(),
            },
        };
    }

    // --- Policy comparison -------------------------------------------------

    let mut reasons: Vec<TranscodeReason> = Vec::new();
    let mut encode_video = false;
    let mut encode_audio = false;

    if !policy.accepts_video_codec(&video.codec) {
        reasons.push(TranscodeReason::VideoCodecNotAccepted {
            found: video.codec.clone(),
        });
        encode_video = true;
    }

    if width > policy.max_width || height > policy.max_height {
        reasons.push(TranscodeReason::ResolutionAboveCeiling {
            width,
            height,
            max_width: policy.max_width,
            max_height: policy.max_height,
        });
        encode_video = true;
    }

    let ceiling = policy.effective_video_bitrate_ceiling();
    let bitrate = bitrate_verdict(video.bitrate_bps, probe.format_bitrate_bps, ceiling);
    if let BitrateVerdict::Exceeds { found_bps } = bitrate {
        reasons.push(TranscodeReason::VideoBitrateAboveCeiling {
            found_bps,
            ceiling_bps: ceiling,
        });
        encode_video = true;
    }

    for a in &probe.audio {
        if !policy.accepts_audio_codec(&a.codec) {
            reasons.push(TranscodeReason::AudioCodecNotAccepted {
                stream_index: a.index,
                found: a.codec.clone(),
            });
            encode_audio = true;
        }
        if let Some(ch) = a.channels {
            if ch > policy.max_audio_channels {
                reasons.push(TranscodeReason::AudioChannelsAboveCeiling {
                    stream_index: a.index,
                    found: ch,
                    max: policy.max_audio_channels,
                });
                encode_audio = true;
            }
        }
    }

    if !policy.accepts_container(container) {
        reasons.push(TranscodeReason::ContainerNotAccepted { found: container });
    }

    // An unknown bitrate only blocks the verdict when there is nothing else to
    // do. If the file is being re-encoded anyway the question is moot: the
    // encode caps the output bitrate by construction, so the unknown resolves
    // itself. If the file is otherwise clean, though, declaring it optimal
    // would assert a bitrate nobody measured.
    if reasons.is_empty() && bitrate == BitrateVerdict::Unknown {
        return TranscodeDecision::CannotDecide {
            why: Undecidable::UnknownVideoBitrate,
        };
    }

    if reasons.is_empty() {
        // Reached only after every dimension above was checked and passed.
        return TranscodeDecision::AlreadyOptimal;
    }

    let scale = if encode_video {
        let (sw, sh) = scale_to_fit(width, height, policy.max_width, policy.max_height);
        // Only emit a filter when it actually changes something — a
        // scale=1920:1080 on an already-1080p source is a wasted filter pass
        // and an extra resample of untouched pixels.
        (sw != width || sh != height).then_some((sw, sh))
    } else {
        None
    };

    let plan = TranscodePlan {
        video_stream_index: video.index,
        video: if encode_video {
            VideoAction::Encode { scale }
        } else {
            VideoAction::Copy
        },
        audio: if encode_audio {
            AudioAction::Encode
        } else {
            AudioAction::Copy
        },
        // Shared with `crate::foundry::forge`, which needs the same answer to
        // name the staging file — see `output_container`.
        container: if policy.accepts_container(container) {
            container
        } else {
            policy.output_container
        },
    };

    let args = build_transcode_args(&plan, policy, input_path, output_path);
    TranscodeDecision::Transcode { plan, args, reasons }
}

/// Build the exact ffmpeg argv for a plan. Pure — this is the only place an
/// encode command is constructed, and it is asserted on directly in tests.
pub fn build_transcode_args(
    plan: &TranscodePlan,
    policy: &TranscodePolicy,
    input_path: &str,
    output_path: &str,
) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let push = |a: &mut Vec<String>, s: &str| a.push(s.to_string());

    push(&mut a, "-hide_banner");
    push(&mut a, "-loglevel");
    push(&mut a, "error");
    // `-nostdin` is not optional for a background worker. ffmpeg reads stdin
    // for interactive keys by default; run from a service or a test harness it
    // will consume whatever is on the inherited stdin, and can wedge waiting on
    // it. This is a long-running encode spawned unattended — it must never be
    // able to block on, or steal, input.
    push(&mut a, "-nostdin");
    // The output path is a freshly-resolved staging file that we may be
    // retrying over; `-y` avoids ffmpeg's interactive overwrite prompt, which
    // with `-nostdin` would otherwise be an immediate failure.
    push(&mut a, "-y");

    push(&mut a, "-i");
    a.push(input_path.to_string());

    // Map exactly the primary video stream by ABSOLUTE index, plus all audio
    // and all subtitles. Absolute, not `0:v:0`, because the probe parser
    // filtered cover art out of its video list — so `v:0` in ffmpeg's own
    // numbering may well be the poster, while the index we carry is the one we
    // actually judged. The `?` on audio/subtitles makes them optional so a file
    // with no subtitle track is not a hard failure.
    a.push("-map".to_string());
    a.push(format!("0:{}", plan.video_stream_index));
    push(&mut a, "-map");
    push(&mut a, "0:a?");
    push(&mut a, "-map");
    push(&mut a, "0:s?");
    // Attachments — subtitle fonts. Mapped only into a container that can hold
    // them; the planner refuses the file outright rather than reaching here
    // with fonts and an MP4 target (see `Undecidable::AttachmentsCannotBeCarried`).
    //
    // This map was MISSING in the first version of this argv, which silently
    // dropped every font in the file while reporting the result "rewritten" —
    // for anime and many foreign releases that breaks styled subtitle
    // rendering outright. `-c:t copy` is explicit for the same reason the
    // other codec flags are: a default is not a guarantee.
    if container_holds_attachments(plan.container) {
        push(&mut a, "-map");
        push(&mut a, "0:t?");
    }
    // Explicit rather than relying on ffmpeg's defaults: chapters and global
    // metadata (title, tags the *arr tools wrote) must survive the rewrite, and
    // a default is not a guarantee across ffmpeg versions.
    push(&mut a, "-map_metadata");
    push(&mut a, "0");
    push(&mut a, "-map_chapters");
    push(&mut a, "0");

    match plan.video {
        VideoAction::Copy => {
            push(&mut a, "-c:v");
            push(&mut a, "copy");
        }
        VideoAction::Encode { scale } => {
            push(&mut a, "-c:v");
            a.push(policy.encode_video.ffmpeg_name().to_string());
            push(&mut a, "-crf");
            a.push(policy.crf.to_string());
            push(&mut a, "-preset");
            a.push(policy.preset.clone());
            // yuv420p explicitly: a 10-bit or 4:2:2 source would otherwise
            // produce an output most clients in this fleet cannot direct-play,
            // which is the exact problem this stage exists to solve.
            push(&mut a, "-pix_fmt");
            push(&mut a, "yuv420p");
            if let Some((w, h)) = scale {
                push(&mut a, "-vf");
                a.push(format!("scale={w}:{h}"));
            }
            // CRF alone sets quality, not a ceiling — a high-motion source can
            // still blow past the policy bitrate. maxrate/bufsize cap it. The
            // 2x bufsize is the conventional pairing: it lets the rate
            // controller spend above maxrate briefly (a hard cut, an explosion)
            // and pay it back, rather than degrading those frames.
            push(&mut a, "-maxrate");
            a.push(policy.max_video_bitrate_bps.to_string());
            push(&mut a, "-bufsize");
            a.push(policy.max_video_bitrate_bps.saturating_mul(2).to_string());
        }
    }

    match plan.audio {
        AudioAction::Copy => {
            push(&mut a, "-c:a");
            push(&mut a, "copy");
        }
        AudioAction::Encode => {
            push(&mut a, "-c:a");
            push(&mut a, "aac");
            push(&mut a, "-ac");
            a.push(policy.max_audio_channels.to_string());
        }
    }

    // Subtitles are always copied, never encoded or dropped. This is only
    // safe because the output container is Matroska (or an already-acceptable
    // source container) — see `TranscodePolicy::output_container` for why an
    // MP4 target would have forced a lossy choice here.
    push(&mut a, "-c:s");
    push(&mut a, "copy");

    if container_holds_attachments(plan.container) {
        push(&mut a, "-c:t");
        push(&mut a, "copy");
    }

    push(&mut a, "-f");
    a.push(plan.container.ffmpeg_format().to_string());
    a.push(output_path.to_string());

    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundry::probe::{AudioStream, MediaProbe, VideoStream};

    fn probe(container: &str, video: Vec<VideoStream>, audio: Vec<AudioStream>) -> MediaProbe {
        MediaProbe {
            container: container.to_string(),
            duration_secs: Some(5400.0),
            format_bitrate_bps: Some(6_000_000),
            size_bytes: Some(4_000_000_000),
            video,
            audio,
            subtitles: Vec::new(),
            attachments: Vec::new(),
            data_stream_count: 0,
            unindexed_stream_count: 0,
            chapter_count: 0,
            title: None,
            other_stream_count: 0,
        }
    }

    fn vid(codec: &str, w: u32, h: u32, bitrate: Option<u64>) -> VideoStream {
        VideoStream {
            index: 0,
            codec: codec.to_string(),
            width: Some(w),
            height: Some(h),
            bitrate_bps: bitrate,
            pix_fmt: Some("yuv420p".to_string()),
            attached_pic: false,
            ..VideoStream::default()
        }
    }

    fn aud(index: u32, codec: &str, channels: u32) -> AudioStream {
        AudioStream {
            index,
            codec: codec.to_string(),
            channels: Some(channels),
            language: Some("eng".to_string()),
            bitrate_bps: Some(640_000),
        }
    }

    /// A file that conforms on every axis. Every test that wants to prove one
    /// specific thing triggers a rewrite starts from this and breaks exactly
    /// one field.
    fn optimal() -> MediaProbe {
        probe(
            "matroska,webm",
            vec![vid("h264", 1920, 1080, Some(5_000_000))],
            vec![aud(1, "eac3", 6)],
        )
    }

    fn decide(p: &MediaProbe) -> TranscodeDecision {
        plan_transcode(p, &TranscodePolicy::default(), "/in.mkv", "/out.mkv")
    }

    // --- AlreadyOptimal ----------------------------------------------------

    #[test]
    fn a_conforming_file_is_already_optimal() {
        assert_eq!(decide(&optimal()), TranscodeDecision::AlreadyOptimal);
    }

    #[test]
    fn an_hevc_1080p_mp4_is_also_already_optimal() {
        // Both "accepted but not the encode target" paths at once: HEVC video
        // and an MP4 container are left completely alone.
        let p = probe(
            "mov,mp4,m4a,3gp,3g2,mj2",
            vec![vid("hevc", 1920, 1080, Some(4_000_000))],
            vec![aud(1, "aac", 2)],
        );
        assert_eq!(decide(&p), TranscodeDecision::AlreadyOptimal);
    }

    // --- Undecidables ------------------------------------------------------

    #[test]
    fn an_unrecognized_container_is_undecidable_not_optimal() {
        let mut p = optimal();
        p.container = "ogg".to_string();
        assert!(matches!(
            decide(&p),
            TranscodeDecision::CannotDecide {
                why: Undecidable::UnrecognizedContainer { .. }
            }
        ));
    }

    #[test]
    fn an_audio_only_file_is_undecidable_not_optimal() {
        // "Optimal" would be a claim about a file whose policy does not apply.
        let mut p = optimal();
        p.video.clear();
        assert_eq!(
            decide(&p),
            TranscodeDecision::CannotDecide { why: Undecidable::NoVideoStream }
        );
    }

    #[test]
    fn a_missing_duration_is_undecidable_because_the_result_could_not_be_verified() {
        // THE fail-closed case on the destructive path: with no source
        // duration, no output of this transcode could ever be proven
        // un-truncated, so the transcode must not be planned at all.
        let mut p = optimal();
        p.duration_secs = None;
        // Make the file otherwise clearly in need of work, to prove the
        // duration check wins over the policy comparison rather than merely
        // coinciding with it.
        p.video = vec![vid("mpeg4", 3840, 2160, Some(40_000_000))];
        assert_eq!(
            decide(&p),
            TranscodeDecision::CannotDecide { why: Undecidable::UnknownDuration },
            "a file that cannot be verified must not be transcoded, however bad it is"
        );
    }

    #[test]
    fn unknown_dimensions_or_codec_are_undecidable() {
        let mut p = optimal();
        p.video[0].width = None;
        assert!(matches!(
            decide(&p),
            TranscodeDecision::CannotDecide { why: Undecidable::UnknownVideoDimensions }
        ));

        let mut p = optimal();
        p.video[0].height = Some(0);
        assert!(
            matches!(
                decide(&p),
                TranscodeDecision::CannotDecide { why: Undecidable::UnknownVideoDimensions }
            ),
            "a zero dimension is as unusable as a missing one"
        );

        let mut p = optimal();
        p.video[0].codec = String::new();
        assert!(matches!(
            decide(&p),
            TranscodeDecision::CannotDecide { why: Undecidable::UnknownVideoCodec }
        ));
    }

    #[test]
    fn an_audio_stream_with_no_channel_count_is_undecidable_not_conforming() {
        // Review finding 3. Skipping the ceiling check when the count is
        // absent let `AlreadyOptimal` be returned for a file that was never
        // checked on that axis — while `AlreadyOptimal` is documented as
        // "every dimension checked and passed". Same rule as duration.
        let mut p = optimal();
        p.audio[0].channels = None;
        assert_eq!(
            decide(&p),
            TranscodeDecision::CannotDecide {
                why: Undecidable::UnknownAudioChannels { stream_index: 1 }
            },
            "an unchecked dimension must never resolve to 'fine'"
        );

        // Zero is as unusable as absent.
        let mut p = optimal();
        p.audio[0].channels = Some(0);
        assert!(matches!(
            decide(&p),
            TranscodeDecision::CannotDecide {
                why: Undecidable::UnknownAudioChannels { .. }
            }
        ));
    }

    #[test]
    fn streams_we_could_not_address_make_the_whole_file_undecidable() {
        // Every judgement is made on the streams we could see; if some could
        // not be addressed at all, none of those judgements is trustworthy.
        let mut p = optimal();
        p.unindexed_stream_count = 1;
        assert_eq!(
            decide(&p),
            TranscodeDecision::CannotDecide {
                why: Undecidable::UnindexedStreams { count: 1 }
            }
        );
    }

    #[test]
    fn data_streams_are_refused_rather_than_silently_dropped() {
        // Foundry cannot carry them into Matroska, and dropping them while
        // reporting the file "rewritten" is a false claim.
        let mut p = optimal();
        p.data_stream_count = 2;
        assert_eq!(
            decide(&p),
            TranscodeDecision::CannotDecide {
                why: Undecidable::DataStreamsCannotBeCarried { count: 2 }
            }
        );
    }

    #[test]
    fn an_mp4_carrying_fonts_is_refused_rather_than_losing_them() {
        // MP4 has no attachment concept. An MP4 that needs work but carries
        // fonts cannot be rewritten without losing them, so it is refused.
        let mut p = probe(
            "mov,mp4,m4a,3gp,3g2,mj2",
            vec![vid("mpeg4", 1280, 720, Some(2_000_000))],
            vec![aud(1, "aac", 2)],
        );
        p.attachments = vec![crate::foundry::probe::AttachmentStream {
            index: 5,
            codec: "ttf".into(),
            filename: Some("Gandhi Sans.ttf".into()),
        }];
        assert_eq!(
            decide(&p),
            TranscodeDecision::CannotDecide {
                why: Undecidable::AttachmentsCannotBeCarried { count: 1 }
            }
        );
    }

    #[test]
    fn a_matroska_file_with_fonts_carries_them_through_the_encode() {
        // The regression for the silent-media-loss finding: the argv must
        // actually map and copy attachments, not merely be allowed to.
        let mut p = probe(
            "matroska,webm",
            vec![vid("mpeg4", 1920, 1080, Some(5_000_000))],
            vec![aud(1, "aac", 2)],
        );
        p.attachments = vec![crate::foundry::probe::AttachmentStream {
            index: 5,
            codec: "ttf".into(),
            filename: Some("Gandhi Sans.ttf".into()),
        }];
        let TranscodeDecision::Transcode { args, .. } = decide(&p) else {
            panic!("expected a transcode");
        };
        assert!(
            args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:t?"),
            "subtitle fonts must be mapped, got {args:?}"
        );
        assert!(
            args.windows(2).any(|w| w[0] == "-c:t" && w[1] == "copy"),
            "and copied, got {args:?}"
        );
    }

    #[test]
    fn an_mp4_target_does_not_ask_ffmpeg_for_attachments_it_cannot_write() {
        // The flip side: emitting `-map 0:t?` into an MP4 mux would be asking
        // for something the container cannot express.
        let p = probe(
            "mov,mp4,m4a,3gp,3g2,mj2",
            vec![vid("mpeg4", 1280, 720, Some(2_000_000))],
            vec![aud(1, "aac", 2)],
        );
        let TranscodeDecision::Transcode { plan, args, .. } = decide(&p) else {
            panic!("expected a transcode");
        };
        assert_eq!(plan.container, Container::Mp4);
        assert!(!args.iter().any(|s| s == "0:t?"), "got {args:?}");
        assert!(!args.iter().any(|s| s == "-c:t"), "got {args:?}");
    }

    #[test]
    fn an_unidentifiable_audio_stream_is_undecidable() {
        // Neither passing it through as acceptable nor re-encoding it is a
        // judgement we have any basis for.
        let mut p = optimal();
        p.audio.push(AudioStream {
            index: 2,
            codec: String::new(),
            channels: Some(2),
            language: None,
            bitrate_bps: None,
        });
        assert_eq!(
            decide(&p),
            TranscodeDecision::CannotDecide {
                why: Undecidable::UnknownAudioCodec { stream_index: 2 }
            }
        );
    }

    #[test]
    fn an_unmeasurable_bitrate_on_an_otherwise_clean_file_is_undecidable() {
        // Declaring it optimal would assert a bitrate nobody measured.
        let mut p = optimal();
        p.video[0].bitrate_bps = None;
        p.format_bitrate_bps = Some(50_000_000); // above the ceiling: bounds nothing
        assert_eq!(
            decide(&p),
            TranscodeDecision::CannotDecide { why: Undecidable::UnknownVideoBitrate }
        );
    }

    #[test]
    fn a_container_bitrate_below_the_ceiling_proves_the_video_is_within_it() {
        // The sound half of the bound: video bitrate <= container bitrate, so
        // a container figure under the ceiling settles the question. Without
        // this, most Matroska files (which carry no per-stream bit_rate) would
        // be permanently undecidable and nothing would ever be optimized.
        let mut p = optimal();
        p.video[0].bitrate_bps = None;
        p.format_bitrate_bps = Some(6_000_000);
        assert_eq!(decide(&p), TranscodeDecision::AlreadyOptimal);
    }

    #[test]
    fn bitrate_verdict_never_infers_exceeds_from_the_container_figure() {
        // The UNSOUND half, explicitly not taken: the container figure
        // includes audio and subtitles, so it overstates video. Inferring
        // "exceeds" from it would order irreversible re-encodes of files that
        // are actually within policy.
        assert_eq!(
            bitrate_verdict(None, Some(50_000_000), 15_000_000),
            BitrateVerdict::Unknown,
            "an over-ceiling container bitrate must never be read as an over-ceiling video bitrate"
        );
        assert_eq!(
            bitrate_verdict(None, Some(10_000_000), 15_000_000),
            BitrateVerdict::WithinCeiling
        );
        assert_eq!(bitrate_verdict(None, None, 15_000_000), BitrateVerdict::Unknown);
        // A measured stream bitrate is always authoritative over the bound.
        assert_eq!(
            bitrate_verdict(Some(20_000_000), Some(1_000), 15_000_000),
            BitrateVerdict::Exceeds { found_bps: 20_000_000 }
        );
        assert_eq!(
            bitrate_verdict(Some(15_000_000), None, 15_000_000),
            BitrateVerdict::WithinCeiling,
            "exactly at the ceiling is within it"
        );
    }

    // --- Transcode decisions ----------------------------------------------

    #[test]
    fn a_transcode_decision_always_carries_at_least_one_reason() {
        // "We rewrote this file but cannot say why" is the shape of a false
        // claim, so it must be unreachable.
        for p in [
            probe("matroska,webm", vec![vid("mpeg4", 640, 480, Some(1_000_000))], vec![aud(1, "mp3", 2)]),
            probe("avi", vec![vid("h264", 1920, 1080, Some(5_000_000))], vec![aud(1, "aac", 2)]),
            probe("matroska,webm", vec![vid("h264", 3840, 2160, Some(9_000_000))], vec![aud(1, "aac", 2)]),
        ] {
            match decide(&p) {
                TranscodeDecision::Transcode { reasons, .. } => {
                    assert!(!reasons.is_empty(), "a transcode with no reason is a false claim")
                }
                other => panic!("expected a transcode, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_unacceptable_video_codec_orders_a_re_encode() {
        let p = probe(
            "matroska,webm",
            vec![vid("mpeg4", 720, 480, Some(1_500_000))],
            vec![aud(1, "aac", 2)],
        );
        let TranscodeDecision::Transcode { plan, reasons, .. } = decide(&p) else {
            panic!("expected a transcode");
        };
        assert!(reasons.contains(&TranscodeReason::VideoCodecNotAccepted {
            found: "mpeg4".into()
        }));
        assert_eq!(plan.video, VideoAction::Encode { scale: None }, "480p must not be upscaled");
        assert_eq!(plan.audio, AudioAction::Copy, "conforming audio is not touched");
        assert!(!plan.is_remux_only());
    }

    #[test]
    fn a_4k_source_is_downscaled_to_the_ceiling() {
        let p = probe(
            "matroska,webm",
            vec![vid("h264", 3840, 2160, Some(9_000_000))],
            vec![aud(1, "aac", 2)],
        );
        let TranscodeDecision::Transcode { plan, reasons, args } = decide(&p) else {
            panic!("expected a transcode");
        };
        assert!(reasons.iter().any(|r| matches!(r, TranscodeReason::ResolutionAboveCeiling { .. })));
        assert_eq!(plan.video, VideoAction::Encode { scale: Some((1920, 1080)) });
        assert!(args.windows(2).any(|w| w[0] == "-vf" && w[1] == "scale=1920:1080"));
    }

    #[test]
    fn a_1080p_re_encode_emits_no_scale_filter_at_all() {
        // A scale=1920:1080 on already-1080p pixels is a wasted resample pass.
        let p = probe(
            "matroska,webm",
            vec![vid("mpeg4", 1920, 1080, Some(5_000_000))],
            vec![aud(1, "aac", 2)],
        );
        let TranscodeDecision::Transcode { plan, args, .. } = decide(&p) else {
            panic!("expected a transcode");
        };
        assert_eq!(plan.video, VideoAction::Encode { scale: None });
        assert!(!args.iter().any(|s| s == "-vf"), "got {args:?}");
    }

    #[test]
    fn unacceptable_audio_re_encodes_audio_but_copies_conforming_video() {
        // The partial case: paying a video generation loss to fix an audio
        // track would be gratuitous.
        let p = probe(
            "matroska,webm",
            vec![vid("h264", 1920, 1080, Some(5_000_000))],
            vec![aud(1, "truehd", 8)],
        );
        let TranscodeDecision::Transcode { plan, reasons, args } = decide(&p) else {
            panic!("expected a transcode");
        };
        assert_eq!(plan.video, VideoAction::Copy, "conforming video must be stream-copied");
        assert_eq!(plan.audio, AudioAction::Encode);
        assert!(reasons.contains(&TranscodeReason::AudioCodecNotAccepted {
            stream_index: 1,
            found: "truehd".into()
        }));
        assert!(reasons.contains(&TranscodeReason::AudioChannelsAboveCeiling {
            stream_index: 1,
            found: 8,
            max: 6
        }));
        assert!(args.windows(2).any(|w| w[0] == "-c:v" && w[1] == "copy"));
        assert!(args.windows(2).any(|w| w[0] == "-c:a" && w[1] == "aac"));
        assert!(args.windows(2).any(|w| w[0] == "-ac" && w[1] == "6"));
    }

    #[test]
    fn a_bad_container_alone_is_a_lossless_remux_not_a_re_encode() {
        // Real value, and honest: nothing about the streams is wrong, so
        // nothing about the streams is re-encoded.
        let p = probe(
            "avi",
            vec![vid("h264", 1280, 720, Some(3_000_000))],
            vec![aud(1, "aac", 2)],
        );
        let TranscodeDecision::Transcode { plan, reasons, args } = decide(&p) else {
            panic!("expected a transcode");
        };
        assert_eq!(reasons, vec![TranscodeReason::ContainerNotAccepted { found: Container::Avi }]);
        assert!(plan.is_remux_only(), "a container-only fix must not re-encode anything");
        assert_eq!(plan.container, Container::Matroska);
        assert!(args.windows(2).any(|w| w[0] == "-c:v" && w[1] == "copy"));
        assert!(args.windows(2).any(|w| w[0] == "-c:a" && w[1] == "copy"));
        assert!(args.windows(2).any(|w| w[0] == "-f" && w[1] == "matroska"));
    }

    #[test]
    fn an_acceptable_container_is_kept_rather_than_rewritten_to_the_default() {
        // Rewriting a conforming MP4 to MKV purely because MKV is the default
        // output would churn files that are fine and move paths the *arr tools
        // track.
        let p = probe(
            "mov,mp4,m4a,3gp,3g2,mj2",
            vec![vid("mpeg4", 1280, 720, Some(2_000_000))],
            vec![aud(1, "aac", 2)],
        );
        let TranscodeDecision::Transcode { plan, .. } = decide(&p) else {
            panic!("expected a transcode");
        };
        assert_eq!(plan.container, Container::Mp4);
    }

    #[test]
    fn the_shared_output_container_helper_always_agrees_with_the_plan() {
        // The forge names its staging file from `output_container` and the
        // argv's `-f` comes from `plan.container`. If they ever disagree, the
        // staged file is an mkv named `.mp4` — an unplayable library file, and
        // a silent one.
        let policy = TranscodePolicy::default();
        for p in [
            probe("avi", vec![vid("h264", 1280, 720, Some(3_000_000))], vec![aud(1, "aac", 2)]),
            probe("matroska,webm", vec![vid("mpeg4", 1920, 1080, Some(5_000_000))], vec![aud(1, "aac", 2)]),
            probe("mov,mp4,m4a,3gp,3g2,mj2", vec![vid("mpeg4", 1280, 720, Some(2_000_000))], vec![aud(1, "aac", 2)]),
            probe("flv", vec![vid("h264", 640, 360, Some(800_000))], vec![aud(1, "aac", 2)]),
        ] {
            let TranscodeDecision::Transcode { plan, .. } =
                plan_transcode(&p, &policy, "/i", "/o")
            else {
                panic!("expected a transcode for container {}", p.container);
            };
            assert_eq!(
                output_container(&p, &policy),
                Some(plan.container),
                "helper and plan disagreed for container {}",
                p.container
            );
        }
    }

    #[test]
    fn the_output_container_helper_is_none_for_an_unrecognized_source() {
        let mut p = optimal();
        p.container = "ogg".to_string();
        assert_eq!(output_container(&p, &TranscodePolicy::default()), None);
    }

    #[test]
    fn an_over_ceiling_video_bitrate_orders_a_re_encode() {
        let p = probe(
            "matroska,webm",
            vec![vid("h264", 1920, 1080, Some(30_000_000))],
            vec![aud(1, "aac", 2)],
        );
        let TranscodeDecision::Transcode { reasons, .. } = decide(&p) else {
            panic!("expected a transcode");
        };
        assert!(reasons.contains(&TranscodeReason::VideoBitrateAboveCeiling {
            found_bps: 30_000_000,
            ceiling_bps: 15_000_000,
        }));
    }

    #[test]
    fn a_bitrate_inside_the_tolerance_band_is_left_alone() {
        // 13 Mbps is over the stated 12 Mbps ceiling but inside the 1.25x
        // tolerance — not worth a generation loss.
        let p = probe(
            "matroska,webm",
            vec![vid("h264", 1920, 1080, Some(13_000_000))],
            vec![aud(1, "aac", 2)],
        );
        assert_eq!(decide(&p), TranscodeDecision::AlreadyOptimal);
    }

    // --- argv --------------------------------------------------------------

    #[test]
    fn the_remux_argv_is_exactly_this() {
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Copy,
            audio: AudioAction::Copy,
            container: Container::Matroska,
        };
        assert_eq!(
            build_transcode_args(&plan, &TranscodePolicy::default(), "/in.avi", "/work/out.mkv"),
            vec![
                "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
                "-i", "/in.avi",
                "-map", "0:0", "-map", "0:a?", "-map", "0:s?", "-map", "0:t?",
                "-map_metadata", "0", "-map_chapters", "0",
                "-c:v", "copy",
                "-c:a", "copy",
                "-c:s", "copy",
                "-c:t", "copy",
                "-f", "matroska", "/work/out.mkv",
            ]
        );
    }

    #[test]
    fn the_full_re_encode_argv_is_exactly_this() {
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: Some((1920, 1080)) },
            audio: AudioAction::Encode,
            container: Container::Matroska,
        };
        assert_eq!(
            build_transcode_args(&plan, &TranscodePolicy::default(), "/in.mkv", "/work/out.mkv"),
            vec![
                "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
                "-i", "/in.mkv",
                "-map", "0:0", "-map", "0:a?", "-map", "0:s?", "-map", "0:t?",
                "-map_metadata", "0", "-map_chapters", "0",
                "-c:v", "libx264", "-crf", "20", "-preset", "medium",
                "-pix_fmt", "yuv420p", "-vf", "scale=1920:1080",
                "-maxrate", "12000000", "-bufsize", "24000000",
                "-c:a", "aac", "-ac", "6",
                "-c:s", "copy",
                "-c:t", "copy",
                "-f", "matroska", "/work/out.mkv",
            ]
        );
    }

    #[test]
    fn the_argv_maps_the_video_stream_by_its_absolute_index() {
        // Not `0:v:0`. The probe parser filtered cover art out of its video
        // list, so ffmpeg's own `v:0` may be the poster while the index we
        // judged is the feature.
        let plan = TranscodePlan {
            video_stream_index: 3,
            video: VideoAction::Copy,
            audio: AudioAction::Copy,
            container: Container::Matroska,
        };
        let args = build_transcode_args(&plan, &TranscodePolicy::default(), "/in.mkv", "/out.mkv");
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:3"), "got {args:?}");
        assert!(!args.iter().any(|s| s == "0:v:0"));
    }

    #[test]
    fn the_argv_never_blocks_on_stdin_and_never_prompts() {
        // A background encode that can consume or wait on inherited stdin is a
        // worker that wedges.
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Copy,
            audio: AudioAction::Copy,
            container: Container::Matroska,
        };
        let args = build_transcode_args(&plan, &TranscodePolicy::default(), "/in.mkv", "/out.mkv");
        assert!(args.contains(&"-nostdin".to_string()));
        assert!(args.contains(&"-y".to_string()));
    }

    #[test]
    fn the_argv_puts_the_output_last_and_the_input_after_dash_i() {
        // ffmpeg is positional: an output path that is not last, or an input
        // not immediately after -i, silently means something else entirely.
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Encode,
            container: Container::Mp4,
        };
        let args = build_transcode_args(&plan, &TranscodePolicy::default(), "/in.mkv", "/out.mp4");
        assert_eq!(args.last().unwrap(), "/out.mp4");
        let i = args.iter().position(|s| s == "-i").unwrap();
        assert_eq!(args[i + 1], "/in.mkv");
        assert!(i < args.len() - 1);
    }

    #[test]
    fn subtitles_are_always_copied_never_dropped_or_burned_in() {
        for (video, audio) in [
            (VideoAction::Copy, AudioAction::Copy),
            (VideoAction::Encode { scale: None }, AudioAction::Encode),
        ] {
            let plan = TranscodePlan {
                video_stream_index: 0,
                video,
                audio,
                container: Container::Matroska,
            };
            let args = build_transcode_args(&plan, &TranscodePolicy::default(), "/i", "/o");
            assert!(args.windows(2).any(|w| w[0] == "-c:s" && w[1] == "copy"));
            assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:s?"));
        }
    }

    #[test]
    fn metadata_and_chapters_are_carried_across_explicitly() {
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Copy,
            audio: AudioAction::Copy,
            container: Container::Matroska,
        };
        let args = build_transcode_args(&plan, &TranscodePolicy::default(), "/i", "/o");
        assert!(args.windows(2).any(|w| w[0] == "-map_metadata" && w[1] == "0"));
        assert!(args.windows(2).any(|w| w[0] == "-map_chapters" && w[1] == "0"));
    }

    #[test]
    fn the_argv_follows_the_policy_rather_than_hardcoding_it() {
        // If the policy were baked into the builder, changing it would
        // silently do nothing — the operator's setting would be decorative.
        let policy = TranscodePolicy {
            crf: 26,
            preset: "veryfast".to_string(),
            max_video_bitrate_bps: 4_000_000,
            max_audio_channels: 2,
            ..TranscodePolicy::default()
        };
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Encode,
            container: Container::Matroska,
        };
        let args = build_transcode_args(&plan, &policy, "/i", "/o");
        assert!(args.windows(2).any(|w| w[0] == "-crf" && w[1] == "26"));
        assert!(args.windows(2).any(|w| w[0] == "-preset" && w[1] == "veryfast"));
        assert!(args.windows(2).any(|w| w[0] == "-maxrate" && w[1] == "4000000"));
        assert!(args.windows(2).any(|w| w[0] == "-bufsize" && w[1] == "8000000"));
        assert!(args.windows(2).any(|w| w[0] == "-ac" && w[1] == "2"));
    }

    #[test]
    fn paths_are_argv_elements_never_interpolated_into_a_string() {
        // A library filename with a space, a quote or a leading dash must not
        // be able to change the command.
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Copy,
            audio: AudioAction::Copy,
            container: Container::Matroska,
        };
        let nasty = "/srv/media/Movies/It's a \"Wonderful\" Life (1946).mkv";
        let args = build_transcode_args(&plan, &TranscodePolicy::default(), nasty, "/o.mkv");
        let i = args.iter().position(|s| s == "-i").unwrap();
        assert_eq!(args[i + 1], nasty);
    }

    #[test]
    fn reasons_render_as_operator_readable_text() {
        // These strings end up in a report the operator reads; a Debug dump
        // would not do.
        let r = TranscodeReason::ResolutionAboveCeiling {
            width: 3840,
            height: 2160,
            max_width: 1920,
            max_height: 1080,
        };
        assert!(r.to_string().contains("3840x2160"), "got {r}");
        assert!(r.to_string().contains("1920x1080"), "got {r}");

        let u = Undecidable::UnknownDuration;
        assert!(u.to_string().contains("truncation"), "got {u}");
    }
}
