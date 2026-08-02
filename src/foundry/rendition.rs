//! FOUNDRY-03 **Path B** — format renditions: additive, per-title, and only
//! for titles the operator has explicitly marked.
//!
//! ## This is not a library policy and must never become one
//!
//! Path A (see [`crate::foundry::directplay`]) runs on every ingest. Path B
//! does not. A rendition ladder is produced **only** for a title the operator
//! marked in the interface — at a movie, a season, or a show — exactly the way
//! Plex's "Optimize" action works. Nothing here is applied to a library, a
//! scan, or a scheduled sweep.
//!
//! That is a design constraint, not a preference, and it is why the entry point
//! is [`RenditionRequest`] — a named title plus the specific rungs asked for —
//! rather than a policy object that a worker could iterate over a candidate
//! list. The library holds **16,221 media files**; a ladder applied to it would
//! be tens of thousands of encodes and several times the library's size on
//! disk. The type is shaped so the accident cannot be written casually: there
//! is no "all rungs for everything" constructor, and [`RenditionRequest::rungs`]
//! is a list the caller had to enumerate.
//!
//! ## Renditions never replace anything
//!
//! Output goes in a subfolder **beside** the source, and the source is
//! untouched. [`may_delete_original`](crate::foundry::directplay::may_delete_original)
//! is not consulted and is not relevant on this path: nothing is deleted,
//! because a rendition is a lower-quality derivative and the source remains
//! the copy of record.
//!
//! ## Nothing in this module performs I/O
//!
//! [`rendition_output_path`] computes where a rendition *would* go. It creates
//! no directory and touches no file. Execution belongs to
//! [`crate::foundry::forge`], and the paths it is given must still go through
//! [`crate::foundry::paths::PathGuard`] like every other path in Foundry.

use std::path::{Component, Path, PathBuf};

use crate::foundry::hdr::ToneMapAlgorithm;
use crate::foundry::policy::{Container, TranscodePolicy, VideoEncoder};

/// The named rungs of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RenditionName {
    Mobile,
    Web,
    Tv,
    HiFi,
}

impl RenditionName {
    /// The machine-readable name, for API fields and logs.
    /// Parse the value the UI sends. Unknown rungs are REFUSED, never
    /// defaulted: a typo silently becoming `mobile` would produce a rendition
    /// the operator did not ask for, at a quality they did not choose.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mobile" => Some(Self::Mobile),
            "web" => Some(Self::Web),
            "tv" => Some(Self::Tv),
            "hifi" => Some(Self::HiFi),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mobile => "mobile",
            Self::Web => "web",
            Self::Tv => "tv",
            Self::HiFi => "hifi",
        }
    }

    /// The directory this rendition is written into, beneath the versions
    /// subfolder.
    ///
    /// A closed set of literals with no path separators, no `..`, and no
    /// user-supplied text — which is what makes [`rendition_output_path`]
    /// unable to escape the title's own directory however strange the source
    /// filename is.
    pub fn directory_label(self) -> &'static str {
        match self {
            Self::Mobile => "Mobile",
            Self::Web => "Web",
            Self::Tv => "TV",
            Self::HiFi => "HiFi",
        }
    }

    /// Every rung, lowest to highest. Used for reporting and for the
    /// resolution-dedupe ordering — **not** as a default request. There is
    /// deliberately no constructor that turns this into "make all four".
    pub fn all() -> [RenditionName; 4] {
        [Self::Mobile, Self::Web, Self::Tv, Self::HiFi]
    }
}

/// How a rung treats video.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoTreatment {
    /// Re-encode with the given quality settings.
    Encode { crf: u8, preset: String },
    /// **Never re-encode.** The rung is a remux or nothing. The hifi rung's
    /// entire value is that it holds this variant — see [`Ladder::hifi`].
    CopyOnly,
}

/// How a rung treats audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioTreatment {
    /// Downmix to stereo. For rungs whose clients cannot use more.
    Stereo,
    /// Keep up to 5.1, re-encoding only what the target cannot carry.
    UpToFiveOne,
    /// **Copy every stream untouched.** The only treatment under which TrueHD,
    /// DTS-HD and Atmos survive, because they cannot survive a decode/encode
    /// at all.
    Preserve,
}

impl AudioTreatment {
    /// The channel ceiling this treatment imposes.
    pub fn max_channels(self) -> u32 {
        match self {
            Self::Stereo => 2,
            Self::UpToFiveOne => 6,
            // Not `u32::MAX`: a ceiling that large is indistinguishable from
            // an absent check when read in a log. 16 is above any real layout
            // and is still visibly a number somebody chose.
            Self::Preserve => 16,
        }
    }

