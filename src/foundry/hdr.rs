//! FOUNDRY-03 — dynamic range: what the source actually is, and what may
//! safely be done to it.
//!
//! This is the module most likely to produce **visibly broken output**, so it
//! is separated from both the normalization path and the rendition ladder and
//! is a pure function of one [`VideoStream`]. Every rule below is a rule about
//! a failure a viewer would see immediately and an operator would struggle to
//! attribute.
//!
//! ## The three ways a transcode of HDR content goes visibly wrong
//!
//! 1. **Dolby Vision profile 5, transcoded naively → green and purple.**
//!    Profile 5 is single-layer with *no HDR10 fallback*: its base layer is
//!    encoded in a non-standard colour space (IPT-PQ-C2) that is only correct
//!    once the per-frame RPU metadata has been applied. Drop the RPU — which
//!    every re-encode does, because no encoder available to this fleet can
//!    carry it — and the base layer is decoded as if it were BT.2020, which
//!    renders as the notorious green/purple cast. It is not subtle and it is
//!    not recoverable by adjusting the output. So profile 5 is **refused**,
//!    with a stated reason, rather than transcoded. A visibly wrong rendition
//!    is worse than no rendition.
//! 2. **HDR tone-mapped to SDR badly → washed out, or too dark.** Simply
//!    telling an encoder `-pix_fmt yuv420p` on a PQ source does not tone-map at
//!    all: it truncates 10-bit PQ code values into 8-bit and reinterprets them
//!    as gamma-2.2, producing the flat, grey, low-contrast image that is the
//!    single most common complaint about automated transcoders. Doing it
//!    *properly* needs an explicit linearize → tone-map → re-encode-transfer
//!    chain, and the chain's parameters (peak luminance, desaturation
//!    strength, the tone curve) are what decide whether the result looks
//!    right. So the chain is built here, in one place, recorded verbatim on
//!    the decision, and never left implicit — a bad result must be
//!    diagnosable from the plan rather than mysterious.
//! 3. **HDR passed through as if it were SDR → washed out, on every client.**
//!    The mirror of (2): failing to *detect* HDR and copying it into a
//!    rendition labelled for an SDR client. This is why an undetermined
//!    dynamic range is [`HdrVerdict::Unknown`] and blocks a decision, rather
//!    than defaulting to SDR.
//!
//! ## What this module can and cannot see — read before trusting it
//!
//! ffprobe is **not installed on the dev box or on <host>**, so none of the
//! detection below has been run against a real Dolby Vision file by this item.
//! It is written against ffprobe's documented output shape and is tested
//! against fixtures of that shape. See [`undetectable_formats`] for the list of
//! things that are known to be invisible to us, which is deliberately exposed
//! as data rather than buried in prose: the deletion rule consumes it.

use crate::foundry::probe::{StreamSideData, VideoStream};

// --- Dynamic range ---------------------------------------------------------

/// The transfer function of an HDR source. Only the two that are actually
/// delivered: PQ (HDR10, HDR10+, Dolby Vision) and HLG (broadcast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdrTransfer {
    /// SMPTE ST 2084, "PQ". What HDR10 and Dolby Vision use.
    Pq,
    /// ARIB STD-B67, "HLG". Broadcast HDR; partially SDR-compatible by design,
    /// but *partially* is not a basis for skipping a tone-map.
    Hlg,
}

impl HdrTransfer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pq => "pq",
            Self::Hlg => "hlg",
        }
    }

    /// The nominal peak luminance, in nits, used to linearize this transfer
    /// before tone-mapping.
    ///
    /// These are the conventional values, not measurements: 100 for PQ is the
    /// reference white that `zscale`'s linearization is defined against, and
    /// 1000 for HLG reflects HLG's nominal 1000-nit system peak. A wrong value
    /// here is precisely the "too dark" / "washed out" failure — too low
    /// crushes highlights, too high lifts the whole image — which is why it is
    /// a named, recorded parameter rather than a literal inside a format
    /// string. **Not verified against real content** (no ffmpeg on this host).
    pub fn nominal_peak_nits(self) -> u32 {
        match self {
            Self::Pq => 100,
            Self::Hlg => 1000,
        }
    }
}

/// What the dynamic range of a video stream is.
#[derive(Debug, Clone, PartialEq)]
pub enum HdrVerdict {
    /// Proven standard dynamic range. No tone-map is needed or wanted.
    Sdr,
    /// Proven high dynamic range. An SDR-targeted rendition must tone-map.
    Hdr { transfer: HdrTransfer },
    /// Could not be established. **Never** folded into `Sdr`: treating an HDR
    /// source as SDR produces a washed-out file on every client, and treating
    /// an SDR source as HDR produces a wrongly-tone-mapped one. Both are
    /// visible errors, so an unknown blocks the decision.
    Unknown { why: DynamicRangeUnknown },
}

/// Why the dynamic range could not be established.
#[derive(Debug, Clone, PartialEq)]
pub enum DynamicRangeUnknown {
    /// No `color_transfer` tag, and the pixel format did not settle it either.
    NoTransferTagAndUnknownBitDepth { pix_fmt: Option<String> },
    /// A `color_transfer` value this module has no rule for.
    UnrecognizedTransfer { found: String },
    /// The tags contradict each other: an HDR transfer function on a pixel
    /// format that cannot carry one. There is no 8-bit PQ or HLG delivery
    /// format, so one of the two facts is wrong and we cannot tell which.
    /// Believing the transfer would tone-map SDR content; believing the depth
    /// would pass HDR through untouched. Both are visible errors.
    TransferContradictsBitDepth { transfer: String, bit_depth: u8 },
}

impl std::fmt::Display for DynamicRangeUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTransferTagAndUnknownBitDepth { pix_fmt } => write!(
                f,
                "ffprobe reported no colour transfer function, and the pixel format {} \
                 does not establish the bit depth either — the dynamic range cannot be \
                 determined, so neither tone-mapping nor passing through is safe",
                pix_fmt.as_deref().unwrap_or("(absent)")
            ),
            Self::UnrecognizedTransfer { found } => write!(
                f,
                "colour transfer function `{found}` is not one Foundry has a rule for — \
                 refusing to guess whether it needs tone-mapping"
            ),
            Self::TransferContradictsBitDepth { transfer, bit_depth } => write!(
                f,
                "the stream claims transfer `{transfer}` at {bit_depth} bits per component, \
                 which is not a combination that exists — one of the two tags is wrong and \
                 Foundry cannot tell which"
            ),
        }
    }
}

