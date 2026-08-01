//! MUSEF-02 — the transcode target policy: what "already optimal" means.
//!
//! ## Why this is a struct and not a set of constants
//! The policy is the only place in Foundry where a *judgement* is encoded —
//! everything else is mechanism. A judgement that lives inline in the planner
//! cannot be printed, diffed, reviewed, or overridden per-library. So it is a
//! plain data struct with a documented [`Default`], the planner takes it by
//! reference, and every test states its own policy explicitly rather than
//! relying on the default. That means changing a default cannot silently
//! change what a test proves.
//!
//! ## The defaults are conservative on purpose
//! Every default here is chosen so that the *most likely error is doing
//! nothing*. Foundry re-encodes files the operator cannot re-download; a policy
//! that is too eager destroys quality irreversibly, while a policy that is too
//! lax merely leaves a file un-optimized and visible in a report. Given that
//! asymmetry the acceptable-codec lists are deliberately wide and the bitrate
//! ceiling is deliberately generous.

/// What Foundry considers an acceptable delivered file.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscodePolicy {
    /// Video codecs that are left alone. Lowercase `ffprobe` `codec_name`s.
    ///
    /// Default `["h264", "hevc"]`. H.264 is the universal direct-play baseline;
    /// HEVC is accepted-if-present but is deliberately **not** the encode
    /// target (see [`TranscodePolicy::encode_video`]) — re-encoding an existing
    /// HEVC file to H.264 would be a quality loss and a CPU cost for no gain,
    /// while re-encoding H.264 *to* HEVC would break the older clients that are
    /// exactly why H.264 is the target.
    pub acceptable_video_codecs: Vec<String>,

    /// The encoder used when a re-encode is unavoidable. Default
    /// [`VideoEncoder::X264`] — see above.
    pub encode_video: VideoEncoder,

    /// Constant Rate Factor for the x264 encode. Default **20**.
    ///
    /// x264's own scale runs 0 (lossless) to 51 (worst); 23 is the ffmpeg
    /// default and is widely described as "visually transparent-ish". 20 is a
    /// deliberate step *better* than that default: this is a one-way,
    /// generation-losing re-encode of a file the operator cannot re-acquire, so
    /// the setting errs toward file size rather than toward quality loss.
    pub crf: u8,

    /// x264 speed/compression preset. Default `"medium"`.
    ///
    /// Not `"slow"`: Foundry's encode host is shared with inference workloads
    /// on this fleet, and a preset that doubles wall-clock for a few percent of
    /// bitrate is the wrong trade for a background library pass.
    pub preset: String,

    /// Resolution ceiling. Default **1920x1080**.
    ///
    /// The operator's display tier. Above this, a downscale is ordered; at or
    /// below it, resolution is never touched — Foundry never *upscales*, which
    /// would invent pixels and grow the file for no information gain.
    pub max_width: u32,
    pub max_height: u32,

    /// Video bitrate ceiling in bits/sec, for the video stream. Default
    /// **12 Mbps**.
    ///
    /// Generous on purpose. A 1080p H.264 encode is typically 5-10 Mbps, so
    /// 12 Mbps only catches genuinely wasteful files (remuxed Blu-ray, 4K-rate
    /// video in a 1080p container) rather than merely high-quality ones. A
    /// tighter ceiling here would order irreversible re-encodes of files that
    /// are fine.
    pub max_video_bitrate_bps: u64,

    /// Multiplicative slack on [`TranscodePolicy::max_video_bitrate_bps`]
    /// before a re-encode is ordered. Default **1.25**.
    ///
    /// Without slack, a file 2% over the ceiling would be fully re-encoded for
    /// a saving no viewer could perceive — paying a generation loss to enforce
    /// a round number. The ceiling states the target; the tolerance states how
    /// far over it is worth destroying quality to fix.
    pub bitrate_tolerance: f64,

    /// Audio codecs that are left alone. Default `["aac", "ac3", "eac3"]` —
    /// the three every client in this fleet direct-plays. Lossless and
    /// object-based formats (`truehd`, `dts`, `flac`) are deliberately *not*
    /// here: they are the ones that actually force a client-side transcode.
    pub acceptable_audio_codecs: Vec<String>,

    /// Audio channel ceiling. Default **6** (5.1). A 7.1 or object-based track
    /// is downmixed rather than passed through, because the clients that
    /// cannot handle it are the reason this stage exists.
    pub max_audio_channels: u32,

    /// Containers that are left alone. Default `[Matroska, Mp4]` — the two
    /// that every client and every *arr tool in this fleet handles.
    pub acceptable_containers: Vec<Container>,

    /// The container written when Foundry must produce a new file. Default
    /// [`Container::Matroska`].
    ///
    /// MKV, not MP4, and the reason is subtitles. MP4's subtitle support is a
    /// narrow matrix (no PGS/VobSub image subs, no ASS styling), so an MP4
    /// target would force Foundry to either drop subtitle tracks or burn them
    /// in — both destructive, and both a decision this stage has no business
    /// making. Matroska holds every stream type this fleet encounters, so
    /// `-c:s copy` is always valid and no track is ever lost in a remux.
    pub output_container: Container,
}

