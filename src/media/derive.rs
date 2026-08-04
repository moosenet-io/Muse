//! Derived views over a [`MediaProbe`] — the accessors a delivery decision
//! reads, so that it never has to restate a rule (S130-A MPRB-03).
//!
//! # This module owns no rules
//!
//! That is its entire design, and it is a correction rather than a preference.
//! Spec S130-A instructed a `media::hdr` with a new `HdrFormat` and a new
//! `classify()`, a new image-subtitle codec list, and a new resolution class.
//! **All three already exist**, and building a second copy of any of them is
//! how this repo has already been hurt:
//! `foundry::directplay::may_delete_original` consumes
//! [`crate::foundry::hdr::classify_hdr`] to refuse deleting a source whose
//! HDR/Dolby-Vision characteristics the output does not preserve. A second,
//! drifting classifier would be plausible, self-consistent, tested — and free
//! to authorize an irreversible delete of a Dolby Vision master. The same shape
//! already cost this repo once, when `predicted_deletion_refusals` restated the
//! deletion gate instead of calling it and was wrong by a factor of twenty.
//!
//! So every function below **delegates**. Where it delegates, and to whom:
//!
//! | Accessor | Sole authority |
//! |---|---|
//! | [`dynamic_range`], [`is_hdr`] | [`crate::foundry::hdr::classify_hdr`] |
//! | [`dolby_vision`] | [`crate::foundry::hdr::classify_dolby_vision`] |
//! | [`bit_depth`], [`is_10bit`] | [`crate::foundry::hdr::pixel_bit_depth`] |
//! | [`resolution_class`] | [`crate::foundry::validate::resolution_band`] |
//! | [`has_preservation_worthy_audio`] | [`crate::foundry::ladder::PRESERVATION_WORTHY_AUDIO`] |
//! | [`has_image_subtitles`] | [`crate::subtitles::discover::is_image_codec`] |
//!
//! What is genuinely new here is only the composition — "over the whole probe,
//! not one stream" — plus [`effective_bitrate_bps`] and [`suspicion`], which
//! restate nothing because nothing states them today.
//!
//! # The layering is upside down, deliberately and temporarily
//!
//! `media` is meant to be the shared core and `foundry` a consumer of it, so a
//! `media::` module reaching into `foundry::` is backwards. It is done that way
//! anyway because the alternative — a local copy — is the failure mode above,
//! and correctness of the rule outranks tidiness of the graph. The right fix is
//! to MOVE `foundry::hdr` (and `resolution_band`, and the codec lists) into
//! `media` behind a re-export shim, exactly as MPRB-01 did for
//! probe/capability/paths. That is a separate item: it touches `directplay.rs`,
//! `ladder.rs`, `validate.rs` and `subtitles/`, three of which are being edited
//! concurrently, and a move done under those conditions is how a rule quietly
//! becomes two.
//!
//! # Totality
//!
//! Every function here is pure and total. None panics, none indexes, none
//! divides without first proving the divisor nonzero. An answer that cannot be
//! established is `None` or an explicit `Unknown` — never a benign default,
//! which is the same rule [`crate::media::probe`] applies to the document
//! itself.

use crate::foundry::hdr::{classify_dolby_vision, classify_hdr, DolbyVisionVerdict, HdrVerdict};
use crate::foundry::ladder::PRESERVATION_WORTHY_AUDIO;
use crate::foundry::validate::{resolution_band, ResolutionBand};
use crate::media::probe::{AudioStream, MediaProbe, VideoStream};
use crate::subtitles::discover::is_image_codec;

// --- Bit depth -------------------------------------------------------------

/// How a bit depth was arrived at.
///
/// Carried in the return type rather than only in [`MediaProbe::notes`], and
/// that is a deliberate departure from S130-A step 6. A note can be ignored; an
/// enum a caller has to match on cannot. Bit depth feeds a tone-mapping
/// decision, and "10-bit, because ffprobe said so" and "10-bit, because the
/// pixel format usually means that" are not interchangeable inputs to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthSource {
    /// ffprobe reported `bits_per_raw_sample` outright.
    Observed,
    /// Inferred from `pix_fmt` by [`crate::foundry::hdr::pixel_bit_depth`].
    /// Sound for the delivery formats this library holds, and still an
    /// inference.
    DerivedFromPixFmt,
}

/// A bit depth and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitDepth {
    pub bits: u8,
    pub source: DepthSource,
}