    /// The AAC bitrate used when this treatment has to re-encode, or `None`
    /// when it never re-encodes.
    ///
    /// 160 kbps for stereo AAC-LC is the point above which listeners stop
    /// distinguishing it from source in blind tests; 128 would save bandwidth
    /// the video is already spending far more of. 384 kbps for 5.1 is 64 kbps
    /// per channel, the usual working figure for multichannel AAC — noticeably
    /// below AC-3's 448, but AC-3 is not what the shared argv builder emits
    /// (see [`crate::foundry::ladder::build_rendition_args`]).
    pub fn encode_bitrate_bps(self) -> Option<u64> {
        match self {
            Self::Stereo => Some(160_000),
            Self::UpToFiveOne => Some(384_000),
            Self::Preserve => None,
        }
    }

    /// Audio codecs this treatment leaves alone.
    pub fn acceptable_codecs(self) -> Vec<String> {
        let list: &[&str] = match self {
            // AAC only for the stereo rungs. AC-3 stereo exists but is
            // pointless and less widely supported in browsers than AAC.
            Self::Stereo => &["aac"],
            Self::UpToFiveOne => &["aac", "ac3", "eac3"],
            // Everything a home release realistically carries. A codec outside
            // this list makes the hifi rung refuse rather than re-encode,
            // which is the correct outcome for a rung defined by not
            // re-encoding.
            Self::Preserve => &[
                "aac", "ac3", "eac3", "truehd", "dts", "mlp", "flac", "opus", "vorbis",
                "pcm_s16le", "pcm_s24le", "pcm_bluray", "pcm_dvd",
            ],
        };
        list.iter().map(|s| s.to_string()).collect()
    }
}

/// How a rung treats dynamic range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicRangeTreatment {
    /// Tone-map HDR down to SDR BT.709. The rung's client cannot display HDR,
    /// and passing HDR through to it produces a washed-out picture.
    ToneMapToSdr,
    /// **Never tone-map, never touch the grade.** HDR stays HDR, Dolby Vision
    /// stays Dolby Vision. Only meaningful in combination with
    /// [`VideoTreatment::CopyOnly`], because no encoder available to this
    /// fleet can re-encode and preserve either.
    Preserve,
}

/// One rung of the ladder.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendition {
    pub name: RenditionName,
    pub max_width: u32,
    pub max_height: u32,
    pub video: VideoTreatment,
    /// Video bitrate ceiling, and the `-maxrate` cap on the encode.
    pub max_video_bitrate_bps: u64,
    pub audio: AudioTreatment,
    pub dynamic_range: DynamicRangeTreatment,
    pub container: Container,
}

impl Rendition {
    /// Whether this rung ever re-encodes video.
    pub fn re_encodes_video(&self) -> bool {
        matches!(self.video, VideoTreatment::Encode { .. })
    }

    /// Express this rung as a [`TranscodePolicy`], so that
    /// [`crate::foundry::plan::plan_transcode`] — with all of its undecidable
    /// handling, its never-a-fallthrough `AlreadyOptimal`, and its non-empty
    /// reasons — makes the per-rung decision, rather than a second, parallel
    /// decision procedure written here.
    ///
    /// ## Why H.264 only, on the encoding rungs
    /// [`TranscodePolicy::default`] accepts HEVC because it is judging whether
    /// to *leave a file alone*. A rendition is the opposite question: the file
    /// is being created specifically so a named class of client can play it,
    /// and HEVC in a browser or on an older phone cannot be relied on. So the
    /// encoding rungs accept only `h264`, which means an HEVC source is
    /// re-encoded for them — deliberately, that is the point of the rung — and
    /// an H.264 source that already fits the ceiling comes back
    /// `AlreadyOptimal` and is skipped, because the source itself already
    /// serves that client.
    pub fn as_policy(&self) -> TranscodePolicy {
        let (crf, preset) = match &self.video {
            VideoTreatment::Encode { crf, preset } => (*crf, preset.clone()),
            // Unused: a CopyOnly rung that reached an encode is refused by the
            // ladder before any argv is built. Chosen to be visibly wrong
            // rather than plausible, so that if one ever DID appear in a
            // command line it is recognisable as a bug.
            VideoTreatment::CopyOnly => (0, "veryslow".to_string()),
        };
        TranscodePolicy {
            acceptable_video_codecs: match self.video {
                VideoTreatment::Encode { .. } => vec!["h264".to_string()],
                // The hifi rung accepts anything a modern release carries, so
                // that the only thing which can order work is the container.
                VideoTreatment::CopyOnly => {
                    ["h264", "hevc", "av1", "vp9", "vc1", "mpeg2video", "mpeg4"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                }
            },
            encode_video: VideoEncoder::X264,
            crf,
            preset,
            max_width: self.max_width,
            max_height: self.max_height,
            max_video_bitrate_bps: self.max_video_bitrate_bps,
            bitrate_tolerance: 1.0,
            acceptable_audio_codecs: self.audio.acceptable_codecs(),
            max_audio_channels: self.audio.max_channels(),
            acceptable_containers: vec![self.container],
            output_container: self.container,
        }
    }
}

/// The ladder: the rung definitions. **Not** a schedule, and not a policy that
/// anything iterates a library with — see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct Ladder {
    pub mobile: Rendition,
    pub web: Rendition,
    pub tv: Rendition,
    pub hifi: Rendition,
    /// The tone-mapping curve used by the SDR rungs.
    pub tone_map_algorithm: ToneMapAlgorithm,
    /// Where renditions are placed relative to the source.
    pub layout: RenditionLayout,
    /// When two requested rungs would resolve to the same output resolution
    /// for this particular source, keep only the higher-quality one.
    ///
    /// Default **true**. On a 480p source, "web" (720p ceiling) and "tv"
    /// (1080p ceiling) both leave the picture at 480p and differ only in CRF
    /// and audio treatment — two near-identical files where the operator
    /// wanted a ladder. The skipped rung is reported with the rung that
    /// superseded it, so a client asking for `web` knows to fall back to `tv`
    /// rather than finding nothing.
    pub dedupe_by_resolution: bool,
}