/// `color_transfer` values that mean standard dynamic range.
///
/// An allowlist, matching the house fail-closed idiom in
/// [`crate::safety`]: a transfer function nobody has classified escalates to
/// `Unknown` rather than falling through to "probably SDR". The cost of a
/// wrong `Unknown` is a file left alone; the cost of a wrong `Sdr` is a
/// washed-out rendition of somebody's 4K disc.
pub const SDR_TRANSFERS: &[&str] = &[
    "bt709",
    "bt470m",
    "bt470bg",
    "smpte170m",
    "smpte240m",
    "gamma22",
    "gamma28",
    "iec61966-2-1",
    "iec61966-2-4",
    "bt1361e",
    "bt2020-10",
    "bt2020-12",
    "linear",
    "log100",
    "log316",
];

/// `color_transfer` value for PQ, as ffprobe spells it.
pub const PQ_TRANSFER: &str = "smpte2084";
/// `color_transfer` value for HLG, as ffprobe spells it.
pub const HLG_TRANSFER: &str = "arib-std-b67";

/// Bits per component for a pixel format, or `None` when unrecognized.
///
/// A table plus a suffix rule rather than either alone. The table covers the
/// formats that have no depth in their name (`yuv420p` is 8-bit, and nothing
/// about the string says so); the suffix rule covers the open-ended
/// `<layout>p<depth><endianness>` family so a format ffmpeg adds next year is
/// still read correctly. Anything matching neither is `None` — never 8, which
/// would silently assert SDR for an unfamiliar high-depth format.
pub fn pixel_bit_depth(pix_fmt: &str) -> Option<u8> {
    let f = pix_fmt.trim().to_ascii_lowercase();
    if f.is_empty() {
        return None;
    }

    // Named 8-bit formats: no depth token in the name.
    const EIGHT_BIT: &[&str] = &[
        "yuv410p", "yuv411p", "yuv420p", "yuv422p", "yuv440p", "yuv444p", "yuvj411p", "yuvj420p",
        "yuvj422p", "yuvj440p", "yuvj444p", "nv12", "nv21", "nv16", "nv24", "nv42", "gray",
        "rgb24", "bgr24", "rgba", "bgra", "argb", "abgr", "0rgb", "rgb0", "0bgr", "bgr0", "gbrp",
        "pal8", "yuva420p", "yuva422p", "yuva444p", "monow", "monob",
    ];
    if EIGHT_BIT.contains(&f.as_str()) {
        return Some(8);
    }
    // The semi-planar `pNNN` family names its depth in a different position.
    for (name, depth) in [
        ("p010", 10u8),
        ("p210", 10),
        ("p410", 10),
        ("p012", 12),
        ("p212", 12),
        ("p412", 12),
        ("p016", 16),
        ("p216", 16),
        ("p416", 16),
    ] {
        if f.starts_with(name) {
            return Some(depth);
        }
    }

    // `<layout>p<depth>[le|be]`, e.g. yuv420p10le, gbrp12be, yuv444p16le.
    let body = f
        .strip_suffix("le")
        .or_else(|| f.strip_suffix("be"))
        .unwrap_or(&f);
    let digits: String = body
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if digits.is_empty() {
        return None;
    }
    let before = &body[..body.len() - digits.len()];
    // Guard against reading the CHROMA LAYOUT as a depth: `yuv420p` has no
    // trailing digits so it never reaches here, but a hypothetical `yuv420`
    // would, and 420 bits per component is not a thing. The depth token is
    // only a depth when it directly follows the plane marker.
    if !before.ends_with('p') {
        return None;
    }
    let depth: u8 = digits.parse().ok()?;
    (8..=16).contains(&depth).then_some(depth)
}

/// Decide the dynamic range of a video stream.
///
/// The inference drawn when `color_transfer` is absent is the interesting part,
/// and it is deliberately one-sided:
///
/// - **8 bits per component ⇒ SDR.** There is no 8-bit PQ or HLG *delivery*
///   format; both are defined for 10-bit and above, and no consumer HDR
///   encoder emits 8-bit. This inference is sound in the direction it is used
///   and it is what keeps the ~99% of this library that is ordinary 8-bit
///   H.264 — where ffprobe very often writes no transfer tag at all —
///   decidable rather than a wall of unknowns.
/// - **10 bits or more with no transfer tag ⇒ Unknown, never SDR.** A 10-bit
///   file with no tag is exactly the shape a badly-muxed HDR release has, and
///   the whole point of the rule is that it is the case we must not guess at.
///   10-bit SDR does exist (anime encodes, notably), so this is a real cost:
///   some 10-bit SDR files will be reported undecidable. That is the correct
///   direction to be wrong in.
pub fn classify_hdr(stream: &VideoStream) -> HdrVerdict {
    let depth = stream.pix_fmt.as_deref().and_then(pixel_bit_depth);

    match stream.color_transfer.as_deref() {
        Some(t) if t == PQ_TRANSFER || t == HLG_TRANSFER => {
            let transfer = if t == PQ_TRANSFER {
                HdrTransfer::Pq
            } else {
                HdrTransfer::Hlg
            };
            // A claimed HDR transfer at 8 bits is a contradiction, not a fact.
            if let Some(d) = depth {
                if d < 10 {
                    return HdrVerdict::Unknown {
                        why: DynamicRangeUnknown::TransferContradictsBitDepth {
                            transfer: t.to_string(),
                            bit_depth: d,
                        },
                    };
                }
            }
            HdrVerdict::Hdr { transfer }
        }
        Some(t) if SDR_TRANSFERS.contains(&t) => HdrVerdict::Sdr,
        Some(t) => HdrVerdict::Unknown {
            why: DynamicRangeUnknown::UnrecognizedTransfer {
                found: t.to_string(),
            },
        },
        None => match depth {
            Some(d) if d <= 8 => HdrVerdict::Sdr,
            _ => HdrVerdict::Unknown {
                why: DynamicRangeUnknown::NoTransferTagAndUnknownBitDepth {
                    pix_fmt: stream.pix_fmt.clone(),
                },
            },
        },
    }
}

// --- Dolby Vision ----------------------------------------------------------

/// What the base layer of a Dolby Vision stream is on its own, once the RPU is
/// discarded. This is the only thing that decides whether a transcode is
/// possible at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseLayerFormat {
    /// `dv_bl_signal_compatibility_id == 1`: the base layer is valid HDR10.
    /// Tone-mappable exactly like any HDR10 source.
    Hdr10,
    /// `== 2`: the base layer is valid SDR (BT.709). Needs no tone-map.
    Sdr,
    /// `== 4`: the base layer is valid HLG.
    Hlg,
}

