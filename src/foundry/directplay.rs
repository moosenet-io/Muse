//! FOUNDRY-03 **Path A** — ingest normalization: one output, replacing the
//! source, targeting "this direct plays" rather than "this is small".
//!
//! ## What this module adds, and what it deliberately does not
//!
//! Path A's *mechanism* already exists and is not reimplemented here. Every
//! incoming title goes through the machinery MUSEF-02 built:
//! [`crate::media::probe`] describes it, [`crate::foundry::plan`] decides,
//! and [`crate::foundry::forge`] encodes to a staging file, re-probes it,
//! verifies it against the source and only then performs the atomic-claim
//! swap. That swap is the thing standing between "normalize on ingest" and a
//! destroyed library, and it is used as-is.
//!
//! The *target* also already exists, almost. The compatibility half of direct
//! play is exactly what [`TranscodePolicy`]'s accepted-codec, accepted-audio
//! and accepted-container lists encode, so Path A's policy is
//! [`TranscodePolicy::direct_play_normalization`] — the same struct with the
//! two **size** ceilings relaxed, because a 4K file direct-plays on a 4K
//! client and a high bitrate has never made a server decode anything. See that
//! constructor's docs for the field-by-field reasoning.
//!
//! So this module contains only the two genuinely new things:
//!
//! 1. [`direct_play_blockers`] — a diagnostic naming *why* a client would
//!    transcode, including two blockers the existing policy cannot express at
//!    all (see "the two gaps" below).
//! 2. [`may_delete_original`] — the rule that decides whether the source may be
//!    removed once the replacement has been verified. **This is the most
//!    important function in the item.**
//!
//! ## Why the deletion rule is the dangerous part
//!
//! Path A deletes by default. Roughly **1% of this library is 4K/HDR/DV** (43
//! of 4000 sampled by filename), and that number is what makes the rule easy to
//! get wrong and never notice: a bug that destroys HDR masters is invisible in
//! 99 out of 100 ingests, and the operator finds out months later, watching a
//! film that used to be Dolby Vision and now is not. There is no undo — the
//! source came from a transfer that has completed and gone.
//!
//! The rule is therefore built as a **blocker list, not a score**. It collects
//! every reason to refuse and allows only when the list is empty *and* a
//! positive [`DeletionEvidence`] record was assembled. Allow is never a
//! fallthrough, on the same principle as
//! [`crate::foundry::plan::TranscodeDecision::AlreadyOptimal`].
//!
//! ## The two gaps, stated rather than half-closed
//!
//! [`direct_play_blockers`] reports two conditions that
//! [`crate::foundry::plan::plan_transcode`] does **not** currently act on:
//! 10-bit H.264, and image-based subtitles marked default. Both genuinely
//! force client-side transcoding. Neither is wired into the planner by this
//! item, because doing so would change what the existing, reviewed
//! single-target path does to files — a behaviour change that belongs in its
//! own item with its own survey. They are reported so the gap is visible in
//! output rather than absent from the code.

use crate::foundry::hdr::{
    classify_dolby_vision, classify_hdr, undetectable_formats, DolbyVisionVerdict, HdrVerdict,
};
use crate::foundry::plan::TranscodeDecision;
use crate::foundry::policy::{normalize_container, Container, TranscodePolicy};
use crate::media::probe::MediaProbe;

// --- What forces a client-side transcode -----------------------------------

/// A reason a media server would have to transcode this file rather than
/// streaming it untouched.
#[derive(Debug, Clone, PartialEq)]
pub enum DirectPlayBlocker {
    VideoCodecNotWidelySupported {
        found: String,
    },
    /// 10-bit (or deeper) H.264 — "Hi10P". Almost no client has a hardware
    /// decoder for it, so it forces a **software** transcode on the server
    /// even though the codec name looks fine. Common in anime releases.
    ///
    /// Not a blocker for HEVC, where 10-bit (Main 10) is the normal, widely
    /// hardware-decoded case.
    HighBitDepthH264 {
        bit_depth: u8,
    },
    ContainerNotStreamable {
        found: Container,
    },
    AudioCodecNotWidelySupported {
        stream_index: u32,
        found: String,
    },
    AudioChannelsAboveClientCeiling {
        stream_index: u32,
        found: u32,
        max: u32,
    },
    /// A bitmap subtitle track (PGS, VobSub, DVB) marked default or forced.
    /// A client that cannot render bitmap subs asks the server to **burn them
    /// in**, which is a full video re-encode of an otherwise direct-playable
    /// file. Bitmap subs that are merely present and not default are fine —
    /// nobody burns in a track that was not selected.
    DefaultBitmapSubtitles {
        stream_index: u32,
        codec: String,
    },
    ResolutionAboveCeiling {
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    },
}

impl std::fmt::Display for DirectPlayBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VideoCodecNotWidelySupported { found } => write!(
                f,
                "video codec `{found}` is not one clients direct-play — the server would decode it"
            ),
            Self::HighBitDepthH264 { bit_depth } => write!(
                f,
                "{bit_depth}-bit H.264 (Hi10P) has almost no hardware decoder support, so \
                 clients fall back to a server-side SOFTWARE transcode despite the codec \
                 name looking fine"
            ),
            Self::ContainerNotStreamable { found } => write!(
                f,
                "container `{}` is not one clients stream directly — a remux is forced at minimum",
                found.ffmpeg_format()
            ),
            Self::AudioCodecNotWidelySupported { stream_index, found } => write!(
                f,
                "audio stream {stream_index} codec `{found}` forces a server-side audio transcode"
            ),
            Self::AudioChannelsAboveClientCeiling { stream_index, found, max } => write!(
                f,
                "audio stream {stream_index} has {found} channels, above the {max} most \
                 clients render — the server would downmix"
            ),
            Self::DefaultBitmapSubtitles { stream_index, codec } => write!(
                f,
                "subtitle stream {stream_index} is bitmap-based (`{codec}`) and is marked \
                 default/forced — clients that cannot render it ask the server to burn it in, \
                 which is a full video re-encode"
            ),
            Self::ResolutionAboveCeiling { width, height, max_width, max_height } => write!(
                f,
                "resolution {width}x{height} is above the {max_width}x{max_height} ceiling"
            ),
        }
    }
}

/// Bitmap (image) subtitle codecs. Text codecs (`subrip`, `ass`, `mov_text`,
/// `webvtt`) render client-side and never force anything.
pub const BITMAP_SUBTITLE_CODECS: &[&str] =
    &["hdmv_pgs_subtitle", "pgssub", "dvd_subtitle", "dvdsub", "dvb_subtitle", "dvbsub", "xsub"];

/// Name every reason a client would have to transcode this file.
///
/// Pure and diagnostic. It does **not** decide anything — the decision belongs
/// to [`crate::foundry::plan::plan_transcode`] with
/// [`TranscodePolicy::direct_play_normalization`]. The two blockers that
/// planner cannot express ([`DirectPlayBlocker::HighBitDepthH264`] and
/// [`DirectPlayBlocker::DefaultBitmapSubtitles`]) are reported here anyway, so
/// the gap is visible in a report rather than invisible in the code.
///
/// An empty list is **not** a promise of direct play: it means no blocker in
/// the set above was found. Facts we cannot observe (see
/// [`crate::foundry::hdr::undetectable_formats`]) are not in the set.
pub fn direct_play_blockers(
    probe: &MediaProbe,
    policy: &TranscodePolicy,
) -> Vec<DirectPlayBlocker> {
    let mut out = Vec::new();

    if let Some(container) = normalize_container(&probe.container) {
        if !policy.accepts_container(container) {
            out.push(DirectPlayBlocker::ContainerNotStreamable { found: container });
        }
    }

    if let Some(v) = probe.primary_video() {
        if !v.codec.trim().is_empty() && !policy.accepts_video_codec(&v.codec) {
            out.push(DirectPlayBlocker::VideoCodecNotWidelySupported {
                found: v.codec.clone(),
            });
        }
        // Hi10P: 10-bit H.264 specifically. 10-bit HEVC is the normal case and
        // is widely hardware-decoded, so the check is codec-conditional.
        if v.codec.eq_ignore_ascii_case("h264") {
            if let Some(depth) = v
                .pix_fmt
                .as_deref()
                .and_then(crate::foundry::hdr::pixel_bit_depth)
            {
                if depth > 8 {
                    out.push(DirectPlayBlocker::HighBitDepthH264 { bit_depth: depth });
                }
            }
        }
        if let (Some(w), Some(h)) = (v.width, v.height) {
            if w > policy.max_width || h > policy.max_height {
                out.push(DirectPlayBlocker::ResolutionAboveCeiling {
                    width: w,
                    height: h,
                    max_width: policy.max_width,
                    max_height: policy.max_height,
                });
            }
        }
    }

    for a in &probe.audio {
        if !a.codec.trim().is_empty() && !policy.accepts_audio_codec(&a.codec) {
            out.push(DirectPlayBlocker::AudioCodecNotWidelySupported {
                stream_index: a.index,
                found: a.codec.clone(),
            });
        }
        if let Some(ch) = a.channels {
            if ch > policy.max_audio_channels {
                out.push(DirectPlayBlocker::AudioChannelsAboveClientCeiling {
                    stream_index: a.index,
                    found: ch,
                    max: policy.max_audio_channels,
                });
            }
        }
    }

    for s in &probe.subtitles {
        let is_bitmap = BITMAP_SUBTITLE_CODECS
            .iter()
            .any(|c| c.eq_ignore_ascii_case(s.codec.trim()));
        if is_bitmap && (s.default || s.forced) {
            out.push(DirectPlayBlocker::DefaultBitmapSubtitles {
                stream_index: s.index,
                codec: s.codec.clone(),
            });
        }
    }

    out
}