impl Ladder {
    pub fn get(&self, name: RenditionName) -> &Rendition {
        match name {
            RenditionName::Mobile => &self.mobile,
            RenditionName::Web => &self.web,
            RenditionName::Tv => &self.tv,
            RenditionName::HiFi => &self.hifi,
        }
    }

    /// **mobile** — a phone, over cellular or hotel wifi.
    ///
    /// 640x360 at 1.2 Mbps. 360p is where H.264 still holds together at phone
    /// pixel density held at arm's length; 480p costs roughly double the
    /// bitrate for a difference that panel cannot resolve, and the rung exists
    /// precisely for the case where bandwidth is the binding constraint. CRF
    /// 26 and the `veryfast` preset because at this resolution the encoder is
    /// not the quality bottleneck and the rung should not monopolise the
    /// shared encode host — this is the one rung whose value is being
    /// *available*, not being good.
    ///
    /// Stereo, because no phone speaker or Bluetooth path renders more; a 5.1
    /// track shipped here is downmixed by the client anyway, usually worse
    /// than a proper encoder would.
    ///
    /// MP4, not Matroska: iOS will not play MKV through AVPlayer or Safari at
    /// all, and the mobile rung is the one most likely to be consumed in a
    /// browser.
    pub fn mobile() -> Rendition {
        Rendition {
            name: RenditionName::Mobile,
            max_width: 640,
            max_height: 360,
            video: VideoTreatment::Encode {
                crf: 26,
                preset: "veryfast".to_string(),
            },
            max_video_bitrate_bps: 1_200_000,
            audio: AudioTreatment::Stereo,
            dynamic_range: DynamicRangeTreatment::ToneMapToSdr,
            container: Container::Mp4,
        }
    }

    /// **web** — a browser, typically remote, over the operator's *upstream*.
    ///
    /// 1280x720 at 3.5 Mbps. The binding constraint for remote streaming is
    /// the house's upload capacity, not the viewer's download: a residential
    /// upstream is commonly 10-40 Mbps and shared with everything else. 720p
    /// at 3.5 Mbps leaves room for a second concurrent viewer, which 1080p at
    /// 8 Mbps does not. CRF 23 is ffmpeg's own default and is the right place
    /// for a rung watched attentively on a laptop but which is not the
    /// living-room copy.
    ///
    /// Stereo again, and for a specific reason: browsers downmix multichannel
    /// AAC unpredictably, and a 5.1 track played through a browser is a common
    /// source of "the dialogue is inaudible" — the centre channel goes
    /// missing. A properly encoded stereo downmix does not have that failure.
    ///
    /// MP4 for Media Source Extensions compatibility.
    pub fn web() -> Rendition {
        Rendition {
            name: RenditionName::Web,
            max_width: 1280,
            max_height: 720,
            video: VideoTreatment::Encode {
                crf: 23,
                preset: "medium".to_string(),
            },
            max_video_bitrate_bps: 3_500_000,
            audio: AudioTreatment::Stereo,
            dynamic_range: DynamicRangeTreatment::ToneMapToSdr,
            container: Container::Mp4,
        }
    }