impl BaseLayerFormat {
    /// The dynamic range the base layer presents once the RPU is gone.
    pub fn as_hdr_verdict(self) -> HdrVerdict {
        match self {
            Self::Hdr10 => HdrVerdict::Hdr {
                transfer: HdrTransfer::Pq,
            },
            Self::Hlg => HdrVerdict::Hdr {
                transfer: HdrTransfer::Hlg,
            },
            Self::Sdr => HdrVerdict::Sdr,
        }
    }
}

/// Whether, and how, a stream carries Dolby Vision.
#[derive(Debug, Clone, PartialEq)]
pub enum DolbyVisionVerdict {
    /// No Dolby Vision signal was found. See [`undetectable_formats`] for why
    /// this is "not found", not "not present".
    NotDetected,
    /// **Profile 5.** Single-layer, `dv_bl_signal_compatibility_id == 0`:
    /// there is no fallback, the base layer is not viewable without the RPU,
    /// and any re-encode renders green/purple. Refuse.
    Profile5NoFallback,
    /// A DOVI record whose base layer is independently viewable (profile 7 or
    /// 8 with a stated compatibility). The RPU is still lost by any re-encode
    /// — the result is the base layer, correct but no longer Dolby Vision.
    BaseLayerViewable {
        profile: u32,
        base: BaseLayerFormat,
    },
    /// A DOVI record that states no usable base-layer compatibility (id 0, or
    /// absent). Same hazard as profile 5, arrived at differently: we cannot
    /// show that dropping the RPU leaves a viewable picture.
    BaseLayerNotViewable {
        profile: Option<u32>,
        compatibility_id: Option<u32>,
    },
    /// A DOVI record with a profile number Foundry has no rule for.
    UnknownProfile { profile: u32 },
    /// The `dvh1`/`dvhe`/`dvav`/`dav1` codec tag says Dolby Vision, but no
    /// DOVI configuration record was reported, so the profile is unknown.
    ///
    /// This is a *real* state, not a hypothetical: whether ffprobe surfaces the
    /// `dvcC`/`dvvC` box as stream side data has varied by version and by
    /// container. Unknown profile means it could be profile 5, so it is
    /// refused on the same grounds.
    SignalledWithoutProfile { tag: String },
}

impl DolbyVisionVerdict {
    /// Whether any Dolby Vision signal at all was seen. Used by the deletion
    /// rule, which does not care which flavour.
    pub fn is_present(&self) -> bool {
        !matches!(self, Self::NotDetected)
    }

    /// Whether video may be re-encoded from this stream at all.
    ///
    /// `false` for every state except a viewable base layer and no DV. Note
    /// that `true` does **not** mean the output is still Dolby Vision — it is
    /// not, the RPU is gone — only that the output is a correct picture.
    pub fn base_layer_is_transcodable(&self) -> bool {
        matches!(self, Self::NotDetected | Self::BaseLayerViewable { .. })
    }
}

impl std::fmt::Display for DolbyVisionVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotDetected => write!(f, "no Dolby Vision signal detected"),
            Self::Profile5NoFallback => write!(
                f,
                "Dolby Vision profile 5: single-layer with no HDR10 fallback. Re-encoding \
                 discards the RPU and the base layer then decodes green/purple — Foundry \
                 refuses rather than producing a visibly broken file"
            ),
            Self::BaseLayerViewable { profile, base } => write!(
                f,
                "Dolby Vision profile {profile} with a {base:?} base layer: the base layer is \
                 viewable on its own, so a re-encode produces a correct picture — but it is \
                 no longer Dolby Vision, the RPU is lost"
            ),
            Self::BaseLayerNotViewable {
                profile,
                compatibility_id,
            } => write!(
                f,
                "Dolby Vision (profile {}, base-layer compatibility {}) states no usable \
                 fallback — Foundry cannot show that discarding the RPU leaves a viewable \
                 picture, so it refuses",
                profile.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                compatibility_id
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "absent".into())
            ),
            Self::UnknownProfile { profile } => write!(
                f,
                "Dolby Vision profile {profile} is not one Foundry has a rule for — refusing \
                 rather than guessing whether it survives a transcode"
            ),
            Self::SignalledWithoutProfile { tag } => write!(
                f,
                "the `{tag}` codec tag says this is Dolby Vision but no configuration record \
                 was reported, so the profile is unknown — it could be profile 5, and is \
                 refused on that basis"
            ),
        }
    }
}

/// Codec tags that assert Dolby Vision independently of any side data.
pub const DOLBY_VISION_CODEC_TAGS: &[&str] = &["dvh1", "dvhe", "dvav", "dva1", "dav1"];

/// Whether a side-data entry is a Dolby Vision configuration record.
///
/// Matched loosely (case-insensitive substring) rather than against an exact
/// string, because ffprobe's spelling of `side_data_type` has drifted — "DOVI
/// configuration record" is the current one. An exact match that silently
/// stopped matching would turn every DV file into `NotDetected`, which is the
/// single worst failure this module can have: it is the state in which a
/// profile 5 file gets transcoded.
fn is_dovi_record(d: &StreamSideData) -> bool {
    let k = d.kind.to_ascii_lowercase();
    k.contains("dovi") || k.contains("dolby vision")
}

/// Detect Dolby Vision on a video stream.
///
/// Two independent signals are consulted, and the *stronger refusal wins*: a
/// DOVI record gives the profile, and the codec tag gives presence without a
/// profile. Presence without a profile is treated as hazardous, not as absence.
pub fn classify_dolby_vision(stream: &VideoStream) -> DolbyVisionVerdict {
    // EVERY DOVI record, not the first. A stream carrying a profile 8 record
    // followed by a profile 5 one used to classify as transcodable, because
    // the refusal was never read — list order decided whether the original
    // got deleted. Refusal wins regardless of position.
    let strictest = stream
        .side_data
        .iter()
        .filter(|d| is_dovi_record(d))
        .map(classify_dovi_record)
        .max_by_key(refusal_rank);
    if let Some(v) = strictest {
        return v;
    }
    if let Some(tag) = stream.codec_tag.as_deref() {
        let t = tag.trim().to_ascii_lowercase();
        if DOLBY_VISION_CODEC_TAGS.contains(&t.as_str()) {
            return DolbyVisionVerdict::SignalledWithoutProfile { tag: t };
        }
    }
    DolbyVisionVerdict::NotDetected
}

