//! FOUNDRY-03 **Path B**, the planner — decide, per rung, what a rendition of
//! one marked title would be.
//!
//! ## The decision is not re-implemented here
//!
//! Each rung is expressed as a [`TranscodePolicy`](crate::foundry::policy::TranscodePolicy) by
//! [`Rendition::as_policy`] and handed to
//! [`plan_transcode`](crate::foundry::plan::plan_transcode). That is
//! deliberate and it is the load-bearing reuse in this item: every undecidable
//! MUSEF-02 established — unindexed streams, data streams, an unknown
//! duration, an unidentifiable audio codec, attachments that cannot survive
//! the target container — applies unchanged to every rung, and
//! `AlreadyOptimal` keeps its meaning of "every dimension was checked and
//! passed" rather than being re-derived by a second, subtly different
//! procedure.
//!
//! What this module adds on top of that decision is only what a *ladder* needs
//! and a single target does not:
//!
//! - the dynamic-range gates (Dolby Vision, tone-mapping), which are the only
//!   place a rendition can come out visibly broken;
//! - the pointlessness rules — a rung that would upscale, duplicate another
//!   rung, or merely copy the source is skipped **with a stated reason**,
//!   never silently emitted and never silently dropped;
//! - the container-specific subtitle rules, which differ per rung because the
//!   rungs target different containers.
//!
//! ## Everything here is pure
//!
//! No filesystem, no `Command`, no clock. [`plan_ladder`] is a total function
//! of its inputs and every branch is unit-tested on a host with no ffmpeg at
//! all — which is this one.

use crate::foundry::directplay::BITMAP_SUBTITLE_CODECS;
use crate::foundry::hdr::{
    classify_dolby_vision, classify_hdr, tone_map, DolbyVisionVerdict, DynamicRangeUnknown,
    HdrTransfer, HdrVerdict, ToneMap, ToneMapSupport,
};
use crate::foundry::plan::{
    container_holds_attachments, plan_transcode, TranscodeDecision, TranscodePlan, TranscodeReason,
    Undecidable, VideoAction,
};
use crate::foundry::policy::Container;
use crate::foundry::probe::MediaProbe;
use crate::foundry::rendition::{
    rendition_output_path, DynamicRangeTreatment, Ladder, PathModelError, Rendition,
    RenditionName, RenditionRequest, VideoTreatment,
};

/// Host facts the planner cannot observe for itself.
///
/// Currently just the tone-mapper. It is a parameter rather than a lookup
/// because this module performs no I/O — and because its default is
/// [`ToneMapSupport::Unverified`], so a caller that forgets to establish it
/// gets undecidable HDR rungs rather than a hopeful encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LadderContext {
    pub tone_map_support: ToneMapSupport,
}

/// Why a rung was skipped: it would have been pointless, not wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum RenditionSkip {
    /// The source already satisfies this rung on every axis, so the rung would
    /// be a copy. Clients asking for it should be served the source.
    SourceAlreadySatisfiesRung,
    /// Another requested rung resolves to the same output resolution for this
    /// source and is the better of the two. Named rather than silent so a
    /// client asking for the skipped rung knows what to fall back to.
    DuplicatesRung {
        superseded_by: RenditionName,
        resolution: (u32, u32),
    },
    /// The hifi rung has nothing to preserve: the source is SDR, within 1080p,
    /// and carries no lossless or object-based audio. A hifi rendition would
    /// be a byte-for-byte second copy of a file that is not going anywhere —
    /// Path B never deletes the source, so the source *is* the hifi copy.
    NothingToPreserve,
}

impl std::fmt::Display for RenditionSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceAlreadySatisfiesRung => write!(
                f,
                "the source already satisfies this rung on every axis — serve the source \
                 rather than making a copy of it"
            ),
            Self::DuplicatesRung {
                superseded_by,
                resolution,
            } => write!(
                f,
                "this rung resolves to {}x{} for this source, the same as the `{}` rung, \
                 which is the better of the two — fall back to that one",
                resolution.0,
                resolution.1,
                superseded_by.as_str()
            ),
            Self::NothingToPreserve => write!(
                f,
                "this title is SDR, within 1080p, and has no lossless or object-based audio, \
                 so a hifi rendition would just be a second copy of the source — and the \
                 source is never removed on this path"
            ),
        }
    }
}

/// Why a rung was refused: producing it would give a wrong or broken file.
#[derive(Debug, Clone, PartialEq)]
pub enum RenditionRefusal {
    /// The source is Dolby Vision in a form that cannot be re-encoded. The
    /// headline case is profile 5, which renders green and purple once its RPU
    /// is discarded.
    DolbyVisionCannotBeTranscoded { verdict: String },
    /// The source carries Dolby Vision and this rung would have to change the
    /// container to produce anything. Whether a DV configuration record
    /// survives a container change depends on the ffmpeg version and on the
    /// container pair, and Foundry cannot verify that here — so the rung that
    /// exists to preserve Dolby Vision does not gamble with it.
    DolbyVisionRemuxNotAttempted { verdict: String, from: Container, to: Container },
    /// The hifi rung would have to re-encode, which it never does.
    HifiWouldRequireReEncoding { video: bool, audio: bool },
    /// Tone-mapping is needed and this host cannot do it.
    ToneMapperUnavailable { transfer: HdrTransfer },
    /// The rung's container is MP4 and the source has bitmap subtitles. MP4
    /// cannot carry PGS or VobSub at all, so the encode would fail outright —
    /// or, worse, succeed with the track silently dropped.
    BitmapSubtitlesCannotEnterMp4 { stream_index: u32, codec: String },
    /// The rung asks to re-encode HDR while declaring
    /// [`DynamicRangeTreatment::Preserve`]. Preserve means "do not touch the
    /// picture", which only holds for a copy — encoding under it would emit
    /// HDR pixels through an SDR encode with no tone map, which is the
    /// washed-out/clipped result the module exists to avoid.
    ///
    /// Not reachable from the built-in ladder. It IS reachable from a
    /// caller-supplied `Rendition`, since the fields are public, which is why
    /// this is a refusal and no longer a `debug_assert` — that check vanished
    /// in release builds, so the malformed config panicked in tests and
    /// silently produced a bad encode in production. Flagged by codex.
    HdrEncodeUnderPreserve { transfer: HdrTransfer },
    /// The plan would enlarge the picture. Should be unreachable — the shared
    /// `scale_to_fit` never upscales — and is checked on the emitted plan
    /// anyway, because "unreachable" is a claim about today's code.
    WouldUpscale { source: (u32, u32), target: (u32, u32) },
}

impl std::fmt::Display for RenditionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HdrEncodeUnderPreserve { transfer } => write!(
                f,
                "this rung re-encodes but declares Preserve, and the source is {transfer:?} HDR. \
                 Preserve is only valid alongside a copy: encoding without a tone map would \
                 emit HDR pixels into an SDR rendition"
            ),
            Self::DolbyVisionCannotBeTranscoded { verdict } => write!(
                f,
                "refusing to re-encode this rung: {verdict}. A visibly wrong rendition is \
                 worse than no rendition"
            ),
            Self::DolbyVisionRemuxNotAttempted { verdict, from, to } => write!(
                f,
                "refusing to remux Dolby Vision from `{}` to `{}` ({verdict}) — whether the \
                 configuration record survives that change depends on the ffmpeg build, and \
                 the rung whose job is preserving Dolby Vision will not gamble with it",
                from.ffmpeg_format(),
                to.ffmpeg_format()
            ),
            Self::HifiWouldRequireReEncoding { video, audio } => write!(
                f,
                "the hifi rung would have to re-encode {}{}{} to satisfy this source, and it \
                 never re-encodes — re-encoding is exactly what destroys the HDR grade and \
                 the object audio this rung exists to keep",
                if *video { "video" } else { "" },
                if *video && *audio { " and " } else { "" },
                if *audio { "audio" } else { "" }
            ),
            Self::ToneMapperUnavailable { transfer } => write!(
                f,
                "this rung must tone-map {} to SDR, but this host's ffmpeg has no `zscale` \
                 filter — encoding without the tone-map would produce the washed-out result \
                 the filter exists to prevent",
                transfer.as_str()
            ),
            Self::BitmapSubtitlesCannotEnterMp4 { stream_index, codec } => write!(
                f,
                "subtitle stream {stream_index} is bitmap-based (`{codec}`) and this rung \
                 targets MP4, which cannot carry it — the encode would fail, or silently drop \
                 the track"
            ),
            Self::WouldUpscale { source, target } => write!(
                f,
                "the plan would scale {}x{} UP to {}x{}, inventing pixels and growing the file",
                source.0, source.1, target.0, target.1
            ),
        }
    }
}