    /// **tv** — the living-room set-top box on the local network.
    ///
    /// 1920x1080 at 8 Mbps, CRF 20. The resolution matches
    /// [`TranscodePolicy::default`]'s display tier and CRF 20 matches its
    /// justification: a deliberate step better than ffmpeg's 23 default,
    /// because this is watched on the largest screen in the house. The bitrate
    /// ceiling is *tighter* than that policy's 12 Mbps, and the difference is
    /// principled rather than arbitrary — 12 Mbps there is the threshold above
    /// which an existing file is judged wasteful enough to be worth destroying
    /// quality to fix, whereas 8 Mbps here is a target for a file being
    /// created from scratch. A ceiling for leaving things alone and a target
    /// for making something new are not the same number.
    ///
    /// **5.1 is kept, not downmixed.** This is the one encoding rung whose
    /// client has a real speaker layout, and downmixing would make the rung
    /// worse than the source for the exact use it exists for. Note the
    /// limitation recorded on
    /// [`crate::foundry::ladder::build_rendition_args`]: the argv builder's
    /// audio encoder is AAC, so a re-encoded surround track here becomes 5.1
    /// AAC rather than AC-3.
    ///
    /// Matroska, so PGS and ASS subtitle tracks and their fonts survive.
    pub fn tv() -> Rendition {
        Rendition {
            name: RenditionName::Tv,
            max_width: 1920,
            max_height: 1080,
            video: VideoTreatment::Encode {
                crf: 20,
                preset: "medium".to_string(),
            },
            max_video_bitrate_bps: 8_000_000,
            audio: AudioTreatment::UpToFiveOne,
            dynamic_range: DynamicRangeTreatment::ToneMapToSdr,
            container: Container::Matroska,
        }
    }

    /// **hifi** — UHD/HDR/Dolby/Atmos content for a proper display chain.
    ///
    /// ## This rung's defining property is that it does not re-encode
    ///
    /// It is [`VideoTreatment::CopyOnly`] and [`AudioTreatment::Preserve`], and
    /// that is not conservatism, it is the only correct design. Nothing
    /// available to this fleet can re-encode this content without destroying
    /// what makes it worth having:
    ///
    /// - **Dolby Vision** — no encoder here can carry an RPU. A re-encode
    ///   turns profile 5 into a green/purple picture and profile 8 into plain
    ///   HDR10.
    /// - **HDR10** — libx264 has no reliable path for the static metadata, and
    ///   the pipeline's `-pix_fmt yuv420p` would truncate PQ to 8 bits and
    ///   destroy the grade outright.
    /// - **Atmos / TrueHD / DTS-HD** — object and lossless audio do not
    ///   survive a decode-encode cycle by definition. There is no "high
    ///   bitrate" setting that recovers them.
    ///
    /// So the hifi rung is a **remux rung**: it can put the source in a
    /// container the hifi client direct-plays, and otherwise it does nothing.
    /// Its most common answer is "the source is already this" — and that is
    /// the rung working correctly, not failing. A rung that never re-encodes
    /// cannot produce a broken HDR file, which is the whole point.
    ///
    /// The 7680x4320 ceiling is nominal: nothing is ever downscaled by this
    /// rung, because a downscale would require an encode, which it refuses.
    /// The bitrate ceiling is likewise set high enough never to fire — a
    /// bitrate objection would be an objection to file size, and this rung's
    /// premise is that size is the price of the thing.
    pub fn hifi() -> Rendition {
        Rendition {
            name: RenditionName::HiFi,
            max_width: 7680,
            max_height: 4320,
            video: VideoTreatment::CopyOnly,
            max_video_bitrate_bps: 10_000_000_000,
            audio: AudioTreatment::Preserve,
            dynamic_range: DynamicRangeTreatment::Preserve,
            container: Container::Matroska,
        }
    }
}

impl Default for Ladder {
    /// See each rung's constructor for why it holds the values it does.
    fn default() -> Self {
        Self {
            mobile: Self::mobile(),
            web: Self::web(),
            tv: Self::tv(),
            hifi: Self::hifi(),
            tone_map_algorithm: ToneMapAlgorithm::Hable,
            layout: RenditionLayout::default(),
            dedupe_by_resolution: true,
        }
    }
}

// --- Output placement ------------------------------------------------------

/// Where renditions live relative to their source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenditionLayout {
    /// The subfolder created beside the source file, e.g. `"Muse Versions"`.
    ///
    /// Plex uses `Plex Versions/Optimized for <profile>/<original name>`, and
    /// this mirrors that shape deliberately: media scanners, the *arr tools
    /// and the operator all already understand "a versions folder beside the
    /// film".
    pub subfolder: String,
}

impl Default for RenditionLayout {
    fn default() -> Self {
        Self {
            // Named for Muse rather than reusing Plex's own folder name, so
            // that Muse's output can never be confused with — or cleaned up
            // by — Plex's own optimizer.
            subfolder: "Muse Versions".to_string(),
        }
    }
}