/// How strongly a verdict refuses, so the strictest of several DOVI records
/// can be selected. Only the ORDER matters, not the numbers.
///
/// Deliberately total rather than a `transcodable`/`not transcodable` split:
/// among refusals, `Profile5NoFallback` is the one whose reason is certain, so
/// it is the message the operator should see when a stream carries several.
fn refusal_rank(v: &DolbyVisionVerdict) -> u8 {
    match v {
        DolbyVisionVerdict::NotDetected => 0,
        DolbyVisionVerdict::BaseLayerViewable { .. } => 1,
        DolbyVisionVerdict::SignalledWithoutProfile { .. } => 2,
        DolbyVisionVerdict::UnknownProfile { .. } => 3,
        DolbyVisionVerdict::BaseLayerNotViewable { .. } => 4,
        DolbyVisionVerdict::Profile5NoFallback => 5,
    }
}

/// The profile/compatibility rules, split out so each branch is directly
/// testable from a record rather than only through a whole stream.
pub fn classify_dovi_record(d: &StreamSideData) -> DolbyVisionVerdict {
    // Profile 5 first and by itself. It is the dangerous one, it is checked
    // before the compatibility id, and it is refused regardless of what that
    // id claims — a profile 5 stream asserting a viewable base layer is a
    // contradiction, and the safe reading of a contradiction is the refusal.
    if d.dv_profile == Some(5) {
        return DolbyVisionVerdict::Profile5NoFallback;
    }

    let base = match d.dv_bl_signal_compatibility_id {
        Some(1) => Some(BaseLayerFormat::Hdr10),
        Some(2) => Some(BaseLayerFormat::Sdr),
        Some(4) => Some(BaseLayerFormat::Hlg),
        _ => None,
    };

    match (d.dv_profile, base) {
        // Profiles 4, 7 and 8 are the ones with a defined viewable base layer.
        (Some(p @ (4 | 7 | 8)), Some(base)) => DolbyVisionVerdict::BaseLayerViewable { profile: p, base },
        (Some(p @ (4 | 7 | 8)), None) => DolbyVisionVerdict::BaseLayerNotViewable {
            profile: Some(p),
            compatibility_id: d.dv_bl_signal_compatibility_id,
        },
        (Some(p), _) => DolbyVisionVerdict::UnknownProfile { profile: p },
        // A record with no profile at all: we know it is Dolby Vision and know
        // nothing else. It could be profile 5.
        (None, _) => DolbyVisionVerdict::BaseLayerNotViewable {
            profile: None,
            compatibility_id: d.dv_bl_signal_compatibility_id,
        },
    }
}

/// The formats this fleet's tooling **cannot** detect, as data rather than
/// prose, because the deletion rule has to reason about them.
///
/// Each entry is a format that may be present in a source, would be lost by a
/// transcode, and cannot be observed from `ffprobe -show_streams` output. They
/// are the reason [`crate::foundry::directplay::may_delete_original`] refuses
/// on codecs that merely *might* carry them.
pub fn undetectable_formats() -> &'static [UndetectableFormat] {
    &[
        UndetectableFormat {
            name: "Dolby Atmos (JOC) inside E-AC-3",
            carried_by_codec: "eac3",
            why: "JOC is object metadata embedded in the E-AC-3 bitstream. ffprobe reports \
                  the stream as plain `eac3` with a channel count of 6; there is no field \
                  that distinguishes an Atmos track from an ordinary 5.1 one.",
        },
        UndetectableFormat {
            name: "Dolby Atmos inside TrueHD",
            carried_by_codec: "truehd",
            why: "Same shape: the Atmos substream is inside the TrueHD bitstream and is not \
                  surfaced by -show_streams.",
        },
        UndetectableFormat {
            name: "DTS:X inside DTS-HD MA",
            carried_by_codec: "dts",
            why: "ffprobe reports `dts` with a profile string that does not reliably \
                  distinguish DTS:X from DTS-HD MA.",
        },
        UndetectableFormat {
            name: "HDR10+ dynamic metadata (SMPTE ST 2094-40)",
            carried_by_codec: "hevc",
            why: "Carried as per-frame SEI. It appears in ffprobe FRAME side data, which \
                  requires -show_frames — a full decode Foundry does not perform.",
        },
        UndetectableFormat {
            name: "HDR10 static metadata (mastering display / MaxCLL)",
            carried_by_codec: "hevc",
            why: "Reported as stream side data on some builds and only as frame side data \
                  on others. Its absence from our probe is therefore not evidence it is \
                  absent from the file.",
        },
    ]
}

/// One format Foundry knows it cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndetectableFormat {
    pub name: &'static str,
    /// The ffprobe `codec_name` that may be hiding it.
    pub carried_by_codec: &'static str,
    pub why: &'static str,
}

// --- Tone-mapping ----------------------------------------------------------

/// The tone curve applied when mapping HDR to SDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneMapAlgorithm {
    /// The filmic Hable curve. The default here because it rolls highlights
    /// off gradually: `clip` blows out anything above the SDR peak (skies,
    /// specular highlights, explosions become flat white), and `linear`
    /// preserves highlight detail only by dimming the entire image, which is
    /// the "too dark" complaint. `mobius` is the other reasonable choice and
    /// is deliberately selectable rather than hard-coded.
    Hable,
    /// Reinhard. Softer, tends to desaturate more.
    Reinhard,
    /// Mobius. Preserves mid-tone contrast better than Hable at the cost of
    /// slightly harder highlight clipping.
    Mobius,
}

impl ToneMapAlgorithm {
    /// The value ffmpeg's `tonemap` filter expects.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Hable => "hable",
            Self::Reinhard => "reinhard",
            Self::Mobius => "mobius",
        }
    }
}

/// A fully-specified tone-map, recorded on the decision that ordered it.
///
/// Every parameter that affects the look is a named field, and
/// [`ToneMap::filter_chain`] is the exact string handed to `-vf`. That is the
/// point: when an operator says a rendition looks washed out, the plan that
/// produced it states the curve, the peak luminance and the desaturation
/// strength that were used, so the complaint is diagnosable instead of
/// mysterious.
#[derive(Debug, Clone, PartialEq)]
pub struct ToneMap {
    pub source_transfer: HdrTransfer,
    pub algorithm: ToneMapAlgorithm,
    /// Nominal peak luminance in nits, fed to the linearization step.
    pub peak_nits: u32,
    /// `tonemap`'s `desat` parameter.
    pub desat: u32,
    /// The exact `-vf` value.
    pub filter_chain: String,
}

/// Desaturation strength for the `tonemap` filter. **0**, not ffmpeg's default.
///
/// ffmpeg's `tonemap` defaults to `desat=2`, which desaturates highlights as
/// they approach the clipping point. On real film content that default is the
/// direct cause of the washed-out, pastel look people attribute to "HDR
/// transcoding" in general: skies and skin tones in bright scenes lose their
/// colour. Setting it to 0 keeps saturation and lets the tone curve alone
/// handle the range compression.
pub const TONE_MAP_DESAT: u32 = 0;