/// The video encoder Foundry will invoke. An enum rather than a free string so
/// a typo cannot become an ffmpeg argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoder {
    /// libx264 — software H.264.
    X264,
}

impl VideoEncoder {
    /// The `-c:v` value.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::X264 => "libx264",
        }
    }

    /// The `codec_name` ffprobe reports for output this encoder produced.
    /// Used by the post-encode verification to confirm the encoder actually
    /// did what was asked, rather than assuming a zero exit code meant so.
    pub fn probe_codec_name(self) -> &'static str {
        match self {
            Self::X264 => "h264",
        }
    }
}

/// A container Foundry recognizes.
///
/// Deliberately a small closed set. An unrecognized container is *not* mapped
/// to a catch-all: the planner reports it as undecidable, because Foundry
/// cannot know whether an unknown format's streams survive a remux, and
/// guessing is how a file gets silently mangled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Matroska,
    Mp4,
    Avi,
    MpegTs,
    Asf,
    Flv,
}

impl Container {
    /// The `-f` value ffmpeg needs to *write* this container.
    pub fn ffmpeg_format(self) -> &'static str {
        match self {
            Self::Matroska => "matroska",
            Self::Mp4 => "mp4",
            Self::Avi => "avi",
            Self::MpegTs => "mpegts",
            Self::Asf => "asf",
            Self::Flv => "flv",
        }
    }

    /// The filename extension for this container.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Matroska => "mkv",
            Self::Mp4 => "mp4",
            Self::Avi => "avi",
            Self::MpegTs => "ts",
            Self::Asf => "asf",
            Self::Flv => "flv",
        }
    }
}

/// Map an ffprobe `format.format_name` to a [`Container`], or `None` when it is
/// not one Foundry recognizes.
///
/// `format_name` is a **comma-separated list of every demuxer that claimed the
/// file**, not a single format: Matroska reports `"matroska,webm"` and MP4
/// reports `"mov,mp4,m4a,3gp,3g2,mj2"`. A caller comparing the whole string to
/// `"mp4"` therefore never matches a real MP4 file — so this matches on
/// membership of the list, not on the list itself.
///
/// Returning `None` rather than a default is the fail-closed choice: the
/// planner turns `None` into "cannot decide", so an unrecognized container
/// leaves the file untouched and reported, never remuxed on a guess.
pub fn normalize_container(format_name: &str) -> Option<Container> {
    // Order matters where a name is ambiguous, so match the most specific
    // members first. `mov,mp4,m4a,...` contains `mp4`; nothing else does.
    let parts: Vec<&str> = format_name
        .split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    let has = |name: &str| parts.iter().any(|p| p.eq_ignore_ascii_case(name));

    if has("matroska") || has("webm") {
        Some(Container::Matroska)
    } else if has("mp4") || has("mov") || has("m4v") {
        Some(Container::Mp4)
    } else if has("avi") {
        Some(Container::Avi)
    } else if has("mpegts") || has("mpeg") {
        Some(Container::MpegTs)
    } else if has("asf") || has("wmv") {
        Some(Container::Asf)
    } else if has("flv") {
        Some(Container::Flv)
    } else {
        None
    }
}