/// Why a rendition path could not be modelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathModelError {
    /// The source path has no parent directory to place a sibling folder in.
    NoParentDirectory { source: String },
    /// The source path has no filename.
    NoFileName { source: String },
    /// **The source is itself inside a renditions folder.**
    ///
    /// The footgun this check exists for: a library scanner ingests
    /// `Muse Versions/Mobile/Film.mp4` as a title, which is then marked for
    /// optimization, which produces
    /// `Muse Versions/Mobile/Muse Versions/Mobile/Film.mp4`, and so on. Each
    /// generation is a re-encode of a re-encode, so quality collapses while
    /// disk usage grows. Refusing at the path level stops it at the first
    /// step, wherever the request came from.
    SourceIsItselfARendition { source: String, subfolder: String },
    /// The layout's subfolder name is empty or contains a path separator, so
    /// it could redirect output outside the title's directory.
    UnusableSubfolderName { subfolder: String },
}

impl std::fmt::Display for PathModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoParentDirectory { source } => write!(
                f,
                "`{source}` has no parent directory, so a sibling versions folder cannot be placed"
            ),
            Self::NoFileName { source } => write!(f, "`{source}` has no file name"),
            Self::SourceIsItselfARendition { source, subfolder } => write!(
                f,
                "`{source}` is already inside a `{subfolder}` folder — refusing to make a \
                 rendition of a rendition, which would compound quality loss with every pass"
            ),
            Self::UnusableSubfolderName { subfolder } => write!(
                f,
                "the versions subfolder name `{subfolder}` is empty or contains a path \
                 separator, so output could land outside the title's own directory"
            ),
        }
    }
}

/// Where a rendition of `source_path` would be written. **Computes a path;
/// creates nothing.**
///
/// The result is always a descendant of the source's own parent directory, and
/// the source file itself is never the target — a rendition is additive by
/// construction here, not merely by intention elsewhere.
///
/// The original filename is kept and the rung becomes a directory, rather than
/// mangling the name with a suffix. That keeps the file recognisable to a
/// scanner and to the operator, and it means the rung is visible in the path
/// without depending on anyone parsing a filename convention.
pub fn rendition_output_path(
    source_path: &str,
    rung: RenditionName,
    container: Container,
    layout: &RenditionLayout,
) -> Result<PathBuf, PathModelError> {
    let sub = layout.subfolder.trim();
    // The subfolder is the one configurable string in this path, so it is the
    // one place a value could redirect output. Anything that is not exactly
    // one plain component is refused.
    let is_single_plain_component = Path::new(sub).components().count() == 1
        && matches!(Path::new(sub).components().next(), Some(Component::Normal(_)));
    if sub.is_empty() || sub.contains('/') || sub.contains('\\') || !is_single_plain_component {
        return Err(PathModelError::UnusableSubfolderName {
            subfolder: layout.subfolder.clone(),
        });
    }

    let source = Path::new(source_path);

    // Refuse before computing anything: a source already inside a versions
    // folder must not produce a path at all, so no caller can act on one.
    if source.components().any(|c| {
        matches!(c, Component::Normal(n) if n.to_string_lossy().eq_ignore_ascii_case(sub))
    }) {
        return Err(PathModelError::SourceIsItselfARendition {
            source: source_path.to_string(),
            subfolder: sub.to_string(),
        });
    }

    let parent = source
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| PathModelError::NoParentDirectory {
            source: source_path.to_string(),
        })?;
    let stem = source
        .file_stem()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PathModelError::NoFileName {
            source: source_path.to_string(),
        })?;

    let mut out = parent.to_path_buf();
    out.push(sub);
    out.push(rung.directory_label());
    out.push(stem);
    out.set_extension(container.extension());
    Ok(out)
}

/// A request to build renditions for **one title's file**.
///
/// The operator marked this title in the interface and named the rungs. There
/// is no variant of this type that means "the whole library", and adding one
/// would be a design regression rather than a convenience.
#[derive(Debug, Clone, PartialEq)]
pub struct RenditionRequest {
    /// The source media file. One file — a season or show marked in the UI
    /// expands to one request per episode at the call site, where the
    /// expansion is visible and countable, rather than hidden in here.
    pub source_path: String,
    /// The rungs the operator asked for. Order is not significant; duplicates
    /// are collapsed by the planner.
    pub rungs: Vec<RenditionName>,
}

impl RenditionRequest {
    pub fn new(source_path: impl Into<String>, rungs: Vec<RenditionName>) -> Self {
        Self {
            source_path: source_path.into(),
            rungs,
        }
    }
}

#[cfg(test)]
mod tests {