/// Build the tone-map filter chain, and the [`ToneMap`] record describing it.
///
/// The chain, and why each link is there — this is the part that is wrong in
/// most naive implementations:
///
/// 1. `zscale=transfer=linear:npl=<peak>` — undo the PQ/HLG curve into linear
///    light. **Tone-mapping in the encoded domain is the single most common
///    mistake**; PQ code values are perceptually spaced, so arithmetic on them
///    without linearizing first produces the crushed, grey result.
/// 2. `format=gbrpf32le` — float RGB. The tone-map operates per-channel in
///    linear light, and doing it at integer precision introduces banding in
///    exactly the smooth gradients (skies, fades) where it is most visible.
/// 3. `zscale=primaries=bt709` — gamut-convert BT.2020 → BT.709 *before* the
///    curve. Done afterwards, out-of-gamut colours have already been clipped
///    by the tone-map and the conversion cannot recover them.
/// 4. `tonemap=tonemap=<algo>:desat=<desat>` — the range compression itself.
/// 5. `zscale=transfer=bt709:matrix=bt709:range=limited` — re-apply the SDR
///    transfer and matrix, at limited (TV) range. `range=limited` is not
///    optional: a full-range output shown by a client expecting limited range
///    is the "blacks are grey and whites are blown" failure.
/// 6. `format=yuv420p` — 8-bit 4:2:0, which is what the SDR renditions target.
///
/// **This chain requires an ffmpeg built with `libzimg` (for `zscale`).** That
/// has *not* been verified on the deployment host — see [`ToneMapSupport`],
/// which is why ordering a tone-map is gated on a capability the planner
/// refuses to assume.
/// Wide-gamut primaries that a BT.709 client cannot interpret correctly.
///
/// These are *gamut* tags, deliberately independent of the transfer curve:
/// `bt2020-10` and `bt2020-12` are SDR curves that nonetheless sit on BT.2020
/// primaries, so a stream can need this conversion while needing no tone map
/// at all.
pub const WIDE_GAMUT_PRIMARIES: &[&str] = &["bt2020", "bt2020nc", "bt2020c", "smpte431", "smpte432"];

/// The filter that brings wide-gamut SDR into BT.709.
///
/// Deliberately NOT part of [`tone_map`]'s chain — that chain already contains
/// `zscale=primaries=bt709` and applies to HDR sources. This is the SDR-only
/// case, which skips the tone-map chain entirely and would otherwise pass the
/// source gamut straight through to the encoder untouched.
pub const BT709_GAMUT_CHAIN: &str = "zscale=primaries=bt709:matrix=bt709:transfer=bt709";

/// The gamut conversion an SDR rendition of this stream needs, if any.
///
/// `None` when the source is already BT.709, when the primaries are untagged
/// (converting on a guess would be its own colour bug), or when the stream is
/// not SDR — an HDR source gets its conversion from the tone-map chain, and
/// applying both would convert twice.
///
/// Returning `None` for untagged primaries is a deliberate asymmetry with the
/// rest of this module, where unknown refuses. Here "refuse" would mean
/// "convert anyway", and mis-converting a BT.709 file is a visible error in
/// the opposite direction. Leaving an untagged file alone is what every other
/// tool does with it.
pub fn sdr_gamut_conversion(stream: &VideoStream) -> Option<&'static str> {
    if classify_hdr(stream) != HdrVerdict::Sdr {
        return None;
    }
    let p = stream.color_primaries.as_deref()?.trim().to_ascii_lowercase();
    WIDE_GAMUT_PRIMARIES
        .contains(&p.as_str())
        .then_some(BT709_GAMUT_CHAIN)
}

pub fn tone_map(transfer: HdrTransfer, algorithm: ToneMapAlgorithm) -> ToneMap {
    let peak_nits = transfer.nominal_peak_nits();
    let desat = TONE_MAP_DESAT;
    let filter_chain = format!(
        "zscale=transfer=linear:npl={peak_nits},\
         format=gbrpf32le,\
         zscale=primaries=bt709,\
         tonemap=tonemap={algo}:desat={desat},\
         zscale=transfer=bt709:matrix=bt709:range=limited,\
         format=yuv420p",
        algo = algorithm.ffmpeg_name(),
    );
    ToneMap {
        source_transfer: transfer,
        algorithm,
        peak_nits,
        desat,
        filter_chain,
    }
}

/// Whether this host can actually tone-map.
///
/// Its default is [`ToneMapSupport::Unverified`], and that is the whole reason
/// the type exists. The chain above needs `zscale`, which is only present in an
/// ffmpeg built against `libzimg`; a build without it fails the encode with
/// "No such filter: 'zscale'" *after* however long ffmpeg spent getting to the
/// filter graph. Worse, a naive fallback to "just encode it without the filter"
/// would produce precisely the washed-out output this module exists to prevent.
///
/// So an unverified tone-mapper does not proceed hopefully — it makes the
/// affected rungs undecidable, and the operator resolves it by checking
/// `ffmpeg -filters` once. **No host in this fleet has ffmpeg on the dev box,
/// and this has not been checked on <host> or <host> by this item.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToneMapSupport {
    /// Nobody has checked. Fail closed.
    #[default]
    Unverified,
    /// `zscale` and `tonemap` were both observed in `ffmpeg -filters`.
    Available,
    /// They were looked for and are not present.
    Unavailable,
}