/// Bits per component for a video stream, with its provenance.
///
/// Observed wins over derived — an explicit `bits_per_raw_sample` is a
/// statement by the muxer, and the pixel-format table is a generalization about
/// what such formats usually carry. `None` when neither settles it, which is a
/// real and common state (ffprobe omits `bits_per_raw_sample` for most H.264)
/// and must not be read as 8.
///
/// The derivation itself is **not implemented here**; it is
/// [`crate::foundry::hdr::pixel_bit_depth`], the same function
/// [`classify_hdr`] uses to decide whether a claimed HDR transfer is
/// physically possible. Two tables would let a stream be 8-bit for the deletion
/// gate and 10-bit for the delivery decision.
pub fn bit_depth(stream: &VideoStream) -> Option<BitDepth> {
    if let Some(bits) = stream.bits_per_raw_sample.filter(|b| (8..=16).contains(b)) {
        return Some(BitDepth {
            bits,
            source: DepthSource::Observed,
        });
    }
    let bits = crate::foundry::hdr::pixel_bit_depth(stream.pix_fmt.as_deref()?)?;
    Some(BitDepth {
        bits,
        source: DepthSource::DerivedFromPixFmt,
    })
}

/// Whether the primary video stream carries more than 8 bits per component.
///
/// Tri-state on purpose: `None` means the depth could not be established, and
/// a client-capability check must treat that as undecided rather than as 8-bit.
///
/// **Independent of [`is_hdr`].** 10-bit SDR is a real and common thing (most
/// modern anime encodes), and 8-bit HDR does not exist. Collapsing the two axes
/// into one flag is how a 10-bit SDR file gets tone-mapped.
pub fn is_10bit(probe: &MediaProbe) -> Option<bool> {
    Some(bit_depth(probe.primary_video()?)?.bits > 8)
}

// --- Dynamic range ---------------------------------------------------------

/// The dynamic range of the primary video stream.
///
/// A thin delegation to [`classify_hdr`], which is the single authority. It is
/// here so a caller holding a `MediaProbe` does not have to reach for the
/// primary stream and then for another module — not to add, adjust or reinterpret
/// anything. `None` only when there is no video stream at all.
pub fn dynamic_range(probe: &MediaProbe) -> Option<HdrVerdict> {
    Some(classify_hdr(probe.primary_video()?))
}

/// Whether the file is HDR: `Some(true)`, `Some(false)`, or **`None` for both
/// "no video" and "could not be established"**.
///
/// The `None` on [`HdrVerdict::Unknown`] is the whole contract. Rendering an
/// unknown as `false` is precisely the "never silently default to SDR" rule
/// that [`crate::foundry::hdr`] exists to enforce, and a `bool` return type
/// would make that mistake unavoidable at every call site. A caller that wants
/// to know WHY it is unknown calls [`dynamic_range`] and matches the verdict.
pub fn is_hdr(probe: &MediaProbe) -> Option<bool> {
    match dynamic_range(probe)? {
        HdrVerdict::Hdr { .. } => Some(true),
        HdrVerdict::Sdr => Some(false),
        HdrVerdict::Unknown { .. } => None,
    }
}

/// The Dolby Vision verdict for the primary video stream.
///
/// Delegates to [`classify_dolby_vision`] — the function
/// `foundry::directplay::may_delete_original` consumes to refuse deleting a DV
/// master. There is exactly one implementation of this rule and this is not it.
pub fn dolby_vision(probe: &MediaProbe) -> Option<DolbyVisionVerdict> {
    Some(classify_dolby_vision(probe.primary_video()?))
}

// --- Shape -----------------------------------------------------------------

/// The resolution band of the primary video stream.
///
/// Delegates to [`resolution_band`], whose boundaries were drawn around the
/// operator's measured sample of this library (scope releases at 1918x802 land
/// with 1080p rather than being filed as 720p). Re-deriving them from nominal
/// resolutions here would misfile exactly the files that band was tuned for.
///
/// A file with no video stream is [`ResolutionBand::Unknown`], the same band a
/// file with unusable dimensions gets — in both cases we cannot measure it.
pub fn resolution_class(probe: &MediaProbe) -> ResolutionBand {
    match probe.primary_video() {
        Some(v) => resolution_band(v.width, v.height),
        None => ResolutionBand::Unknown,
    }
}

/// Whether any audio track is lossless or object-bearing.
///
/// **Named for the list it consults, not for the spec's wording.** S130-A calls
/// this `has_lossless_audio()`; the one existing list,
/// [`PRESERVATION_WORTHY_AUDIO`], is "lossless *or* object-bearing" and
/// includes `dts`, which is lossy. Exposing it under the name `lossless` would
/// make the accessor and its source disagree about what they mean — and the
/// next reader would then be tempted to write the "real" lossless list, which
/// is a fourth codec list. The honest name is the cheaper fix.
pub fn has_preservation_worthy_audio(probe: &MediaProbe) -> bool {
    probe.audio.iter().any(|a| {
        PRESERVATION_WORTHY_AUDIO.contains(&a.codec.trim().to_ascii_lowercase().as_str())
    })
}