    /// An unknown rung is REFUSED, never defaulted.
    ///
    /// A typo silently becoming `mobile` would produce a rendition the
    /// operator did not ask for, at a quality they did not choose — and the
    /// operator's whole constraint is that renditions are only what they
    /// explicitly requested.
    #[test]
    fn an_unknown_rung_is_refused_rather_than_defaulted() {
        assert_eq!(RenditionName::parse("mobile"), Some(RenditionName::Mobile));
        assert_eq!(RenditionName::parse(" TV "), Some(RenditionName::Tv));
        assert_eq!(RenditionName::parse("hifi"), Some(RenditionName::HiFi));
        assert_eq!(RenditionName::parse("web"), Some(RenditionName::Web));
        assert_eq!(RenditionName::parse("ultra"), None);
        assert_eq!(RenditionName::parse(""), None);
        assert_eq!(RenditionName::parse("4k"), None);
    }

    /// parse and as_str must round-trip, or a mark stored by one and read by
    /// the other would silently change rung.
    #[test]
    fn every_rung_round_trips_through_its_string_form() {
        for r in RenditionName::all() {
            assert_eq!(
                RenditionName::parse(r.as_str()),
                Some(r),
                "{} does not round-trip",
                r.as_str()
            );
        }
    }
    use super::*;

    // --- the defaults, asserted rather than described ----------------------

    #[test]
    fn the_documented_rung_defaults_are_the_actual_defaults() {
        let l = Ladder::default();
        assert_eq!((l.mobile.max_width, l.mobile.max_height), (640, 360));
        assert_eq!(l.mobile.max_video_bitrate_bps, 1_200_000);
        assert_eq!(l.mobile.audio, AudioTreatment::Stereo);
        assert_eq!(l.mobile.container, Container::Mp4);

        assert_eq!((l.web.max_width, l.web.max_height), (1280, 720));
        assert_eq!(l.web.max_video_bitrate_bps, 3_500_000);
        assert_eq!(l.web.container, Container::Mp4);

        assert_eq!((l.tv.max_width, l.tv.max_height), (1920, 1080));
        assert_eq!(l.tv.max_video_bitrate_bps, 8_000_000);
        assert_eq!(l.tv.audio, AudioTreatment::UpToFiveOne);
        assert_eq!(l.tv.container, Container::Matroska);

        assert_eq!(l.hifi.video, VideoTreatment::CopyOnly);
        assert_eq!(l.hifi.audio, AudioTreatment::Preserve);
        assert_eq!(l.hifi.dynamic_range, DynamicRangeTreatment::Preserve);
        assert_eq!(l.hifi.container, Container::Matroska);
    }

    #[test]
    fn the_quality_ladder_actually_ascends() {
        // A "ladder" whose rungs are not ordered is not a ladder. Each rung up
        // must be at least as large and at least as generous on bitrate.
        let l = Ladder::default();
        for (lower, upper) in [(&l.mobile, &l.web), (&l.web, &l.tv), (&l.tv, &l.hifi)] {
            assert!(
                upper.max_width >= lower.max_width && upper.max_height >= lower.max_height,
                "{:?} is not above {:?} in resolution",
                upper.name,
                lower.name
            );
            assert!(
                upper.max_video_bitrate_bps >= lower.max_video_bitrate_bps,
                "{:?} is not above {:?} in bitrate",
                upper.name,
                lower.name
            );
        }
        // ...and CRF descends (lower CRF = better) across the encoding rungs.
        let crf = |r: &Rendition| match &r.video {
            VideoTreatment::Encode { crf, .. } => *crf,
            VideoTreatment::CopyOnly => 0,
        };
        assert!(crf(&l.mobile) > crf(&l.web) && crf(&l.web) > crf(&l.tv));
    }

    #[test]
    fn only_the_hifi_rung_preserves_dynamic_range_and_audio() {
        // The load-bearing asymmetry. If a second rung ever gains Preserve
        // without gaining CopyOnly, it would claim to keep HDR while
        // re-encoding it away.
        let l = Ladder::default();
        for r in [&l.mobile, &l.web, &l.tv] {
            assert_eq!(
                r.dynamic_range,
                DynamicRangeTreatment::ToneMapToSdr,
                "{:?}",
                r.name
            );
            assert!(r.re_encodes_video(), "{:?}", r.name);
            assert_ne!(r.audio, AudioTreatment::Preserve, "{:?}", r.name);
        }
        assert!(!l.hifi.re_encodes_video());
    }