impl Default for TranscodePolicy {
    /// See each field's doc comment for why it holds the value it does.
    fn default() -> Self {
        Self {
            acceptable_video_codecs: vec!["h264".to_string(), "hevc".to_string()],
            encode_video: VideoEncoder::X264,
            crf: 20,
            preset: "medium".to_string(),
            max_width: 1920,
            max_height: 1080,
            max_video_bitrate_bps: 12_000_000,
            bitrate_tolerance: 1.25,
            acceptable_audio_codecs: vec![
                "aac".to_string(),
                "ac3".to_string(),
                "eac3".to_string(),
            ],
            max_audio_channels: 6,
            acceptable_containers: vec![Container::Matroska, Container::Mp4],
            output_container: Container::Matroska,
        }
    }
}

impl TranscodePolicy {
    /// FOUNDRY-03 — the **ingest normalization** target: "will this direct
    /// play", rather than "is this file wasteful".
    ///
    /// ## Why this is a constructor and not a new type
    /// The compatibility half of direct play is *already* what
    /// [`TranscodePolicy::default`] encodes, and re-expressing it as a parallel
    /// struct would create two places for the accepted-codec lists to drift
    /// apart. Concretely, these fields are already direct-play criteria and are
    /// reused unchanged:
    ///
    /// - `acceptable_video_codecs` — H.264 direct-plays everywhere; the codecs
    ///   *not* in the list (mpeg4, vc1, vp9, av1) are exactly the ones that
    ///   make a server decode.
    /// - `acceptable_audio_codecs` — AAC/AC-3/E-AC-3 pass through; TrueHD, DTS
    ///   and FLAC are the formats that force a server-side audio transcode.
    /// - `acceptable_containers` / `output_container` — AVI, WMV and FLV force
    ///   a remux at least.
    ///
    /// ## What differs, and why
    /// Two of the default's ceilings are **size** judgements, not
    /// compatibility ones, and applying them on ingest would destroy quality
    /// for a reason unrelated to direct play:
    ///
    /// - **Resolution ceiling raised to 3840x2160.** A 4K client direct-plays
    ///   4K. Downscaling a 4K source to 1080p on the way in — and then, on
    ///   Path A, *deleting the original* — is an irreversible quality loss
    ///   imposed to solve a problem (bandwidth) that is per-session and is not
    ///   a property of the file. Sources above 4K are still capped, because
    ///   nothing in this fleet displays 8K.
    /// - **Bitrate ceiling raised to 100 Mbps.** Same reasoning: a high
    ///   bitrate never prevents direct play, it only costs bandwidth. The
    ///   ceiling is kept rather than removed so that a genuinely pathological
    ///   file (an uncompressed or intra-only remux) is still caught.
    ///
    /// The audio channel ceiling is **kept at 6**: a 7.1 or object-based track
    /// is a real direct-play blocker on most clients in this fleet, which is
    /// the original field's own stated justification.
    ///
    /// This policy is the input to the ordinary [`crate::foundry::plan`] /
    /// [`crate::foundry::forge`] path — Path A reuses that machinery whole and
    /// adds nothing to it but this target and the deletion rule in
    /// [`crate::foundry::directplay`].
    pub fn direct_play_normalization() -> Self {
        Self {
            max_width: 3840,
            max_height: 2160,
            max_video_bitrate_bps: 100_000_000,
            ..Self::default()
        }
    }