/// Whether any subtitle track is bitmap-based.
///
/// Delegates to [`is_image_codec`]. Note what is NOT done here: S130-A asks for
/// an unknown subtitle codec to be treated as image-based (fail-closed).
/// `is_image_codec` returns `false` for an unknown codec, and this function
/// does not override it — a wrapper with different semantics from the list it
/// wraps is a second rule wearing the first one's name, which is the failure
/// this whole module is shaped to avoid.
///
/// There is now exactly **one** home for this rule:
/// [`crate::media::probe::BITMAP_SUBTITLE_CODECS`] and its predicate
/// [`crate::media::probe::is_bitmap_subtitle_codec`]. Until SUBCODEC-01 (#145)
/// the rule was stated twice — a 4-entry list in `subtitles::discover` and a
/// 7-entry one in `foundry::directplay` — and they disagreed over the
/// `pgssub`/`dvdsub`/`dvbsub` aliases. Both are gone;
/// [`is_image_codec`] is a thin forwarder to the one predicate, so this
/// accessor inherits any future correction for free, which it could not do if
/// it carried its own copy.
pub fn has_image_subtitles(probe: &MediaProbe) -> bool {
    probe.subtitles.iter().any(|s| is_image_codec(&s.codec))
}

// --- Audio selection -------------------------------------------------------

/// The audio track a player picks when the viewer expresses no preference.
///
/// The stream marked `default`, else the first audio stream, else `None`.
/// The fallback is deliberate and matches what players do: a file whose muxer
/// wrote no dispositions at all still has a track that will be played, and
/// reporting `None` for it would describe a silent file.
pub fn default_audio(probe: &MediaProbe) -> Option<&AudioStream> {
    probe
        .audio
        .iter()
        .find(|a| a.default)
        .or_else(|| probe.audio.first())
}

/// Every distinct audio language in the file, lowercased and sorted.
///
/// Untagged tracks contribute nothing — an absent language is not a language,
/// and inventing `"und"` for it would make a single-track untagged file look
/// like it offers a choice. Sorted and deduped so the value is stable across
/// two probes of the same file, which is what makes it usable as a stored
/// comparison key.
pub fn audio_languages(probe: &MediaProbe) -> Vec<String> {
    let mut langs: Vec<String> = probe
        .audio
        .iter()
        .filter_map(|a| a.language.clone())
        .collect();
    langs.sort();
    langs.dedup();
    langs
}

// --- Bitrate ---------------------------------------------------------------

/// Where an effective bitrate came from. Ordered by how directly it was
/// observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitrateSource {
    /// `format.bit_rate` — the container said so.
    Container,
    /// The sum of the per-stream bitrates. Every stream we model had one.
    SumOfStreams,
    /// `size * 8 / duration`. An average over the whole file, including
    /// container overhead, so it reads slightly high.
    SizeOverDuration,
}

/// A bitrate and how it was obtained.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveBitrate {
    pub bps: u64,
    pub source: BitrateSource,
}

/// The file's overall bitrate, by the most direct route available.
///
/// Container, then sum-of-streams, then size over duration, then `None`. The
/// source is returned alongside because the three are not equally trustworthy:
/// a sum-of-streams total omits any stream we do not model, and a
/// size-over-duration figure includes container overhead. A bitrate ceiling
/// applied to the third as if it were the first rejects files that are inside
/// it.
///
/// The sum tier requires **every** modelled stream to have reported a bitrate.
/// A partial sum is not a smaller bitrate, it is a wrong one, and it would sit
/// silently below any ceiling it was compared against — so a missing per-stream
/// bitrate falls through to the next tier rather than shrinking the answer.
///
/// # Division by zero
/// The last tier divides, and it only divides after proving the duration is
/// finite and strictly positive. `duration_secs` is already `None` for a
/// negative or NaN value ([`crate::media::probe`] folds those), so the
/// remaining case is a literal `0.0`, which is refused here.
pub fn effective_bitrate_bps(probe: &MediaProbe) -> Option<EffectiveBitrate> {
    if let Some(bps) = probe.format_bitrate_bps.filter(|b| *b > 0) {
        return Some(EffectiveBitrate {
            bps,
            source: BitrateSource::Container,
        });
    }

    let modelled = probe.video.len() + probe.audio.len();
    if modelled > 0 {
        let rates: Vec<u64> = probe
            .video
            .iter()
            .map(|v| v.bitrate_bps)
            .chain(probe.audio.iter().map(|a| a.bitrate_bps))
            .flatten()
            .collect();
        if rates.len() == modelled {
            // `checked_add`, not `+`: the inputs come from an untrusted
            // document, and a wrapped sum would report a huge file as a tiny
            // one. An overflow falls through to the next tier.
            if let Some(sum) = rates
                .iter()
                .try_fold(0u64, |acc, r| acc.checked_add(*r))
                .filter(|s| *s > 0)
            {
                return Some(EffectiveBitrate {
                    bps: sum,
                    source: BitrateSource::SumOfStreams,
                });
            }
        }
    }

    let size = probe.size_bytes.filter(|s| *s > 0)?;
    let duration = probe.duration_secs.filter(|d| d.is_finite() && *d > 0.0)?;
    let bps = (size as f64) * 8.0 / duration;
    (bps.is_finite() && bps >= 1.0).then(|| EffectiveBitrate {
        bps: bps as u64,
        source: BitrateSource::SizeOverDuration,
    })
}

// --- Suspicion -------------------------------------------------------------