    #[test]
    fn preserving_dynamic_range_is_only_ever_paired_with_never_re_encoding() {
        // Stated as a property over the whole ladder, because the pairing is
        // what makes the hifi promise true: no encoder here can re-encode and
        // keep HDR or Dolby Vision, so Preserve + Encode would be a lie.
        let l = Ladder::default();
        for r in [&l.mobile, &l.web, &l.tv, &l.hifi] {
            if r.dynamic_range == DynamicRangeTreatment::Preserve {
                assert!(
                    !r.re_encodes_video(),
                    "{:?} claims to preserve dynamic range while re-encoding",
                    r.name
                );
                assert_eq!(r.audio, AudioTreatment::Preserve, "{:?}", r.name);
            }
        }
    }

    // --- as_policy ---------------------------------------------------------

    #[test]
    fn the_encoding_rungs_accept_only_h264_so_an_hevc_source_is_actually_re_encoded() {
        // A rendition exists so a NAMED CLIENT can play it. Accepting HEVC —
        // as the leave-it-alone policy does — would let a "mobile" rendition
        // be an HEVC file no phone browser can play.
        let l = Ladder::default();
        for r in [&l.mobile, &l.web, &l.tv] {
            let p = r.as_policy();
            assert!(p.accepts_video_codec("h264"), "{:?}", r.name);
            assert!(!p.accepts_video_codec("hevc"), "{:?}", r.name);
        }
        // The hifi rung is the opposite: it accepts what it finds, because it
        // will not re-encode either way.
        assert!(l.hifi.as_policy().accepts_video_codec("hevc"));
        assert!(l.hifi.as_policy().accepts_video_codec("av1"));
    }

    #[test]
    fn only_the_hifi_policy_tolerates_lossless_and_object_audio() {
        let l = Ladder::default();
        for codec in ["truehd", "dts", "flac"] {
            assert!(!l.mobile.as_policy().accepts_audio_codec(codec));
            assert!(!l.web.as_policy().accepts_audio_codec(codec));
            assert!(!l.tv.as_policy().accepts_audio_codec(codec));
            assert!(
                l.hifi.as_policy().accepts_audio_codec(codec),
                "hifi must pass `{codec}` through untouched — it cannot survive a re-encode"
            );
        }
    }

    #[test]
    fn the_stereo_rungs_cap_channels_at_two_and_the_tv_rung_keeps_five_one() {
        let l = Ladder::default();
        assert_eq!(l.mobile.as_policy().max_audio_channels, 2);
        assert_eq!(l.web.as_policy().max_audio_channels, 2);
        assert_eq!(
            l.tv.as_policy().max_audio_channels,
            6,
            "the one encoding rung with a real speaker layout must not downmix"
        );
        assert!(l.hifi.as_policy().max_audio_channels >= 8);
    }

    #[test]
    fn a_rungs_policy_carries_its_own_quality_settings_rather_than_the_default() {
        // If as_policy dropped these, every rung would encode identically and
        // the ladder would be four copies of one file.
        let l = Ladder::default();
        assert_eq!(l.mobile.as_policy().crf, 26);
        assert_eq!(l.mobile.as_policy().preset, "veryfast");
        assert_eq!(l.tv.as_policy().crf, 20);
        assert_eq!(l.web.as_policy().max_video_bitrate_bps, 3_500_000);
        assert_eq!(l.mobile.as_policy().max_width, 640);
    }

    #[test]
    fn a_rung_policy_has_no_bitrate_tolerance_slack() {
        // The single-target policy's 1.25x slack exists so a file 2% over a
        // ceiling is not DESTROYED to enforce a round number. A rendition is
        // created from scratch, so there is nothing to destroy and the ceiling
        // is simply the target.
        let l = Ladder::default();
        assert_eq!(l.tv.as_policy().bitrate_tolerance, 1.0);
        assert_eq!(l.tv.as_policy().effective_video_bitrate_ceiling(), 8_000_000);
    }

    // --- output placement --------------------------------------------------

    #[test]
    fn a_rendition_goes_in_a_subfolder_beside_the_source_never_over_it() {
        let layout = RenditionLayout::default();
        let src = "/srv/media/Movies/The Thing (1982)/The Thing (1982).mkv";
        let out =
            rendition_output_path(src, RenditionName::Mobile, Container::Mp4, &layout).unwrap();
        assert_eq!(
            out,
            PathBuf::from(
                "/srv/media/Movies/The Thing (1982)/Muse Versions/Mobile/The Thing (1982).mp4"
            )
        );
        // The properties that matter, stated independently of the exact string:
        assert_ne!(
            out,
            PathBuf::from(src),
            "a rendition must never target its own source"
        );
        assert!(
            out.starts_with("/srv/media/Movies/The Thing (1982)"),
            "output must stay inside the title's own directory"
        );
    }