/// Why a rung could not be judged.
#[derive(Debug, Clone, PartialEq)]
pub enum LadderUndecidable {
    /// The shared planner declined to judge the source. Wrapped rather than
    /// re-worded so the ladder inherits MUSEF-02's reasons verbatim.
    Source { why: Undecidable },
    /// The source's dynamic range could not be established, so the rung cannot
    /// know whether to tone-map. Tone-mapping SDR and passing HDR through are
    /// both visible errors.
    UnknownDynamicRange { why: DynamicRangeUnknown },
    /// The rung must tone-map and nobody has established whether this host's
    /// ffmpeg can. Fail closed — see [`ToneMapSupport`].
    ToneMapperUnverified { transfer: HdrTransfer },
    /// The output path could not be modelled.
    OutputPathNotModelled { why: PathModelError },
}

impl std::fmt::Display for LadderUndecidable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source { why } => write!(f, "{why}"),
            Self::UnknownDynamicRange { why } => write!(
                f,
                "this rung targets SDR clients but the source's dynamic range is undetermined \
                 ({why}) — tone-mapping SDR content and passing HDR through are both visibly \
                 wrong, so neither is chosen"
            ),
            Self::ToneMapperUnverified { transfer } => write!(
                f,
                "this rung must tone-map {} to SDR, but nobody has verified that this host's \
                 ffmpeg has the `zscale` filter — check `ffmpeg -filters` and re-run",
                transfer.as_str()
            ),
            Self::OutputPathNotModelled { why } => write!(f, "{why}"),
        }
    }
}

/// What the ladder concluded about one rung.
#[derive(Debug, Clone, PartialEq)]
pub enum RenditionOutcome {
    /// Nothing to do, and why. Not a failure.
    Skip { why: RenditionSkip },
    /// Producing this rung would give a wrong file, and why.
    Refused { why: RenditionRefusal },
    /// Could not judge, and why. Never folded into `Skip`.
    CannotDecide { why: LadderUndecidable },
    /// Produce it. Carries the plan, the exact argv, the reasons, and the
    /// tone-map — if one was applied — in full.
    Encode {
        plan: TranscodePlan,
        args: Vec<String>,
        reasons: Vec<TranscodeReason>,
        /// `Some` exactly when the filter chain tone-maps. Recorded so a
        /// washed-out or too-dark result names the curve, the peak luminance
        /// and the desaturation that produced it.
        tone_map: Option<ToneMap>,
        /// The picture size this rung will actually produce.
        output_resolution: (u32, u32),
    },
}

impl RenditionOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Skip { .. } => "skip",
            Self::Refused { .. } => "refused",
            Self::CannotDecide { .. } => "cannot_decide",
            Self::Encode { .. } => "encode",
        }
    }

    pub fn is_encode(&self) -> bool {
        matches!(self, Self::Encode { .. })
    }
}

/// One rung's decision, with where its output would go.
#[derive(Debug, Clone, PartialEq)]
pub struct RenditionDecision {
    pub rendition: RenditionName,
    /// The modelled output path. `None` when the path itself could not be
    /// modelled — the outcome then says why.
    pub output_path: Option<String>,
    pub outcome: RenditionOutcome,
}

/// Audio codecs whose presence gives the hifi rung something to preserve.
/// Lossless or object-bearing: none of them survives a re-encode.
pub const PRESERVATION_WORTHY_AUDIO: &[&str] = &[
    "truehd", "dts", "mlp", "flac", "pcm_s16le", "pcm_s24le", "pcm_bluray", "pcm_dvd",
];

/// Plan a rendition ladder for one marked title.
///
/// `request` names the title and the rungs the operator asked for; nothing
/// else is planned. Duplicate rungs in the request are collapsed. The returned
/// vector has one entry per distinct requested rung, in ladder order, and
/// **every** rung gets an entry — a rung that will not be produced appears as
/// a `Skip`, `Refused` or `CannotDecide` with its reason, never as an absence.
pub fn plan_ladder(
    probe: &MediaProbe,
    ladder: &Ladder,
    request: &RenditionRequest,
    ctx: &LadderContext,
) -> Vec<RenditionDecision> {
    let mut rungs: Vec<RenditionName> = request.rungs.clone();
    rungs.sort();
    rungs.dedup();

    let mut decisions: Vec<RenditionDecision> = rungs
        .iter()
        .map(|name| plan_rung(probe, ladder, *name, &request.source_path, ctx))
        .collect();

    if ladder.dedupe_by_resolution {
        apply_resolution_dedupe(ladder, &mut decisions);
    }
    decisions
}

/// Plan one rung. Split out so every branch is reachable from a test without
/// constructing a whole request.
pub fn plan_rung(
    probe: &MediaProbe,
    ladder: &Ladder,
    name: RenditionName,
    source_path: &str,
    ctx: &LadderContext,
) -> RenditionDecision {
    let rendition = ladder.get(name);
    let output_path =
        match rendition_output_path(source_path, name, rendition.container, &ladder.layout) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(why) => {
                return RenditionDecision {
                    rendition: name,
                    output_path: None,
                    outcome: RenditionOutcome::CannotDecide {
                        why: LadderUndecidable::OutputPathNotModelled { why },
                    },
                }
            }
        };

    let outcome = decide_rung(probe, ladder, rendition, source_path, &output_path, ctx);
    RenditionDecision {
        rendition: name,
        output_path: Some(output_path),
        outcome,
    }
}

fn decide_rung(
    probe: &MediaProbe,
    ladder: &Ladder,
    rendition: &Rendition,
    input_path: &str,
    output_path: &str,
    ctx: &LadderContext,
) -> RenditionOutcome {
    let policy = rendition.as_policy();

    // The shared planner first, unchanged. Its undecidables are about whether
    // the FILE can be judged at all, and they must win over every rung-level
    // rule below — a rung-specific refusal computed from a partially-understood
    // file would be a judgement on facts we do not have.
    let decision = plan_transcode(probe, &policy, input_path, output_path);
    let (plan, reasons) = match decision {
        TranscodeDecision::CannotDecide { why } => {
            return RenditionOutcome::CannotDecide {
                why: LadderUndecidable::Source { why },
            }
        }
        TranscodeDecision::AlreadyOptimal => {
            return RenditionOutcome::Skip {
                why: RenditionSkip::SourceAlreadySatisfiesRung,
            }
        }
        TranscodeDecision::Transcode { plan, reasons, .. } => (plan, reasons),
    };

    // Unreachable today: `plan_transcode` returns `CannotDecide` for a file
    // with no video stream, so a `Transcode` implies one exists. Written as a
    // refusal rather than an `expect` anyway — the invariant lives in another
    // module, and a planner that panics takes out the worker for every other
    // title in the queue, not just this one.
    let Some(video) = probe.primary_video() else {
        return RenditionOutcome::CannotDecide {
            why: LadderUndecidable::Source {
                why: Undecidable::NoVideoStream,
            },
        };
    };
    let dv = classify_dolby_vision(video);

    match rendition.video {
        // --- the hifi rung: remux or nothing -------------------------------
        VideoTreatment::CopyOnly => {
            let re_encodes_video = matches!(plan.video, VideoAction::Encode { .. });
            let re_encodes_audio = matches!(plan.audio, crate::foundry::plan::AudioAction::Encode { .. });
            if re_encodes_video || re_encodes_audio {
                return RenditionOutcome::Refused {
                    why: RenditionRefusal::HifiWouldRequireReEncoding {
                        video: re_encodes_video,
                        audio: re_encodes_audio,
                    },
                };
            }
            // A pure remux. Dolby Vision does not go through one unverified.
            if dv.is_present() {
                let from = crate::foundry::policy::normalize_container(&probe.container)
                    .unwrap_or(plan.container);
                return RenditionOutcome::Refused {
                    why: RenditionRefusal::DolbyVisionRemuxNotAttempted {
                        verdict: dv.to_string(),
                        from,
                        to: plan.container,
                    },
                };
            }
            if !hifi_has_anything_to_preserve(probe, video) {
                return RenditionOutcome::Skip {
                    why: RenditionSkip::NothingToPreserve,
                };
            }
            let resolution = (video.width.unwrap_or(0), video.height.unwrap_or(0));
            let args = build_rendition_args(&plan, rendition, None, None, input_path, output_path);
            return RenditionOutcome::Encode {
                plan,
                args,
                reasons,
                tone_map: None,
                output_resolution: resolution,
            };
        }

        // --- the encoding rungs --------------------------------------------
        VideoTreatment::Encode { .. } => {}
    }

    // Dolby Vision, before anything else about this rung: a profile 5 source
    // re-encoded is green and purple, whatever else is true of the plan.
    if !dv.base_layer_is_transcodable() {
        return RenditionOutcome::Refused {
            why: RenditionRefusal::DolbyVisionCannotBeTranscoded {
                verdict: dv.to_string(),
            },
        };
    }

    // MP4 cannot carry bitmap subtitles. `-c:s copy` of a PGS track into MP4
    // fails the mux; the alternative (dropping it) is a silent loss.
    if plan.container == Container::Mp4 {
        if let Some(s) = probe.subtitles.iter().find(|s| {
            BITMAP_SUBTITLE_CODECS
                .iter()
                .any(|c| c.eq_ignore_ascii_case(s.codec.trim()))
        }) {
            return RenditionOutcome::Refused {
                why: RenditionRefusal::BitmapSubtitlesCannotEnterMp4 {
                    stream_index: s.index,
                    codec: s.codec.clone(),
                },
            };
        }
    }

    // Dynamic range. For a Dolby Vision source with a viewable base layer, the
    // thing being encoded IS the base layer, so its format — not the stream's
    // outer tags — is what decides whether a tone-map is needed.
    let effective_range = match &dv {
        DolbyVisionVerdict::BaseLayerViewable { base, .. } => base.as_hdr_verdict(),
        _ => classify_hdr(video),
    };

    let tone_map_record = match effective_range {
        HdrVerdict::Sdr => None,
        HdrVerdict::Unknown { why } => {
            return RenditionOutcome::CannotDecide {
                why: LadderUndecidable::UnknownDynamicRange { why },
            }
        }
        HdrVerdict::Hdr { transfer } => {
            if rendition.dynamic_range != DynamicRangeTreatment::ToneMapToSdr {
                return RenditionOutcome::Refused {
                    why: RenditionRefusal::HdrEncodeUnderPreserve { transfer },
                };
            }
            match ctx.tone_map_support {
                ToneMapSupport::Unverified => {
                    return RenditionOutcome::CannotDecide {
                        why: LadderUndecidable::ToneMapperUnverified { transfer },
                    }
                }
                ToneMapSupport::Unavailable => {
                    return RenditionOutcome::Refused {
                        why: RenditionRefusal::ToneMapperUnavailable { transfer },
                    }
                }
                ToneMapSupport::Available => Some(tone_map(transfer, ladder.tone_map_algorithm)),
            }
        }
    };

    // The upscale guard, checked on the EMITTED plan rather than trusted from
    // `scale_to_fit`'s documented behaviour.
    let source_dims = (video.width.unwrap_or(0), video.height.unwrap_or(0));
    if let Some(why) = upscale_refusal(source_dims, plan.video) {
        return RenditionOutcome::Refused { why };
    }
    let output_resolution = resolved_output_resolution(source_dims, plan.video);

    let args = build_rendition_args(
        &plan,
        rendition,
        tone_map_record.as_ref(),
        crate::foundry::hdr::sdr_gamut_conversion(video),
        input_path,
        output_path,
    );
    RenditionOutcome::Encode {
        plan,
        args,
        reasons,
        tone_map: tone_map_record,
        output_resolution,
    }
}