    /// The effective video-bitrate ceiling: the stated maximum plus the
    /// tolerance slack.
    ///
    /// A non-finite or below-1.0 tolerance is clamped to 1.0 rather than
    /// trusted. Below 1.0 it would make the *effective* ceiling tighter than
    /// the stated one — ordering re-encodes of files the operator was told
    /// were within policy — and a NaN would make every comparison against it
    /// silently false, which is a policy that never fires at all. Neither
    /// failure is one a caller would notice, so it is clamped here rather than
    /// validated at some call site that might forget.
    pub fn effective_video_bitrate_ceiling(&self) -> u64 {
        let tol = if self.bitrate_tolerance.is_finite() && self.bitrate_tolerance >= 1.0 {
            self.bitrate_tolerance
        } else {
            1.0
        };
        // Saturating: a policy with an absurd ceiling must not wrap to a tiny
        // one, which would order a re-encode of everything.
        (self.max_video_bitrate_bps as f64 * tol).min(u64::MAX as f64) as u64
    }

    pub fn accepts_video_codec(&self, codec: &str) -> bool {
        self.acceptable_video_codecs
            .iter()
            .any(|c| c.eq_ignore_ascii_case(codec))
    }

    pub fn accepts_audio_codec(&self, codec: &str) -> bool {
        self.acceptable_audio_codecs
            .iter()
            .any(|c| c.eq_ignore_ascii_case(codec))
    }

    pub fn accepts_container(&self, container: Container) -> bool {
        self.acceptable_containers.contains(&container)
    }
}

/// Fit `(width, height)` inside `(max_width, max_height)` preserving aspect
/// ratio, **never upscaling**, and snapping both dimensions to even numbers.
///
/// Pure and separately tested because two of its three properties are silent
/// failures if wrong:
///
/// - **Even dimensions** are not cosmetic. The `yuv420p` pixel format
///   subsamples chroma 2x2, so libx264 *fails outright* on an odd dimension
///   ("width not divisible by 2"). A 1920x817 source scaled naively to fit
///   1080 height yields an odd width, and the encode dies after however long
///   it took to get there.
/// - **Never upscaling** matters because a policy ceiling is a maximum, not a
///   target. Scaling 720p up to 1080p invents pixels and grows the file.
///
/// Returns the input unchanged when it already fits.
pub fn scale_to_fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 || max_width == 0 || max_height == 0 {
        // Nothing sensible to compute; the caller (the planner) only reaches
        // here with dimensions it observed, and a zero dimension is handled as
        // undecidable before this point.
        return (width, height);
    }
    if width <= max_width && height <= max_height {
        return (width, height);
    }

    let scale = f64::min(
        max_width as f64 / width as f64,
        max_height as f64 / height as f64,
    );
    let w = (width as f64 * scale).round() as u32;
    let h = (height as f64 * scale).round() as u32;

    // Snap down to even. Down rather than up so the result can never exceed
    // the ceiling it was computed to respect.
    let even = |n: u32| if n % 2 == 0 { n } else { n.saturating_sub(1) };
    (even(w).max(2), even(h).max(2))
}

#[cfg(test)]
mod tests {