// --- The deletion rule -----------------------------------------------------

/// Where a normalization got to. [`may_delete_original`] refuses on every
/// variant but [`NormalizationOutcome::Verified`], and that variant can only be
/// built by a caller holding a probe **of the output file that was actually
/// written** — which is to say, by [`crate::foundry::forge`] after its own
/// verification, and by nobody else.
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizationOutcome {
    /// The planner found the source already conforms. Nothing was written, so
    /// there is no replacement and nothing to delete.
    SourceAlreadyDirectPlays,
    /// A plan exists but has not been executed. A plan is not a file.
    Planned { decision: TranscodeDecision },
    /// The encode ran, the output was re-probed, and forge verified it. The
    /// probe is of the **output**, not of the plan.
    Verified { output: MediaProbe },
    /// The encode or its verification failed.
    Failed { why: String },
}

impl NormalizationOutcome {
    fn state_name(&self) -> &'static str {
        match self {
            Self::SourceAlreadyDirectPlays => "source_already_direct_plays",
            Self::Planned { .. } => "planned_but_not_executed",
            Self::Verified { .. } => "verified",
            Self::Failed { .. } => "failed",
        }
    }
}

/// A reason the original must not be deleted.
///
/// Every variant names a fact about the source that the replacement does not
/// carry, or a fact we could not establish. There is no "minor" variant and no
/// severity ordering: any single blocker refuses.
#[derive(Debug, Clone, PartialEq)]
pub enum DeletionBlocker {
    /// Nothing verified has replaced the original.
    NoVerifiedReplacement { state: &'static str },
    /// Foundry's view of the *source* is incomplete, so it cannot enumerate
    /// what would be lost.
    SourceNotFullyDescribed {
        data_streams: usize,
        unindexed_streams: usize,
        other_streams: usize,
    },
    /// The source is HDR and the replacement is not.
    HighDynamicRangeNotReproduced { source: String, output: String },
    /// The source's dynamic range could not be determined at all, so it cannot
    /// be shown to have survived.
    SourceDynamicRangeUnknown { why: String },
    /// The source carries a Dolby Vision signal the replacement does not.
    DolbyVisionNotReproduced { source: String },
    /// An audio stream in the source has no matching stream in the output.
    AudioStreamNotReproduced {
        stream_index: u32,
        codec: String,
        channels: Option<u32>,
    },
    /// An audio stream was not reproduced *and* its codec is one that may be
    /// hiding object-based audio we cannot detect. Emitted alongside
    /// [`DeletionBlocker::AudioStreamNotReproduced`] to name the specific
    /// unrecoverable thing rather than only the generic one.
    PossiblyObjectBearingAudioLost {
        stream_index: u32,
        codec: String,
        format: &'static str,
    },
    /// The replacement has fewer streams of some kind than the source.
    StreamsLost {
        kind: &'static str,
        source: usize,
        output: usize,
    },
    /// The replacement is lower resolution than the source. Deleting the
    /// original then makes the downscale permanent.
    ResolutionReduced { from: (u32, u32), to: (u32, u32) },
    /// Source or output duration is unknown, or they disagree — the output may
    /// be truncated.
    DurationNotProvenEqual {
        source: Option<f64>,
        output: Option<f64>,
    },
}

impl std::fmt::Display for DeletionBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoVerifiedReplacement { state } => write!(
                f,
                "no verified replacement exists (normalization state: {state}) — there is \
                 nothing for the original to be redundant with"
            ),
            Self::SourceNotFullyDescribed {
                data_streams,
                unindexed_streams,
                other_streams,
            } => write!(
                f,
                "Foundry's view of the source is incomplete ({data_streams} data, \
                 {unindexed_streams} unindexed, {other_streams} unmodelled streams), so it \
                 cannot enumerate what deleting it would lose"
            ),
            Self::HighDynamicRangeNotReproduced { source, output } => write!(
                f,
                "the source is HDR ({source}) and the replacement is not ({output}) — the \
                 original is the only copy of the high dynamic range grade"
            ),
            Self::SourceDynamicRangeUnknown { why } => write!(
                f,
                "the source's dynamic range could not be determined ({why}), so it cannot be \
                 shown to have survived — refusing to delete on an unknown"
            ),
            Self::DolbyVisionNotReproduced { source } => write!(
                f,
                "the source carries Dolby Vision ({source}) which the replacement does not — \
                 deleting it destroys the only copy"
            ),
            Self::AudioStreamNotReproduced {
                stream_index,
                codec,
                channels,
            } => write!(
                f,
                "source audio stream {stream_index} (`{codec}`, {} channels) has no matching \
                 stream in the replacement",
                channels.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
            ),
            Self::PossiblyObjectBearingAudioLost {
                stream_index,
                codec,
                format,
            } => write!(
                f,
                "source audio stream {stream_index} is `{codec}`, which may carry {format} — \
                 Foundry CANNOT tell whether it does, so it must assume it does and refuse"
            ),
            Self::StreamsLost { kind, source, output } => write!(
                f,
                "the replacement has {output} {kind} stream(s) where the source has {source}"
            ),
            Self::ResolutionReduced { from, to } => write!(
                f,
                "the replacement is {}x{} where the source is {}x{} — deleting the original \
                 makes the downscale permanent",
                to.0, to.1, from.0, from.1
            ),
            Self::DurationNotProvenEqual { source, output } => write!(
                f,
                "durations could not be proven equal (source {source:?}, output {output:?}) — \
                 the replacement may be truncated"
            ),
        }
    }
}

/// The positive facts that were checked before allowing a deletion.
///
/// Exists so that [`DeletionDecision::Allow`] carries evidence rather than
/// merely an absence of objections. Same discipline as
/// [`crate::foundry::plan::TranscodeDecision::AlreadyOptimal`]: allow is only
/// reachable after every dimension was checked and passed.
#[derive(Debug, Clone, PartialEq)]
pub struct DeletionEvidence {
    pub dynamic_range_preserved: bool,
    pub dolby_vision_absent_from_source: bool,
    pub audio_streams_reproduced: usize,
    pub subtitle_streams_reproduced: usize,
    pub attachments_reproduced: usize,
    pub chapters_reproduced: usize,
    pub resolution: (u32, u32),
    pub duration_secs: f64,
}

/// Whether the original may be deleted.
#[derive(Debug, Clone, PartialEq)]
pub enum DeletionDecision {
    /// Refuse. Carries **every** blocker found, not just the first, so an
    /// operator fixing one is not surprised by the next.
    Refuse { blockers: Vec<DeletionBlocker> },
    /// Allow, with the evidence that was checked.
    Allow { evidence: DeletionEvidence },
}

impl DeletionDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}

/// How close two durations must be to count as equal, in seconds.
///
/// A container rewrite can legitimately shift the reported duration by a frame
/// or so as timestamps are re-based. One second is comfortably above that and
/// far below any truncation worth worrying about.
pub const DURATION_TOLERANCE_SECS: f64 = 1.0;