/// The picture size a plan will actually produce, given the source's.
pub fn resolved_output_resolution(source: (u32, u32), video: VideoAction) -> (u32, u32) {
    match video {
        VideoAction::Encode { scale: Some(target) } => target,
        _ => source,
    }
}

/// Refuse a plan that would make the picture **larger** than the source.
///
/// ## Why this exists when it should be unreachable
/// [`crate::foundry::policy::scale_to_fit`] never upscales, so with today's
/// planner this never fires. That is an argument for checking it, not against:
/// "unreachable" is a claim about the current implementation of a function in
/// another module, and this one is the last thing between that claim and a
/// rendition that invents pixels and grows the file. Upscaling is also the one
/// pointlessness the operator would not notice — a 360p source blown up to
/// 1080p looks like a working rendition until you compare it.
///
/// Extracted and exported rather than inlined because a guard that no test can
/// reach through the normal path is a guard nobody can prove works. This one is
/// tested directly against a plan that would upscale, which is a plan the
/// planner cannot currently produce.
pub fn upscale_refusal(source: (u32, u32), video: VideoAction) -> Option<RenditionRefusal> {
    let VideoAction::Encode { scale: Some(target) } = video else {
        return None;
    };
    (target.0 > source.0 || target.1 > source.1)
        .then_some(RenditionRefusal::WouldUpscale { source, target })
}

/// Whether a hifi rendition of this source would preserve anything the source
/// does not already trivially provide.
///
/// Path B never removes the source, so "preserve" here means "hold in a
/// container the hifi client can direct-play". If the title is ordinary
/// 1080p SDR with ordinary audio, the answer is no and the rung is skipped
/// rather than duplicating a file for nothing.
pub fn hifi_has_anything_to_preserve(
    probe: &MediaProbe,
    video: &crate::foundry::probe::VideoStream,
) -> bool {
    let above_1080p = video.width.unwrap_or(0) > 1920 || video.height.unwrap_or(0) > 1080;
    let hdr = !matches!(classify_hdr(video), HdrVerdict::Sdr);
    let dv = classify_dolby_vision(video).is_present();
    let rich_audio = probe.audio.iter().any(|a| {
        PRESERVATION_WORTHY_AUDIO
            .iter()
            .any(|c| c.eq_ignore_ascii_case(a.codec.trim()))
            || a.channels.unwrap_or(0) > 6
    });
    above_1080p || hdr || dv || rich_audio
}

/// Collapse rungs that would produce the same picture size.
///
/// Only rungs that **re-encode** take part. The hifi rung is excluded on
/// purpose: it can share a resolution with the tv rung while being a
/// completely different artefact — HEVC kept as-is versus H.264 guaranteed —
/// so deduping them against each other would delete a rung's whole reason for
/// existing.
fn apply_resolution_dedupe(ladder: &Ladder, decisions: &mut [RenditionDecision]) {
    // Winner per resolution: the highest rung, which is also the
    // highest-quality one (the ladder ascends — asserted in `rendition.rs`).
    let mut winner: std::collections::HashMap<(u32, u32), RenditionName> =
        std::collections::HashMap::new();
    for d in decisions.iter() {
        if !ladder.get(d.rendition).re_encodes_video() {
            continue;
        }
        if let RenditionOutcome::Encode {
            output_resolution, ..
        } = &d.outcome
        {
            winner
                .entry(*output_resolution)
                .and_modify(|w| {
                    if d.rendition > *w {
                        *w = d.rendition;
                    }
                })
                .or_insert(d.rendition);
        }
    }

    for d in decisions.iter_mut() {
        if !ladder.get(d.rendition).re_encodes_video() {
            continue;
        }
        let RenditionOutcome::Encode {
            output_resolution, ..
        } = &d.outcome
        else {
            continue;
        };
        let resolution = *output_resolution;
        if let Some(&w) = winner.get(&resolution) {
            if w != d.rendition {
                d.outcome = RenditionOutcome::Skip {
                    why: RenditionSkip::DuplicatesRung {
                        superseded_by: w,
                        resolution,
                    },
                };
            }
        }
    }
}