/// A document that parsed cleanly and still describes something implausible.
///
/// **Never inferred from a [`crate::media::probe::ProbeError`].** A suspicion
/// is a statement about a SUCCESSFUL parse — "ffprobe answered, and the answer
/// does not hang together". A failed probe is a different state with a
/// different remedy, and folding the two would let a tool problem be reported
/// as a file problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suspicion {
    /// Neither video nor audio. Note the asymmetry with the audio-only case
    /// below: missing ONE of the two is ordinary (a music file has no video,
    /// and `probe.rs` has a test pinning that), missing BOTH means we are
    /// holding a description of something that is not playable content.
    NoStreamsOfInterest,
    /// A duration of exactly zero on a file that has streams.
    ZeroDuration,
    /// A video stream with no usable width or height — absent, or literally
    /// zero.
    ///
    /// **Both cases, and that is a correction to S130-A.** The spec's edge-case
    /// list asserts that a `width` of 0 is "already `None` via `as_u32`, so an
    /// aspect-ratio calculation cannot divide by zero". Checked against the
    /// tree: it is not. `as_u64` folds NEGATIVE values to `None`; zero is a
    /// perfectly good `u64` and arrives as `Some(0)`. A check written from the
    /// spec's claim would have tested `is_none()` and passed on every real file
    /// while never firing on the one shape it exists for.
    ZeroDimensions,
    /// The container bitrate and `size/duration` disagree by more than an order
    /// of magnitude in either direction — the shape of a truncated download or
    /// a container whose header outlived its payload.
    DurationBitrateInconsistent,
}

impl Suspicion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoStreamsOfInterest => "no_streams_of_interest",
            Self::ZeroDuration => "zero_duration",
            Self::ZeroDimensions => "zero_dimensions",
            Self::DurationBitrateInconsistent => "duration_bitrate_inconsistent",
        }
    }
}

/// How far the container bitrate and the size/duration bitrate may disagree
/// before the file is suspicious.
///
/// **A factor of 10, in either direction.** The two legitimately differ: the
/// container figure is a nominal or target rate, the computed one is a true
/// average including overhead, and a VBR file with a long quiet passage moves
/// them apart. Measured disagreement on ordinary files is a few percent to
/// perhaps 2x. An order of magnitude is far outside that and is the shape of a
/// file whose header describes content it no longer contains — a half-finished
/// download, or a copy interrupted mid-write.
///
/// Wide rather than tight on purpose: this flag makes a file *undecidable*
/// downstream, so a false positive costs a good file being left alone until
/// someone looks, and there are 16,221 of them.
const BITRATE_DISAGREEMENT_FACTOR: f64 = 10.0;