/// Will the deletion gate refuse this file, decided BEFORE any encode runs?
///
/// Path A's promise is "optimize the file and remove the original". When
/// `may_delete_original` refuses, the original stays — so the encode has cost
/// a full re-encode and DOUBLED the disk for that title instead of reclaiming
/// any. That is a legitimate outcome, but at ~16,000 items it is a large
/// amount of work to discover only afterwards.
///
/// Several of the gate's refusals are predictable from the SOURCE and the PLAN
/// alone, with no output to probe:
///
/// - the source is HDR or of UNDETERMINED dynamic range and the plan
///   re-encodes video — the argv forces 8-bit `yuv420p` and applies no tone
///   map, so the output cannot preserve either;
/// - the source carries audio in a codec that can hide a format ffprobe cannot
///   see, and the plan re-encodes audio;
/// - the source is not fully described, which no encode can repair.
///
/// Returns the reasons it WILL refuse, empty when nothing is predictable.
/// Deliberately CONSERVATIVE: it only claims refusals it can establish, so an
/// empty result means "no refusal predicted", never "deletion is guaranteed".
/// The gate itself remains the authority.
///
/// Observed live: an AV1 1080p 10-bit UNTAGGED episode was re-encoded for
/// hours, then refused with `SourceDynamicRangeUnknown`. Untagged 10-bit is
/// common in AV1 and anime web encodes, so this is not a rare tail.
/// **The audio matching rule, in one place.**
///
/// Returns the source streams that the output does not reproduce. Empty means
/// every source track was accounted for.
///
/// This exists as its own function because the deletion gate and the
/// *prediction* of that gate had drifted apart: the gate refused an mp3 -> aac
/// swap while the prediction said the title would reclaim disk. Two copies of a
/// rule this subtle will always drift, so there is now one copy and both call
/// it. The prediction runs it against the output the plan *will* produce.
///
/// A codec that may be hiding a format ffprobe cannot see gets the strict rule:
/// same codec and enough channels is NOT enough, because a re-encoded E-AC-3
/// 6-channel track looks exactly like the Atmos track it was made from. Nothing
/// in the probe output distinguishes a copy from a re-encode, so the closest
/// available stand-in is "every visible property is unchanged" — evidence, not
/// proof, which is why it errs toward keeping the file.
pub(crate) fn unreproduced_audio<'a>(
    source: &'a [crate::media::probe::AudioStream],
    output: &[crate::media::probe::AudioStream],
) -> Vec<&'a crate::media::probe::AudioStream> {
    let mut unmatched_output: Vec<&crate::media::probe::AudioStream> = output.iter().collect();
    let mut unmatched_source = Vec::new();

    for a in source {
        let may_hide_object_audio = undetectable_formats()
            .iter()
            .any(|u| u.carried_by_codec.eq_ignore_ascii_case(a.codec.trim()));

        let found = unmatched_output.iter().position(|o| {
            o.codec.eq_ignore_ascii_case(&a.codec)
                && match (a.channels, o.channels) {
                    (Some(src), Some(out)) => out >= src,
                    // An unknown channel count on either side cannot be shown
                    // to match, so it does not.
                    _ => false,
                }
                && (!may_hide_object_audio
                    || match (a.bitrate_bps, o.bitrate_bps) {
                        (Some(src), Some(out)) => src == out,
                        // An absent bitrate on either side is not evidence of
                        // a copy. Unknown refuses.
                        _ => false,
                    })
        });
        match found {
            Some(i) => {
                unmatched_output.remove(i);
            }
            None => unmatched_source.push(a),
        }
    }
    unmatched_source
}

/// The audio streams an `AudioAction::Encode` will produce.
///
/// `Encode` always emits **aac** (`-c:a aac`) with a per-stream channel target
/// (`-ac:a:{i}`), so the output is fully determined by the plan. Bitrate is
/// left unknown because it is — which is exactly why a re-encode of a
/// possibly-object-bearing track cannot be shown to be a copy.
fn audio_streams_an_encode_will_produce(
    source: &MediaProbe,
    channels: &[u32],
) -> Vec<crate::media::probe::AudioStream> {
    source
        .audio
        .iter()
        .enumerate()
        .map(|(i, a)| crate::media::probe::AudioStream {
            index: a.index,
            codec: "aac".to_string(),
            channels: channels.get(i).copied().or(a.channels),
            language: a.language.clone(),
            bitrate_bps: None,
        })
        .collect()
}

pub fn predicted_deletion_refusals(
    source: &MediaProbe,
    plan: &crate::foundry::plan::TranscodePlan,
) -> Vec<String> {
    use crate::foundry::plan::{AudioAction, VideoAction};
    let mut out = Vec::new();

    if source.data_stream_count > 0
        || source.unindexed_stream_count > 0
        || source.other_stream_count > 0
    {
        out.push(
            "the source carries streams Foundry cannot describe, which no encode repairs"
                .to_string(),
        );
    }

    let re_encodes_video = matches!(plan.video, VideoAction::Encode { .. });
    if re_encodes_video {
        if let Some(v) = source.primary_video() {
            match classify_hdr(v) {
                HdrVerdict::Hdr { transfer } => out.push(format!(
                    "the source is {transfer:?} HDR and this plan re-encodes video to 8-bit \
                     with no tone map, so the output cannot preserve it"
                )),
                HdrVerdict::Unknown { why } => out.push(format!(
                    "the source's dynamic range is undetermined ({why}) and this plan \
                     re-encodes video, so the gate cannot be shown the range survived"
                )),
                HdrVerdict::Sdr => {}
            }
            if classify_dolby_vision(v).is_present() {
                out.push(
                    "the source carries Dolby Vision, which no re-encode preserves".to_string(),
                );
            }
        }
    }

    // ANY audio re-encode blocks deletion — not only the undetectable formats.
    //
    // This prediction under-reported badly, and a real end-to-end swap exposed
    // it. `may_delete_original` matches a source stream to an output stream
    // with `o.codec.eq_ignore_ascii_case(&a.codec)`, so a planned mp3 -> aac
    // re-encode produces NO matching stream and the gate refuses. The codec
    // changed, which is the entire point of the re-encode, and the gate
    // correctly cannot show nothing was lost.
    //
    // Predicting only the Atmos/DTS:X cases told the operator ~160 titles would
    // re-encode without reclaiming disk, when the real population is every
    // title needing any audio work — a number a 16,000-item run was to be sized
    // on.
    if let AudioAction::Encode { channels } = &plan.audio {
        // Run the GATE'S OWN RULE against the output this plan will produce,
        // rather than restating it. Two descriptions of one rule drift, and
        // this one already had: the previous version flagged only the codecs
        // that might hide Atmos or DTS:X, so an mp3 -> aac re-encode was
        // predicted to reclaim disk while the gate refused it.
        //
        // Simulating instead of describing also avoids the opposite error. An
        // aac source re-encoded to aac with the same or more channels DOES
        // match, and the gate allows the deletion — a blanket "any encode
        // refuses" would over-report those and understate reclaimable disk.
        let predicted_output = audio_streams_an_encode_will_produce(source, channels);
        for a in unreproduced_audio(&source.audio, &predicted_output) {
            match undetectable_formats()
                .iter()
                .find(|u| u.carried_by_codec.eq_ignore_ascii_case(a.codec.trim()))
            {
                Some(u) => out.push(format!(
                    "audio stream {} is `{}`, which may carry {} — invisible to ffprobe, so a \
                     re-encode cannot be shown to have preserved it",
                    a.index, a.codec, u.name
                )),
                None => out.push(format!(
                    "audio stream {} is `{}` and this plan re-encodes it to aac; the deletion \
                     gate matches streams by CODEC, so the re-encoded track will not match its \
                     source and the original cannot be shown to have been reproduced",
                    a.index, a.codec
                )),
            }
        }
    }

    out
}