/// Build the exact ffmpeg argv for one rendition.
///
/// ## Why this is not [`crate::foundry::plan::build_transcode_args`]
///
/// Three things differ per rung and none of them can be expressed by appending
/// to, or overriding, the single-target builder's output — ffmpeg's handling of
/// a repeated `-vf` or `-c:a` is version-dependent, and relying on "the last
/// one wins" is exactly the kind of implicit contract that produces a wrong
/// file rather than an error:
///
/// 1. **The filter chain** carries the tone-map, possibly combined with a
///    scale.
/// 2. **Subtitle handling** depends on the container. Matroska takes
///    `-c:s copy`; MP4 cannot copy `subrip` at all and needs `mov_text`, and
///    cannot take bitmap subtitles in any form (which the planner refuses
///    before reaching here).
/// 3. **Audio bitrate** is stated per treatment rather than left to ffmpeg's
///    default.
///
/// The safety-shaped flags are identical to the single-target builder's and are
/// asserted to be so in the tests, so the two cannot drift on the parts where
/// drifting is dangerous.
///
/// ## Stated limitation: the audio encoder is AAC
/// The tv rung's 5.1 output is 5.1 AAC, not AC-3. AC-3 would be the more
/// widely direct-played choice on set-top boxes, and adding it means adding an
/// audio-encoder enum to the policy — a change to the shared single-target
/// path, which this item deliberately does not make. Recorded here rather than
/// left for someone to discover from a file.
///
/// ## Stated limitation: scaling happens in the encoded domain
/// When a rung both scales and tone-maps, the scale runs **first**, on
/// PQ-encoded values, before linearization. That is what standard pipelines do
/// and it is materially cheaper — tone-mapping 4K pixels that are about to be
/// thrown away is most of the cost of the operation. It is also very slightly
/// wrong: resampling a non-linear signal introduces a small error at
/// high-contrast edges. Doing it correctly would mean scaling inside the linear
/// section of the chain, which is left as a deliberate future change rather
/// than done silently either way.
pub fn build_rendition_args(
    plan: &TranscodePlan,
    rendition: &Rendition,
    tone_map: Option<&ToneMap>,
    // A BT.709 gamut conversion for a wide-gamut SDR source, from
    // `crate::foundry::hdr::sdr_gamut_conversion`. Mutually exclusive with
    // `tone_map`: the tone-map chain already converts primaries, and applying
    // both would convert twice.
    gamut: Option<&str>,
    input_path: &str,
    output_path: &str,
) -> Vec<String> {
    let policy = rendition.as_policy();
    let mut a: Vec<String> = Vec::new();
    let push = |a: &mut Vec<String>, s: &str| a.push(s.to_string());

    push(&mut a, "-hide_banner");
    push(&mut a, "-loglevel");
    push(&mut a, "error");
    // Both flags are load-bearing for an unattended worker, and for the same
    // reasons as in the single-target builder: ffmpeg reads stdin for
    // interactive keys and will otherwise consume or wedge on the inherited
    // one, and without `-y` the overwrite prompt is an immediate failure.
    push(&mut a, "-nostdin");
    push(&mut a, "-y");

    push(&mut a, "-i");
    a.push(input_path.to_string());

    // Absolute index, for the same reason as the single-target builder: the
    // probe parser filtered cover art out, so ffmpeg's own `v:0` may be the
    // poster while the index we judged is the feature.
    a.push("-map".to_string());
    a.push(format!("0:{}", plan.video_stream_index));
    push(&mut a, "-map");
    push(&mut a, "0:a?");
    push(&mut a, "-map");
    push(&mut a, "0:s?");
    if container_holds_attachments(plan.container) {
        push(&mut a, "-map");
        push(&mut a, "0:t?");
    }
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

            // The filter chain. Order and combination matter — see the
            // "scaling happens in the encoded domain" note above.
            let mut filters: Vec<String> = Vec::new();
            if let Some((w, h)) = scale {
                filters.push(format!("scale={w}:{h}"));
            }
            // A wide-gamut SDR source needs its primaries brought to BT.709,
            // and the tone-map chain is not running to do it. Only ever one of
            // the two: `sdr_gamut_conversion` returns `None` for anything that
            // is not SDR, so a tone-mapped source cannot also land here and be
            // converted twice.
            if tone_map.is_none() {
                if let Some(g) = gamut {
                    filters.push(g.to_string());
                }
            }
            match tone_map {
                Some(tm) => {
                    // The tone-map chain ends in `format=yuv420p`, so a
                    // separate `-pix_fmt` would be redundant; it is emitted
                    // anyway because it is the ENCODER's guarantee rather than
                    // the filter graph's, and a filter chain that changed
                    // shape must not be able to silently change the output
                    // pixel format.
                    filters.push(tm.filter_chain.clone());
                }
                None => {}
            }
            if !filters.is_empty() {
                push(&mut a, "-vf");
                a.push(filters.join(","));
            }
            push(&mut a, "-pix_fmt");
            push(&mut a, "yuv420p");

            push(&mut a, "-maxrate");
            a.push(rendition.max_video_bitrate_bps.to_string());
            push(&mut a, "-bufsize");
            a.push(rendition.max_video_bitrate_bps.saturating_mul(2).to_string());
        }
    }

    match plan.audio {
        crate::foundry::plan::AudioAction::Copy => {
            push(&mut a, "-c:a");
            push(&mut a, "copy");
        }
        crate::foundry::plan::AudioAction::Encode { .. } => {
            push(&mut a, "-c:a");
            push(&mut a, "aac");
            push(&mut a, "-ac");
            a.push(rendition.audio.max_channels().to_string());
            if let Some(bps) = rendition.audio.encode_bitrate_bps() {
                push(&mut a, "-b:a");
                a.push(bps.to_string());
            }
        }
    }

    // Subtitles are never dropped and never burned in. The codec depends on
    // the container: MP4 cannot copy `subrip`, and bitmap subtitles never
    // reach an MP4 rung at all (the planner refuses them).
    push(&mut a, "-c:s");
    match plan.container {
        Container::Mp4 => push(&mut a, "mov_text"),
        _ => push(&mut a, "copy"),
    }

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
    use crate::foundry::hdr::ToneMapAlgorithm;
    use crate::foundry::probe::{
        AudioStream, StreamSideData, SubtitleStream, VideoStream,
    };

    const SRC: &str = "/srv/media/Movies/Dune (2021)/Dune (2021).mkv";

    fn vid(codec: &str, w: u32, h: u32, bitrate: u64) -> VideoStream {
        VideoStream {
            index: 0,
            codec: codec.into(),
            width: Some(w),
            height: Some(h),
            bitrate_bps: Some(bitrate),
            pix_fmt: Some("yuv420p".into()),
            ..VideoStream::default()
        }
    }

    fn aud(index: u32, codec: &str, channels: u32) -> AudioStream {
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
            duration_secs: Some(9000.0),
            format_bitrate_bps: Some(20_000_000),
            size_bytes: Some(20_000_000_000),
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

    /// An ordinary 1080p SDR H.264 release with stereo AAC.
    fn ordinary_1080p() -> MediaProbe {
        probe(
            vec![vid("h264", 1920, 1080, 8_000_000)],
            vec![aud(1, "aac", 2)],
        )
    }

    /// A 4K HDR10 HEVC release with a TrueHD track — the hifi case.
    fn uhd_hdr10() -> MediaProbe {
        let mut v = vid("hevc", 3840, 2160, 60_000_000);
        v.pix_fmt = Some("yuv420p10le".into());
        v.color_transfer = Some("smpte2084".into());
        v.color_primaries = Some("bt2020".into());
        probe(vec![v], vec![aud(1, "truehd", 8)])
    }

    fn dv_profile(profile: u32, compat: u32) -> MediaProbe {
        let mut p = uhd_hdr10();
        p.video[0].side_data = vec![StreamSideData {
            kind: "DOVI configuration record".into(),
            dv_profile: Some(profile),
            dv_bl_signal_compatibility_id: Some(compat),
            rpu_present: Some(true),
            bl_present: Some(true),
            el_present: Some(false),
            ..StreamSideData::default()
        }];
        p
    }

    fn ctx_available() -> LadderContext {
        LadderContext {
            tone_map_support: ToneMapSupport::Available,
        }
    }

    fn rung(p: &MediaProbe, name: RenditionName, ctx: &LadderContext) -> RenditionOutcome {
        plan_rung(p, &Ladder::default(), name, SRC, ctx).outcome
    }

    // --- the ladder as a whole ---------------------------------------------

    #[test]
    fn every_requested_rung_gets_an_entry_even_when_it_will_not_be_produced() {
        // A rung that vanishes from the output is indistinguishable from one
        // nobody asked for. The operator marked it; they get an answer.
        let req = RenditionRequest::new(SRC, RenditionName::all().to_vec());
        let d = plan_ladder(&ordinary_1080p(), &Ladder::default(), &req, &ctx_available());
        assert_eq!(d.len(), 4);
        let names: Vec<RenditionName> = d.iter().map(|x| x.rendition).collect();
        assert_eq!(names, RenditionName::all().to_vec());
        for x in &d {
            assert!(
                !matches!(&x.outcome, RenditionOutcome::Encode { .. })
                    || x.output_path.is_some(),
                "an encode must know where it goes: {x:?}"
            );
        }
    }

    #[test]
    fn only_the_requested_rungs_are_planned() {
        // The scope rule, at the API boundary: asking for `mobile` must never
        // produce the other three.
        let req = RenditionRequest::new(SRC, vec![RenditionName::Mobile]);
        let d = plan_ladder(&ordinary_1080p(), &Ladder::default(), &req, &ctx_available());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].rendition, RenditionName::Mobile);
    }

    #[test]
    fn a_repeated_rung_in_the_request_is_planned_once() {
        let req = RenditionRequest::new(
            SRC,
            vec![RenditionName::Tv, RenditionName::Tv, RenditionName::Tv],
        );
        let d = plan_ladder(&ordinary_1080p(), &Ladder::default(), &req, &ctx_available());
        assert_eq!(d.len(), 1);
    }

    // --- pointless rungs ---------------------------------------------------

    #[test]
    fn a_rung_the_source_already_satisfies_is_skipped_with_a_reason_not_emitted() {
        // A 1080p H.264 source already IS the tv rung. Producing it would be
        // a re-encode of a file into itself, losing a generation for nothing.
        let out = rung(&ordinary_1080p(), RenditionName::Tv, &ctx_available());
        assert_eq!(
            out,
            RenditionOutcome::Skip {
                why: RenditionSkip::SourceAlreadySatisfiesRung
            },
            "got {out:?}"
        );
    }

    #[test]
    fn a_rendition_is_never_upscaled() {
        // The mobile rung on a 360p source: the ceiling is at or above the
        // source, so there is nothing to do and no pixels are invented.
        let small = probe(
            vec![vid("h264", 640, 360, 800_000)],
            vec![aud(1, "aac", 2)],
        );
        let out = rung(&small, RenditionName::Tv, &ctx_available());
        assert!(
            matches!(out, RenditionOutcome::Skip { .. }),
            "a rung above the source resolution must not encode: {out:?}"
        );

        // And where an encode IS ordered, the emitted plan never enlarges.
        let sd = probe(
            vec![vid("mpeg4", 720, 480, 1_500_000)],
            vec![aud(1, "aac", 2)],
        );
        for name in [RenditionName::Web, RenditionName::Tv] {
            if let RenditionOutcome::Encode {
                output_resolution, ..
            } = rung(&sd, name, &ctx_available())
            {
                assert!(
                    output_resolution.0 <= 720 && output_resolution.1 <= 480,
                    "{name:?} would upscale to {output_resolution:?}"
                );
            }
        }
    }

    #[test]
    fn the_upscale_guard_refuses_a_plan_that_would_enlarge_the_picture() {
        // Tested directly, because the planner cannot currently produce such a
        // plan — `scale_to_fit` never upscales — and a guard no test can reach
        // through the normal path is a guard nobody can prove works.
        let src = (1920u32, 1080u32);
        assert_eq!(
            upscale_refusal(
                src,
                VideoAction::Encode {
                    scale: Some((3840, 2160))
                }
            ),
            Some(RenditionRefusal::WouldUpscale {
                source: src,
                target: (3840, 2160)
            })
        );
        // Either dimension growing is enough — an anamorphic-shaped plan that
        // widened without heightening would still be inventing pixels.
        assert!(upscale_refusal(src, VideoAction::Encode { scale: Some((2560, 1080)) }).is_some());
        assert!(upscale_refusal(src, VideoAction::Encode { scale: Some((1920, 1440)) }).is_some());

        // A downscale, an exact match, and a copy are all fine.
        assert_eq!(
            upscale_refusal(src, VideoAction::Encode { scale: Some((1280, 720)) }),
            None
        );
        assert_eq!(
            upscale_refusal(src, VideoAction::Encode { scale: Some((1920, 1080)) }),
            None
        );
        assert_eq!(upscale_refusal(src, VideoAction::Encode { scale: None }), None);
        assert_eq!(upscale_refusal(src, VideoAction::Copy), None);
    }

    #[test]
    fn the_resolved_output_resolution_follows_the_plans_scale() {
        assert_eq!(
            resolved_output_resolution((3840, 2160), VideoAction::Encode { scale: Some((1280, 720)) }),
            (1280, 720)
        );
        assert_eq!(
            resolved_output_resolution((1920, 1080), VideoAction::Encode { scale: None }),
            (1920, 1080),
            "no scale filter means the source size is what comes out"
        );
        assert_eq!(
            resolved_output_resolution((1920, 1080), VideoAction::Copy),
            (1920, 1080)
        );
    }

    #[test]
    fn two_rungs_that_resolve_to_the_same_size_collapse_to_the_better_one() {
        // A 480p mpeg4 source: web (720p ceiling) and tv (1080p ceiling) both
        // leave it at 480p and differ only in CRF — two near-identical files
        // where the operator wanted a ladder.
        let sd = probe(
            vec![vid("mpeg4", 720, 480, 1_500_000)],
            vec![aud(1, "aac", 2)],
        );
        let req = RenditionRequest::new(SRC, vec![RenditionName::Web, RenditionName::Tv]);
        let d = plan_ladder(&sd, &Ladder::default(), &req, &ctx_available());

        let web = &d.iter().find(|x| x.rendition == RenditionName::Web).unwrap().outcome;
        let tv = &d.iter().find(|x| x.rendition == RenditionName::Tv).unwrap().outcome;
        assert!(tv.is_encode(), "the better rung survives: {tv:?}");
        assert!(
            matches!(
                web,
                RenditionOutcome::Skip {
                    why: RenditionSkip::DuplicatesRung {
                        superseded_by: RenditionName::Tv,
                        ..
                    }
                }
            ),
            "and the duplicate names its replacement rather than vanishing: {web:?}"
        );
    }

    #[test]
    fn dedupe_never_collapses_an_encoding_rung_into_the_hifi_rung() {
        // They can share a resolution while being completely different
        // artefacts: hifi keeps HEVC and TrueHD as they are, tv guarantees
        // H.264 with tone-mapped SDR. Deduping them against each other would
        // delete a rung's whole reason for existing.
        //
        // The setup has to make BOTH rungs produce an encode at the same size,
        // or the rule is not exercised at all: a 1080p HDR HEVC/TrueHD title in
        // an MP4 container, which the tv rung re-encodes to 1080p H.264 and the
        // hifi rung merely remuxes to Matroska. An earlier version of this test
        // used a source the hifi rung reported `AlreadyOptimal` for, so hifi
        // never entered the dedupe at all and the test proved nothing.
        let mut p = uhd_hdr10();
        p.video[0].width = Some(1920);
        p.video[0].height = Some(1080);
        p.container = "mov,mp4,m4a,3gp,3g2,mj2".into();
        p.audio = vec![aud(1, "truehd", 8)];

        let req = RenditionRequest::new(SRC, vec![RenditionName::Tv, RenditionName::HiFi]);
        let d = plan_ladder(&p, &Ladder::default(), &req, &ctx_available());

        let tv = &d.iter().find(|x| x.rendition == RenditionName::Tv).unwrap().outcome;
        let hifi = &d.iter().find(|x| x.rendition == RenditionName::HiFi).unwrap().outcome;

        // Precondition: both really are encodes at the same resolution, which
        // is what makes the dedupe rule applicable in the first place.
        let (RenditionOutcome::Encode { output_resolution: tv_res, .. }, RenditionOutcome::Encode { output_resolution: hifi_res, .. }) =
            (tv, hifi)
        else {
            panic!("both rungs must produce an encode for this test to mean anything: tv={tv:?} hifi={hifi:?}");
        };
        assert_eq!(
            tv_res, hifi_res,
            "precondition: the two rungs must collide on resolution"
        );

        // ...and neither is collapsed into the other.
        for x in &d {
            assert!(
                !matches!(
                    x.outcome,
                    RenditionOutcome::Skip {
                        why: RenditionSkip::DuplicatesRung { .. }
                    }
                ),
                "{x:?}"
            );
        }
    }

    #[test]
    fn dedupe_can_be_switched_off() {
        let sd = probe(
            vec![vid("mpeg4", 720, 480, 1_500_000)],
            vec![aud(1, "aac", 2)],
        );
        let ladder = Ladder {
            dedupe_by_resolution: false,
            ..Ladder::default()
        };
        let req = RenditionRequest::new(SRC, vec![RenditionName::Web, RenditionName::Tv]);
        let d = plan_ladder(&sd, &ladder, &req, &ctx_available());
        assert!(d.iter().all(|x| x.outcome.is_encode()), "{d:?}");
    }

    // --- the hifi rung -----------------------------------------------------

    #[test]
    fn the_hifi_rung_refuses_rather_than_re_encoding_hdr_or_object_audio() {
        // The defining property. A 4K HDR10 HEVC source with TrueHD sits
        // outside the Matroska container's needs, so nothing is ordered — but
        // if anything WERE ordered that required an encode, the rung refuses.
        let mut p = uhd_hdr10();
        // Force work by putting it in a container the rung does not accept,
        // with a video codec the rung does not accept either.
        p.container = "avi".into();
        p.video[0].codec = "prores".into();
        let out = rung(&p, RenditionName::HiFi, &ctx_available());
        assert!(
            matches!(
                out,
                RenditionOutcome::Refused {
                    why: RenditionRefusal::HifiWouldRequireReEncoding { video: true, .. }
                }
            ),
            "got {out:?}"
        );
    }

    #[test]
    fn the_hifi_rung_never_tone_maps_and_never_downmixes() {
        // Stated as a property of every possible hifi outcome on the content
        // the rung exists for.
        //
        // The fixtures are in AVI deliberately. Opus caught this test at the
        // FOUNDRY-03 gate: with the sources left in Matroska every case came
        // back `Skip { AlreadyOptimal }`, the `if let Encode` body never ran,
        // and the test passed while asserting nothing. It would have kept
        // passing if hifi tone-mapped and downmixed on every encode. Forcing a
        // container change gives the rung actual work to plan, so the
        // assertions execute — and `encodes` below fails the test outright if
        // that ever stops being true.
        let mut encodes = 0;
        for mut p in [uhd_hdr10(), dv_profile(8, 1), dv_profile(5, 0)] {
            p.container = "avi".into();
            let out = rung(&p, RenditionName::HiFi, &ctx_available());
            if let RenditionOutcome::Encode {
                tone_map, plan, args, ..
            } = &out
            {
                encodes += 1;
                assert!(tone_map.is_none(), "hifi must never tone-map: {out:?}");
                assert!(plan.is_remux_only(), "hifi must never re-encode: {out:?}");
                assert!(
                    args.windows(2).any(|w| w[0] == "-c:a" && w[1] == "copy"),
                    "hifi must never downmix: {args:?}"
                );
                assert!(!args.iter().any(|s| s == "-vf"), "{args:?}");
            }
        }
        assert!(
            encodes > 0,
            "no fixture produced a hifi encode, so every assertion above was \
             skipped and this test proved nothing"
        );
    }

    /// Codex, at the FOUNDRY-03 gate: hifi's "never re-encodes" guarantee was
    /// not structural. `Rendition`'s fields are public, so a caller can build
    /// one that encodes while declaring `Preserve`. The old check was a
    /// `debug_assert_eq!`, which is compiled OUT of release builds — so the
    /// malformed config panicked in tests and, in production, quietly encoded
    /// HDR pixels into an SDR rendition with no tone map.
    ///
    /// It is now a refusal, which behaves the same in both profiles.
    #[test]
    fn an_hdr_encode_that_declares_preserve_is_refused_not_asserted() {
        let mut ladder = Ladder::default();
        // A rung that encodes (web's treatment) but claims Preserve.
        ladder.web.dynamic_range = DynamicRangeTreatment::Preserve;
        let out = plan_rung(&uhd_hdr10(), &ladder, RenditionName::Web, SRC, &ctx_available()).outcome;
        assert!(
            matches!(
                out,
                RenditionOutcome::Refused {
                    why: RenditionRefusal::HdrEncodeUnderPreserve { .. }
                }
            ),
            "expected a refusal, got {out:?}"
        );
    }

    #[test]
    fn the_hifi_rung_does_a_remux_when_that_is_all_that_is_needed() {
        // A 4K HDR10 file in an AVI container: nothing about the streams is
        // wrong for this rung, so it is a lossless container change.
        let mut p = uhd_hdr10();
        p.container = "avi".into();
        let out = rung(&p, RenditionName::HiFi, &ctx_available());
        let RenditionOutcome::Encode { plan, tone_map, .. } = &out else {
            panic!("expected a remux, got {out:?}");
        };
        assert!(plan.is_remux_only());
        assert_eq!(*tone_map, None);
        assert_eq!(plan.container, Container::Matroska);
    }

    #[test]
    fn the_hifi_rung_is_skipped_for_ordinary_content_rather_than_duplicating_it() {
        // Path B never removes the source, so for a 1080p SDR AAC title the
        // source already IS the best copy and a hifi rendition is pure waste.
        let mut p = ordinary_1080p();
        p.container = "avi".into(); // force work, so the skip is not merely AlreadyOptimal
        let out = rung(&p, RenditionName::HiFi, &ctx_available());
        assert_eq!(
            out,
            RenditionOutcome::Skip {
                why: RenditionSkip::NothingToPreserve
            },
            "got {out:?}"
        );
    }

    #[test]
    fn what_counts_as_worth_preserving_is_uhd_hdr_dv_or_lossless_audio() {
        let v = |p: &MediaProbe| hifi_has_anything_to_preserve(p, p.primary_video().unwrap());
        assert!(!v(&ordinary_1080p()));
        assert!(v(&uhd_hdr10()));

        // 1080p SDR, but with a lossless track.
        let lossless = probe(
            vec![vid("h264", 1920, 1080, 8_000_000)],
            vec![aud(1, "dts", 6)],
        );
        assert!(v(&lossless));

        // 1080p SDR AAC, but 7.1 — more channels than any encoding rung keeps.
        let seven_one = probe(
            vec![vid("h264", 1920, 1080, 8_000_000)],
            vec![aud(1, "aac", 8)],
        );
        assert!(v(&seven_one));
    }

    // --- Dolby Vision ------------------------------------------------------

    #[test]
    fn profile_5_is_refused_on_every_encoding_rung() {
        // THE case. Re-encoded without its RPU, profile 5 renders green and
        // purple. Every rung that would re-encode must refuse, and say why.
        let p = dv_profile(5, 0);
        for name in [RenditionName::Mobile, RenditionName::Web, RenditionName::Tv] {
            let out = rung(&p, name, &ctx_available());
            let RenditionOutcome::Refused {
                why: RenditionRefusal::DolbyVisionCannotBeTranscoded { verdict },
            } = &out
            else {
                panic!("{name:?} must refuse profile 5, got {out:?}");
            };
            assert!(verdict.contains("green"), "the reason must be stated: {verdict}");
        }
    }

    #[test]
    fn profile_5_is_refused_even_when_the_tone_mapper_is_unavailable() {
        // Ordering: the Dolby Vision refusal must not be reachable only by way
        // of the tone-map branch, or a host without zscale would report the
        // wrong reason for the right refusal.
        let p = dv_profile(5, 0);
        for support in [
            ToneMapSupport::Unverified,
            ToneMapSupport::Unavailable,
            ToneMapSupport::Available,
        ] {
            let out = rung(
                &p,
                RenditionName::Web,
                &LadderContext {
                    tone_map_support: support,
                },
            );
            assert!(
                matches!(
                    out,
                    RenditionOutcome::Refused {
                        why: RenditionRefusal::DolbyVisionCannotBeTranscoded { .. }
                    }
                ),
                "{support:?}: {out:?}"
            );
        }
    }

    #[test]
    fn a_dv_signal_with_no_usable_profile_is_refused_just_as_hard_as_profile_5() {
        let mut p = uhd_hdr10();
        p.video[0].codec_tag = Some("dvhe".into());
        let out = rung(&p, RenditionName::Web, &ctx_available());
        assert!(
            matches!(
                out,
                RenditionOutcome::Refused {
                    why: RenditionRefusal::DolbyVisionCannotBeTranscoded { .. }
                }
            ),
            "got {out:?}"
        );
    }

    #[test]
    fn profile_8_is_transcoded_from_its_hdr10_base_layer_with_a_tone_map() {
        // The one DV shape that CAN be rendered: the base layer is valid
        // HDR10, so the rung tone-maps it exactly as it would any HDR10 file.
        let p = dv_profile(8, 1);
        let out = rung(&p, RenditionName::Web, &ctx_available());
        let RenditionOutcome::Encode { tone_map, .. } = &out else {
            panic!("expected an encode, got {out:?}");
        };
        let tm = tone_map.as_ref().expect("an HDR10 base layer must be tone-mapped");
        assert_eq!(tm.source_transfer, HdrTransfer::Pq);
    }

    #[test]
    fn a_profile_8_source_with_an_sdr_base_layer_is_not_tone_mapped() {
        // Compatibility id 2 means the base layer is already BT.709. Tone-
        // mapping it would apply a curve to SDR content — the mirror failure,
        // and just as visible.
        let p = dv_profile(8, 2);
        let out = rung(&p, RenditionName::Web, &ctx_available());
        let RenditionOutcome::Encode { tone_map, .. } = &out else {
            panic!("expected an encode, got {out:?}");
        };
        assert_eq!(*tone_map, None, "an SDR base layer must not be tone-mapped");
    }

    #[test]
    fn the_hifi_rung_will_not_remux_dolby_vision_into_a_different_container() {
        // Whether the configuration record survives depends on the ffmpeg
        // build. The rung whose job is preserving DV does not gamble with it.
        let mut p = dv_profile(8, 1);
        p.container = "mov,mp4,m4a,3gp,3g2,mj2".into();
        let out = rung(&p, RenditionName::HiFi, &ctx_available());
        assert!(
            matches!(
                out,
                RenditionOutcome::Refused {
                    why: RenditionRefusal::DolbyVisionRemuxNotAttempted { .. }
                }
            ),
            "got {out:?}"
        );
    }

    // --- tone-mapping ------------------------------------------------------

    #[test]
    fn an_hdr_source_tone_maps_and_records_exactly_how() {
        // The whole point of recording it: a washed-out result must name the
        // curve, the peak luminance and the desaturation that produced it.
        let out = rung(&uhd_hdr10(), RenditionName::Tv, &ctx_available());
        let RenditionOutcome::Encode {
            tone_map, args, ..
        } = &out
        else {
            panic!("expected an encode, got {out:?}");
        };
        let tm = tone_map.as_ref().expect("HDR must be tone-mapped for an SDR rung");
        assert_eq!(tm.source_transfer, HdrTransfer::Pq);
        assert_eq!(tm.algorithm, ToneMapAlgorithm::Hable);
        assert_eq!(tm.peak_nits, 100);
        assert_eq!(tm.desat, 0);
        // ...and the chain it describes is the chain that is actually run.
        let vf = args
            .windows(2)
            .find(|w| w[0] == "-vf")
            .map(|w| w[1].clone())
            .expect("a tone-mapped rung must carry a filter chain");
        assert!(vf.contains(&tm.filter_chain), "recorded chain not in argv: {vf}");

        // ...and the argv itself carries `desat=0`. Asserting `tm.desat == 0`
        // above only proves the STRUCT says zero: if the filter chain were
        // built without the parameter, ffmpeg would silently apply its default
        // of 2 and every tone-mapped rendition would come out washed out,
        // while both assertions above still passed. Flagged by codex at the
        // FOUNDRY-03 gate as a test that could not fail.
        assert!(
            vf.contains("desat=0"),
            "the tone-map chain actually passed to ffmpeg must set desat=0, \
             otherwise ffmpeg's default of 2 washes the picture out: {vf}"
        );
        assert!(
            !vf.contains("desat=2"),
            "ffmpeg's washed-out default must never be emitted: {vf}"
        );
    }

    /// Raised independently by codex and opus at the FOUNDRY-03 gate.
    ///
    /// `bt2020-10`/`bt2020-12` are SDR TRANSFER curves, so `classify_hdr`
    /// correctly calls them SDR and no tone map is applied. But the transfer
    /// is not the gamut: such a stream still carries BT.2020 PRIMARIES. The
    /// encoder was emitting those pixels untouched and tagging nothing, so a
    /// BT.709 client interprets wide-gamut values as if they were BT.709 and
    /// the picture comes out visibly wrong.
    ///
    /// The HDR path never had this bug — its tone-map chain already contains
    /// `zscale=primaries=bt709`. It was only the SDR path, which skipped the
    /// filter entirely, that shipped the source gamut through unconverted.
    #[test]
    fn a_wide_gamut_sdr_source_is_converted_to_bt709() {
        let mut v = vid("hevc", 3840, 2160, 60_000_000);
        v.pix_fmt = Some("yuv420p10le".into());
        // An SDR transfer curve...
        v.color_transfer = Some("bt2020-10".into());
        // ...on a wide-gamut container.
        v.color_primaries = Some("bt2020".into());
        let p = probe(vec![v], vec![aud(1, "aac", 6)]);

        // Sanity: this really is classified SDR, so no tone map runs. If that
        // ever changes this test is testing the wrong path and should fail
        // loudly here rather than pass for the wrong reason.
        assert_eq!(
            crate::foundry::hdr::classify_hdr(&p.video[0]),
            HdrVerdict::Sdr,
            "fixture must exercise the SDR path"
        );

        let out = rung(&p, RenditionName::Web, &ctx_available());
        let RenditionOutcome::Encode { tone_map, args, .. } = &out else {
            panic!("expected an encode, got {out:?}");
        };
        assert!(tone_map.is_none(), "SDR must not be tone-mapped");

        let vf = args
            .windows(2)
            .find(|w| w[0] == "-vf")
            .map(|w| w[1].clone())
            .expect("a wide-gamut source must carry a filter chain");
        assert!(
            vf.contains("primaries=bt709"),
            "a BT.2020 SDR source must be converted to BT.709, or clients \
             render it oversaturated: {vf}"
        );
    }

    /// The control: an ordinary BT.709 source must NOT get a gamut filter.
    /// Converting BT.709 to BT.709 is wasted work, and a rule that fires on
    /// everything would pass the test above while meaning nothing.
    #[test]
    fn an_ordinary_bt709_source_gets_no_gamut_conversion() {
        let mut v = vid("hevc", 3840, 2160, 60_000_000);
        v.color_transfer = Some("bt709".into());
        v.color_primaries = Some("bt709".into());
        let p = probe(vec![v], vec![aud(1, "aac", 6)]);
        let out = rung(&p, RenditionName::Web, &ctx_available());
        let RenditionOutcome::Encode { args, .. } = &out else {
            panic!("expected an encode, got {out:?}");
        };
        let vf = args
            .windows(2)
            .find(|w| w[0] == "-vf")
            .map(|w| w[1].clone())
            .unwrap_or_default();
        assert!(
            !vf.contains("primaries="),
            "a BT.709 source needs no gamut conversion: {vf}"
        );
    }

    #[test]
    fn an_sdr_source_is_never_tone_mapped() {
        // The mirror failure: applying an HDR tone curve to SDR content is as
        // visible as failing to apply one to HDR.
        let sd = probe(
            vec![vid("mpeg4", 1920, 1080, 8_000_000)],
            vec![aud(1, "aac", 2)],
        );
        let out = rung(&sd, RenditionName::Tv, &ctx_available());
        let RenditionOutcome::Encode { tone_map, args, .. } = &out else {
            panic!("expected an encode, got {out:?}");
        };
        assert_eq!(*tone_map, None);
        assert!(!args.iter().any(|s| s.contains("tonemap")), "{args:?}");
    }

    #[test]
    fn an_unverified_tone_mapper_makes_an_hdr_rung_undecidable_rather_than_hopeful() {
        // zscale needs libzimg. Encoding without the filter produces exactly
        // the washed-out output the filter exists to prevent, so "we have not
        // checked" must not proceed.
        let out = rung(
            &uhd_hdr10(),
            RenditionName::Tv,
            &LadderContext::default(),
        );
        assert!(
            matches!(
                out,
                RenditionOutcome::CannotDecide {
                    why: LadderUndecidable::ToneMapperUnverified { .. }
                }
            ),
            "got {out:?}"
        );
        assert_eq!(LadderContext::default().tone_map_support, ToneMapSupport::Unverified);
    }

    #[test]
    fn a_missing_tone_mapper_refuses_rather_than_encoding_without_the_filter() {
        let out = rung(
            &uhd_hdr10(),
            RenditionName::Tv,
            &LadderContext {
                tone_map_support: ToneMapSupport::Unavailable,
            },
        );
        assert!(
            matches!(
                out,
                RenditionOutcome::Refused {
                    why: RenditionRefusal::ToneMapperUnavailable { .. }
                }
            ),
            "got {out:?}"
        );
    }

    #[test]
    fn an_undetermined_dynamic_range_makes_the_rung_undecidable() {
        // 10-bit with no transfer tag: tone-mapping and passing through are
        // both visibly wrong, so neither is chosen.
        let mut p = probe(
            vec![vid("hevc", 1920, 1080, 10_000_000)],
            vec![aud(1, "aac", 2)],
        );
        p.video[0].pix_fmt = Some("yuv420p10le".into());
        let out = rung(&p, RenditionName::Tv, &ctx_available());
        assert!(
            matches!(
                out,
                RenditionOutcome::CannotDecide {
                    why: LadderUndecidable::UnknownDynamicRange { .. }
                }
            ),
            "got {out:?}"
        );
    }

    #[test]
    fn the_scale_runs_before_the_tone_map_chain_in_the_filter_string() {
        // Documented as a deliberate cost/correctness trade in
        // `build_rendition_args`. Pinned so it cannot change silently.
        let out = rung(&uhd_hdr10(), RenditionName::Web, &ctx_available());
        let RenditionOutcome::Encode { args, .. } = &out else {
            panic!("got {out:?}");
        };
        let vf = args.windows(2).find(|w| w[0] == "-vf").unwrap()[1].clone();
        let scale = vf.find("scale=1280:720").expect("must scale");
        let tm = vf.find("zscale=transfer=linear").expect("must tone-map");
        assert!(scale < tm, "scale must precede the tone-map chain: {vf}");
    }

    // --- undecidables are inherited, not re-derived -------------------------

    #[test]
    fn the_shared_planners_undecidables_apply_unchanged_to_every_rung() {
        // The reuse this module is built on. A file the single-target planner
        // will not judge is not one a rendition can be planned from either.
        let mut p = ordinary_1080p();
        p.duration_secs = None;
        for name in RenditionName::all() {
            let out = rung(&p, name, &ctx_available());
            assert!(
                matches!(
                    out,
                    RenditionOutcome::CannotDecide {
                        why: LadderUndecidable::Source {
                            why: Undecidable::UnknownDuration
                        }
                    }
                ),
                "{name:?}: {out:?}"
            );
        }
    }

    #[test]
    fn an_undecidable_source_wins_over_every_rung_level_rule() {
        // Ordering: a rung-specific refusal computed from a partially
        // understood file would be a judgement on facts we do not have. Even
        // a profile 5 Dolby Vision file reports the SOURCE problem first.
        let mut p = dv_profile(5, 0);
        p.unindexed_stream_count = 1;
        let out = rung(&p, RenditionName::Web, &ctx_available());
        assert!(
            matches!(
                out,
                RenditionOutcome::CannotDecide {
                    why: LadderUndecidable::Source {
                        why: Undecidable::UnindexedStreams { .. }
                    }
                }
            ),
            "got {out:?}"
        );
    }

    #[test]
    fn a_source_inside_a_versions_folder_is_undecidable_for_every_rung() {
        let ladder = Ladder::default();
        let src = "/srv/media/Movies/Dune (2021)/Muse Versions/Mobile/Dune (2021).mp4";
        for name in RenditionName::all() {
            let d = plan_rung(&ordinary_1080p(), &ladder, name, src, &ctx_available());
            assert_eq!(d.output_path, None, "no path may be offered: {d:?}");
            assert!(
                matches!(
                    d.outcome,
                    RenditionOutcome::CannotDecide {
                        why: LadderUndecidable::OutputPathNotModelled { .. }
                    }
                ),
                "{name:?}: {d:?}"
            );
        }
    }

    // --- container-specific subtitle rules -----------------------------------

    #[test]
    fn bitmap_subtitles_are_refused_by_the_mp4_rungs_and_carried_by_the_mkv_one() {
        // MP4 cannot hold PGS in any form: `-c:s copy` fails the mux and the
        // alternative is silently losing the track.
        let mut p = probe(
            vec![vid("hevc", 1920, 1080, 10_000_000)],
            vec![aud(1, "aac", 2)],
        );
        p.subtitles = vec![SubtitleStream {
            index: 2,
            codec: "hdmv_pgs_subtitle".into(),
            language: Some("eng".into()),
            forced: false,
            default: true,
        }];

        for name in [RenditionName::Mobile, RenditionName::Web] {
            let out = rung(&p, name, &ctx_available());
            assert!(
                matches!(
                    out,
                    RenditionOutcome::Refused {
                        why: RenditionRefusal::BitmapSubtitlesCannotEnterMp4 { .. }
                    }
                ),
                "{name:?}: {out:?}"
            );
        }
        // The Matroska rung carries them.
        let out = rung(&p, RenditionName::Tv, &ctx_available());
        assert!(out.is_encode(), "got {out:?}");
    }

    #[test]
    fn text_subtitles_are_converted_for_mp4_and_copied_for_matroska() {
        // `-c:s copy` of subrip into MP4 fails outright — MP4 needs mov_text.
        // Getting this wrong is an encode that dies after doing all the work.
        let mut p = probe(
            vec![vid("hevc", 1920, 1080, 10_000_000)],
            vec![aud(1, "aac", 2)],
        );
        p.subtitles = vec![SubtitleStream {
            index: 2,
            codec: "subrip".into(),
            language: Some("eng".into()),
            forced: false,
            default: true,
        }];

        let RenditionOutcome::Encode { args, .. } = rung(&p, RenditionName::Web, &ctx_available())
        else {
            panic!("expected an encode");
        };
        assert!(
            args.windows(2).any(|w| w[0] == "-c:s" && w[1] == "mov_text"),
            "{args:?}"
        );

        let RenditionOutcome::Encode { args, .. } = rung(&p, RenditionName::Tv, &ctx_available())
        else {
            panic!("expected an encode");
        };
        assert!(
            args.windows(2).any(|w| w[0] == "-c:s" && w[1] == "copy"),
            "{args:?}"
        );
    }

    // --- argv ----------------------------------------------------------------

    #[test]
    fn the_rendition_argv_keeps_every_safety_flag_the_single_target_builder_has() {
        // The two builders may differ on filters, subtitles and audio bitrate.
        // They must NOT differ on the flags that stop a background worker
        // wedging, or on stream mapping.
        let ladder = Ladder::default();
        let out = rung(&uhd_hdr10(), RenditionName::Tv, &ctx_available());
        let RenditionOutcome::Encode { plan, args, .. } = &out else {
            panic!("got {out:?}");
        };
        let shared = crate::foundry::plan::build_transcode_args(
            plan,
            &ladder.tv.as_policy(),
            "/in.mkv",
            "/out.mkv",
        );
        for flag in ["-hide_banner", "-nostdin", "-y"] {
            assert!(args.contains(&flag.to_string()), "{flag} missing: {args:?}");
            assert!(shared.contains(&flag.to_string()), "precondition");
        }
        for pair in [("-map_metadata", "0"), ("-map_chapters", "0")] {
            assert!(
                args.windows(2).any(|w| w[0] == pair.0 && w[1] == pair.1),
                "{pair:?} missing: {args:?}"
            );
        }
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:a?"));
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:s?"));
    }

    #[test]
    fn the_argv_maps_the_video_stream_by_absolute_index_not_by_v0() {
        // Same reason as the single-target builder: the probe filtered cover
        // art out, so ffmpeg's own `v:0` may be the poster.
        let mut p = uhd_hdr10();
        p.video[0].index = 3;
        let out = rung(&p, RenditionName::Tv, &ctx_available());
        let RenditionOutcome::Encode { args, .. } = &out else {
            panic!("got {out:?}");
        };
        assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0:3"), "{args:?}");
        assert!(!args.iter().any(|s| s == "0:v:0"));
    }

    #[test]
    fn the_argv_follows_the_rung_rather_than_hardcoding_one_set_of_settings() {
        // If the builder ignored the rung, the ladder would be four copies of
        // one file — the exact failure the whole item exists to avoid.
        let sd = probe(
            vec![vid("mpeg4", 1920, 1080, 20_000_000)],
            vec![aud(1, "truehd", 8)],
        );
        let mobile = match rung(&sd, RenditionName::Mobile, &ctx_available()) {
            RenditionOutcome::Encode { args, .. } => args,
            o => panic!("{o:?}"),
        };
        let tv = match rung(&sd, RenditionName::Tv, &ctx_available()) {
            RenditionOutcome::Encode { args, .. } => args,
            o => panic!("{o:?}"),
        };

        assert!(mobile.windows(2).any(|w| w[0] == "-crf" && w[1] == "26"), "{mobile:?}");
        assert!(mobile.windows(2).any(|w| w[0] == "-preset" && w[1] == "veryfast"));
        assert!(mobile.windows(2).any(|w| w[0] == "-vf" && w[1] == "scale=640:360"));
        assert!(mobile.windows(2).any(|w| w[0] == "-maxrate" && w[1] == "1200000"));
        assert!(mobile.windows(2).any(|w| w[0] == "-ac" && w[1] == "2"));
        assert!(mobile.windows(2).any(|w| w[0] == "-b:a" && w[1] == "160000"));

        assert!(tv.windows(2).any(|w| w[0] == "-crf" && w[1] == "20"), "{tv:?}");
        assert!(tv.windows(2).any(|w| w[0] == "-maxrate" && w[1] == "8000000"));
        assert!(tv.windows(2).any(|w| w[0] == "-bufsize" && w[1] == "16000000"));
        assert!(
            tv.windows(2).any(|w| w[0] == "-ac" && w[1] == "6"),
            "the tv rung keeps 5.1 rather than downmixing: {tv:?}"
        );
        assert!(tv.windows(2).any(|w| w[0] == "-b:a" && w[1] == "384000"));
    }

    #[test]
    fn the_argv_puts_the_output_last_and_the_input_after_dash_i() {
        // ffmpeg is positional: an output that is not last silently means
        // something else entirely.
        let out = rung(&uhd_hdr10(), RenditionName::Tv, &ctx_available());
        let RenditionOutcome::Encode { args, .. } = &out else {
            panic!("got {out:?}");
        };
        assert_eq!(args.last().unwrap(), SRC.replace(
            "Dune (2021).mkv",
            "Muse Versions/TV/Dune (2021).mkv"
        ).as_str());
        let i = args.iter().position(|s| s == "-i").unwrap();
        assert_eq!(args[i + 1], SRC);
    }

    #[test]
    fn the_filter_chain_is_one_argv_element() {
        // A stray space would make ffmpeg read the tail as separate options.
        let out = rung(&uhd_hdr10(), RenditionName::Web, &ctx_available());
        let RenditionOutcome::Encode { args, .. } = &out else {
            panic!("got {out:?}");
        };
        let vf = args.windows(2).find(|w| w[0] == "-vf").unwrap()[1].clone();
        assert!(!vf.contains(' '), "{vf:?}");
        assert_eq!(args.iter().filter(|s| *s == "-vf").count(), 1, "{args:?}");
    }

    #[test]
    fn an_encode_always_carries_at_least_one_reason() {
        // Inherited from the shared planner, and re-checked because the ladder
        // is what the operator actually reads.
        let sd = probe(
            vec![vid("mpeg4", 1920, 1080, 20_000_000)],
            vec![aud(1, "aac", 2)],
        );
        for name in [RenditionName::Mobile, RenditionName::Web, RenditionName::Tv] {
            if let RenditionOutcome::Encode { reasons, .. } = rung(&sd, name, &ctx_available()) {
                assert!(!reasons.is_empty(), "{name:?} encodes without a reason");
            }
        }
    }

    #[test]
    fn every_outcome_label_is_distinct() {
        let labels = [
            RenditionOutcome::Skip {
                why: RenditionSkip::NothingToPreserve,
            }
            .as_str(),
            RenditionOutcome::Refused {
                why: RenditionRefusal::ToneMapperUnavailable {
                    transfer: HdrTransfer::Pq,
                },
            }
            .as_str(),
            RenditionOutcome::CannotDecide {
                why: LadderUndecidable::ToneMapperUnverified {
                    transfer: HdrTransfer::Pq,
                },
            }
            .as_str(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
    }

    #[test]
    fn refusals_and_skips_render_as_operator_readable_text() {
        // These end up in the interface the operator marked the title from.
        let r = RenditionRefusal::DolbyVisionCannotBeTranscoded {
            verdict: DolbyVisionVerdict::Profile5NoFallback.to_string(),
        };
        assert!(r.to_string().contains("green"), "got {r}");
        assert!(r.to_string().contains("worse than no rendition"), "got {r}");

        let s = RenditionSkip::DuplicatesRung {
            superseded_by: RenditionName::Tv,
            resolution: (720, 480),
        };
        assert!(s.to_string().contains("tv"), "got {s}");
        assert!(s.to_string().contains("720x480"), "got {s}");
    }
}