    #[test]
    fn each_rung_gets_its_own_directory_and_its_own_container_extension() {
        let layout = RenditionLayout::default();
        let src = "/srv/media/Movies/Dune (2021)/Dune (2021).mkv";
        let paths: Vec<PathBuf> = RenditionName::all()
            .iter()
            .map(|r| {
                let c = if matches!(r, RenditionName::Mobile | RenditionName::Web) {
                    Container::Mp4
                } else {
                    Container::Matroska
                };
                rendition_output_path(src, *r, c, &layout).unwrap()
            })
            .collect();
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len(), "rungs must not collide: {paths:?}");
        assert!(paths[0].ends_with("Mobile/Dune (2021).mp4"), "{:?}", paths[0]);
        assert!(paths[3].ends_with("HiFi/Dune (2021).mkv"), "{:?}", paths[3]);
    }

    #[test]
    fn a_source_already_inside_a_versions_folder_is_refused() {
        // THE recursion footgun: a scanner ingests a rendition as a title, the
        // operator marks it, and each generation re-encodes a re-encode while
        // disk usage grows.
        let layout = RenditionLayout::default();
        let src = "/srv/media/Movies/Dune (2021)/Muse Versions/Mobile/Dune (2021).mp4";
        let e =
            rendition_output_path(src, RenditionName::Web, Container::Mp4, &layout).unwrap_err();
        assert!(
            matches!(e, PathModelError::SourceIsItselfARendition { .. }),
            "got {e:?}"
        );
        assert!(e.to_string().contains("rendition of a rendition"), "got {e}");
    }

    #[test]
    fn the_recursion_check_is_case_insensitive_because_filesystems_here_are_not_uniform() {
        let layout = RenditionLayout::default();
        for src in [
            "/srv/media/Movies/X/muse versions/Mobile/X.mp4",
            "/srv/media/Movies/X/MUSE VERSIONS/TV/X.mkv",
        ] {
            assert!(
                matches!(
                    rendition_output_path(src, RenditionName::Web, Container::Mp4, &layout),
                    Err(PathModelError::SourceIsItselfARendition { .. })
                ),
                "{src}"
            );
        }
    }

    #[test]
    fn a_subfolder_name_that_could_escape_the_directory_is_refused() {
        // The subfolder is configurable, so it is the one place operator input
        // could redirect output. `..` or a separator must not be usable.
        let src = "/srv/media/Movies/X/X.mkv";
        for bad in ["", "   ", "..", "../../etc", "a/b", "/absolute", "."] {
            let layout = RenditionLayout {
                subfolder: bad.to_string(),
            };
            let r = rendition_output_path(src, RenditionName::Web, Container::Mp4, &layout);
            assert!(
                matches!(r, Err(PathModelError::UnusableSubfolderName { .. })),
                "subfolder {bad:?} must be refused, got {r:?}"
            );
        }
    }

    #[test]
    fn a_source_with_no_parent_directory_is_refused_rather_than_defaulted() {
        let layout = RenditionLayout::default();
        for src in ["X.mkv", ""] {
            assert!(
                rendition_output_path(src, RenditionName::Web, Container::Mp4, &layout).is_err(),
                "{src:?}"
            );
        }
    }

    #[test]
    fn an_awkward_filename_does_not_change_the_shape_of_the_path() {
        // Library filenames contain quotes, dots, brackets and dashes. The
        // rung directory and the extension must still land where they should.
        let layout = RenditionLayout::default();
        let src =
            "/srv/media/Movies/It's a \"Wonderful\" Life (1946)/It's a \"Wonderful\" Life. 1946.mkv";
        let out =
            rendition_output_path(src, RenditionName::Tv, Container::Matroska, &layout).unwrap();
        assert!(
            out.to_string_lossy().contains("/Muse Versions/TV/"),
            "{out:?}"
        );
        assert_eq!(out.extension().unwrap(), "mkv");
        assert!(
            out.file_stem().unwrap().to_string_lossy().contains("Wonderful"),
            "{out:?}"
        );
    }

    #[test]
    fn a_request_names_a_title_and_its_rungs_and_cannot_name_a_library() {
        // The type-level statement of the scope rule: there is no constructor
        // meaning "everything". This test is here so adding one is a
        // deliberate act that breaks a stated expectation.
        let r = RenditionRequest::new("/srv/media/Movies/X/X.mkv", vec![RenditionName::Mobile]);
        assert_eq!(r.rungs, vec![RenditionName::Mobile]);
        assert_eq!(r.source_path, "/srv/media/Movies/X/X.mkv");
    }

    #[test]
    fn the_rung_directory_labels_are_distinct_and_contain_no_separators() {
        let labels: Vec<&str> = RenditionName::all()
            .iter()
            .map(|r| r.directory_label())
            .collect();
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
        for l in labels {
            assert!(
                !l.contains('/') && !l.contains('\\') && !l.contains(".."),
                "{l}"
            );
        }
    }
}