    /// Path A's policy must be the one production actually uses.
    ///
    /// It was not. `direct_play_normalization` — 4K ceiling, 100 Mbps, built
    /// by FOUNDRY-03 precisely so Path A would not destroy 4K/HDR — was
    /// referenced only from tests and doc comments. Both the validation
    /// harness and the survey ran `TranscodePolicy::default()`, which caps at
    /// 1080p and 12 Mbps.
    ///
    /// The consequence was not academic: a validation run on a real Dolby
    /// Vision file was observed emitting
    /// `-vf scale=1920:1036 -pix_fmt yuv420p` with no tone-map — downscaling
    /// 4K and forcing 10-bit HDR to 8-bit, during a run whose entire purpose
    /// was to prove 4K/HDR is handled safely.
    #[test]
    fn the_production_endpoints_use_path_as_policy_not_the_default() {
        let dash = include_str!("../web/dashboard.rs");

        // Scan the ASSIGNMENT SITES, not a "non-test section".
        //
        // dashboard.rs has four separate `#[cfg(test)]` modules and the
        // production handlers sit AFTER the first one, so splitting on it
        // hides exactly the code under test — my first version of this test
        // did that and failed for the wrong reason. Matching the assignment
        // is unambiguous and does not depend on module boundaries.
        let sites: Vec<&str> = dash
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("let policy") && l.contains("TranscodePolicy::"))
            .collect();