/// **May the original be deleted?**
///
/// The single most important rule in FOUNDRY-03. It refuses whenever the
/// original is the only source of something the replacement does not carry:
/// HDR, Dolby Vision, object-based audio, or any stream at all.
///
/// It is written to refuse by default in the strongest sense available — the
/// blocker list is built first and `Allow` is constructed only in the branch
/// where that list is empty, so adding a new blocker cannot accidentally leave
/// a path that allows. The unknown cases refuse alongside the known-bad ones:
/// an undetermined dynamic range is as blocking as a proven HDR source,
/// because both mean "we cannot show this survived".
///
/// ## The E-AC-3 decision, stated plainly
/// Dolby Atmos inside E-AC-3 (JOC) is **not detectable** from ffprobe stream
/// output — an Atmos track and an ordinary 5.1 track are both reported as
/// `eac3` with 6 channels (see
/// [`crate::foundry::hdr::undetectable_formats`]).
///
/// **Nothing available here proves a stream was copied rather than re-encoded.**
/// ffprobe reports a re-encoded E-AC-3 6-channel track identically to the
/// Atmos track it was made from, and JOC does not survive a re-encode. So the
/// rule cannot be "detect the loss"; it is: for any codec on the undetectable
/// list, a stream counts as reproduced only when *every probe-visible property*
/// — codec, channel count and bitrate — is unchanged, and an absent bitrate on
/// either side refuses. That is the strongest evidence obtainable from a probe.
/// It is still evidence and not proof, which is why it errs toward keeping.
///
/// This is deliberately over-broad: it will refuse deletions for ordinary 5.1
/// E-AC-3 tracks that lost nothing. The cost of being wrong that way is a kept
/// file; the cost of being wrong the other way is the operator's only Atmos mix.
///
/// Codecs *not* on that list keep the loose rule (same codec, enough channels),
/// because applying the strict one everywhere would make most of the library
/// permanently undeletable and the gate would stop meaning anything.
pub fn may_delete_original(
    source: &MediaProbe,
    normalization: &NormalizationOutcome,
) -> DeletionDecision {
    let mut blockers: Vec<DeletionBlocker> = Vec::new();

    // Fail closed on the outcome first: without a verified replacement there
    // is nothing further worth computing, and no combination of the checks
    // below could make deletion safe.
    let NormalizationOutcome::Verified { output } = normalization else {
        return DeletionDecision::Refuse {
            blockers: vec![DeletionBlocker::NoVerifiedReplacement {
                state: normalization.state_name(),
            }],
        };
    };

    // An incomplete view of the SOURCE means we cannot enumerate what deleting
    // it would lose. Same reasoning as `Undecidable::UnindexedStreams`, applied
    // to a destructive act rather than to a plan.
    if source.data_stream_count > 0
        || source.unindexed_stream_count > 0
        || source.other_stream_count > 0
    {
        blockers.push(DeletionBlocker::SourceNotFullyDescribed {
            data_streams: source.data_stream_count,
            unindexed_streams: source.unindexed_stream_count,
            other_streams: source.other_stream_count,
        });
    }

    // --- Dynamic range and Dolby Vision ------------------------------------

    let source_video = source.primary_video();
    let output_video = output.primary_video();

    match source_video.map(classify_hdr) {
        Some(HdrVerdict::Hdr { transfer }) => {
            let out_verdict = output_video.map(classify_hdr);
            let preserved = matches!(
                out_verdict,
                Some(HdrVerdict::Hdr { transfer: t }) if t == transfer
            );
            if !preserved {
                blockers.push(DeletionBlocker::HighDynamicRangeNotReproduced {
                    source: format!("{transfer:?}"),
                    output: match out_verdict {
                        Some(HdrVerdict::Hdr { transfer: t }) => format!("{t:?}"),
                        Some(HdrVerdict::Sdr) => "SDR".to_string(),
                        Some(HdrVerdict::Unknown { why }) => format!("unknown: {why}"),
                        None => "no video stream".to_string(),
                    },
                });
            }
        }
        Some(HdrVerdict::Unknown { why }) => {
            blockers.push(DeletionBlocker::SourceDynamicRangeUnknown {
                why: why.to_string(),
            });
        }
        Some(HdrVerdict::Sdr) => {}
        None => blockers.push(DeletionBlocker::StreamsLost {
            kind: "video",
            source: source.video.len(),
            output: output.video.len(),
        }),
    }

    let source_dv = source_video
        .map(classify_dolby_vision)
        .unwrap_or(DolbyVisionVerdict::NotDetected);
    if source_dv.is_present() {
        let output_dv = output_video
            .map(classify_dolby_vision)
            .unwrap_or(DolbyVisionVerdict::NotDetected);
        // Equality, not merely presence: a profile 8 source whose output
        // reports a different DV shape has not been reproduced either.
        if output_dv != source_dv {
            blockers.push(DeletionBlocker::DolbyVisionNotReproduced {
                source: source_dv.to_string(),
            });
        }
    }

    // --- Audio -------------------------------------------------------------
    //
    // Every source audio stream must have a counterpart in the output with the
    // SAME codec and at least as many channels. Same-codec is required rather
    // than "some audio exists": a TrueHD 7.1 replaced by AAC 5.1 is a loss
    // that no channel-count comparison alone would catch.

    let unmatched_source = unreproduced_audio(&source.audio, &output.audio);
    let reproduced = source.audio.len() - unmatched_source.len();
    {
        for a in unmatched_source {
            {
                blockers.push(DeletionBlocker::AudioStreamNotReproduced {
                    stream_index: a.index,
                    codec: a.codec.clone(),
                    channels: a.channels,
                });
                // Name the specific unrecoverable thing when the codec is one
                // that may be hiding a format we cannot see.
                if let Some(u) = undetectable_formats()
                    .iter()
                    .find(|u| u.carried_by_codec.eq_ignore_ascii_case(a.codec.trim()))
                {
                    blockers.push(DeletionBlocker::PossiblyObjectBearingAudioLost {
                        stream_index: a.index,
                        codec: a.codec.clone(),
                        format: u.name,
                    });
                }
            }
        }
    }

    // --- Everything else the source carries --------------------------------

    for (kind, s, o) in [
        ("video", source.video.len(), output.video.len()),
        ("audio", source.audio.len(), output.audio.len()),
        ("subtitle", source.subtitles.len(), output.subtitles.len()),
        (
            "attachment",
            source.attachments.len(),
            output.attachments.len(),
        ),
        ("chapter", source.chapter_count, output.chapter_count),
    ] {
        if o < s {
            blockers.push(DeletionBlocker::StreamsLost {
                kind,
                source: s,
                output: o,
            });
        }
    }

    // --- Resolution --------------------------------------------------------

    let resolution = match (source_video, output_video) {
        (Some(s), Some(o)) => match (s.width, s.height, o.width, o.height) {
            (Some(sw), Some(sh), Some(ow), Some(oh)) => {
                if ow < sw || oh < sh {
                    blockers.push(DeletionBlocker::ResolutionReduced {
                        from: (sw, sh),
                        to: (ow, oh),
                    });
                }
                Some((ow, oh))
            }
            _ => {
                blockers.push(DeletionBlocker::ResolutionReduced {
                    from: (s.width.unwrap_or(0), s.height.unwrap_or(0)),
                    to: (o.width.unwrap_or(0), o.height.unwrap_or(0)),
                });
                None
            }
        },
        _ => None,
    };

    // --- Duration ----------------------------------------------------------

    let duration = match (source.duration_secs, output.duration_secs) {
        (Some(s), Some(o)) if (s - o).abs() <= DURATION_TOLERANCE_SECS => Some(o),
        (s, o) => {
            blockers.push(DeletionBlocker::DurationNotProvenEqual {
                source: s,
                output: o,
            });
            None
        }
    };

    if !blockers.is_empty() {
        return DeletionDecision::Refuse { blockers };
    }

    // Reached only when every check above passed. These `else` arms cannot
    // fire here — an absent resolution or duration pushed a blocker — but they
    // are written as refusals rather than as `unwrap()` so that a future edit
    // removing one of those blockers fails closed instead of panicking.
    let (Some(resolution), Some(duration_secs)) = (resolution, duration) else {
        return DeletionDecision::Refuse {
            blockers: vec![DeletionBlocker::NoVerifiedReplacement {
                state: "evidence_incomplete",
            }],
        };
    };

    DeletionDecision::Allow {
        evidence: DeletionEvidence {
            dynamic_range_preserved: true,
            dolby_vision_absent_from_source: !source_dv.is_present(),
            audio_streams_reproduced: reproduced,
            subtitle_streams_reproduced: output.subtitles.len(),
            attachments_reproduced: output.attachments.len(),
            chapters_reproduced: output.chapter_count,
            resolution,
            duration_secs,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundry::probe::{
        AttachmentStream, AudioStream, StreamSideData, SubtitleStream, VideoStream,
    };

    fn video(codec: &str, w: u32, h: u32) -> VideoStream {
        VideoStream {
            index: 0,
            codec: codec.into(),
            width: Some(w),
            height: Some(h),
            bitrate_bps: Some(5_000_000),
            pix_fmt: Some("yuv420p".into()),
            ..VideoStream::default()
        }
    }

    fn audio(index: u32, codec: &str, channels: u32) -> AudioStream {
        AudioStream {
            index,
            codec: codec.into(),
            channels: Some(channels),
            language: Some("eng".into()),
            bitrate_bps: Some(640_000),
        }
    }

    fn probe(video: Vec<VideoStream>, audio: Vec<AudioStream>) -> MediaProbe {
        MediaProbe {
            container: "matroska,webm".into(),
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

    /// An ordinary SDR file and a faithful replacement of it: same streams,
    /// same codecs, same resolution, same duration. The one case that SHOULD
    /// allow deletion. Every other test breaks exactly one thing about it.
    fn faithful() -> (MediaProbe, NormalizationOutcome) {
        let source = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 2)]);
        let output = source.clone();
        (source, NormalizationOutcome::Verified { output })
    }

    fn decide(source: &MediaProbe, out: &NormalizationOutcome) -> DeletionDecision {
        may_delete_original(source, out)
    }

    fn blockers(d: &DeletionDecision) -> Vec<DeletionBlocker> {
        match d {
            DeletionDecision::Refuse { blockers } => blockers.clone(),
            DeletionDecision::Allow { .. } => Vec::new(),
        }
    }

    // --- the only allowing case --------------------------------------------

    #[test]
    fn a_faithful_replacement_of_an_ordinary_sdr_file_may_replace_its_original() {
        let (s, o) = faithful();
        let d = decide(&s, &o);
        let DeletionDecision::Allow { evidence } = &d else {
            panic!("expected Allow, got {d:?}");
        };
        assert!(evidence.dynamic_range_preserved);
        assert!(evidence.dolby_vision_absent_from_source);
        assert_eq!(evidence.audio_streams_reproduced, 1);
        assert_eq!(evidence.resolution, (1920, 1080));
        assert_eq!(evidence.duration_secs, 5400.0);
    }

    // --- no verified replacement -------------------------------------------

    #[test]
    fn a_plan_is_not_a_file_and_never_authorises_a_deletion() {
        // The most basic fail-closed case: planning to produce a replacement
        // is not the same as having produced one.
        let (s, _) = faithful();
        for outcome in [
            NormalizationOutcome::SourceAlreadyDirectPlays,
            NormalizationOutcome::Planned {
                decision: TranscodeDecision::AlreadyOptimal,
            },
            NormalizationOutcome::Failed {
                why: "ffmpeg exited 1".into(),
            },
        ] {
            let d = decide(&s, &outcome);
            assert!(!d.is_allowed(), "{outcome:?} must not allow deletion");
            assert!(matches!(
                blockers(&d).as_slice(),
                [DeletionBlocker::NoVerifiedReplacement { .. }]
            ));
        }
    }

    // --- HDR ----------------------------------------------------------------

    #[test]
    fn an_hdr_source_normalized_to_sdr_may_never_be_deleted() {
        // THE rule. x264 at yuv420p cannot carry PQ, so this is what every
        // real HDR normalization looks like, and it must refuse every time.
        let mut sv = video("hevc", 3840, 2160);
        sv.pix_fmt = Some("yuv420p10le".into());
        sv.color_transfer = Some("smpte2084".into());
        let source = probe(vec![sv], vec![audio(1, "aac", 2)]);

        let output = probe(vec![video("h264", 3840, 2160)], vec![audio(1, "aac", 2)]);
        let d = decide(&source, &NormalizationOutcome::Verified { output });

        assert!(!d.is_allowed());
        assert!(
            blockers(&d)
                .iter()
                .any(|b| matches!(b, DeletionBlocker::HighDynamicRangeNotReproduced { .. })),
            "got {:?}",
            blockers(&d)
        );
    }

    #[test]
    fn an_hdr_source_whose_hdr_actually_survived_is_deletable() {
        // The rule must not be a blanket "never delete anything 10-bit" — that
        // would be right for the wrong reason and would keep being right even
        // if the HDR comparison broke entirely.
        let mut v = video("hevc", 3840, 2160);
        v.pix_fmt = Some("yuv420p10le".into());
        v.color_transfer = Some("smpte2084".into());
        let source = probe(vec![v], vec![audio(1, "aac", 2)]);
        let output = source.clone();
        assert!(decide(&source, &NormalizationOutcome::Verified { output }).is_allowed());
    }

    #[test]
    fn an_hlg_source_replaced_by_a_pq_one_is_not_reproduced() {
        // "Still HDR" is not the same as "the same HDR": a transfer swap is a
        // regrade, not a copy.
        let mut sv = video("hevc", 3840, 2160);
        sv.pix_fmt = Some("yuv420p10le".into());
        sv.color_transfer = Some("arib-std-b67".into());
        let source = probe(vec![sv], vec![audio(1, "aac", 2)]);

        let mut ov = video("hevc", 3840, 2160);
        ov.pix_fmt = Some("yuv420p10le".into());
        ov.color_transfer = Some("smpte2084".into());
        let output = probe(vec![ov], vec![audio(1, "aac", 2)]);

        let d = decide(&source, &NormalizationOutcome::Verified { output });
        assert!(!d.is_allowed(), "got {d:?}");
    }

    #[test]
    fn an_undetermined_source_dynamic_range_refuses_just_as_hard_as_a_known_hdr_one() {
        // A 10-bit file with no transfer tag is exactly the shape of a
        // badly-muxed HDR release. "We could not tell" must not be the state
        // in which the only copy gets deleted.
        let mut sv = video("hevc", 1920, 1080);
        sv.pix_fmt = Some("yuv420p10le".into()); // 10-bit, no transfer tag
        let source = probe(vec![sv.clone()], vec![audio(1, "aac", 2)]);
        let output = probe(vec![sv], vec![audio(1, "aac", 2)]);

        let d = decide(&source, &NormalizationOutcome::Verified { output });
        assert!(!d.is_allowed(), "an unknown must refuse: {d:?}");
        assert!(blockers(&d)
            .iter()
            .any(|b| matches!(b, DeletionBlocker::SourceDynamicRangeUnknown { .. })));
    }

    // --- Dolby Vision -------------------------------------------------------

    #[test]
    fn a_dolby_vision_source_may_never_be_deleted_after_a_transcode() {
        let mut sv = video("hevc", 3840, 2160);
        sv.pix_fmt = Some("yuv420p10le".into());
        sv.color_transfer = Some("smpte2084".into());
        sv.side_data = vec![StreamSideData {
            kind: "DOVI configuration record".into(),
            dv_profile: Some(8),
            dv_bl_signal_compatibility_id: Some(1),
            ..StreamSideData::default()
        }];
        let source = probe(vec![sv.clone()], vec![audio(1, "aac", 2)]);

        // The output keeps the HDR but loses the RPU — the realistic outcome
        // of any re-encode, and the one that looks most like success.
        let mut ov = sv.clone();
        ov.side_data.clear();
        let output = probe(vec![ov], vec![audio(1, "aac", 2)]);

        let d = decide(&source, &NormalizationOutcome::Verified { output });
        assert!(!d.is_allowed());
        assert!(
            blockers(&d)
                .iter()
                .any(|b| matches!(b, DeletionBlocker::DolbyVisionNotReproduced { .. })),
            "the HDR check alone must not be what catches this — got {:?}",
            blockers(&d)
        );
    }

    #[test]
    fn a_dv_signal_visible_only_as_a_codec_tag_still_blocks_deletion() {
        // The weakest DV signal must be as blocking as the strongest: an
        // unknown profile could be profile 5.
        let mut sv = video("hevc", 3840, 2160);
        sv.pix_fmt = Some("yuv420p10le".into());
        sv.color_transfer = Some("smpte2084".into());
        sv.codec_tag = Some("dvh1".into());
        let source = probe(vec![sv.clone()], vec![audio(1, "aac", 2)]);

        let mut ov = sv.clone();
        ov.codec_tag = Some("hvc1".into());
        let output = probe(vec![ov], vec![audio(1, "aac", 2)]);

        let d = decide(&source, &NormalizationOutcome::Verified { output });
        assert!(!d.is_allowed(), "got {d:?}");
        assert!(blockers(&d)
            .iter()
            .any(|b| matches!(b, DeletionBlocker::DolbyVisionNotReproduced { .. })));
    }

    // --- audio ---------------------------------------------------------------

    #[test]
    fn a_truehd_track_downmixed_to_aac_blocks_deletion() {
        let source = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "truehd", 8)]);
        let output = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 6)]);
        let d = decide(&source, &NormalizationOutcome::Verified { output });
        assert!(!d.is_allowed());
        assert!(blockers(&d)
            .iter()
            .any(|b| matches!(b, DeletionBlocker::AudioStreamNotReproduced { .. })));
        assert!(
            blockers(&d).iter().any(|b| matches!(
                b,
                DeletionBlocker::PossiblyObjectBearingAudioLost { format, .. }
                    if format.contains("Atmos")
            )),
            "the Atmos-inside-TrueHD risk must be named, not just 'audio lost': {:?}",
            blockers(&d)
        );
    }

    #[test]
    fn an_eac3_track_that_was_not_carried_through_blocks_deletion_because_atmos_is_invisible() {
        // The deliberately over-broad rule. An Atmos (JOC) E-AC-3 track and an
        // ordinary 5.1 one are IDENTICAL in ffprobe output, so any E-AC-3 that
        // was not copied verbatim must be assumed to have been Atmos.
        let source = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "eac3", 6)]);
        let output = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 6)]);
        let d = decide(&source, &NormalizationOutcome::Verified { output });
        assert!(!d.is_allowed());
        assert!(
            blockers(&d).iter().any(|b| matches!(
                b,
                DeletionBlocker::PossiblyObjectBearingAudioLost { codec, .. } if codec == "eac3"
            )),
            "got {:?}",
            blockers(&d)
        );
    }

    #[test]
    fn an_eac3_track_carried_through_untouched_does_not_block() {
        // The over-broad rule must still have an off state, otherwise every
        // 5.1 release in the library is permanently undeletable and the rule
        // is indistinguishable from "never delete".
        let source = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "eac3", 6)]);
        let output = source.clone();
        assert!(decide(&source, &NormalizationOutcome::Verified { output }).is_allowed());
    }

    /// `may_delete_original` currently has NO CALLER, and that is deliberate —
    /// but it must not stay invisible.
    ///
    /// Nothing in Muse permanently deletes an original today. `forge`'s swap
    /// hard-links the original to a sibling `.muse-superseded` name *before* it
    /// releases the original name, so the bytes stay reachable; what looks like
    /// a delete in `swap_verified_output` is one of two names being unlinked.
    /// Permanent loss happens only when something removes that
    /// `.muse-superseded` entry, and no such reaper exists yet.
    ///
    /// This gate is for that reaper. The failure mode worth guarding is someone
    /// writing the reaper later and never finding this function — a gate with
    /// no caller reads as "already handled". So: any module that both knows the
    /// superseded suffix and removes files must also mention this gate.
    #[test]
    fn a_module_that_reaps_superseded_files_must_consult_the_deletion_gate() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/foundry");
        let mut offenders = Vec::new();
        for e in std::fs::read_dir(&dir).expect("foundry sources must be readable") {
            let p = e.expect("a readable dir entry").path();
            if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            // forge owns the swap itself and is covered by its own invariant
            // tests (it keeps the original; it does not reap).
            if name == "forge.rs" {
                continue;
            }
            let src = std::fs::read_to_string(&p).expect("a readable source file");
            let knows_the_backup = src.contains("muse-superseded");
            let removes_files = src.contains("remove_file") || src.contains("remove_dir_all");
            let consults_the_gate = src.contains("may_delete_original");
            if knows_the_backup && removes_files && !consults_the_gate {
                offenders.push(name);
            }
        }
        assert!(
            offenders.is_empty(),
            "these modules can reap a preserved original without consulting \
             may_delete_original: {offenders:?}. Deleting a `.muse-superseded` \
             entry is the only permanent data loss in Foundry — call the gate \
             first, or exclude the module here with a stated reason."
        );
    }

    /// The live case that motivated this.
    ///
    /// Silicon Valley S05E01: AV1, 1080p, 10-bit, NO color_transfer tag. AV1
    /// is not an accepted codec so the planner correctly re-encodes — and the
    /// argv forces 8-bit yuv420p — after which the gate refuses with
    /// SourceDynamicRangeUnknown. Hours of CPU for an original that is then
    /// kept. Predicting it needs no encode.
    #[test]
    fn an_untagged_ten_bit_source_that_will_be_re_encoded_is_predicted_undeletable() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let mut p = probe(vec![video("av1", 1920, 1080)], vec![audio(1, "aac", 2)]);
        p.video[0].pix_fmt = Some("yuv420p10le".into());
        p.video[0].color_transfer = None; // untagged: the ambiguous case

        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Copy,
            container: Container::Matroska,
        };
        let predicted = predicted_deletion_refusals(&p, &plan);
        assert!(
            predicted.iter().any(|r| r.contains("undetermined")),
            "an untagged 10-bit source that will be re-encoded must be predicted \
             undeletable: {predicted:?}"
        );
    }

    /// A file the gate WILL allow must predict nothing, or the number is
    /// useless — a prediction that fires on everything tells an operator
    /// nothing about which titles reclaim disk.
    #[test]
    fn an_ordinary_sdr_re_encode_predicts_no_refusal() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let mut p = probe(vec![video("mpeg4", 720, 480)], vec![audio(1, "aac", 2)]);
        p.video[0].pix_fmt = Some("yuv420p".into());
        p.video[0].color_transfer = Some("bt709".into());

        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Copy,
            container: Container::Matroska,
        };
        assert!(
            predicted_deletion_refusals(&p, &plan).is_empty(),
            "a plain SDR re-encode reclaims disk and must not be flagged"
        );
    }

    /// The bug this prediction had, stated as a test.
    ///
    /// A plain mp3 stereo track carries no hidden object audio, so the old
    /// prediction said "nothing will refuse". The gate then refused anyway,
    /// because mp3 -> aac changes the codec and `may_delete_original` matches
    /// streams BY CODEC. Found by running the swap end-to-end on a real file,
    /// not by reading the code.
    #[test]
    fn a_plain_audio_reencode_is_predicted_to_block_deletion() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let p = probe(vec![video("mpeg4", 720, 480)], vec![audio(1, "mp3", 2)]);
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Encode { channels: vec![2] },
            container: Container::Matroska,
        };

        let refusals = predicted_deletion_refusals(&p, &plan);
        assert!(
            !refusals.is_empty(),
            "an mp3 -> aac re-encode must be predicted to block deletion: the gate \
             matches streams by codec, so a re-encoded track never matches its source"
        );
        assert!(
            refusals.iter().any(|r| r.contains("CODEC")),
            "the refusal must name the real reason (codec matching), not a \
             hidden-object-audio guess that does not apply to mp3: {refusals:?}"
        );
    }

    /// The prediction must agree with the gate, not merely be non-empty.
    /// Both are run over the same source and the same re-encode: if the gate
    /// refuses, the prediction must have said so.
    #[test]
    fn the_prediction_agrees_with_the_gate_for_a_plain_audio_reencode() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let source = probe(vec![video("mpeg4", 720, 480)], vec![audio(1, "mp3", 2)]);
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Encode { channels: vec![2] },
            container: Container::Matroska,
        };

        // What the encode actually produces: h264 video, aac audio.
        let output = probe(vec![video("h264", 720, 480)], vec![audio(1, "aac", 2)]);

        let gate_refuses = !decide(&source, &NormalizationOutcome::Verified { output }).is_allowed();
        let predicted = !predicted_deletion_refusals(&source, &plan).is_empty();

        assert!(
            gate_refuses,
            "sanity: the gate must refuse an mp3 -> aac swap, since no output \
             stream matches the source codec"
        );
        assert_eq!(
            gate_refuses, predicted,
            "the prediction must agree with the gate — a prediction that says \
             'reclaims disk' where the gate refuses is what mis-sized the survey"
        );
    }

    /// The opposite error, which the first version of this fix committed: a
    /// blanket "any audio encode refuses" over-reports.
    ///
    /// `Encode` emits aac. An aac source re-encoded to aac with the SAME
    /// channel count matches the gate's rule, so the gate allows the deletion
    /// and the prediction must not claim otherwise — over-reporting understates
    /// how much disk a run reclaims.
    #[test]
    fn an_aac_to_aac_reencode_at_the_same_channel_count_predicts_no_refusal() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let p = probe(vec![video("mpeg4", 720, 480)], vec![audio(1, "aac", 2)]);
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Encode { channels: vec![2] },
            container: Container::Matroska,
        };
        assert!(
            predicted_deletion_refusals(&p, &plan).is_empty(),
            "aac -> aac at the same channel count matches the gate's rule, so \
             predicting a refusal would understate reclaimable disk"
        );
    }

    /// An upmix still matches: the gate's rule is `out >= src`.
    #[test]
    fn an_aac_upmix_predicts_no_refusal_because_the_gate_allows_it() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let p = probe(vec![video("mpeg4", 720, 480)], vec![audio(1, "aac", 2)]);
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Encode { channels: vec![6] },
            container: Container::Matroska,
        };
        assert!(predicted_deletion_refusals(&p, &plan).is_empty());
    }

    /// A DOWNMIX does not: 2 channels out cannot reproduce 6 in.
    #[test]
    fn an_aac_downmix_is_predicted_to_refuse_because_channels_are_lost() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let p = probe(vec![video("mpeg4", 720, 480)], vec![audio(1, "aac", 6)]);
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Encode { channels: vec![2] },
            container: Container::Matroska,
        };
        assert!(
            !predicted_deletion_refusals(&p, &plan).is_empty(),
            "a 5.1 -> stereo downmix loses channels, so the gate refuses and the \
             prediction must say so"
        );
    }

    /// The property that matters, over a spread of cases rather than one:
    /// whatever the plan does to audio, the prediction and the gate agree.
    /// The mis-count happened because nothing asserted this.
    #[test]
    fn the_prediction_and_the_gate_agree_across_audio_shapes() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        // (source codec, source channels, planned channels)
        let cases = [
            ("mp3", 2u32, 2u32),
            ("aac", 2, 2),
            ("aac", 2, 6),
            ("aac", 6, 2),
            ("ac3", 6, 6),
            ("eac3", 6, 6),
            ("flac", 2, 2),
            ("truehd", 8, 6),
        ];

        for (codec, src_ch, out_ch) in cases {
            let source = probe(
                vec![video("mpeg4", 720, 480)],
                vec![audio(1, codec, src_ch)],
            );
            let plan = TranscodePlan {
                video_stream_index: 0,
                video: VideoAction::Encode { scale: None },
                audio: AudioAction::Encode {
                    channels: vec![out_ch],
                },
                container: Container::Matroska,
            };

            // The file the plan actually produces, probed.
            let mut out_audio = audio(1, "aac", out_ch);
            out_audio.bitrate_bps = None;
            let output = probe(vec![video("h264", 720, 480)], vec![out_audio]);

            let gate_refuses = !decide(&source, &NormalizationOutcome::Verified { output })
                .is_allowed();
            let predicted = !predicted_deletion_refusals(&source, &plan).is_empty();

            assert_eq!(
                gate_refuses, predicted,
                "prediction and gate disagree for {codec} {src_ch}ch -> aac {out_ch}ch \
                 (gate refuses: {gate_refuses}, predicted: {predicted})"
            );
        }
    }

    /// Multi-stream, because per-stream index mapping is the one place the
    /// prediction and the gate could still diverge (raised at the FOUNDRY-29
    /// gate). Two source tracks with different channel targets: the first is
    /// reproduced, the second is downmixed and must refuse.
    #[test]
    fn the_prediction_and_the_gate_agree_across_multiple_audio_streams() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let source = probe(
            vec![video("mpeg4", 720, 480)],
            vec![audio(1, "aac", 2), audio(2, "aac", 6)],
        );
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Encode {
                channels: vec![2, 2],
            },
            container: Container::Matroska,
        };

        let mut o1 = audio(1, "aac", 2);
        o1.bitrate_bps = None;
        let mut o2 = audio(2, "aac", 2);
        o2.bitrate_bps = None;
        let output = probe(vec![video("h264", 720, 480)], vec![o1, o2]);

        let gate_refuses =
            !decide(&source, &NormalizationOutcome::Verified { output }).is_allowed();
        let refusals = predicted_deletion_refusals(&source, &plan);

        assert!(gate_refuses, "the 6ch track is downmixed to 2ch, so the gate must refuse");
        assert_eq!(
            gate_refuses,
            !refusals.is_empty(),
            "prediction and gate must agree with more than one audio stream: {refusals:?}"
        );
        assert!(
            refusals.iter().any(|r| r.contains("stream 2")),
            "the refusal must name the stream that actually lost channels, not the \
             one that was fine: {refusals:?}"
        );
    }

    /// The channel target is read PER STREAM, not once for all of them.
    ///
    /// The previous multi-stream test used `[2, 2]`, where the first target and
    /// the i-th target are the same value — so `channels.first()` passed it and
    /// the index mapping was never actually constrained. Caught by mutating
    /// `channels.get(i)` to `channels.first()` and finding the mutant survived.
    ///
    /// Here the two targets differ: stream 1 keeps its 6 channels, stream 2 is
    /// downmixed. Reading the first target for both would predict that stream 2
    /// keeps 6 channels too, and miss the refusal the gate raises.
    #[test]
    fn each_audio_stream_uses_its_own_channel_target() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let source = probe(
            vec![video("mpeg4", 720, 480)],
            vec![audio(1, "aac", 6), audio(2, "aac", 6)],
        );
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Encode {
                channels: vec![6, 2],
            },
            container: Container::Matroska,
        };

        let mut o1 = audio(1, "aac", 6);
        o1.bitrate_bps = None;
        let mut o2 = audio(2, "aac", 2);
        o2.bitrate_bps = None;
        let output = probe(vec![video("h264", 720, 480)], vec![o1, o2]);

        let gate_refuses =
            !decide(&source, &NormalizationOutcome::Verified { output }).is_allowed();
        let refusals = predicted_deletion_refusals(&source, &plan);

        assert!(gate_refuses, "stream 2 loses channels, so the gate refuses");
        assert_eq!(
            gate_refuses,
            !refusals.is_empty(),
            "reading the first channel target for every stream hides stream 2's \
             downmix: {refusals:?}"
        );
    }

    /// Fewer channel targets than streams: the trailing stream falls back to
    /// its source channel count, so it is reproduced and must not be flagged.
    #[test]
    fn a_short_channel_list_falls_back_to_the_source_channel_count() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let source = probe(
            vec![video("mpeg4", 720, 480)],
            vec![audio(1, "aac", 2), audio(2, "aac", 2)],
        );
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Encode { scale: None },
            audio: AudioAction::Encode { channels: vec![2] },
            container: Container::Matroska,
        };
        assert!(
            predicted_deletion_refusals(&source, &plan).is_empty(),
            "the second stream keeps its own channel count, so both are reproduced"
        );
    }

    /// A COPY preserves everything, so even an HDR source predicts nothing.
    /// Without this the prediction could be satisfied by keying on the source
    /// alone and ignoring the plan.
    #[test]
    fn a_remux_of_an_hdr_source_predicts_no_refusal() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let mut p = probe(vec![video("hevc", 3840, 2160)], vec![audio(1, "aac", 2)]);
        p.video[0].pix_fmt = Some("yuv420p10le".into());
        p.video[0].color_transfer = Some("smpte2084".into());

        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Copy,
            audio: AudioAction::Copy,
            container: Container::Matroska,
        };
        assert!(
            predicted_deletion_refusals(&p, &plan).is_empty(),
            "a remux preserves HDR, so nothing should be predicted"
        );
    }

    /// Object-bearing audio that will be re-encoded is predictable too.
    #[test]
    fn re_encoding_possibly_atmos_audio_is_predicted_undeletable() {
        use crate::foundry::plan::{AudioAction, TranscodePlan, VideoAction};
        let p = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "eac3", 6)]);
        let plan = TranscodePlan {
            video_stream_index: 0,
            video: VideoAction::Copy,
            audio: AudioAction::Encode { channels: vec![6] },
            container: Container::Matroska,
        };
        let predicted = predicted_deletion_refusals(&p, &plan);
        assert!(
            predicted.iter().any(|r| r.contains("Atmos") || r.contains("invisible")),
            "{predicted:?}"
        );
    }

    /// Raised by all three reviewers at the FOUNDRY-03 gate, and correct.
    ///
    /// The docstring promised that an E-AC-3 stream "not carried through
    /// byte-for-byte" blocks deletion. The code checked codec and channel
    /// count. Those are not the same claim: a RE-ENCODED E-AC-3 6-channel
    /// track has the same codec and the same channel count as the Atmos track
    /// it was made from, and JOC does not survive a re-encode.
    ///
    /// ffprobe cannot tell a copied E-AC-3 stream from a re-encoded one — that
    /// is the whole reason these formats are on the undetectable list. So the
    /// rule cannot be "prove it was re-encoded"; it has to be "refuse unless
    /// every probe-visible property is unchanged", which is the strongest
    /// evidence of a copy available here and still not proof.
    #[test]
    fn a_reencoded_eac3_track_of_the_same_shape_does_not_count_as_reproduced() {
        let source = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "eac3", 6)]);

        // Same codec, same channel count — the old match — but re-encoded to a
        // different bitrate. Under the old rule this was "reproduced" and the
        // original could be deleted, taking the only Atmos mix with it.
        let mut out = source.clone();
        out.audio[0].bitrate_bps = Some(384_000);
        let mut src = source.clone();
        src.audio[0].bitrate_bps = Some(768_000);

        let d = decide(&src, &NormalizationOutcome::Verified { output: out });
        assert!(!d.is_allowed(), "a re-encoded E-AC-3 track must block: {d:?}");

        // ...and it must say WHY, naming the format that may have been lost,
        // not just "a stream did not match".
        assert!(
            format!("{d:?}").contains("PossiblyObjectBearingAudioLost"),
            "must name the unrecoverable format: {d:?}"
        );
    }

    /// The other half: an unknown bitrate is not evidence of a copy either.
    #[test]
    fn an_eac3_track_with_an_unknown_bitrate_cannot_be_shown_to_be_a_copy() {
        let source = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "eac3", 6)]);
        let mut src = source.clone();
        src.audio[0].bitrate_bps = Some(768_000);
        let mut out = source.clone();
        out.audio[0].bitrate_bps = None; // muxer wrote no bitrate
        assert!(
            !decide(&src, &NormalizationOutcome::Verified { output: out }).is_allowed(),
            "unknown must refuse, not pass"
        );
    }

    /// An ordinary codec that hides nothing keeps the loose rule — otherwise
    /// this change would quietly make most of the library undeletable.
    #[test]
    fn a_reencoded_aac_track_still_counts_as_reproduced() {
        let source = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 6)]);
        let mut src = source.clone();
        src.audio[0].bitrate_bps = Some(320_000);
        let mut out = source.clone();
        out.audio[0].bitrate_bps = Some(128_000);
        assert!(
            decide(&src, &NormalizationOutcome::Verified { output: out }).is_allowed(),
            "AAC carries no undetectable format; the strict rule must not apply"
        );
    }

    #[test]
    fn a_second_language_track_that_was_dropped_blocks_deletion() {
        // Not an exotic format — just a stream that is gone. The commentary
        // track nobody noticed is as unrecoverable as the Atmos mix.
        let source = probe(
            vec![video("h264", 1920, 1080)],
            vec![audio(1, "aac", 6), audio(2, "aac", 2)],
        );
        let output = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 6)]);
        let d = decide(&source, &NormalizationOutcome::Verified { output });
        assert!(!d.is_allowed(), "got {d:?}");
    }

    #[test]
    fn two_source_tracks_cannot_both_be_satisfied_by_one_output_track() {
        // The multiset check: without consuming the match, one surviving AAC
        // stereo track would "reproduce" both of the source's.
        let source = probe(
            vec![video("h264", 1920, 1080)],
            vec![audio(1, "aac", 2), audio(2, "aac", 2)],
        );
        let output = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 2)]);
        assert!(!decide(&source, &NormalizationOutcome::Verified { output }).is_allowed());
    }

    #[test]
    fn an_unknown_channel_count_cannot_be_shown_to_match() {
        let mut source = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 6)]);
        source.audio[0].channels = None;
        let output = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 6)]);
        assert!(!decide(&source, &NormalizationOutcome::Verified { output }).is_allowed());
    }

    // --- other streams -------------------------------------------------------

    #[test]
    fn dropped_subtitles_attachments_or_chapters_block_deletion() {
        for break_it in [0usize, 1, 2] {
            let (mut source, _) = faithful();
            match break_it {
                0 => {
                    source.subtitles = vec![SubtitleStream {
                        index: 2,
                        codec: "subrip".into(),
                        language: Some("eng".into()),
                        forced: false,
                        default: true,
                    }]
                }
                1 => {
                    source.attachments = vec![AttachmentStream {
                        index: 3,
                        codec: "ttf".into(),
                        filename: Some("Gandhi Sans.ttf".into()),
                    }]
                }
                _ => source.chapter_count = 12,
            }
            // The output is the *faithful* one, i.e. missing what we just added.
            let output = faithful().0;
            let d = decide(&source, &NormalizationOutcome::Verified { output });
            assert!(!d.is_allowed(), "case {break_it}: {d:?}");
            assert!(
                blockers(&d)
                    .iter()
                    .any(|b| matches!(b, DeletionBlocker::StreamsLost { .. })),
                "case {break_it}: {:?}",
                blockers(&d)
            );
        }
    }

    #[test]
    fn an_incompletely_described_source_blocks_deletion() {
        // We cannot enumerate what deleting it would lose.
        for (data, unindexed, other) in [(1usize, 0usize, 0usize), (0, 1, 0), (0, 0, 1)] {
            let (mut source, o) = faithful();
            source.data_stream_count = data;
            source.unindexed_stream_count = unindexed;
            source.other_stream_count = other;
            let d = decide(&source, &o);
            assert!(!d.is_allowed(), "({data},{unindexed},{other}): {d:?}");
            assert!(blockers(&d)
                .iter()
                .any(|b| matches!(b, DeletionBlocker::SourceNotFullyDescribed { .. })));
        }
    }

    // --- resolution and duration ---------------------------------------------

    #[test]
    fn a_downscaled_replacement_never_authorises_deleting_the_original() {
        // The 4K case Path A's relaxed ceiling is designed to avoid, checked
        // independently of that policy — a policy change must not be able to
        // quietly make a downscale-then-delete possible.
        let source = probe(vec![video("hevc", 3840, 2160)], vec![audio(1, "aac", 2)]);
        let output = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 2)]);
        let d = decide(&source, &NormalizationOutcome::Verified { output });
        assert!(!d.is_allowed());
        assert!(blockers(&d).iter().any(|b| matches!(
            b,
            DeletionBlocker::ResolutionReduced {
                from: (3840, 2160),
                to: (1920, 1080)
            }
        )));
    }

    #[test]
    fn an_unproven_duration_blocks_deletion_because_the_output_may_be_truncated() {
        for (s, o) in [
            (Some(5400.0), None),
            (None, Some(5400.0)),
            (None, None),
            (Some(5400.0), Some(2700.0)),
        ] {
            let (mut source, _) = faithful();
            source.duration_secs = s;
            let mut out = faithful().0;
            out.duration_secs = o;
            let d = decide(&source, &NormalizationOutcome::Verified { output: out });
            assert!(!d.is_allowed(), "({s:?},{o:?}): {d:?}");
            assert!(blockers(&d)
                .iter()
                .any(|b| matches!(b, DeletionBlocker::DurationNotProvenEqual { .. })));
        }
    }

    #[test]
    fn a_sub_second_duration_difference_is_a_remux_artefact_not_a_truncation() {
        let (mut source, _) = faithful();
        source.duration_secs = Some(5400.0);
        let mut out = faithful().0;
        out.duration_secs = Some(5400.04);
        assert!(decide(&source, &NormalizationOutcome::Verified { output: out }).is_allowed());
    }

    #[test]
    fn every_blocker_is_reported_not_just_the_first() {
        // An operator who fixes one refusal must not be ambushed by the next.
        let mut sv = video("hevc", 3840, 2160);
        sv.pix_fmt = Some("yuv420p10le".into());
        sv.color_transfer = Some("smpte2084".into());
        sv.codec_tag = Some("dvhe".into());
        let source = probe(vec![sv], vec![audio(1, "truehd", 8)]);
        let output = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 2)]);
        let d = decide(&source, &NormalizationOutcome::Verified { output });
        let b = blockers(&d);
        assert!(b.len() >= 4, "expected several blockers, got {b:?}");
        assert!(b
            .iter()
            .any(|x| matches!(x, DeletionBlocker::HighDynamicRangeNotReproduced { .. })));
        assert!(b
            .iter()
            .any(|x| matches!(x, DeletionBlocker::DolbyVisionNotReproduced { .. })));
        assert!(b
            .iter()
            .any(|x| matches!(x, DeletionBlocker::AudioStreamNotReproduced { .. })));
        assert!(b
            .iter()
            .any(|x| matches!(x, DeletionBlocker::ResolutionReduced { .. })));
    }

    #[test]
    fn blockers_render_as_operator_readable_text() {
        let b = DeletionBlocker::PossiblyObjectBearingAudioLost {
            stream_index: 1,
            codec: "eac3".into(),
            format: "Dolby Atmos (JOC) inside E-AC-3",
        };
        assert!(b.to_string().contains("CANNOT tell"), "got {b}");
    }

    // --- direct-play diagnostics ---------------------------------------------

    #[test]
    fn a_conforming_file_has_no_direct_play_blockers() {
        let p = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 2)]);
        assert_eq!(
            direct_play_blockers(&p, &TranscodePolicy::direct_play_normalization()),
            Vec::new()
        );
    }

    #[test]
    fn ten_bit_h264_is_reported_even_though_its_codec_name_looks_fine() {
        // Hi10P: the codec is `h264`, the policy accepts it, and yet almost no
        // client can hardware-decode it. One of the two blockers the existing
        // single-target policy cannot express.
        let mut v = video("h264", 1920, 1080);
        v.pix_fmt = Some("yuv420p10le".into());
        let p = probe(vec![v], vec![audio(1, "aac", 2)]);
        let b = direct_play_blockers(&p, &TranscodePolicy::direct_play_normalization());
        assert!(
            b.contains(&DirectPlayBlocker::HighBitDepthH264 { bit_depth: 10 }),
            "got {b:?}"
        );
    }

    #[test]
    fn ten_bit_hevc_is_not_reported_because_main_10_is_the_normal_case() {
        // The mirror: flagging Main 10 HEVC would condemn every legitimate 4K
        // release in the library.
        let mut v = video("hevc", 3840, 2160);
        v.pix_fmt = Some("yuv420p10le".into());
        let p = probe(vec![v], vec![audio(1, "aac", 2)]);
        let b = direct_play_blockers(&p, &TranscodePolicy::direct_play_normalization());
        assert!(
            !b.iter()
                .any(|x| matches!(x, DirectPlayBlocker::HighBitDepthH264 { .. })),
            "got {b:?}"
        );
    }

    #[test]
    fn bitmap_subtitles_only_block_when_they_are_actually_selected() {
        // Present-but-not-default PGS costs nothing: nobody burns in a track
        // that was not selected. Marked default, it forces a full re-encode.
        let sub = |default: bool| SubtitleStream {
            index: 2,
            codec: "hdmv_pgs_subtitle".into(),
            language: Some("eng".into()),
            forced: false,
            default,
        };
        let policy = TranscodePolicy::direct_play_normalization();

        let mut p = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 2)]);
        p.subtitles = vec![sub(false)];
        assert!(direct_play_blockers(&p, &policy).is_empty());

        p.subtitles = vec![sub(true)];
        assert!(
            direct_play_blockers(&p, &policy)
                .iter()
                .any(|b| matches!(b, DirectPlayBlocker::DefaultBitmapSubtitles { .. })),
            "a default PGS track forces a burn-in re-encode"
        );
    }

    #[test]
    fn text_subtitles_never_block_however_they_are_flagged() {
        let policy = TranscodePolicy::direct_play_normalization();
        let mut p = probe(vec![video("h264", 1920, 1080)], vec![audio(1, "aac", 2)]);
        p.subtitles = vec![SubtitleStream {
            index: 2,
            codec: "subrip".into(),
            language: Some("eng".into()),
            forced: true,
            default: true,
        }];
        assert!(direct_play_blockers(&p, &policy).is_empty());
    }

    #[test]
    fn the_familiar_blockers_are_still_reported() {
        let policy = TranscodePolicy::direct_play_normalization();
        // `truehd`, not `dts`. DTS is now an accepted codec (FOUNDRY-23,
        // measured: re-encoding it produced ~3,000 titles the deletion gate
        // then refused), so it no longer raises this blocker. TrueHD still
        // does and is the right fixture for "an audio codec clients cannot
        // direct-play".
        let mut p = probe(vec![video("mpeg4", 1920, 1080)], vec![audio(1, "truehd", 8)]);
        p.container = "avi".into();
        let b = direct_play_blockers(&p, &policy);
        assert!(b
            .iter()
            .any(|x| matches!(x, DirectPlayBlocker::VideoCodecNotWidelySupported { .. })));
        assert!(b
            .iter()
            .any(|x| matches!(x, DirectPlayBlocker::AudioCodecNotWidelySupported { .. })));
        assert!(b
            .iter()
            .any(|x| matches!(x, DirectPlayBlocker::AudioChannelsAboveClientCeiling { .. })));
        assert!(b
            .iter()
            .any(|x| matches!(x, DirectPlayBlocker::ContainerNotStreamable { .. })));
    }

    #[test]
    fn a_4k_file_is_not_a_direct_play_blocker_under_the_normalization_target() {
        // The reason Path A relaxes the resolution ceiling: a 4K client
        // direct-plays 4K, and the default policy's 1080p ceiling would order
        // an irreversible downscale for a bandwidth reason.
        let p = probe(vec![video("hevc", 3840, 2160)], vec![audio(1, "eac3", 6)]);
        assert!(direct_play_blockers(&p, &TranscodePolicy::direct_play_normalization()).is_empty());
        // ...whereas the size-oriented default does flag it.
        assert!(direct_play_blockers(&p, &TranscodePolicy::default())
            .iter()
            .any(|b| matches!(b, DirectPlayBlocker::ResolutionAboveCeiling { .. })));
    }
}