impl ToneMapSupport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundry::probe::VideoStream;

    fn stream(pix_fmt: Option<&str>, transfer: Option<&str>) -> VideoStream {
        VideoStream {
            index: 0,
            codec: "hevc".into(),
            width: Some(3840),
            height: Some(2160),
            pix_fmt: pix_fmt.map(str::to_string),
            color_transfer: transfer.map(str::to_string),
            ..VideoStream::default()
        }
    }

    fn dovi(profile: Option<u32>, compat: Option<u32>) -> StreamSideData {
        StreamSideData {
            kind: "DOVI configuration record".into(),
            dv_profile: profile,
            dv_bl_signal_compatibility_id: compat,
            rpu_present: Some(true),
            bl_present: Some(true),
            el_present: Some(false),
        }
    }

    /// Codex flagged this during the FOUNDRY-03 review gate, and it is real.
    ///
    /// `classify_dolby_vision` took the FIRST DOVI record it found. A stream
    /// carrying more than one — profile 8 first, profile 5 second — therefore
    /// classified as `BaseLayerViewable` and was transcodable, because the
    /// refusal sitting in the second record was never read.
    ///
    /// ffprobe realistically emits one record per stream, so this is not a
    /// shape seen in this library today. It is fixed anyway: the whole point
    /// of the DV rules is that a refusal wins, and "wins unless it happens to
    /// be listed second" is not that. Order must not decide whether a file is
    /// destroyed.
    #[test]
    fn a_refusal_wins_no_matter_where_it_sits_among_several_dovi_records() {
        let viewable = dovi(Some(8), Some(1));
        let refused = dovi(Some(5), Some(0));

        // The dangerous ordering: the permissive record is seen first.
        let mut s = VideoStream::default();
        s.side_data = vec![viewable.clone(), refused.clone()];
        assert_eq!(
            classify_dolby_vision(&s),
            DolbyVisionVerdict::Profile5NoFallback,
            "a profile 5 record must refuse even when a viewable record precedes it"
        );
        assert!(!classify_dolby_vision(&s).base_layer_is_transcodable());

        // ...and the reverse order agrees, so the answer does not depend on it.
        let mut s2 = VideoStream::default();
        s2.side_data = vec![refused, viewable.clone()];
        assert_eq!(classify_dolby_vision(&s2), DolbyVisionVerdict::Profile5NoFallback);

        // A lone viewable record is still transcodable — the fix must not
        // simply refuse everything, which would pass the assertions above.
        let mut s3 = VideoStream::default();
        s3.side_data = vec![viewable];
        assert!(
            classify_dolby_vision(&s3).base_layer_is_transcodable(),
            "the fix must not turn every DV file into a refusal"
        );
    }

    // --- bit depth ---------------------------------------------------------

    #[test]
    fn the_common_eight_bit_formats_are_known_to_be_eight_bit() {
        for f in ["yuv420p", "yuvj420p", "nv12", "yuv444p", "rgb24", "gbrp"] {
            assert_eq!(pixel_bit_depth(f), Some(8), "{f}");
        }
    }

    #[test]
    fn ten_and_twelve_bit_formats_are_read_from_the_suffix() {
        assert_eq!(pixel_bit_depth("yuv420p10le"), Some(10));
        assert_eq!(pixel_bit_depth("yuv422p10le"), Some(10));
        assert_eq!(pixel_bit_depth("yuv444p12be"), Some(12));
        assert_eq!(pixel_bit_depth("gbrp16le"), Some(16));
        assert_eq!(pixel_bit_depth("yuv420p10"), Some(10), "endianness suffix is optional");
        // The semi-planar family names its depth differently.
        assert_eq!(pixel_bit_depth("p010le"), Some(10));
        assert_eq!(pixel_bit_depth("p016le"), Some(16));
    }

    #[test]
    fn a_chroma_layout_is_never_mistaken_for_a_bit_depth() {
        // The trap in the suffix rule: `yuv420` ends in digits that are a
        // subsampling layout, not 420 bits per component. Reading it as a
        // depth would put it outside the 8..=16 range and — worse, if the
        // range check were dropped — make an 8-bit file look high-depth and
        // therefore undecidable.
        assert_eq!(pixel_bit_depth("yuv420"), None);
        assert_eq!(pixel_bit_depth("yuv422"), None);
    }

    #[test]
    fn an_unrecognized_pixel_format_has_no_depth_rather_than_a_default() {
        // Never 8: defaulting would silently assert SDR for an unfamiliar
        // high-depth format.
        assert_eq!(pixel_bit_depth("some-new-format-in-ffmpeg-9"), None);
        assert_eq!(pixel_bit_depth(""), None);
        assert_eq!(pixel_bit_depth("   "), None);
    }

    // --- HDR classification ------------------------------------------------

    #[test]
    fn pq_and_hlg_transfers_are_hdr() {
        assert_eq!(
            classify_hdr(&stream(Some("yuv420p10le"), Some("smpte2084"))),
            HdrVerdict::Hdr { transfer: HdrTransfer::Pq }
        );
        assert_eq!(
            classify_hdr(&stream(Some("yuv420p10le"), Some("arib-std-b67"))),
            HdrVerdict::Hdr { transfer: HdrTransfer::Hlg }
        );
    }

    #[test]
    fn a_tagged_sdr_transfer_is_sdr() {
        for t in ["bt709", "smpte170m", "bt470bg", "iec61966-2-1"] {
            assert_eq!(classify_hdr(&stream(Some("yuv420p"), Some(t))), HdrVerdict::Sdr, "{t}");
        }
    }

    #[test]
    fn an_untagged_eight_bit_file_is_sdr_which_is_what_keeps_the_library_decidable() {
        // ffprobe writes no transfer tag for most ordinary H.264. Without this
        // inference essentially the whole library would be undecidable and the
        // feature would do nothing at all. The inference is sound in this
        // direction: no 8-bit PQ or HLG delivery format exists.
        assert_eq!(classify_hdr(&stream(Some("yuv420p"), None)), HdrVerdict::Sdr);
        assert_eq!(classify_hdr(&stream(Some("yuvj420p"), None)), HdrVerdict::Sdr);
    }

    #[test]
    fn an_untagged_ten_bit_file_is_unknown_and_specifically_not_sdr() {
        // THE case this rule exists for: a badly-muxed HDR release with no
        // transfer tag. Reading it as SDR would pass PQ through into an
        // SDR-labelled rendition, which is washed out on every client.
        // The cost is real and accepted: 10-bit SDR anime lands here too.
        let v = classify_hdr(&stream(Some("yuv420p10le"), None));
        assert!(
            matches!(
                v,
                HdrVerdict::Unknown {
                    why: DynamicRangeUnknown::NoTransferTagAndUnknownBitDepth { .. }
                }
            ),
            "got {v:?}"
        );
    }

    #[test]
    fn an_unknown_pixel_format_with_no_transfer_tag_is_unknown_not_sdr() {
        assert!(matches!(
            classify_hdr(&stream(Some("who-knows"), None)),
            HdrVerdict::Unknown { .. }
        ));
        assert!(matches!(
            classify_hdr(&stream(None, None)),
            HdrVerdict::Unknown { .. }
        ));
    }

    #[test]
    fn an_unrecognized_transfer_function_is_unknown_not_assumed_sdr() {
        // Fail closed, matching the allowlist idiom in `crate::safety`: a
        // transfer nobody has classified escalates rather than falling through.
        let v = classify_hdr(&stream(Some("yuv420p10le"), Some("smpte428")));
        assert!(
            matches!(
                v,
                HdrVerdict::Unknown { why: DynamicRangeUnknown::UnrecognizedTransfer { .. } }
            ),
            "got {v:?}"
        );
    }

    #[test]
    fn a_pq_tag_on_an_eight_bit_stream_is_a_contradiction_not_a_fact() {
        // There is no 8-bit PQ delivery format, so one of the two tags is
        // wrong. Believing the transfer tone-maps SDR content; believing the
        // depth passes HDR through. Both are visible errors, so neither is
        // chosen.
        let v = classify_hdr(&stream(Some("yuv420p"), Some("smpte2084")));
        assert!(
            matches!(
                v,
                HdrVerdict::Unknown {
                    why: DynamicRangeUnknown::TransferContradictsBitDepth { bit_depth: 8, .. }
                }
            ),
            "got {v:?}"
        );
    }

    // --- Dolby Vision ------------------------------------------------------

    #[test]
    fn profile_5_is_refused_and_is_not_transcodable() {
        // The headline rule. Profile 5 re-encoded without its RPU renders
        // green and purple.
        let mut s = stream(Some("yuv420p10le"), None);
        s.side_data = vec![dovi(Some(5), Some(0))];
        let v = classify_dolby_vision(&s);
        assert_eq!(v, DolbyVisionVerdict::Profile5NoFallback);
        assert!(!v.base_layer_is_transcodable());
        assert!(v.is_present());
        assert!(v.to_string().contains("green"), "the reason must be stated: {v}");
    }

    #[test]
    fn profile_5_is_refused_even_when_it_claims_a_viewable_base_layer() {
        // A profile 5 record asserting compatibility id 1 is self-
        // contradictory. The safe reading of a contradiction is the refusal,
        // and the profile check therefore runs BEFORE the compatibility id.
        let mut s = stream(Some("yuv420p10le"), None);
        s.side_data = vec![dovi(Some(5), Some(1))];
        assert_eq!(classify_dolby_vision(&s), DolbyVisionVerdict::Profile5NoFallback);
    }

    #[test]
    fn profile_8_with_an_hdr10_base_layer_is_transcodable_from_its_base_layer() {
        let mut s = stream(Some("yuv420p10le"), Some("smpte2084"));
        s.side_data = vec![dovi(Some(8), Some(1))];
        let v = classify_dolby_vision(&s);
        assert_eq!(
            v,
            DolbyVisionVerdict::BaseLayerViewable {
                profile: 8,
                base: BaseLayerFormat::Hdr10
            }
        );
        assert!(v.base_layer_is_transcodable());
        // ...but the result is no longer Dolby Vision, and the message says so.
        assert!(v.to_string().contains("RPU is lost"), "got {v}");
    }

    #[test]
    fn profile_7_and_the_sdr_and_hlg_compatibility_ids_are_mapped() {
        let mut s = stream(Some("yuv420p10le"), None);
        s.side_data = vec![dovi(Some(7), Some(1))];
        assert!(matches!(
            classify_dolby_vision(&s),
            DolbyVisionVerdict::BaseLayerViewable { profile: 7, base: BaseLayerFormat::Hdr10 }
        ));

        s.side_data = vec![dovi(Some(8), Some(2))];
        assert!(matches!(
            classify_dolby_vision(&s),
            DolbyVisionVerdict::BaseLayerViewable { base: BaseLayerFormat::Sdr, .. }
        ));
        assert_eq!(BaseLayerFormat::Sdr.as_hdr_verdict(), HdrVerdict::Sdr);

        s.side_data = vec![dovi(Some(8), Some(4))];
        assert!(matches!(
            classify_dolby_vision(&s),
            DolbyVisionVerdict::BaseLayerViewable { base: BaseLayerFormat::Hlg, .. }
        ));
        assert_eq!(
            BaseLayerFormat::Hlg.as_hdr_verdict(),
            HdrVerdict::Hdr { transfer: HdrTransfer::Hlg }
        );
    }

    #[test]
    fn a_compatibility_id_of_zero_is_not_viewable_whatever_the_profile() {
        // Id 0 means "no fallback" — the same hazard as profile 5, reached by
        // a different route.
        let mut s = stream(Some("yuv420p10le"), None);
        s.side_data = vec![dovi(Some(8), Some(0))];
        let v = classify_dolby_vision(&s);
        assert!(
            matches!(v, DolbyVisionVerdict::BaseLayerNotViewable { profile: Some(8), .. }),
            "got {v:?}"
        );
        assert!(!v.base_layer_is_transcodable());
    }

    #[test]
    fn a_dovi_record_with_no_profile_at_all_is_refused_not_ignored() {
        // We know it is Dolby Vision and nothing else. It could be profile 5.
        let mut s = stream(Some("yuv420p10le"), None);
        s.side_data = vec![dovi(None, None)];
        let v = classify_dolby_vision(&s);
        assert!(
            matches!(v, DolbyVisionVerdict::BaseLayerNotViewable { profile: None, .. }),
            "got {v:?}"
        );
        assert!(!v.base_layer_is_transcodable());
    }

    #[test]
    fn an_unrecognized_dv_profile_is_refused() {
        let mut s = stream(Some("yuv420p10le"), None);
        s.side_data = vec![dovi(Some(9), Some(1))];
        let v = classify_dolby_vision(&s);
        assert_eq!(v, DolbyVisionVerdict::UnknownProfile { profile: 9 });
        assert!(!v.base_layer_is_transcodable());
    }

    #[test]
    fn the_dvh1_codec_tag_alone_is_enough_to_refuse() {
        // Whether ffprobe surfaces the dvcC box as side data has varied by
        // version and container. Presence without a profile could be profile
        // 5, so it is refused rather than treated as absence.
        let mut s = stream(Some("yuv420p10le"), Some("smpte2084"));
        s.codec_tag = Some("dvh1".into());
        let v = classify_dolby_vision(&s);
        assert_eq!(v, DolbyVisionVerdict::SignalledWithoutProfile { tag: "dvh1".into() });
        assert!(!v.base_layer_is_transcodable());
        assert!(v.is_present());
    }

    #[test]
    fn the_side_data_record_wins_over_the_codec_tag() {
        // A dvh1-tagged file WITH a record must be judged on the record — the
        // record is the more specific fact, and downgrading it to
        // "signalled without profile" would refuse transcodable profile 8
        // content unnecessarily.
        let mut s = stream(Some("yuv420p10le"), Some("smpte2084"));
        s.codec_tag = Some("dvhe".into());
        s.side_data = vec![dovi(Some(8), Some(1))];
        assert!(matches!(
            classify_dolby_vision(&s),
            DolbyVisionVerdict::BaseLayerViewable { profile: 8, .. }
        ));
    }

    #[test]
    fn an_ordinary_hdr10_file_carries_no_dolby_vision_signal() {
        let mut s = stream(Some("yuv420p10le"), Some("smpte2084"));
        s.codec_tag = Some("hvc1".into());
        s.side_data = vec![StreamSideData {
            kind: "Mastering display metadata".into(),
            ..StreamSideData::default()
        }];
        let v = classify_dolby_vision(&s);
        assert_eq!(v, DolbyVisionVerdict::NotDetected);
        assert!(v.base_layer_is_transcodable());
        assert!(!v.is_present());
    }

    #[test]
    fn the_dovi_record_is_matched_loosely_because_ffprobe_has_renamed_it_before() {
        // An exact-string match that silently stopped matching would classify
        // every DV file as NotDetected — the single worst failure this module
        // can have, because that is the state in which a profile 5 file gets
        // transcoded.
        for kind in [
            "DOVI configuration record",
            "dovi configuration record",
            "Dolby Vision configuration record",
            "DOVI Configuration Record (v1.0)",
        ] {
            let mut s = stream(Some("yuv420p10le"), None);
            s.side_data = vec![StreamSideData {
                kind: kind.into(),
                dv_profile: Some(5),
                ..StreamSideData::default()
            }];
            assert_eq!(
                classify_dolby_vision(&s),
                DolbyVisionVerdict::Profile5NoFallback,
                "spelling `{kind}` must still be recognized"
            );
        }
    }

    // --- tone-mapping ------------------------------------------------------

    #[test]
    fn the_tone_map_chain_linearizes_before_it_maps_and_converts_gamut_before_the_curve() {
        // Ordering IS the correctness here. Tone-mapping PQ code values
        // without linearizing first produces the crushed grey result; gamut
        // conversion after the curve cannot recover already-clipped colours.
        let t = tone_map(HdrTransfer::Pq, ToneMapAlgorithm::Hable);
        let c = &t.filter_chain;
        let linear = c.find("transfer=linear").expect("must linearize");
        let gamut = c.find("primaries=bt709").expect("must gamut-convert");
        let curve = c.find("tonemap=tonemap=").expect("must tone-map");
        let back = c.find("transfer=bt709").expect("must re-apply the SDR transfer");
        assert!(linear < gamut, "linearize before gamut conversion: {c}");
        assert!(gamut < curve, "gamut-convert before the tone curve: {c}");
        assert!(curve < back, "tone-map before re-applying the transfer: {c}");
    }

    #[test]
    fn the_tone_map_chain_works_in_float_and_ends_in_limited_range_yuv420p() {
        let c = tone_map(HdrTransfer::Pq, ToneMapAlgorithm::Hable).filter_chain;
        assert!(c.contains("format=gbrpf32le"), "integer maths bands the gradients: {c}");
        assert!(
            c.contains("range=limited"),
            "a full-range output shown as limited is the blacks-are-grey failure: {c}"
        );
        assert!(c.contains("matrix=bt709"), "{c}");
        assert!(c.ends_with("format=yuv420p"), "{c}");
    }

    #[test]
    fn desaturation_is_zero_not_ffmpegs_washed_out_default() {
        // ffmpeg's tonemap defaults to desat=2, which is the direct cause of
        // the pastel skies and skin tones people call "washed out".
        let t = tone_map(HdrTransfer::Pq, ToneMapAlgorithm::Hable);
        assert_eq!(t.desat, 0);
        assert!(t.filter_chain.contains("desat=0"), "{}", t.filter_chain);
    }

    #[test]
    fn the_peak_luminance_matches_the_source_transfer_and_is_recorded() {
        // Wrong npl is the "too dark" / "lifted" failure, so it is a recorded
        // parameter rather than a literal buried in a format string.
        let pq = tone_map(HdrTransfer::Pq, ToneMapAlgorithm::Hable);
        assert_eq!(pq.peak_nits, 100);
        assert!(pq.filter_chain.contains("npl=100"), "{}", pq.filter_chain);

        let hlg = tone_map(HdrTransfer::Hlg, ToneMapAlgorithm::Hable);
        assert_eq!(hlg.peak_nits, 1000);
        assert!(hlg.filter_chain.contains("npl=1000"), "{}", hlg.filter_chain);
        assert_eq!(hlg.source_transfer, HdrTransfer::Hlg);
    }

    #[test]
    fn the_algorithm_is_followed_rather_than_hardcoded() {
        // A hardcoded curve would make the recorded `algorithm` field a lie.
        for (algo, name) in [
            (ToneMapAlgorithm::Hable, "hable"),
            (ToneMapAlgorithm::Reinhard, "reinhard"),
            (ToneMapAlgorithm::Mobius, "mobius"),
        ] {
            let t = tone_map(HdrTransfer::Pq, algo);
            assert_eq!(t.algorithm, algo);
            assert!(
                t.filter_chain.contains(&format!("tonemap=tonemap={name}:")),
                "{}",
                t.filter_chain
            );
        }
    }

    #[test]
    fn the_filter_chain_is_a_single_vf_value_with_no_whitespace() {
        // It is passed as ONE argv element. A stray space would make ffmpeg
        // read the tail as a separate option.
        let c = tone_map(HdrTransfer::Pq, ToneMapAlgorithm::Hable).filter_chain;
        assert!(!c.contains(' '), "got {c:?}");
        assert!(!c.contains('\n'), "got {c:?}");
    }

    #[test]
    fn tone_map_support_defaults_to_unverified_so_nothing_proceeds_hopefully() {
        // zscale needs an ffmpeg built with libzimg. Assuming it is present
        // fails the encode late; falling back to "encode without the filter"
        // produces exactly the washed-out output this module prevents.
        assert_eq!(ToneMapSupport::default(), ToneMapSupport::Unverified);
    }

    #[test]
    fn the_undetectable_list_names_atmos_and_the_dynamic_metadata_formats() {
        // Exposed as data because the deletion rule consumes it. If this list
        // is ever emptied, the deletion rule silently becomes permissive.
        let names: Vec<&str> = undetectable_formats().iter().map(|u| u.name).collect();
        assert!(names.iter().any(|n| n.contains("Atmos") && n.contains("E-AC-3")), "{names:?}");
        assert!(names.iter().any(|n| n.contains("DTS:X")), "{names:?}");
        assert!(names.iter().any(|n| n.contains("HDR10+")), "{names:?}");
        assert!(
            undetectable_formats().iter().any(|u| u.carried_by_codec == "eac3"),
            "the deletion rule keys off this codec"
        );
    }
}