/// Flag a parsed probe that describes something implausible, or `None` for a
/// file with nothing wrong with it.
///
/// Pure and total. First match wins, most fundamental first: a file with no
/// streams of interest is not additionally interesting for having a zero
/// duration.
pub fn suspicion(probe: &MediaProbe) -> Option<Suspicion> {
    if probe.video.is_empty() && probe.audio.is_empty() {
        return Some(Suspicion::NoStreamsOfInterest);
    }
    if probe.duration_secs == Some(0.0) {
        return Some(Suspicion::ZeroDuration);
    }
    // `unwrap_or(0)`, not `is_none()`: see `Suspicion::ZeroDimensions`. An
    // absent dimension and a literal zero are the same unusable fact, and only
    // one of them is what the spec claimed would happen.
    if probe
        .video
        .iter()
        .any(|v| v.width.unwrap_or(0) == 0 || v.height.unwrap_or(0) == 0)
    {
        return Some(Suspicion::ZeroDimensions);
    }

    // Both figures, computed independently. Nothing is compared unless both
    // exist: an absent duration is not a disagreement.
    if let (Some(stated), Some(size), Some(duration)) = (
        probe.format_bitrate_bps.filter(|b| *b > 0),
        probe.size_bytes.filter(|s| *s > 0),
        probe.duration_secs.filter(|d| d.is_finite() && *d > 0.0),
    ) {
        let computed = (size as f64) * 8.0 / duration;
        if computed.is_finite() && computed > 0.0 {
            let ratio = (stated as f64) / computed;
            if ratio > BITRATE_DISAGREEMENT_FACTOR || ratio < 1.0 / BITRATE_DISAGREEMENT_FACTOR {
                return Some(Suspicion::DurationBitrateInconsistent);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundry::hdr::{DynamicRangeUnknown, HdrTransfer};
    use crate::media::probe::{parse_probe_json, SubtitleStream};

    fn probe(json: &str) -> MediaProbe {
        parse_probe_json(json).expect("fixture must parse")
    }

    fn video(extra: &str) -> MediaProbe {
        probe(&format!(
            r#"{{"streams":[{{"index":0,"codec_name":"hevc","codec_type":"video",
               "width":3840,"height":2160{extra}}}],
              "format":{{"format_name":"matroska,webm","duration":"7200.0"}}}}"#
        ))
    }

    // --- bit depth ---------------------------------------------------------

    #[test]
    fn an_observed_bit_depth_beats_the_pixel_format_and_says_which_it_was() {
        // The muxer stated 12; the pixel format would have said 10. The stated
        // value wins, and the caller can tell it was stated.
        let p = video(r#","pix_fmt":"yuv420p10le","bits_per_raw_sample":12"#);
        assert_eq!(
            bit_depth(p.primary_video().unwrap()),
            Some(BitDepth { bits: 12, source: DepthSource::Observed })
        );
    }

    #[test]
    fn a_derived_bit_depth_is_marked_derived_and_comes_from_the_one_table() {
        let p = video(r#","pix_fmt":"yuv420p10le""#);
        let d = bit_depth(p.primary_video().unwrap()).expect("derivable");
        assert_eq!(d, BitDepth { bits: 10, source: DepthSource::DerivedFromPixFmt });

        // Delegation, asserted rather than described: whatever
        // `foundry::hdr::pixel_bit_depth` says is what this returns. A local
        // copy of the table would let these two drift, and the drift would be
        // invisible until a deletion gate and a delivery decision disagreed
        // about the same file.
        for pix in ["yuv420p", "yuv420p10le", "p010le", "gbrp12be", "nv12", "wat"] {
            let p = video(&format!(r#","pix_fmt":"{pix}""#));
            assert_eq!(
                bit_depth(p.primary_video().unwrap()).map(|d| d.bits),
                crate::foundry::hdr::pixel_bit_depth(pix),
                "disagreed with the sole authority on {pix}"
            );
        }
    }

    #[test]
    fn an_unestablishable_bit_depth_is_none_never_eight() {
        let p = video("");
        assert_eq!(bit_depth(p.primary_video().unwrap()), None);
        assert_eq!(is_10bit(&p), None);
    }

    // --- dynamic range -----------------------------------------------------

    #[test]
    fn is_hdr_and_is_10bit_are_independent_axes() {
        // 10-bit SDR: real, common (anime encodes), and the case a single
        // "hdr" flag derived from bit depth would get wrong.
        let sdr10 = video(r#","pix_fmt":"yuv420p10le","color_transfer":"bt709""#);
        assert_eq!(is_10bit(&sdr10), Some(true));
        assert_eq!(is_hdr(&sdr10), Some(false));

        // 10-bit HDR: both true.
        let hdr10 = video(r#","pix_fmt":"yuv420p10le","color_transfer":"smpte2084""#);
        assert_eq!(is_10bit(&hdr10), Some(true));
        assert_eq!(is_hdr(&hdr10), Some(true));

        // 8-bit SDR: the ordinary 99% of this library.
        let sdr8 = video(r#","pix_fmt":"yuv420p","color_transfer":"bt709""#);
        assert_eq!(is_10bit(&sdr8), Some(false));
        assert_eq!(is_hdr(&sdr8), Some(false));
    }

    #[test]
    fn an_unestablished_dynamic_range_is_none_and_never_false() {
        // THE negative test. A 10-bit file with no transfer tag is exactly the
        // shape a badly-muxed HDR release has. Reporting it as "not HDR" is how
        // a tone-mapping decision gets made on a lie, and how a delete gate
        // reads an HDR master as SDR.
        let p = video(r#","pix_fmt":"yuv420p10le""#);
        assert_eq!(is_hdr(&p), None, "unknown must not collapse into false");
        assert!(matches!(
            dynamic_range(&p),
            Some(HdrVerdict::Unknown {
                why: DynamicRangeUnknown::NoTransferTagAndUnknownBitDepth { .. }
            })
        ));

        // An unrecognised transfer is likewise unknown, not SDR.
        let weird = video(r#","pix_fmt":"yuv420p10le","color_transfer":"smpte428""#);
        assert_eq!(is_hdr(&weird), None);
    }

    #[test]
    fn dynamic_range_returns_exactly_what_the_one_classifier_returns() {
        // The delegation itself, mechanically. If `dynamic_range` ever grew its
        // own rule — even a "harmless" extra case — this fails.
        for extra in [
            r#","pix_fmt":"yuv420p10le","color_transfer":"smpte2084""#,
            r#","pix_fmt":"yuv420p10le","color_transfer":"arib-std-b67""#,
            r#","pix_fmt":"yuv420p","color_transfer":"bt709""#,
            r#","pix_fmt":"yuv420p","color_transfer":"smpte2084""#,
            r#","pix_fmt":"yuv420p10le""#,
            "",
        ] {
            let p = video(extra);
            assert_eq!(
                dynamic_range(&p),
                Some(classify_hdr(p.primary_video().unwrap())),
                "diverged from foundry::hdr on {extra}"
            );
        }
        // And HLG really does reach through as HDR rather than being flattened.
        let hlg = video(r#","pix_fmt":"yuv420p10le","color_transfer":"arib-std-b67""#);
        assert_eq!(
            dynamic_range(&hlg),
            Some(HdrVerdict::Hdr { transfer: HdrTransfer::Hlg })
        );
    }

    #[test]
    fn dolby_vision_is_read_through_the_gates_own_classifier() {
        // Profile 5 — the one whose base layer is nothing on its own, and the
        // one `may_delete_original` must refuse. Detected only in side data.
        let p = probe(
            r#"{"streams":[{"index":0,"codec_name":"hevc","codec_type":"video",
                "width":3840,"height":2160,"pix_fmt":"yuv420p10le",
                "side_data_list":[{"side_data_type":"DOVI configuration record",
                  "dv_profile":5,"dv_bl_signal_compatibility_id":0,
                  "rpu_present_flag":1,"bl_present_flag":1,"el_present_flag":0}]}],
               "format":{"format_name":"matroska,webm"}}"#,
        );
        let v = p.primary_video().unwrap();
        assert_eq!(dolby_vision(&p), Some(classify_dolby_vision(v)));
        assert!(dolby_vision(&p).unwrap().is_present());
        assert!(
            !dolby_vision(&p).unwrap().base_layer_is_transcodable(),
            "profile 5 has no usable base layer — this is the refusal that \
             protects a DV master from deletion"
        );

        let plain = video(r#","pix_fmt":"yuv420p""#);
        assert!(!dolby_vision(&plain).unwrap().is_present());
    }

    #[test]
    fn every_video_accessor_is_none_on_a_file_with_no_video() {
        let audio_only = probe(
            r#"{"streams":[{"index":0,"codec_name":"flac","codec_type":"audio","channels":2}],
                "format":{"format_name":"matroska,webm","duration":"200.0"}}"#,
        );
        assert_eq!(is_10bit(&audio_only), None);
        assert_eq!(is_hdr(&audio_only), None);
        assert_eq!(dynamic_range(&audio_only), None);
        assert_eq!(dolby_vision(&audio_only), None);
        assert_eq!(resolution_class(&audio_only), ResolutionBand::Unknown);
    }

    // --- shape -------------------------------------------------------------

    #[test]
    fn resolution_class_is_the_band_tuned_for_this_library_not_a_fresh_one() {
        // 1918x802 is a real scope release in this library. A re-derived
        // classifier keyed on "1920 means 1080p" files it as 720p, which is the
        // specific mistake `resolution_band`'s doc comment records having fixed.
        for (w, h) in [
            (320u32, 240u32),
            (294, 240),
            (720, 480),
            (1280, 720),
            (1918, 802),
            (1920, 1080),
            (3832, 2068),
            (3840, 2160),
        ] {
            let q = probe(&format!(
                r#"{{"streams":[{{"index":0,"codec_name":"h264","codec_type":"video",
                   "width":{w},"height":{h}}}],"format":{{"format_name":"matroska,webm"}}}}"#
            ));
            assert_eq!(
                resolution_class(&q),
                crate::foundry::validate::resolution_band(Some(w), Some(h)),
                "diverged from the sole authority at {w}x{h}"
            );
        }
        let scope = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":1918,"height":802}],"format":{"format_name":"matroska,webm"}}"#,
        );
        assert_eq!(resolution_class(&scope), ResolutionBand::Hd1080);
    }

    #[test]
    fn preservation_worthy_audio_tracks_the_one_list_that_defines_it() {
        let with = |codec: &str| {
            probe(&format!(
                r#"{{"streams":[{{"index":0,"codec_name":"{codec}","codec_type":"audio",
                   "channels":6}}],"format":{{"format_name":"matroska,webm"}}}}"#
            ))
        };
        for codec in PRESERVATION_WORTHY_AUDIO {
            assert!(
                has_preservation_worthy_audio(&with(codec)),
                "{codec} is in the list and must be reported"
            );
        }
        for codec in ["aac", "ac3", "eac3", "opus"] {
            assert!(!has_preservation_worthy_audio(&with(codec)));
        }
        // Case is not a distinction ffprobe guarantees.
        assert!(has_preservation_worthy_audio(&with("TrueHD")));
    }

    #[test]
    fn image_subtitles_are_read_through_the_existing_list_not_a_new_one() {
        let with = |codec: &str| {
            probe(&format!(
                r#"{{"streams":[{{"index":0,"codec_name":"{codec}","codec_type":"subtitle"}}],
                   "format":{{"format_name":"matroska,webm"}}}}"#
            ))
        };
        for codec in ["hdmv_pgs_subtitle", "dvd_subtitle", "dvb_subtitle", "xsub", "subrip", "ass", "mov_text", "webvtt", "somethingnew"] {
            assert_eq!(
                has_image_subtitles(&with(codec)),
                is_image_codec(codec),
                "diverged from subtitles::discover on {codec} — a third bitmap \
                 list is exactly what #149 exists to prevent"
            );
        }
        // A file with no subtitles has no image subtitles.
        assert!(!has_image_subtitles(&video("")));
    }

    // --- audio selection ---------------------------------------------------

    fn two_audio(default_index: Option<u32>, langs: [Option<&str>; 2]) -> MediaProbe {
        let s: Vec<String> = (0u32..2)
            .map(|i| {
                let tags = langs[i as usize]
                    .map(|l| format!(r#","tags":{{"language":"{l}"}}"#))
                    .unwrap_or_default();
                let disp = if default_index == Some(i) { 1 } else { 0 };
                format!(
                    r#"{{"index":{i},"codec_name":"aac","codec_type":"audio","channels":2,
                       "disposition":{{"default":{disp}}}{tags}}}"#
                )
            })
            .collect();
        probe(&format!(
            r#"{{"streams":[{}],"format":{{"format_name":"matroska,webm"}}}}"#,
            s.join(",")
        ))
    }

    #[test]
    fn the_default_audio_track_is_the_marked_one_and_otherwise_the_first() {
        let marked = two_audio(Some(1), [Some("eng"), Some("jpn")]);
        assert_eq!(default_audio(&marked).unwrap().index, 1);

        // No disposition anywhere: a player still plays something, so
        // reporting None would describe a silent file.
        let unmarked = two_audio(None, [Some("eng"), Some("jpn")]);
        assert_eq!(default_audio(&unmarked).unwrap().index, 0);

        assert!(default_audio(&video("")).is_none(), "no audio, no default");
    }

    #[test]
    fn audio_languages_are_sorted_deduped_and_never_invented() {
        assert_eq!(
            audio_languages(&two_audio(None, [Some("jpn"), Some("eng")])),
            vec!["eng".to_string(), "jpn".to_string()]
        );
        assert_eq!(
            audio_languages(&two_audio(None, [Some("eng"), Some("eng")])),
            vec!["eng".to_string()]
        );
        // An untagged track contributes NOTHING. Inventing "und" for it would
        // make a single untagged track look like an offered choice.
        assert_eq!(
            audio_languages(&two_audio(None, [Some("eng"), None])),
            vec!["eng".to_string()]
        );
        assert!(audio_languages(&two_audio(None, [None, None])).is_empty());
    }

    // --- bitrate -----------------------------------------------------------

    #[test]
    fn the_container_bitrate_is_used_first_and_says_so() {
        let p = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":1920,"height":1080,"bit_rate":"5000000"}],
               "format":{"format_name":"matroska,webm","bit_rate":"5037037",
                 "size":"3400000000","duration":"5400.0"}}"#,
        );
        assert_eq!(
            effective_bitrate_bps(&p),
            Some(EffectiveBitrate { bps: 5_037_037, source: BitrateSource::Container })
        );
    }

    #[test]
    fn the_stream_sum_is_used_only_when_every_modelled_stream_reported_one() {
        let complete = probe(
            r#"{"streams":[
                {"index":0,"codec_name":"h264","codec_type":"video","width":1920,
                 "height":1080,"bit_rate":"5000000"},
                {"index":1,"codec_name":"eac3","codec_type":"audio","channels":6,
                 "bit_rate":"640000"}],
               "format":{"format_name":"matroska,webm"}}"#,
        );
        assert_eq!(
            effective_bitrate_bps(&complete),
            Some(EffectiveBitrate { bps: 5_640_000, source: BitrateSource::SumOfStreams })
        );

        // One stream missing its rate. A partial sum is not a smaller bitrate,
        // it is a WRONG one, and it would sit silently under any ceiling it was
        // compared against — so this must fall through, not shrink.
        let partial = probe(
            r#"{"streams":[
                {"index":0,"codec_name":"h264","codec_type":"video","width":1920,
                 "height":1080,"bit_rate":"5000000"},
                {"index":1,"codec_name":"eac3","codec_type":"audio","channels":6}],
               "format":{"format_name":"matroska,webm","size":"800000000",
                 "duration":"1000.0"}}"#,
        );
        assert_eq!(
            effective_bitrate_bps(&partial),
            Some(EffectiveBitrate { bps: 6_400_000, source: BitrateSource::SizeOverDuration }),
            "a partial sum must never be reported as the total"
        );
    }

    #[test]
    fn size_over_duration_is_the_last_resort_and_never_divides_by_zero() {
        let ok = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":1920,"height":1080}],
               "format":{"format_name":"matroska,webm","size":"1000000","duration":"8.0"}}"#,
        );
        assert_eq!(
            effective_bitrate_bps(&ok),
            Some(EffectiveBitrate { bps: 1_000_000, source: BitrateSource::SizeOverDuration })
        );

        // Zero duration: the division that would be `inf`.
        let zero = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":1920,"height":1080}],
               "format":{"format_name":"matroska,webm","size":"1000000","duration":"0"}}"#,
        );
        assert_eq!(effective_bitrate_bps(&zero), None);

        // Nothing to go on at all.
        let nothing = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":1920,"height":1080}],
               "format":{"format_name":"matroska,webm"}}"#,
        );
        assert_eq!(effective_bitrate_bps(&nothing), None);
    }

    // --- suspicion ---------------------------------------------------------

    #[test]
    fn a_healthy_file_and_an_audio_only_file_are_both_unsuspicious() {
        let healthy = probe(
            r#"{"streams":[
                {"index":0,"codec_name":"h264","codec_type":"video","width":1920,
                 "height":1080},
                {"index":1,"codec_name":"eac3","codec_type":"audio","channels":6}],
               "format":{"format_name":"matroska,webm","duration":"5400.0",
                 "bit_rate":"5000000","size":"3375000000"}}"#,
        );
        assert_eq!(suspicion(&healthy), None);

        // The asymmetry: missing ONE of video/audio is ordinary. `probe.rs`
        // already pins the audio-only case as legitimate, and this must agree
        // with it.
        let audio_only = probe(
            r#"{"streams":[{"index":0,"codec_name":"flac","codec_type":"audio","channels":2}],
               "format":{"format_name":"matroska,webm","duration":"200.0"}}"#,
        );
        assert_eq!(suspicion(&audio_only), None);

        let video_only = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":1920,"height":1080}],
               "format":{"format_name":"matroska,webm","duration":"60.0"}}"#,
        );
        assert_eq!(suspicion(&video_only), None);
    }

    #[test]
    fn each_suspicion_is_returned_for_its_own_case() {
        // Neither video nor audio: a description of something unplayable.
        let nothing = probe(
            r#"{"streams":[{"index":0,"codec_name":"subrip","codec_type":"subtitle"}],
               "format":{"format_name":"matroska,webm","duration":"60.0"}}"#,
        );
        assert_eq!(suspicion(&nothing), Some(Suspicion::NoStreamsOfInterest));

        let zero_duration = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":1920,"height":1080}],
               "format":{"format_name":"matroska,webm","duration":"0"}}"#,
        );
        assert_eq!(suspicion(&zero_duration), Some(Suspicion::ZeroDuration));

        // Both unusable shapes. The first one is where S130-A was WRONG: it
        // asserts a zero width is already `None` after parsing. It is not —
        // `as_u64` folds negatives, not zeroes — so this assertion is also the
        // regression test for the spec's claim, pinned here so nobody
        // "simplifies" the check back to `is_none()`.
        let zero_dims = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":0,"height":0}],
               "format":{"format_name":"matroska,webm","duration":"60.0"}}"#,
        );
        assert_eq!(
            zero_dims.primary_video().unwrap().width,
            Some(0),
            "a literal zero reaches the model as Some(0), NOT as None"
        );
        assert_eq!(suspicion(&zero_dims), Some(Suspicion::ZeroDimensions));

        let absent_dims = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video"}],
               "format":{"format_name":"matroska,webm","duration":"60.0"}}"#,
        );
        assert_eq!(suspicion(&absent_dims), Some(Suspicion::ZeroDimensions));

        // A header claiming 5 Mbps over a file that holds ~50 kbps: the shape
        // of a truncated download.
        let truncated = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":1920,"height":1080}],
               "format":{"format_name":"matroska,webm","duration":"5400.0",
                 "bit_rate":"5000000","size":"33750000"}}"#,
        );
        assert_eq!(suspicion(&truncated), Some(Suspicion::DurationBitrateInconsistent));
    }

    #[test]
    fn an_ordinary_vbr_disagreement_is_not_flagged() {
        // The bound has to be loose enough that a normal VBR file is left
        // alone: this flag makes a title undecidable downstream, and there are
        // 16,221 of them. Stated 5 Mbps, actual ~2.5 Mbps — a 2x disagreement,
        // well inside the order-of-magnitude threshold.
        let vbr = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":1920,"height":1080}],
               "format":{"format_name":"matroska,webm","duration":"5400.0",
                 "bit_rate":"5000000","size":"1687500000"}}"#,
        );
        assert_eq!(suspicion(&vbr), None);
    }

    #[test]
    fn suspicion_says_nothing_when_the_inputs_to_compare_are_missing() {
        // An absent duration or size is not a disagreement — it is a fact we
        // do not have, and inventing a suspicion from it would flag most of a
        // 20-year-old library.
        let no_size = probe(
            r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video",
                "width":1920,"height":1080}],
               "format":{"format_name":"matroska,webm","duration":"5400.0",
                 "bit_rate":"5000000"}}"#,
        );
        assert_eq!(suspicion(&no_size), None);
    }

    #[test]
    fn nothing_here_reads_a_probe_error() {
        // A suspicion is a statement about a SUCCESSFUL parse. There is no
        // constructor here that takes a ProbeError, and this test exists to
        // make that a stated property rather than an accident of the current
        // signature: every entry point in this module takes a `&MediaProbe`.
        let _: fn(&MediaProbe) -> Option<Suspicion> = suspicion;
        let _: fn(&MediaProbe) -> Option<bool> = is_hdr;
        let _: fn(&MediaProbe) -> Option<EffectiveBitrate> = effective_bitrate_bps;
        // And an empty subtitle list is not a suspicion of any kind.
        let s = SubtitleStream::default();
        assert!(!s.hearing_impaired);
    }
}