        assert!(
            !sites.is_empty(),
            "expected to find the policy assignments; the scan is looking in the wrong place"
        );
        for site in &sites {
            assert!(
                site.contains("direct_play_normalization()"),
                "every endpoint must use PATH A's policy. The default caps at 1080p/12Mbps/8-bit \
                 and says nothing about what Path A would do — a validation run on a real Dolby \
                 Vision file was observed emitting `-vf scale=1920:1036 -pix_fmt yuv420p` with no \
                 tone-map because of exactly this. Offending site: {site}"
            );
        }
    }

    /// The two policies must actually DIFFER in the ways that matter, or the
    /// test above is guarding a distinction without a difference.
    #[test]
    fn path_as_policy_differs_from_the_default_where_4k_is_concerned() {
        let d = TranscodePolicy::default();
        let a = TranscodePolicy::direct_play_normalization();
        assert!(
            a.max_width > d.max_width && a.max_height > d.max_height,
            "Path A must not downscale 4K: default {}x{} vs path A {}x{}",
            d.max_width, d.max_height, a.max_width, a.max_height
        );
        assert!(a.max_width >= 3840 && a.max_height >= 2160, "must admit 4K");
        assert!(
            a.max_video_bitrate_bps > d.max_video_bitrate_bps,
            "a 4K file at the default 12 Mbps ceiling would be re-encoded needlessly"
        );
    }
    use super::*;

    #[test]
    fn the_documented_defaults_are_the_actual_defaults() {
        // The policy is meant to be auditable, so the defaults its doc
        // comments promise are asserted rather than described.
        let p = TranscodePolicy::default();
        assert_eq!(p.acceptable_video_codecs, vec!["h264", "hevc"]);
        assert_eq!(p.encode_video, VideoEncoder::X264);
        assert_eq!(p.crf, 20);
        assert_eq!(p.preset, "medium");
        assert_eq!((p.max_width, p.max_height), (1920, 1080));
        assert_eq!(p.max_video_bitrate_bps, 12_000_000);
        assert_eq!(p.bitrate_tolerance, 1.25);
        assert_eq!(p.acceptable_audio_codecs, vec!["aac", "ac3", "eac3"]);
        assert_eq!(p.max_audio_channels, 6);
        assert_eq!(p.acceptable_containers, vec![Container::Matroska, Container::Mp4]);
        assert_eq!(p.output_container, Container::Matroska);
    }

    #[test]
    fn the_normalization_target_relaxes_only_the_size_ceilings() {
        // Path A's criterion is "will this direct play", which is a
        // COMPATIBILITY question. The compatibility fields must be the
        // default's, byte for byte — two copies of the accepted-codec lists is
        // two places for them to drift.
        let d = TranscodePolicy::default();
        let n = TranscodePolicy::direct_play_normalization();
        assert_eq!(n.acceptable_video_codecs, d.acceptable_video_codecs);
        assert_eq!(n.acceptable_audio_codecs, d.acceptable_audio_codecs);
        assert_eq!(n.acceptable_containers, d.acceptable_containers);
        assert_eq!(n.output_container, d.output_container);
        assert_eq!(n.encode_video, d.encode_video);
        assert_eq!(
            n.max_audio_channels, 6,
            "7.1 and object audio really are direct-play blockers, so this ceiling stays"
        );

        // ...and relaxes exactly the two that are size judgements.
        assert_eq!((n.max_width, n.max_height), (3840, 2160));
        assert_eq!(n.max_video_bitrate_bps, 100_000_000);
    }

    #[test]
    fn normalization_does_not_downscale_4k_because_a_4k_client_direct_plays_4k() {
        // Path A DELETES THE ORIGINAL. Downscaling on the way in would make
        // that deletion an irreversible quality loss imposed for a bandwidth
        // reason that is per-session and not a property of the file.
        let n = TranscodePolicy::direct_play_normalization();
        assert_eq!(scale_to_fit(3840, 2160, n.max_width, n.max_height), (3840, 2160));
        // 8K is still capped: nothing in this fleet displays it.
        assert_eq!(scale_to_fit(7680, 4320, n.max_width, n.max_height), (3840, 2160));
    }

    #[test]
    fn hevc_is_accepted_but_is_never_the_encode_target() {
        // Both halves are the documented judgement: accept what is already
        // HEVC, but never *produce* HEVC (older clients cannot direct-play it).
        let p = TranscodePolicy::default();
        assert!(p.accepts_video_codec("hevc"));
        assert_eq!(p.encode_video.ffmpeg_name(), "libx264");
        assert_eq!(p.encode_video.probe_codec_name(), "h264");
    }

    #[test]
    fn lossless_and_object_audio_are_not_accepted() {
        // These are exactly the formats that force a client-side transcode,
        // which is the problem this stage exists to remove.
        let p = TranscodePolicy::default();
        assert!(p.accepts_audio_codec("aac"));
        assert!(p.accepts_audio_codec("EAC3"), "codec matching is case-insensitive");
        assert!(!p.accepts_audio_codec("truehd"));
        assert!(!p.accepts_audio_codec("dts"));
        assert!(!p.accepts_audio_codec("flac"));
    }

    #[test]
    fn container_names_are_matched_as_a_list_not_as_a_whole_string() {
        // ffprobe reports a comma-separated demuxer list. A naive equality
        // check against "mp4" never matches a real MP4 file, which would make
        // every MP4 in the library look like an unrecognized container.
        assert_eq!(normalize_container("matroska,webm"), Some(Container::Matroska));
        assert_eq!(
            normalize_container("mov,mp4,m4a,3gp,3g2,mj2"),
            Some(Container::Mp4)
        );
        assert_eq!(normalize_container("avi"), Some(Container::Avi));
        assert_eq!(normalize_container("mpegts"), Some(Container::MpegTs));
        assert_eq!(normalize_container("asf"), Some(Container::Asf));
        assert_eq!(normalize_container("flv"), Some(Container::Flv));
    }

    #[test]
    fn an_unrecognized_container_is_none_not_a_default() {
        // Fail closed: the planner turns None into "cannot decide", so the
        // file is left alone and reported rather than remuxed on a guess.
        assert_eq!(normalize_container("ogg"), None);
        assert_eq!(normalize_container(""), None);
        assert_eq!(normalize_container("   "), None);
        assert_eq!(normalize_container("something-new-in-ffmpeg-9"), None);
    }

    #[test]
    fn the_bitrate_ceiling_includes_the_tolerance_slack() {
        let p = TranscodePolicy::default();
        // 12 Mbps * 1.25 = 15 Mbps. A file at 13 Mbps is over the stated
        // ceiling but not worth a generation loss to fix.
        assert_eq!(p.effective_video_bitrate_ceiling(), 15_000_000);
    }

    #[test]
    fn a_nonsensical_tolerance_clamps_to_one_rather_than_disabling_the_policy() {
        // A NaN tolerance would make every comparison false — a policy that
        // never fires. A sub-1.0 tolerance would make the effective ceiling
        // *tighter* than the one the operator was shown.
        let mut p = TranscodePolicy::default();

        p.bitrate_tolerance = f64::NAN;
        assert_eq!(p.effective_video_bitrate_ceiling(), 12_000_000);

        p.bitrate_tolerance = 0.5;
        assert_eq!(
            p.effective_video_bitrate_ceiling(),
            12_000_000,
            "the effective ceiling must never be tighter than the stated one"
        );

        p.bitrate_tolerance = f64::INFINITY;
        assert_eq!(p.effective_video_bitrate_ceiling(), 12_000_000);
    }

    #[test]
    fn scale_to_fit_leaves_a_conforming_file_completely_alone() {
        assert_eq!(scale_to_fit(1920, 1080, 1920, 1080), (1920, 1080));
        assert_eq!(scale_to_fit(1280, 720, 1920, 1080), (1280, 720));
    }

    #[test]
    fn scale_to_fit_never_upscales() {
        // A ceiling is a maximum, not a target: upscaling invents pixels and
        // grows the file for no information gain.
        assert_eq!(scale_to_fit(640, 480, 1920, 1080), (640, 480));
        assert_eq!(scale_to_fit(720, 480, 1920, 1080), (720, 480));
    }

    #[test]
    fn scale_to_fit_downscales_4k_preserving_aspect_ratio() {
        assert_eq!(scale_to_fit(3840, 2160, 1920, 1080), (1920, 1080));
        // 2.39:1 scope 4K -> width-bound.
        assert_eq!(scale_to_fit(3840, 1608, 1920, 1080), (1920, 804));
    }

    #[test]
    fn scale_to_fit_always_returns_even_dimensions() {
        // libx264 with yuv420p FAILS on an odd dimension. This is the case
        // that kills an encode after it has already spent the CPU: a
        // 1920x817-shaped source scaled to fit produces an odd number unless
        // it is snapped.
        for (w, h) in [
            (3840u32, 1634u32),
            (3840, 1610),
            (1921, 1081),
            (2559, 1439),
            (4096, 1717),
            (3841, 2161),
        ] {
            let (ow, oh) = scale_to_fit(w, h, 1920, 1080);
            assert_eq!(ow % 2, 0, "odd width {ow} from {w}x{h}");
            assert_eq!(oh % 2, 0, "odd height {oh} from {w}x{h}");
        }
    }

    #[test]
    fn scale_to_fit_never_exceeds_the_ceiling_it_was_given() {
        // Snapping to even must round *down*, never up past the maximum.
        for (w, h) in [(3840u32, 2160u32), (4096, 2160), (2560, 1440), (1922, 1082)] {
            let (ow, oh) = scale_to_fit(w, h, 1920, 1080);
            assert!(ow <= 1920 && oh <= 1080, "{w}x{h} -> {ow}x{oh} exceeds the ceiling");
        }
    }

    #[test]
    fn scale_to_fit_is_total_on_degenerate_input() {
        // Never panics, never divides by zero. The planner rejects zero
        // dimensions before this point, but a pure function on the destructive
        // path should not depend on that.
        assert_eq!(scale_to_fit(0, 1080, 1920, 1080), (0, 1080));
        assert_eq!(scale_to_fit(1920, 0, 1920, 1080), (1920, 0));
        assert_eq!(scale_to_fit(1920, 1080, 0, 0), (1920, 1080));
    }

    #[test]
    fn container_extensions_and_ffmpeg_formats_line_up() {
        assert_eq!(Container::Matroska.ffmpeg_format(), "matroska");
        assert_eq!(Container::Matroska.extension(), "mkv");
        assert_eq!(Container::Mp4.ffmpeg_format(), "mp4");
        assert_eq!(Container::Mp4.extension(), "mp4");
        assert_eq!(Container::MpegTs.extension(), "ts");
    }
}
