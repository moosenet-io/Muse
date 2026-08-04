//! Describe a media file: the `ffprobe` invocation and, separately, the pure
//! parser for its output.
//!
//! Built as `foundry::probe` (S128 MUSEF-02) and **promoted unchanged** to
//! `crate::media::probe` by S130-A MPRB-01, because nothing in it is
//! curation-specific and Foundry is inert on a stock deployment. Foundry still
//! consumes it through the permanent re-export shim in [`crate::foundry`].
//!
//! ## The split, and why it is not cosmetic
//! `ffprobe` is **not installed on the dev box**, which is where this suite is
//! written and where its test gate runs. If parsing lived inside the
//! invocation, none of it could be tested there — the parser would ship
//! unexercised, on the one code path that decides whether a file gets
//! re-encoded. So [`parse_probe_json`] is a pure `&str -> Result` function
//! tested against captured `ffprobe` output, and [`run_ffprobe`] is the thin,
//! untestable-here layer that produces that `&str`.
//!
//! **CORRECTION (S130-A `MPRB-04`, 2026-08-04).** An earlier version of this
//! paragraph also said ffprobe is not installed on <host>. **That was wrong.**
//! `ffprobe 5.1.9-0+deb12u1` is installed on <host> — the deployment host, which
//! mounts the library — and running it there is how the golden corpus in
//! [`crate::media::probe_golden`] was captured. The architectural argument
//! above is unchanged and the split stays: the host that RUNS Muse and the host
//! that TESTS it are different machines, and only the second one's capabilities
//! decide what a test can do. But the factual claim was stale, it was repeated
//! in five places in this file, and it was used to justify leaving several
//! things unverified that could have been verified all along.
//!
//! ## What this module refuses to do
//! It never returns a *partial* or *empty* [`MediaProbe`] to paper over a
//! failure. A probe that did not happen, or whose output did not parse, is a
//! [`ProbeError`] — never a `MediaProbe` with empty stream lists, which the
//! planner downstream would read as "this file has no video" and act on. The
//! same rule as the rest of the media core: an unobserved fact is reported as
//! unobserved, not as a benign default.
//!
//! ## Two invocation paths, one parser (S130-A MPRB-02)
//! [`run_ffprobe`] is synchronous and stays: Foundry's survey and validate loops
//! are ordinary blocking code. [`run_ffprobe_async`] is for tokio callers, where
//! parking a worker thread for up to the whole deadline costs every other task
//! in the process, not just this probe. They share the argv builder, the
//! path-shape guard, the limits and the parser; they differ only in how a child
//! is waited on and killed, and in one deliberate respect — the async path stops
//! reading at the output cap and reports it, while the synchronous generic
//! spawner drains to EOF so its OTHER callers (the encoder, the subtitle
//! extractor) can still judge a run by its exit status. Both report an over-cap
//! ffprobe as [`ProbeError::OutputTooLarge`], never as a parse failure.

use std::time::Duration;
use std::process::Command;

use serde::Deserialize;

use crate::media::paths::ResolvedPath;

/// Build the `ffprobe` CLI arguments (everything after the binary name).
///
/// Pure, so the exact argv is asserted in tests on a host with no `ffprobe`
/// — the same posture as [`crate::streaming::ffmpeg::build_args`].
///
/// `-v error` plus `-print_format json` means stdout is *only* JSON —
/// ffprobe writes diagnostics to stderr, and [`spawn_with_timeout`] captures
/// the two on separate pipes, so nothing this level emits can be interleaved
/// into the document we are about to parse.
///
/// It is `error` and **not `quiet`**, and that is the whole point. `-v quiet`
/// suppresses ffprobe's stderr entirely, which made
/// [`ProbeError::ExitFailure`]'s `stderr` field always empty: every probe
/// failure rendered as "...is not media: " — a dangling colon and no
/// diagnostic. A full-library survey produced 7 such failures and not one of
/// them said why. That violates this module's governing rule that ignorance
/// must never render as absence: a failure that cannot state its cause is
/// indistinguishable from one nobody looked into. At `-v error` those same
/// files immediately name the real fault ("EBML header parsing failed"),
/// while stdout stays clean JSON so parsing is unaffected.
///
/// `-show_format` gives the container/duration, `-show_streams` the per-stream
/// detail, `-show_chapters` the chapter list; none is the default and all
/// three are needed.
///
/// `-show_chapters` is not optional decoration: the transcode argv promises
/// `-map_chapters 0`, and a promise that is never checked is the class of
/// false claim this module exists to avoid. Chapters are not streams, so
/// `-show_streams` does not report them.
///
/// ## The `--` terminator (S130-A MPRB-02)
/// The path is preceded by a bare `--`, which ends option parsing: everything
/// after it is a positional argument no matter what it starts with. Without it
/// a file named `-loglevel` is read by ffprobe as an OPTION, not as the file to
/// describe — and a scanned library is exactly where such a name turns up,
/// because the filename comes from a release group, not from us. Passing the
/// path as its own argv element (which this already did, and which
/// `ffprobe_argv_puts_the_path_last_and_never_quotes_it` pins) defeats SHELL
/// interpretation; it does nothing about ffprobe's own getopt-alike.
///
/// **Verified against ffprobe's actual parser, not assumed.** ffprobe options
/// go through `parse_options()` in `fftools/cmdutils.c`, which contains
/// `if (opt[1] == '-' && opt[2] == '\0') { handleoptions = 0; continue; }` —
/// present in the `n5.1` tag, which is the ffprobe build on the deployment host
/// (`5.1.9-0+deb12u1`), and still present on master. This mattered enough to
/// check: ffprobe is not installed on the dev box, so no test in this suite can
/// catch a terminator ffprobe would have rejected, and an unsupported `--`
/// would have failed EVERY probe in production while the suite stayed green.
/// (It IS installed on <host> — see the module docs' correction — and every
/// capture in the golden corpus was taken with this exact argv, `--` included,
/// and succeeded. That is live evidence the terminator is accepted, though it
/// is evidence from a capture run rather than from a test.)
pub fn build_ffprobe_args(file_path: &str) -> Vec<String> {
    vec![
        "-v".to_string(),
        "error".to_string(),
        "-print_format".to_string(),
        "json".to_string(),
        "-show_format".to_string(),
        "-show_streams".to_string(),
        "-show_chapters".to_string(),
        // End of options. See the doc comment: this is load-bearing, and it is
        // why the path below cannot be reinterpreted as a flag.
        "--".to_string(),
        file_path.to_string(),
    ]
}

/// Refuse a path whose SHAPE could reach an argument parser as a flag.
///
/// [`ResolvedPath`] validates **location** — that a path lies inside an allowed
/// root — and says nothing about how it looks. Those are different questions,
/// and only one of them is answered by the guard.
///
/// Honest scope, because it would be easy to overclaim here: a `ResolvedPath`
/// comes out of `std::fs::canonicalize`, so it is absolute and begins with `/`,
/// and a leading dash is therefore **not reachable** through [`run_ffprobe`]
/// today. This check is a fail-closed second line for the other two ways in —
/// a future non-canonical or relative path source, and direct callers of
/// [`build_ffprobe_args`], which is `pub`. It is deliberately paired with the
/// `--` terminator rather than trusted instead of it: the terminator is the
/// mechanism, and this is the assertion that the mechanism was not bypassed.
///
/// Reported as [`ProbeError::Spawn`] because that is the "we did not run the
/// tool, and it is not the file's fault" bucket — and it is refused BEFORE
/// spawn, so nothing is executed with an argv we do not trust.
fn reject_flag_shaped_path(path: &str) -> Result<(), ProbeError> {
    if path.starts_with('-') {
        return Err(ProbeError::Spawn {
            binary: "ffprobe".to_string(),
            message: format!(
                "refusing to probe a path that begins with `-` ({}): it would be read as an \
                 option rather than a filename. Pass an absolute path",
                truncate_for_log(path)
            ),
        });
    }
    Ok(())
}

/// A parsed `ffprobe` description of one media file.
// MPRB-05: serde derives so `MediaInfoDoc` can persist this type WITHOUT
// restating its field list. Structural only — no field, no semantics, and no
// behaviour changes here; a field added to this struct is carried into the
// stored document automatically, which is the entire point.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// What THIS PARSER did to the document that the parsed values alone do
    /// not show (S130-A MPRB-03).
    ///
    /// Deliberately narrow, and the narrowness is the point. A note records a
    /// fact about the *reading* — a tag we cut at [`MAX_TAG_LEN`], a frame-rate
    /// field that was present but unusable, colour tags that contradict each
    /// other — so that a consumer can tell "ffprobe did not say" from "ffprobe
    /// said something we could not use". Those two render identically as
    /// `None`, and the module's governing rule is that ignorance must never be
    /// indistinguishable from absence.
    ///
    /// It is **not** an error channel and **never** changes a verdict: every
    /// field it describes is already populated (or already `None`) before the
    /// note is written, and nothing in the crate branches on the contents of
    /// this vector.
    ///
    /// It is also **not** where derivation honesty lives. "This bit depth was
    /// inferred from `pix_fmt` rather than observed" is carried in the RETURN
    /// TYPE of [`crate::media::derive::bit_depth`] instead — a caller can
    /// ignore a note, and cannot ignore an enum it has to match on. See that
    /// module for why the spec's note-based design was not followed.
    ///
    /// Bounded at [`MAX_NOTES`]; an over-cap parse says so in its last entry
    /// rather than dropping the excess silently.
    pub notes: Vec<String>,
}

/// A Matroska attachment (typically a subtitle font).
// MPRB-05: serde derives so `MediaInfoDoc` can persist this type WITHOUT
// restating its field list. Structural only — no field, no semantics, and no
// behaviour changes here; a field added to this struct is carried into the
// stored document automatically, which is the entire point.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
// MPRB-05: serde derives so `MediaInfoDoc` can persist this type WITHOUT
// restating its field list. Structural only — no field, no semantics, and no
// behaviour changes here; a field added to this struct is carried into the
// stored document automatically, which is the entire point.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
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

// MPRB-05: serde derives so `MediaInfoDoc` can persist this type WITHOUT
// restating its field list. Structural only — no field, no semantics, and no
// behaviour changes here; a field added to this struct is carried into the
// stored document automatically, which is the entire point.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// ffprobe's `level`, the codec level as an integer — 150 is HEVC 5.0,
    /// 41 is H.264 4.1 (S130-A MPRB-03).
    ///
    /// Carried raw, in ffprobe's own encoding, and **not** rescaled: H.264
    /// renders level 4.1 as `41` while HEVC renders 5.0 as `150` (the general
    /// level times 30). Normalizing the two into one "level" number would
    /// require knowing the codec at the point of parse and would produce a
    /// value that means different things for different codecs — a device
    /// profile matches level against a codec, so the codec-relative number is
    /// the one it needs.
    ///
    /// `None` when ffprobe reported no level, or reported `-99`, which is what
    /// it writes for a stream whose level it could not determine — a negative
    /// value is folded to `None` by [`as_u32`] rather than becoming a level
    /// nothing can satisfy.
    pub level: Option<u32>,
    /// ffprobe's `bits_per_raw_sample`, **as observed** (S130-A MPRB-03).
    ///
    /// The OBSERVED depth only. It is frequently absent, and it is deliberately
    /// not back-filled from `pix_fmt` here: that inference already has a home
    /// in [`crate::foundry::hdr::pixel_bit_depth`], and
    /// [`crate::media::derive::bit_depth`] is where the two are combined with
    /// the answer's provenance attached. A `None` here means "ffprobe did not
    /// state it", never "this stream has no depth".
    pub bits_per_raw_sample: Option<u8>,
    /// `r_frame_rate` as frames per second — ffprobe's *base* frame rate, the
    /// lowest rate every frame timestamp is an integer multiple of.
    ///
    /// Parsed from the `"24000/1001"` rational by [`as_ratio`], so a `"0/0"`
    /// (which ffprobe emits for a stream with no meaningful rate) is `None`
    /// rather than a division by zero. When the field was PRESENT but
    /// unusable, [`MediaProbe::notes`] says so — otherwise this `None` would be
    /// indistinguishable from ffprobe not having reported a rate at all.
    pub frame_rate_fps: Option<f64>,
    /// `avg_frame_rate` as frames per second — duration divided by frame count.
    ///
    /// Kept SEPARATE from [`Self::frame_rate_fps`] rather than collapsed into
    /// one "frame rate", because the two disagreeing is itself the fact: a
    /// telecined or variable-frame-rate file has `r_frame_rate` far above its
    /// average, and a device profile that matches the base rate against a
    /// 30fps ceiling would refuse a file that plays at 23.976.
    pub avg_frame_rate_fps: Option<f64>,
    /// `color_range`, lowercased — `"tv"` (limited, 16-235) or `"pc"` (full,
    /// 0-255).
    ///
    /// Carried because getting it wrong is visible: a full-range file played
    /// as limited crushes blacks and clips whites. Not interpreted here.
    pub color_range: Option<String>,
}

// MPRB-05: serde derives so `MediaInfoDoc` can persist this type WITHOUT
// restating its field list. Structural only — no field, no semantics, and no
// behaviour changes here; a field added to this struct is carried into the
// stored document automatically, which is the entire point.
//
// `Default` is retained from MPRB-03: its test fixtures rely on
// `..Default::default()`, so dropping it here would break them. The union of
// both sides is required — neither alone compiles.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AudioStream {
    pub index: u32,
    pub codec: String,
    pub channels: Option<u32>,
    /// ISO-639 language from `tags.language`, lowercased. `None` when the
    /// muxer wrote no tag, which is common and is not an error.
    pub language: Option<String>,
    pub bitrate_bps: Option<u64>,
    /// ffprobe's `profile`, lowercased — `"dts-hd ma"`, `"lc"`, `"ma"`
    /// (S130-A MPRB-03).
    ///
    /// This is the field that separates DTS from DTS-HD MA and AAC-LC from
    /// AAC-HE, which `codec_name` does not: ffprobe reports both DTS variants
    /// as `dts`. A client profile that accepts DTS but not DTS-HD needs this.
    pub profile: Option<String>,
    /// `sample_rate` in Hz. 48000 for almost everything; 44100 for CD-sourced
    /// audio; 96000 for some lossless tracks a client may not accept.
    pub sample_rate_hz: Option<u32>,
    /// `channel_layout` verbatim (trimmed, lowercased) — `"5.1(side)"`,
    /// `"7.1"`, or ffprobe's literal `"unknown"`.
    ///
    /// **`"unknown"` is kept, not folded to `None`.** This is the one place
    /// [`normalize_descriptor`]'s rule is deliberately not applied: for a
    /// layout, "the muxer declared the channel positions unknown" is a
    /// different and more actionable fact than "no layout field was present",
    /// and a downmix decision made on the first is not the same as one made on
    /// the second. [`Self::channels`] stays typed either way, so nothing
    /// depends on parsing this string.
    pub channel_layout: Option<String>,
    /// `disposition.default` — the track a player picks with no user choice.
    pub default: bool,
    /// `disposition.forced` — a track a player must show regardless of the
    /// user's subtitle preference (foreign-dialogue dubs).
    pub forced: bool,
}

// MPRB-05: serde derives so `MediaInfoDoc` can persist this type WITHOUT
// restating its field list. Structural only — no field, no semantics, and no
// behaviour changes here; a field added to this struct is carried into the
// stored document automatically, which is the entire point.
//
// `Default` is retained from MPRB-03: its test fixtures rely on
// `..Default::default()`, so dropping it here would break them. The union of
// both sides is required — neither alone compiles.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SubtitleStream {
    pub index: u32,
    pub codec: String,
    pub language: Option<String>,
    pub forced: bool,
    pub default: bool,
    /// `disposition.hearing_impaired` — an SDH track (S130-A MPRB-03).
    ///
    /// Carried because it changes which track is the right DEFAULT, not
    /// whether one exists: auto-selecting an SDH track for a viewer who did
    /// not ask for one puts `[door creaks]` on screen for the whole film.
    pub hearing_impaired: bool,
}

/// **The** set of subtitle codec names Muse treats as BITMAP (image) rather
/// than text. One list, one home, for every consumer.
///
/// # Why this lives here and nowhere else
/// Until `SUBCODEC-01` this rule was stated **twice** — `IMAGE_SUBTITLE_CODECS`
/// in `crate::subtitles::discover` (4 entries) and `BITMAP_SUBTITLE_CODECS` in
/// `crate::foundry::directplay` (7 entries) — and the two disagreed. A stream
/// whose codec was `pgssub` was bitmap to one module and not-bitmap to the
/// other, for the same input. A restatement is internally consistent and reads
/// correctly, which is exactly why the disagreement survived; the only durable
/// fix is that there is nothing left to disagree with. **Do not add a second
/// const "for convenience" — that is the defect, not the fix.** It lives on
/// `media::probe` because `codec` on [`SubtitleStream`] is populated here, from
/// ffprobe's `codec_name`, so this is the boundary the vocabulary belongs to.
///
/// # Membership, decided per entry — not by taking the longer list
/// ffprobe's `codec_name` is the **`AVCodecDescriptor.name`**, not a decoder
/// name: `fftools/ffprobe.c` does
/// `if (cd = avcodec_descriptor_get(par->codec_id)) print_str("codec_name", cd->name);`.
/// Descriptor names live in `libavcodec/codec_desc.c`, and **exactly four**
/// subtitle descriptors there carry `AV_CODEC_PROP_BITMAP_SUB`:
///
/// | entry | what it is | verdict |
/// |---|---|---|
/// | `hdmv_pgs_subtitle` | descriptor name, `AV_CODEC_ID_HDMV_PGS_SUBTITLE`, `PROP_BITMAP_SUB` | **canonical** — ffprobe emits this |
/// | `dvd_subtitle` | descriptor name, `AV_CODEC_ID_DVD_SUBTITLE`, `PROP_BITMAP_SUB` | **canonical** |
/// | `dvb_subtitle` | descriptor name, `AV_CODEC_ID_DVB_SUBTITLE`, `PROP_BITMAP_SUB` | **canonical** |
/// | `xsub` | descriptor name, `AV_CODEC_ID_XSUB`, `PROP_BITMAP_SUB` | **canonical** |
/// | `pgssub` | *decoder* name only — `pgssubdec.c`, `.p.name = "pgssub"` | alias, kept defensively |
/// | `dvdsub` | *decoder/encoder* name only — `dvdsubdec.c`/`dvdsubenc.c` | alias, kept defensively |
/// | `dvbsub` | *decoder/encoder* name only — `dvbsubdec.c`/`dvbsubenc.c` | alias, kept defensively |
///
/// The three aliases are **not** strings ffprobe can report as `codec_name` —
/// they do not appear in `codec_desc.c` at all. They are kept anyway, and the
/// reason is asymmetric risk, not thoroughness: failing to recognise a bitmap
/// track fails **open** in both consumers (Foundry would report no direct-play
/// blocker and would let a PGS track into an MP4 mux that cannot carry it), and
/// no *text* subtitle codec bears any of these three names, so an alias cannot
/// produce a false positive. Cost of keeping: nothing. Cost of dropping: a
/// silent wrong answer if a `codec` value ever reaches Muse from an ffmpeg
/// `-c:s` argument or a third-party tool rather than from `ffprobe`.
///
/// # What this claim rests on
/// **Source reading, plus one row now confirmed live.** Every row above was
/// verified by reading FFmpeg `master` (`libavcodec/codec_desc.c`,
/// `libavcodec/{pgssub,dvdsub,dvbsub}dec.c`, `fftools/ffprobe.c`).
///
/// S130-A `MPRB-04` then confirmed `dvd_subtitle` against a live
/// `ffprobe 5.1.9` on <host> — the `dvd_vob_data_stream` fixture in
/// [`crate::media::probe_golden`] is a real DVD rip whose subtitle streams come
/// back with exactly that `codec_name`. The other six rows remain source-read
/// only: this library contains no `dvb_subtitle` or `xsub` file to probe, and
/// the three alias rows are names ffprobe cannot emit at all. Stated
/// row-by-row rather than upgrading the whole table on one confirmation.
pub const BITMAP_SUBTITLE_CODECS: &[&str] = &[
    // canonical ffprobe `codec_name` values
    "hdmv_pgs_subtitle",
    "dvd_subtitle",
    "dvb_subtitle",
    "xsub",
    // decoder-name aliases, defensive (see table above)
    "pgssub",
    "dvdsub",
    "dvbsub",
];

/// Whether `codec` names a bitmap (image) subtitle stream.
///
/// The single matching rule as well as the single list: trimmed and
/// ASCII-case-insensitive. Before `SUBCODEC-01` the two call sites also matched
/// *differently* (`contains` over a lowercased needle vs `eq_ignore_ascii_case`),
/// so the comparison is centralised here too — a shared list with two matchers
/// can still diverge.
pub fn is_bitmap_subtitle_codec(codec: &str) -> bool {
    let codec = codec.trim();
    BITMAP_SUBTITLE_CODECS
        .iter()
        .any(|c| c.eq_ignore_ascii_case(codec))
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
    /// ffprobe wrote more than the capture cap allows, so the document we hold
    /// is a PREFIX of what it said and cannot honestly be parsed.
    ///
    /// This variant exists because of what happened without it, which is the
    /// exact class of fault this module was built to prevent. The drain capped
    /// retention **silently**; the truncated JSON then failed to parse; and the
    /// caller was handed [`ProbeError::MalformedOutput`] — an error that blames
    /// the FILE's JSON for a limit WE imposed. The operator's correct response
    /// to the two is opposite (re-mux a broken file vs. raise
    /// `MUSE_PROBE_MAX_OUTPUT_BYTES`), so reporting one as the other sends them
    /// at the wrong thing.
    ///
    /// `cap` is the limit that was actually in force, not the compiled default,
    /// so the message names the number the operator can change.
    OutputTooLarge { cap: usize },
}

/// The two states a failed probe can leave a file in, for the caller that has
/// to decide what to do next.
///
/// Named rather than stringly-typed at the call site so the vocabulary is fixed
/// in one place; `as_str()` is the wire/DB spelling and matches the
/// `probe_failed` bucket [`crate::foundry::survey`] already reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeState {
    /// We could not obtain an answer. Says nothing about the file.
    Unreadable,
    /// We obtained an answer and it was not usable. Says something about the
    /// file, and re-running will say it again.
    ProbeFailed,
}

impl ProbeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unreadable => "unreadable",
            Self::ProbeFailed => "probe_failed",
        }
    }
}

impl ProbeError {
    /// Whether running the identical probe again could plausibly succeed.
    ///
    /// The line is drawn at **whether the tool produced a verdict**. A missing
    /// binary, a spawn refusal and a timeout are all statements about THIS
    /// HOST at THIS MOMENT — ffmpeg gets installed, a fork limit passes, an NFS
    /// stall clears — so a retry is meaningful. A non-zero exit, an
    /// unparseable document, a stream-less file and an over-cap flood are all
    /// statements about the FILE (or about a limit that will not move on its
    /// own); ffprobe already answered, and it will answer identically. Retrying
    /// those is a loop that burns a worker on 16,000 items and never converges.
    ///
    /// # Exhaustive by construction
    /// The `match` below has **no wildcard arm, deliberately**. A variant added
    /// later must be classified by whoever adds it — with a `_` arm it would
    /// silently inherit whichever bucket the wildcard named, which for a new
    /// failure mode is a coin flip between "retry forever" and "give up on a
    /// file that was fine". A compile error is the cheapest possible place to
    /// discover that decision is owed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::ToolMissing { .. } | Self::Spawn { .. } | Self::Timeout { .. } => true,
            Self::ExitFailure { .. }
            | Self::MalformedOutput { .. }
            | Self::NoStreams
            | Self::OutputTooLarge { .. } => false,
        }
    }

    /// The state this failure leaves the file in.
    ///
    /// Tracks [`Self::is_retryable`] exactly — "we never got an answer" is
    /// [`ProbeState::Unreadable`], "we got one and it was unusable" is
    /// [`ProbeState::ProbeFailed`] — but is written as its own exhaustive,
    /// wildcard-free `match` rather than derived from the boolean, so the two
    /// can diverge later without one silently dragging the other with it.
    pub fn state(&self) -> ProbeState {
        match self {
            Self::ToolMissing { .. } | Self::Spawn { .. } | Self::Timeout { .. } => {
                ProbeState::Unreadable
            }
            Self::ExitFailure { .. }
            | Self::MalformedOutput { .. }
            | Self::NoStreams
            | Self::OutputTooLarge { .. } => ProbeState::ProbeFailed,
        }
    }
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
                "ffprobe binary `{binary}` is not installed on this host — media \
                 files cannot be described (set MUSE_PROBE_FFPROBE_BIN, or the \
                 older MUSE_FOUNDRY_FFPROBE_BIN, or install ffmpeg)"
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
            Self::OutputTooLarge { cap } => write!(
                f,
                "ffprobe wrote more than the {cap}-byte capture cap, so what we hold is a \
                 PREFIX of its output and was not parsed — this is OUR limit, not a \
                 statement that the file's metadata is malformed (raise \
                 MUSE_PROBE_MAX_OUTPUT_BYTES if a real file legitimately needs it)"
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
/// The **synchronous** probe entry point, and the one every blocking caller
/// uses — Foundry's survey and validate loops included, via the shim. Async
/// callers use [`run_ffprobe_async`] instead; MPRB-02 added it, so the older
/// claim that this was the crate's ONLY probe spawner no longer holds and has
/// been corrected here rather than left to rot.
///
/// Everything else about the shape is unchanged: it goes through the same argv
/// builder, the same path-shape guard and the same parser as the async twin. It
/// takes a [`ResolvedPath`], not a `&Path`, so "I forgot to validate this path"
/// is a compile error rather than a review catch (see [`crate::media::paths`]).
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
    run_ffprobe_with_limits(ffprobe_bin, path, timeout, MAX_CAPTURED_BYTES)
}

/// [`run_ffprobe`] with both operator-tunable limits given explicitly.
///
/// Split from [`run_ffprobe_with_timeout`] rather than changing its signature
/// because that function is already called with a deadline from elsewhere in
/// the crate, and because the cap is a genuinely separate decision from the
/// deadline. See [`MediaCore::probe`] for where the configured values come
/// from.
///
/// Unlike the generic [`spawn_with_timeout`], an over-cap capture here is an
/// ERROR rather than a truncation marker: what this function does next is parse
/// the bytes as JSON, and a prefix of a JSON document is not a JSON document.
/// Reporting that as `MalformedOutput` — which is what happened before MPRB-02
/// — blames the file for our limit.
pub fn run_ffprobe_with_limits(
    ffprobe_bin: &str,
    path: &ResolvedPath,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<MediaProbe, ProbeError> {
    let file_path = path.as_path().to_string_lossy().into_owned();
    reject_flag_shaped_path(&file_path)?;
    let args = build_ffprobe_args(&file_path);
    let captured = spawn_capturing(ffprobe_bin, &args, timeout, max_output_bytes)?;

    if captured.over_cap() {
        return Err(ProbeError::OutputTooLarge {
            cap: max_output_bytes,
        });
    }

    interpret_probe_output(
        captured.status.success(),
        captured.status.code(),
        &captured.stdout,
        &captured.stderr,
    )
}

/// Turn a finished `ffprobe` run into a [`MediaProbe`] or a [`ProbeError`].
///
/// Extracted by S130-A `MPRB-04` and called by **both** entry points, because
/// until then this rule was written out twice — once in
/// [`run_ffprobe_with_limits`] and once in [`run_ffprobe_async`] — and a
/// restatement is a thing that can drift. It is also the only part of the
/// invocation layer that can be exercised without a process, which is what lets
/// the golden corpus in [`crate::media::probe_golden`] test the REAL failure
/// path against the 7 files the full-library survey could not read, rather than
/// against a rule the test wrote for itself.
///
/// `success` and `code` are both taken because they are not interchangeable:
/// a child killed by a signal has `success() == false` and `code() == None`,
/// and deriving one from the other would render that case as a clean exit.
pub fn interpret_probe_output(
    success: bool,
    code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<MediaProbe, ProbeError> {
    if !success {
        return Err(ProbeError::ExitFailure {
            code,
            stderr: String::from_utf8_lossy(stderr).into_owned(),
        });
    }

    parse_probe_json(&String::from_utf8_lossy(stdout))
}

// --- The async entry point (S130-A MPRB-02) --------------------------------

/// Probe a file **without blocking a runtime thread**.
///
/// This is the entry point for every async caller — the scan integration, the
/// backfill worker, and Maestro. The synchronous [`run_ffprobe`] is correct and
/// stays (Foundry's survey/validate loops are ordinary blocking code), but it
/// parks the calling thread for up to the whole timeout. Inside a tokio worker
/// that is not merely slow: a handful of concurrently-stalled probes occupy
/// worker threads that every other task in the process — HTTP handlers
/// included — is waiting for. The mechanism it replaces is thread-based, which
/// is the right defence for ONE file and the wrong one for a 16,000-item sweep.
///
/// `kill_on_drop(true)` is what makes cancellation safe: if the caller's task
/// is dropped mid-probe (client disconnect, shutdown, `select!` losing a race),
/// the child is killed rather than left running against the library forever.
///
/// The timeout path kills **and reaps**. See [`spawn_with_timeout_async`] for
/// why the reap is not optional.
pub async fn run_ffprobe_async(
    ffprobe_bin: &str,
    path: &ResolvedPath,
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<MediaProbe, ProbeError> {
    let file_path = path.as_path().to_string_lossy().into_owned();
    reject_flag_shaped_path(&file_path)?;
    let args = build_ffprobe_args(&file_path);
    // No `over_cap` check here, deliberately: the async spawn helper stops at
    // the cap and returns `OutputTooLarge` itself (there is no exit status to
    // report alongside a child that never finished), so a `Captured` reaching
    // this point is always a complete capture. The synchronous path is the
    // opposite — it drains to EOF and hands the flag back — which is why
    // `run_ffprobe_with_limits` checks and this does not.
    let captured = spawn_with_timeout_async(ffprobe_bin, &args, timeout, max_output_bytes).await?;

    interpret_probe_output(
        captured.status.success(),
        captured.status.code(),
        &captured.stdout,
        &captured.stderr,
    )
}

/// How long the timeout path waits for a killed child to be reaped.
///
/// A bounded wait, not an unbounded one, and the bound is the whole point: a
/// child wedged in uninterruptible D-state does not die on SIGKILL until its
/// I/O returns, so `wait()` without a ceiling would make the TIMEOUT PATH
/// ITSELF hang — reintroducing exactly the failure this module exists to
/// prevent. Five seconds is far longer than reaping an already-dead child takes
/// and far shorter than any probe deadline. If it expires, `kill_on_drop`
/// leaves the child with tokio's orphan reaper rather than with us.
const REAP_GRACE: Duration = Duration::from_secs(5);

/// Spawn a child on the tokio runtime, drain both pipes concurrently, and kill
/// **and reap** it if the deadline passes.
///
/// ## Why the reap is not optional
/// A killed child that is never waited on stays in the process table as a
/// zombie. The synchronous path accepts that (one zombie per STALL, bounded by
/// stalls rather than by files) because a blocking `wait()` there would hang.
/// The async path has no such excuse — `wait().await` parks a task, not a
/// thread — and the async path is the one that runs 16,000 times. Unreaped, the
/// leak is bounded by the container's pid limit, and when that is reached the
/// worker cannot spawn ANYTHING: not ffprobe, not ffmpeg. The failure does not
/// look like a probe bug, which is what makes it expensive.
pub async fn spawn_with_timeout_async(
    bin: &str,
    args: &[String],
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<Captured, ProbeError> {
    spawn_with_timeout_async_reporting_pid(bin, args, timeout, max_output_bytes)
        .await
        .1
}

/// [`spawn_with_timeout_async`], additionally reporting the child's pid.
///
/// The pid exists for exactly one caller: the test that asserts no zombie is
/// left behind. "We reaped it" is a claim about a process, and the only way to
/// check a claim about a process is to look at that process — so the pid has to
/// leave the function. It is returned even on the error paths, because the
/// timeout path is the one whose reaping is under test.
///
/// Kept as a separate function so the production signature does not carry a
/// diagnostic out-parameter, and so the test drives the SAME code production
/// does rather than a parallel copy of it.
async fn spawn_with_timeout_async_reporting_pid(
    bin: &str,
    args: &[String],
    timeout: Duration,
    max_output_bytes: usize,
) -> (Option<u32>, Result<Captured, ProbeError>) {
    let spawned = tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Cancellation safety: if the caller's future is dropped, the child
        // dies with it instead of outliving the request that asked for it.
        .kill_on_drop(true)
        .spawn();

    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return (None, Err(classify_probe_spawn_error(bin, &e))),
    };
    let pid = child.id();

    // Both pipes are drained on their own TASKS, started before anything is
    // awaited on the child. Same reason as the synchronous path: a pipe holds
    // ~64 KB, and a child whose output exceeds that BLOCKS on write until
    // someone reads. Waiting for exit first would hang on any large probe and
    // then report it as a timeout — a good file failing because of how we read
    // it.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let mut out_task = tokio::spawn(async move {
        drain_capped_async(out_pipe.as_mut(), max_output_bytes).await
    });
    let mut err_task = tokio::spawn(async move {
        drain_capped_async(err_pipe.as_mut(), max_output_bytes).await
    });
    // Either the child exited and we hold its status, or a drain hit the cap
    // and there is no status to hold. Modelled as two cases rather than as a
    // status plus a flag, so no code path can read a placeholder exit status as
    // a real one.
    enum Collected {
        Exited(std::process::ExitStatus, Drained, Drained),
        Capped,
    }

    let collect = async {
        // The two drains are raced, NOT awaited in sequence.
        //
        // Sequencing them (`stdout.await; stderr.await`) would make a cap
        // breach on stdout wait for the OTHER pipe before it could be reported.
        // For every stub in this suite that costs nothing — dropping the
        // over-cap pipe gives the child EPIPE, it dies, and stderr reaches EOF
        // immediately — which is exactly why NO TEST HERE DISTINGUISHES THE TWO
        // FORMS, and the sequential version was measured passing the same
        // suite. Stated plainly rather than dressed up as a caught bug.
        //
        // It is kept as a race because the sequential form's correctness rests
        // on the child dying when we stop reading, and the children this module
        // exists to survive are the ones that do not: a process wedged in
        // D-state, or one still holding stderr open. In that case the breach
        // would surface at the deadline as a `Timeout` — the wrong verdict and
        // the wrong operator action. Racing removes the dependency entirely,
        // for a `select!`.
        //
        // (An earlier revision of this comment credited the race with fixing a
        // 30-second test hang. That was wrong: the hang was `capability::detect`
        // running the sleeping stub through a `Command::output()` with no
        // deadline, and it was worked around in the test stub. Left recorded
        // because a plausible-but-false attribution is worse than no comment.
        // CAPDET-01 later fixed the cause: `capability::detect_tool` calls
        // `spawn_with_timeout` like everything else here, so an unbounded
        // version probe is no longer reachable from production either.)
        let mut stdout: Option<Drained> = None;
        let mut stderr: Option<Drained> = None;
        while stdout.is_none() || stderr.is_none() {
            tokio::select! {
                r = &mut out_task, if stdout.is_none() => {
                    let d = r.unwrap_or_default();
                    if d.over_cap { return Ok::<_, std::io::Error>(Collected::Capped); }
                    stdout = Some(d);
                }
                r = &mut err_task, if stderr.is_none() => {
                    let d = r.unwrap_or_default();
                    if d.over_cap { return Ok(Collected::Capped); }
                    stderr = Some(d);
                }
            }
        }
        let status = child.wait().await?;
        Ok(Collected::Exited(
            status,
            stdout.expect("loop exits only when both are Some"),
            stderr.expect("loop exits only when both are Some"),
        ))
    };

    let outcome = tokio::time::timeout(timeout, collect).await;

    match outcome {
        Ok(Ok(Collected::Exited(status, stdout, stderr))) => (
            pid,
            Ok(Captured {
                status,
                stdout: stdout.bytes,
                stderr: stderr.bytes,
                stdout_over_cap: false,
                stderr_over_cap: false,
            }),
        ),
        Ok(Ok(Collected::Capped)) => {
            // The child is still running (or dying of SIGPIPE). Kill and reap
            // it here for the same reason the timeout path does.
            //
            // The partial capture is DROPPED rather than returned: it is a
            // prefix of a JSON document, and handing a prefix back as if it
            // were output is how a truncation becomes a parse error blaming the
            // file.
            abandon_drains(&out_task, &err_task);
            kill_and_reap(&mut child).await;
            (
                pid,
                Err(ProbeError::OutputTooLarge {
                    cap: max_output_bytes,
                }),
            )
        }
        Ok(Err(e)) => (
            pid,
            Err(ProbeError::Spawn {
                binary: bin.to_string(),
                message: format!("waiting for `{bin}`: {e}"),
            }),
        ),
        Err(_elapsed) => {
            abandon_drains(&out_task, &err_task);
            kill_and_reap(&mut child).await;
            (
                pid,
                Err(ProbeError::Timeout {
                    secs: timeout.as_secs(),
                }),
            )
        }
    }
}

/// Abort the drain tasks, which is what actually RELEASES the pipes.
///
/// Killing the child is not sufficient: we kill the process we spawned, and a
/// `sh -c "…"` stub — or any real tool that pipes internally — has
/// grandchildren that inherit the same pipe write ends and outlive it. Dropping
/// the `JoinHandle` would only DETACH those drain tasks, leaving them parked in
/// `read` on a pipe nobody will close, for the lifetime of the runtime.
///
/// Aborting drops each task's future, which drops its `ChildStdout`/`ChildStderr`
/// and closes the read end; anything still writing then gets EPIPE.
/// `JoinHandle::abort` returns immediately — no awaiting a wedged reader.
///
/// **DELIBERATELY UNTESTED, and honestly so.** The observable is the lifetime of
/// a detached task, which no assertion in this suite can read; removing these
/// two aborts leaves the whole suite green. It is kept as bounded housekeeping
/// on the failure path, not as covered behaviour, and it is documented that way
/// rather than being given a test that would pass either way.
fn abandon_drains<T: Send + 'static>(
    out: &tokio::task::JoinHandle<T>,
    err: &tokio::task::JoinHandle<T>,
) {
    out.abort();
    err.abort();
}

/// Signal the child, then WAIT for it, so the kernel releases its slot.
///
/// `start_kill` only delivers the signal; it does not collect the exit status,
/// and a child whose status is never collected is a zombie. The wait is bounded
/// by [`REAP_GRACE`] so a D-state child cannot turn this into the hang it is
/// here to prevent.
async fn kill_and_reap(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(REAP_GRACE, child.wait()).await;
}

/// The async twin of [`drain_capped`], with one deliberate difference: it
/// STOPS at the cap instead of draining on.
///
/// The synchronous drain keeps reading past the cap so the child dies of its
/// own accord rather than of SIGPIPE, because its callers (Foundry's encoder)
/// judge a run by its exit status and a SIGPIPE death would be indistinguishable
/// from a real failure. This one has no such caller: an over-cap probe is
/// already an error, the child is killed immediately afterwards, and its exit
/// status is discarded. Continuing to read a flooding child would just spend
/// unbounded time on output nobody will look at.
async fn drain_capped_async(
    pipe: Option<&mut (impl tokio::io::AsyncRead + Unpin)>,
    cap: usize,
) -> Drained {
    use tokio::io::AsyncReadExt;

    let mut drained = Drained::default();
    let Some(p) = pipe else { return drained };
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match p.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(drained.bytes.len());
                drained.bytes.extend_from_slice(&chunk[..n.min(room)]);
                if n > room {
                    drained.over_cap = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    drained
}

/// Most output this will hold from one child, per stream — the DEFAULT, now
/// that `MUSE_PROBE_MAX_OUTPUT_BYTES` can override it (S130-A MPRB-02).
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
pub const MAX_CAPTURED_BYTES: usize = 8 * 1024 * 1024;

/// What one drained pipe yielded, and whether the cap cut it short.
///
/// `over_cap` is the whole point of this type existing: before MPRB-02 the
/// drain returned a bare `Vec<u8>` and the fact that it had been cut was
/// carried only by an advisory string appended to the bytes. Nothing downstream
/// could act on it — the ffprobe path parsed the prefix as JSON and reported
/// the resulting failure as [`ProbeError::MalformedOutput`], i.e. as the file's
/// fault. A truncation has to be a VALUE the caller can branch on, not a note
/// inside the data.
#[derive(Debug, Default, Clone)]
pub struct Drained {
    pub bytes: Vec<u8>,
    pub over_cap: bool,
}

/// A finished child: its status, its (possibly capped) output, and whether
/// either stream hit the cap.
#[derive(Debug)]
pub struct Captured {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_over_cap: bool,
    pub stderr_over_cap: bool,
}

impl Captured {
    /// Whether EITHER stream was cut. Both matter: a flooding stderr can be the
    /// only symptom, and it still means we are holding a partial record.
    pub fn over_cap(&self) -> bool {
        self.stdout_over_cap || self.stderr_over_cap
    }
}

/// Drain a pipe to EOF, keeping at most `cap` bytes.
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
///
/// Returns the cut as a FLAG ([`Drained::over_cap`]) rather than only as an
/// appended note — see [`Drained`] for why that distinction is the whole of
/// MPRB-02's second gap.
fn drain_capped(pipe: Option<&mut impl std::io::Read>, cap: usize) -> Drained {
    let mut drained = Drained::default();
    let Some(p) = pipe else { return drained };
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match p.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                // `room` alone enforces the cap — saturating so it is 0 once
                // full. A guarding `if` around this was redundant, which
                // mutation testing correctly reported as an equivalent mutant.
                let room = cap.saturating_sub(drained.bytes.len());
                drained.bytes.extend_from_slice(&chunk[..n.min(room)]);
                if n > room {
                    drained.over_cap = true;
                }
            }
            Err(_) => break,
        }
    }
    drained
}

/// The advisory note appended to a truncated capture by [`spawn_with_timeout`].
///
/// Kept for the callers that treat the capture as human-readable log text
/// (Foundry's encoder, the subtitle extractor): to them the bytes ARE the
/// report, and a report that ends mid-sentence with no explanation is a lie by
/// omission. Callers that PARSE the bytes must use [`Drained::over_cap`]
/// instead — a note inside a JSON document does not make it parseable.
fn truncation_note(cap: usize) -> Vec<u8> {
    format!("\n[muse: output truncated at {cap} bytes; the child kept writing and was still drained]\n")
        .into_bytes()
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
    let captured = spawn_capturing(bin, args, timeout, MAX_CAPTURED_BYTES)?;
    let mut stdout = captured.stdout;
    let mut stderr = captured.stderr;
    // The note goes back into the bytes for THESE callers only. They log the
    // capture; they do not parse it. `run_ffprobe_with_limits` takes the other
    // branch and reports `OutputTooLarge` instead.
    if captured.stdout_over_cap {
        stdout.extend_from_slice(&truncation_note(MAX_CAPTURED_BYTES));
    }
    if captured.stderr_over_cap {
        stderr.extend_from_slice(&truncation_note(MAX_CAPTURED_BYTES));
    }
    Ok(std::process::Output {
        status: captured.status,
        stdout,
        stderr,
    })
}

/// [`spawn_with_timeout`] with an explicit cap, reporting truncation as a flag
/// instead of folding an advisory note into the bytes.
///
/// This is where the process handling actually lives; `spawn_with_timeout` is
/// the compatibility wrapper its existing log-consuming callers keep using.
pub fn spawn_capturing(
    bin: &str,
    args: &[String],
    timeout: Duration,
    cap: usize,
) -> Result<Captured, ProbeError> {
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
    let out_thread = std::thread::spawn(move || drain_capped(out_pipe.as_mut(), cap));
    let err_thread = std::thread::spawn(move || drain_capped(err_pipe.as_mut(), cap));

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
    Ok(Captured {
        status,
        stdout_over_cap: stdout.over_cap,
        stderr_over_cap: stderr.over_cap,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
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

    // --- S130-A MPRB-03 ---------------------------------------------------
    // Already present in the JSON `build_ffprobe_args` asks for today; the argv
    // is UNCHANGED by this item. These are fields the struct simply did not
    // extract. Every one is the permissive `Option<serde_json::Value>` /
    // `Option<String>` shape the doc comment on `RawProbe` explains, for the
    // same reason: ffprobe renders `level` as a number and `sample_rate` as a
    // string, in the same document.
    #[serde(default)]
    level: Option<serde_json::Value>,
    #[serde(default)]
    bits_per_raw_sample: Option<serde_json::Value>,
    #[serde(default)]
    r_frame_rate: Option<serde_json::Value>,
    #[serde(default)]
    avg_frame_rate: Option<serde_json::Value>,
    #[serde(default)]
    color_range: Option<String>,
    #[serde(default)]
    sample_rate: Option<serde_json::Value>,
    #[serde(default)]
    channel_layout: Option<String>,
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
    /// S130-A MPRB-03. ffprobe spells it with the underscore; there is no
    /// second spelling to alias.
    #[serde(default)]
    hearing_impaired: Option<i64>,
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

fn as_u8(v: &Option<serde_json::Value>) -> Option<u8> {
    as_u64(v).and_then(|n| u8::try_from(n).ok())
}

/// Read one of ffprobe's rational fields — `"24000/1001"`, `"25/1"`, `"0/0"` —
/// as a `f64` (S130-A MPRB-03).
///
/// **A zero denominator is `None`, and that is the whole reason this function
/// exists rather than a `split('/')` at the call site.** `"0/0"` is what
/// ffprobe writes for a stream with no meaningful rate — an attachment, a
/// still, a damaged track — and it is not rare. `0/0` in f64 is `NaN`, which
/// does not panic but is worse: it compares FALSE against everything, so a
/// `fps > 30` ceiling silently passes and a `fps < 30` floor silently fails,
/// on the same file, with no error anywhere. A zero numerator over a nonzero
/// denominator is a genuine "zero frames per second" and is likewise `None`,
/// because a rate of zero is not a rate.
///
/// Follows the [`as_u64`]/[`as_f64`] convention exactly: unparseable, negative
/// or non-finite is `None`, never `0`. A bare number (some ffprobe builds emit
/// `sample_aspect_ratio`-style fields unrationalized) is accepted too, so the
/// function is about the VALUE's meaning rather than about its rendering.
pub fn as_ratio(v: &Option<serde_json::Value>) -> Option<f64> {
    let s = match v.as_ref()? {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => return finite_positive(n.as_f64()?),
        _ => return None,
    };
    let Some((num, den)) = s.split_once('/') else {
        // Not a rational at all — try it as a plain decimal rather than
        // discarding a usable value.
        return finite_positive(num_from(&s)?);
    };
    let num = num_from(num)?;
    let den = num_from(den)?;
    if den == 0.0 {
        return None;
    }
    finite_positive(num / den)
}

fn num_from(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|f| f.is_finite())
}

/// A rate is a fact only when it is finite and strictly positive.
fn finite_positive(f: f64) -> Option<f64> {
    (f.is_finite() && f > 0.0).then_some(f)
}

/// Most streams one document may describe before it is refused
/// (S130-A MPRB-03).
///
/// **512.** The three pieces of evidence, because a bound with no evidence is
/// a magic number:
///
/// 1. **Measured, in this library.** The largest stream count the operator's
///    survey found on one file is **42 subtitle streams** — recorded twice,
///    independently, in [`crate::foundry::validate::SubtitleBand::Extreme`]
///    and in [`crate::subtitles::discover::embedded_from_probe`]. Add a video
///    stream, a handful of audio tracks and the attached-font set an ASS-styled
///    release carries, and the worst REAL shape in this 16,221-file library is
///    comfortably under 200 streams.
/// 2. **Measured, independently.** The largest ffprobe document in this
///    library is ~71 KB (see [`MAX_CAPTURED_BYTES`]). A `-show_streams` stream
///    object runs a few hundred bytes to ~1.5 KB with side data, which puts
///    that document at tens-to-low-hundreds of streams — the same ceiling
///    arrived at from a different measurement.
/// 3. **Corroboration, NOT verified on this host.** libavformat carries its own
///    `max_streams` option, documented as defaulting to 1000, above which
///    ffmpeg itself refuses a file. That would make a >1000-stream document one
///    ffprobe would never have produced for us in the first place. ffmpeg is
///    not installed on the dev box, so this is read from
///    documentation rather than checked, and it is corroboration only — points
///    1 and 2 carry the bound on their own.
///
/// So 512 is ~2.5x above the worst real shape and below the point where the
/// producing tool would itself have given up. The cost of it being too low is
/// a real file refused, which is why the margin is that wide; the cost of no
/// bound at all is a hostile or corrupt document turning one file into
/// unbounded allocation across a 16,221-item sweep.
///
/// Counted over the RAW stream list, before cover art and unindexed streams are
/// filtered out, because the bound is on how much document we agree to read —
/// filtering happens after we have already paid for it.
///
/// Reported as [`ProbeError::MalformedOutput`], not as a truncation: a probe of
/// only the first 512 streams is a partial view of the file, and a partial view
/// is exactly what this module refuses to hand back.
pub const MAX_STREAMS: usize = 512;

/// Longest tag or descriptor string carried out of a document
/// (S130-A MPRB-03).
///
/// **1024 bytes.** The strings this bounds are a language code, a container or
/// stream title, an attachment's font filename, and ffprobe's colour/profile
/// descriptors. The real ceiling among those is the FILENAME: every filesystem
/// this library is served from caps a single name at 255 bytes (`NAME_MAX`), and
/// titles in this library are derived from filenames, so 1024 is four times the
/// largest value that can legitimately occur. No real tag is cut by this.
///
/// It is also chosen to agree with the bound above it rather than to be able to
/// defeat it: 512 streams x a handful of bounded strings each stays in the low
/// megabytes, comfortably inside the 8 MiB capture cap, so no combination of
/// the two limits can be walked past.
///
/// Truncation is **recorded in [`MediaProbe::notes`]**, never silent. A cut
/// string that says nothing about having been cut is the exact fault this
/// module has already been bitten by once, in the output drain: a truncation
/// that presents as intact data gets read as data.
///
/// Cut on a `char` boundary, so a multi-byte name is shortened rather than
/// producing invalid UTF-8.
pub const MAX_TAG_LEN: usize = 1024;

/// Most entries [`MediaProbe::notes`] will hold.
///
/// **32.** Notes are per-document observations, and a healthy file produces
/// none; the cap exists so a pathological document (512 streams each with a
/// cut tag) cannot turn an advisory list into unbounded growth. When it binds,
/// the last entry SAYS how many were dropped rather than the list ending
/// without explanation.
pub const MAX_NOTES: usize = 32;

/// Bounded, order-preserving collector for [`MediaProbe::notes`].
///
/// The suppression counter is the point: a list that silently stops growing is
/// a list that lies about how much it saw.
#[derive(Debug, Default)]
struct Notes {
    entries: Vec<String>,
    suppressed: usize,
}

impl Notes {
    fn push(&mut self, note: String) {
        if self.entries.len() < MAX_NOTES {
            self.entries.push(note);
        } else {
            self.suppressed += 1;
        }
    }

    /// Finish, never exceeding [`MAX_NOTES`] entries.
    ///
    /// When anything was suppressed the LAST slot is spent on saying so, and
    /// the entry it displaces is counted as suppressed too — so the number in
    /// the message is the true number of notes not shown, not one less.
    fn into_vec(mut self) -> Vec<String> {
        if self.suppressed > 0 {
            self.suppressed += 1;
            let n = self.suppressed;
            let last = self.entries.len() - 1;
            self.entries[last] =
                format!("… and {n} further parse notes, suppressed at the {MAX_NOTES}-note cap");
        }
        self.entries
    }
}

/// Trim a tag/descriptor to [`MAX_TAG_LEN`], recording the cut.
///
/// Takes the field's name so the note names WHICH string was cut — "a tag was
/// truncated" is not actionable, "the title tag was truncated" is.
fn bounded(s: String, what: &str, notes: &mut Notes) -> String {
    if s.len() <= MAX_TAG_LEN {
        return s;
    }
    let original = s.len();
    // `floor_char_boundary` is unstable, so walk back to a boundary by hand.
    let mut end = MAX_TAG_LEN;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    notes.push(format!(
        "{what} was {original} bytes and was truncated to {MAX_TAG_LEN} \
         (MAX_TAG_LEN); the value below is a PREFIX, not the whole tag"
    ));
    s[..end].to_string()
}

/// [`normalize_descriptor`], then [`bounded`].
fn bounded_descriptor(
    v: &Option<String>,
    what: &str,
    notes: &mut Notes,
) -> Option<String> {
    normalize_descriptor(v).map(|s| bounded(s, what, notes))
}

/// Read a rational field, and record the case where it was PRESENT but
/// unusable.
///
/// Without the note, ffprobe's `"0/0"` and ffprobe saying nothing at all both
/// arrive as `None`, and a consumer cannot tell "this file has no frame rate"
/// from "we did not look". Same rule the module already applies to a probe
/// failure that cannot state its cause.
fn rate_field(
    v: &Option<serde_json::Value>,
    what: &str,
    index: u32,
    notes: &mut Notes,
) -> Option<f64> {
    let parsed = as_ratio(v);
    if parsed.is_none() {
        if let Some(raw) = v.as_ref() {
            notes.push(format!(
                "stream {index}: {what} was reported as `{}`, which is not a usable rate — \
                 read as unknown, not as zero",
                truncate_for_log(&raw.to_string())
            ));
        }
    }
    parsed
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

    // S130-A MPRB-03. Refused, not truncated — see `MAX_STREAMS` for the
    // evidence behind the number and for why a partial read is worse than an
    // error here.
    if raw.streams.len() > MAX_STREAMS {
        return Err(ProbeError::MalformedOutput {
            message: format!(
                "ffprobe described {} streams, above the {MAX_STREAMS}-stream cap \
                 (MAX_STREAMS) — refusing to describe this file from a partial read of it",
                raw.streams.len()
            ),
        });
    }

    let mut notes = Notes::default();
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
        let codec = bounded(
            s.codec_name.clone().unwrap_or_default(),
            &format!("stream {index}'s codec name"),
            &mut notes,
        );
        let disposition = s.disposition.as_ref();
        let language = s
            .tags
            .as_ref()
            .and_then(|t| t.language.clone().or_else(|| t.language_upper.clone()))
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|l| !l.is_empty() && l != "und")
            .map(|l| bounded(l, &format!("stream {index}'s language tag"), &mut notes));

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
                let color_transfer = bounded_descriptor(
                    &s.color_transfer,
                    &format!("stream {index}'s color_transfer"),
                    &mut notes,
                );
                let color_primaries = bounded_descriptor(
                    &s.color_primaries,
                    &format!("stream {index}'s color_primaries"),
                    &mut notes,
                );
                // A DESCRIPTION of the document's own inconsistency, not a
                // judgement about it. Wide-gamut primaries with an SDR transfer
                // is the shape of a remux whose colour tags were rewritten by
                // something that only understood one of the two fields. Both
                // lists are read from `foundry::hdr` — this deliberately does
                // NOT restate them, and it deliberately does not decide
                // anything: `classify_hdr` remains the single authority on what
                // the dynamic range is, and it is unaffected by this note.
                if let (Some(p), Some(t)) = (color_primaries.as_deref(), color_transfer.as_deref())
                {
                    if crate::foundry::hdr::WIDE_GAMUT_PRIMARIES.contains(&p)
                        && crate::foundry::hdr::SDR_TRANSFERS.contains(&t)
                    {
                        notes.push(format!(
                            "stream {index}: wide-gamut primaries `{p}` are declared alongside \
                             the SDR transfer `{t}` — the two tags disagree, and this note \
                             changes no verdict"
                        ));
                    }
                }
                video.push(VideoStream {
                    index,
                    codec,
                    width: as_u32(&s.width),
                    height: as_u32(&s.height),
                    bitrate_bps: as_u64(&s.bit_rate),
                    pix_fmt: bounded_descriptor(
                        &s.pix_fmt,
                        &format!("stream {index}'s pix_fmt"),
                        &mut notes,
                    ),
                    profile: bounded_descriptor(
                        &match &s.profile {
                            // ffprobe renders `profile` as a string for most
                            // codecs but as a bare integer for a few (and for
                            // unrecognized ones). Both are read; neither is
                            // allowed to fail the whole document.
                            Some(serde_json::Value::String(p)) => Some(p.clone()),
                            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
                            _ => None,
                        },
                        &format!("stream {index}'s video profile"),
                        &mut notes,
                    ),
                    codec_tag: bounded_descriptor(
                        &s.codec_tag_string,
                        &format!("stream {index}'s codec tag"),
                        &mut notes,
                    ),
                    color_transfer,
                    color_primaries,
                    color_space: bounded_descriptor(
                        &s.color_space,
                        &format!("stream {index}'s color_space"),
                        &mut notes,
                    ),
                    level: as_u32(&s.level),
                    bits_per_raw_sample: as_u8(&s.bits_per_raw_sample),
                    frame_rate_fps: rate_field(
                        &s.r_frame_rate,
                        "r_frame_rate",
                        index,
                        &mut notes,
                    ),
                    avg_frame_rate_fps: rate_field(
                        &s.avg_frame_rate,
                        "avg_frame_rate",
                        index,
                        &mut notes,
                    ),
                    color_range: bounded_descriptor(
                        &s.color_range,
                        &format!("stream {index}'s color_range"),
                        &mut notes,
                    ),
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
                profile: bounded_descriptor(
                    &match &s.profile {
                        Some(serde_json::Value::String(p)) => Some(p.clone()),
                        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
                        _ => None,
                    },
                    &format!("stream {index}'s audio profile"),
                    &mut notes,
                ),
                sample_rate_hz: as_u32(&s.sample_rate),
                // NOT `normalize_descriptor`: see the field's doc comment for
                // why ffprobe's literal `"unknown"` is kept here and folded to
                // `None` everywhere else.
                channel_layout: s
                    .channel_layout
                    .as_ref()
                    .map(|l| l.trim().to_ascii_lowercase())
                    .filter(|l| !l.is_empty())
                    .map(|l| bounded(l, &format!("stream {index}'s channel layout"), &mut notes)),
                default: disposition.and_then(|d| d.default).unwrap_or(0) != 0,
                forced: disposition.and_then(|d| d.forced).unwrap_or(0) != 0,
            }),
            Some("subtitle") => subtitles.push(SubtitleStream {
                index,
                codec,
                language,
                forced: disposition.and_then(|d| d.forced).unwrap_or(0) != 0,
                default: disposition.and_then(|d| d.default).unwrap_or(0) != 0,
                hearing_impaired: disposition.and_then(|d| d.hearing_impaired).unwrap_or(0) != 0,
            }),
            Some("attachment") => attachments.push(AttachmentStream {
                index,
                codec,
                filename: s
                    .tags
                    .as_ref()
                    .and_then(|t| t.filename.clone())
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty())
                    .map(|n| {
                        bounded(n, &format!("stream {index}'s attachment filename"), &mut notes)
                    }),
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
        .filter(|t| !t.is_empty())
        .map(|t| bounded(t, "the container title tag", &mut notes));

    let container = bounded(container, "the container format name", &mut notes);

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
        notes: notes.into_vec(),
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

    // --- SUBCODEC-01: the bitmap-subtitle vocabulary ------------------------

    /// The expected membership, written out **as a literal** rather than read
    /// from [`BITMAP_SUBTITLE_CODECS`].
    ///
    /// This is the whole point. A test that loops over the const it is meant to
    /// be guarding cannot fail when an entry is deleted — it just loops one
    /// fewer time and reports success. Every assertion below is driven from
    /// this literal, so removing an alias from the production const kills the
    /// test instead of shrinking it.
    const EXPECTED_BITMAP_CODECS: &[&str] = &[
        "hdmv_pgs_subtitle",
        "dvd_subtitle",
        "dvb_subtitle",
        "xsub",
        "pgssub",
        "dvdsub",
        "dvbsub",
    ];

    fn probe_json_with_subtitle(codec: &str) -> String {
        format!(
            r#"{{
            "streams": [
                {{
                    "index": 0,
                    "codec_name": "h264",
                    "codec_type": "video",
                    "width": 1920,
                    "height": 1080,
                    "pix_fmt": "yuv420p",
                    "bit_rate": "5000000",
                    "disposition": {{ "default": 1, "forced": 0, "attached_pic": 0 }}
                }},
                {{
                    "index": 1,
                    "codec_name": "aac",
                    "codec_type": "audio",
                    "channels": 2,
                    "bit_rate": "192000",
                    "disposition": {{ "default": 1, "forced": 0 }},
                    "tags": {{ "language": "eng" }}
                }},
                {{
                    "index": 2,
                    "codec_name": "{codec}",
                    "codec_type": "subtitle",
                    "disposition": {{ "default": 1, "forced": 0 }},
                    "tags": {{ "language": "eng" }}
                }}
            ],
            "format": {{
                "format_name": "matroska,webm",
                "duration": "5400.048000",
                "size": "3400000000",
                "bit_rate": "5037037"
            }}
        }}"#
        )
    }

    /// **The divergence guard.** `SUBCODEC-01`.
    ///
    /// Before this item there were two lists of bitmap subtitle codecs —
    /// `subtitles::discover::IMAGE_SUBTITLE_CODECS` (4) and
    /// `foundry::directplay::BITMAP_SUBTITLE_CODECS` (7) — and they disagreed
    /// about `pgssub`/`dvdsub`/`dvbsub`. Each list was internally consistent and
    /// read correctly, which is why the disagreement survived review; nothing
    /// compared them.
    ///
    /// This compares them. It asserts, over the **full** alias set and from a
    /// literal, that every consumer that classifies a subtitle codec resolves
    /// through the one symbol in this module — the probe predicate, the
    /// `subtitles` predicate, and Foundry's real `direct_play_blockers` entry
    /// point on a probe parsed from actual ffprobe-shaped JSON. If any consumer
    /// ever grows its own copy again, or if the shared list is trimmed, this
    /// fails.
    #[test]
    fn every_consumer_classifies_the_same_bitmap_subtitle_codecs() {
        use crate::foundry::directplay::{direct_play_blockers, DirectPlayBlocker};
        use crate::foundry::policy::TranscodePolicy;
        use crate::subtitles::discover::is_image_codec;

        // The shared list is exactly the reconciled membership — not shorter
        // (a dropped alias fails open), not longer (an unreviewed entry).
        assert_eq!(
            BITMAP_SUBTITLE_CODECS, EXPECTED_BITMAP_CODECS,
            "the one bitmap-subtitle list changed without this guard being updated"
        );

        let policy = TranscodePolicy::direct_play_normalization();

        for codec in EXPECTED_BITMAP_CODECS {
            // 1. the home predicate, including the matching rule (trim + case).
            assert!(is_bitmap_subtitle_codec(codec), "{codec}");
            assert!(
                is_bitmap_subtitle_codec(&codec.to_ascii_uppercase()),
                "{codec}: matching must be case-insensitive"
            );
            assert!(
                is_bitmap_subtitle_codec(&format!("  {codec}  ")),
                "{codec}: matching must tolerate ffprobe whitespace"
            );

            // 2. `subtitles` — "image" and "bitmap" must be the same rule.
            assert!(
                is_image_codec(codec),
                "{codec}: bitmap at the probe boundary but not image-based to \
                 subtitles::discover — the two lists have diverged again"
            );

            // 3. Foundry's production entry point, over a parsed probe.
            let probe = parse_probe_json(&probe_json_with_subtitle(codec))
                .unwrap_or_else(|e| panic!("{codec}: fixture must parse: {e}"));
            assert_eq!(probe.subtitles.len(), 1, "{codec}: fixture shape");
            let blockers = direct_play_blockers(&probe, &policy);
            assert!(
                blockers
                    .iter()
                    .any(|b| matches!(b, DirectPlayBlocker::DefaultBitmapSubtitles { .. })),
                "{codec}: a default bitmap track must be reported as a direct-play \
                 blocker, got {blockers:?}"
            );
        }

        // Negative control: the guard must not be satisfiable by a predicate
        // that answers `true` to everything.
        for codec in ["subrip", "ass", "ssa", "webvtt", "mov_text", "dvb_teletext", "text", ""] {
            assert!(!is_bitmap_subtitle_codec(codec), "{codec} is text");
            assert!(!is_image_codec(codec), "{codec} is text");
            let probe = parse_probe_json(&probe_json_with_subtitle(codec))
                .unwrap_or_else(|e| panic!("{codec}: fixture must parse: {e}"));
            let blockers = direct_play_blockers(&probe, &policy);
            assert!(
                !blockers
                    .iter()
                    .any(|b| matches!(b, DirectPlayBlocker::DefaultBitmapSubtitles { .. })),
                "{codec}: a text track must not be reported as bitmap, got {blockers:?}"
            );
        }
    }

    /// MODIFIED by S130-A MPRB-02, and this is the ONLY pre-existing test in
    /// this module that changed. The `--` element is a deliberate addition to
    /// the argv contract (see [`build_ffprobe_args`]), so the exact-argv
    /// assertion had to grow it. Nothing was weakened: it is still an exact
    /// whole-vector equality, and the new element is asserted in its exact
    /// position rather than the assertion being loosened to tolerate it.
    #[test]
    fn ffprobe_argv_asks_for_json_only_with_every_section() {
        assert_eq!(
            build_ffprobe_args("/srv/media/Movies/A/A.mkv"),
            vec![
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
                "-show_chapters",
                "--",
                "/srv/media/Movies/A/A.mkv",
            ]
        );
    }

    /// The guard's actual job: the path cannot be read as an OPTION.
    ///
    /// A file named `-loglevel` is not hypothetical in a scanned library —
    /// filenames come from release groups, not from us. Passing the path as its
    /// own argv element (which the test above pins) stops the SHELL
    /// reinterpreting it and does nothing about ffprobe's own option parser;
    /// `--` is what stops that one.
    #[test]
    fn a_flag_shaped_filename_is_positional_because_of_the_terminator() {
        let args = build_ffprobe_args("/srv/media/-loglevel");
        let dashdash = args
            .iter()
            .position(|a| a == "--")
            .expect("the argv must carry an end-of-options terminator");
        assert_eq!(
            dashdash,
            args.len() - 2,
            "`--` must be immediately before the path, so nothing between them \
             can be re-read as an option"
        );
        assert_eq!(args.last().unwrap(), "/srv/media/-loglevel");
    }

    #[test]
    fn a_probe_failure_renders_the_reason_ffprobe_gave_not_an_empty_colon() {
        // The behaviour, not the wording: whatever ffprobe said about WHY the
        // file failed has to survive into the message an operator reads.
        //
        // This is the pair of assertions that has to hold together. The argv
        // must ask for stderr (`-v error`, never `-v quiet`), AND the rendering
        // must carry what stderr contained. With `-v quiet` the first fails
        // here and, live, the second is vacuously true against an empty string
        // — which is exactly how 7 library failures came to render as
        // "...is not media: " with nothing after the colon. Ignorance rendered
        // as absence: unable to say why, and therefore indistinguishable from
        // a failure nobody investigated.
        let argv = build_ffprobe_args("/srv/media/Movies/Broken.mkv");
        assert!(
            argv.windows(2).any(|w| w[0] == "-v" && w[1] == "error"),
            "argv must ask ffprobe for its errors: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "quiet"),
            "`-v quiet` silences the only diagnostic a failed probe can offer: {argv:?}"
        );

        // A real one, from a file this library actually holds.
        let detail = "[matroska,webm @ 0x55d1] 0x00 at pos 0 (0x0) invalid as first byte \
                      of an EBML number\n[matroska,webm @ 0x55d1] EBML header parsing failed";
        let rendered = ProbeError::ExitFailure {
            code: Some(1),
            stderr: detail.to_string(),
        }
        .to_string();

        assert!(
            rendered.contains("EBML header parsing failed"),
            "the captured stderr must reach the operator, got: {rendered}"
        );
        assert!(
            rendered.contains("invalid as first byte"),
            "the whole diagnostic, not just its last line, got: {rendered}"
        );
        // And the failure mode that started this: nothing after the colon.
        assert!(
            !rendered.trim_end().ends_with(':'),
            "a message ending in a bare colon says nothing at all: {rendered}"
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

    // --- S130-A MPRB-02 -----------------------------------------------------

    /// Every variant, listed by hand.
    ///
    /// Written as an explicit list rather than derived from anything, so it is
    /// a SECOND statement of the taxonomy that has to agree with the first. A
    /// list generated from the code under test would agree with it by
    /// construction and prove nothing.
    fn every_probe_error() -> Vec<ProbeError> {
        vec![
            ProbeError::ToolMissing {
                binary: "ffprobe".into(),
            },
            ProbeError::Spawn {
                binary: "ffprobe".into(),
                message: "EAGAIN".into(),
            },
            ProbeError::Timeout { secs: 120 },
            ProbeError::ExitFailure {
                code: Some(1),
                stderr: "moov atom not found".into(),
            },
            ProbeError::MalformedOutput {
                message: "expected value".into(),
            },
            ProbeError::NoStreams,
            ProbeError::OutputTooLarge { cap: 8 * 1024 * 1024 },
        ]
    }

    /// The classification, asserted variant by variant.
    ///
    /// The property that matters is not "some are retryable" but that the two
    /// buckets mean opposite things: retry a host problem, never retry a file
    /// problem. Getting one wrong is not cosmetic — a retryable `ExitFailure`
    /// is an infinite loop on a broken file, and a terminal `Timeout` writes
    /// off a good file because an NFS mount hiccupped.
    #[test]
    fn every_probe_error_is_classified_by_whether_the_tool_gave_a_verdict() {
        for e in every_probe_error() {
            let (want_retry, want_state) = match &e {
                ProbeError::ToolMissing { .. }
                | ProbeError::Spawn { .. }
                | ProbeError::Timeout { .. } => (true, ProbeState::Unreadable),
                ProbeError::ExitFailure { .. }
                | ProbeError::MalformedOutput { .. }
                | ProbeError::NoStreams
                | ProbeError::OutputTooLarge { .. } => (false, ProbeState::ProbeFailed),
            };
            assert_eq!(e.is_retryable(), want_retry, "is_retryable for {e:?}");
            assert_eq!(e.state(), want_state, "state for {e:?}");
        }
    }

    /// The two answers must not collapse into one: a taxonomy where everything
    /// is retryable, or nothing is, would satisfy a per-variant check written
    /// carelessly and is useless in practice.
    #[test]
    fn the_taxonomy_actually_splits_the_variants() {
        let all = every_probe_error();
        assert!(all.iter().any(|e| e.is_retryable()));
        assert!(all.iter().any(|e| !e.is_retryable()));
        assert!(all.iter().any(|e| e.state() == ProbeState::Unreadable));
        assert!(all.iter().any(|e| e.state() == ProbeState::ProbeFailed));
        assert_eq!(ProbeState::Unreadable.as_str(), "unreadable");
        assert_eq!(ProbeState::ProbeFailed.as_str(), "probe_failed");
    }

    /// `is_retryable` and `state` must not disagree about the same error — they
    /// are two views of one decision, and a caller that consults both must not
    /// get contradictory advice.
    #[test]
    fn retryability_and_state_agree_for_every_variant() {
        for e in every_probe_error() {
            let expected = if e.is_retryable() {
                ProbeState::Unreadable
            } else {
                ProbeState::ProbeFailed
            };
            assert_eq!(e.state(), expected, "{e:?}");
        }
    }

    /// The message must name OUR limit and must not read as a verdict on the
    /// file — that confusion is the entire reason this variant exists.
    #[test]
    fn the_over_cap_message_blames_the_cap_not_the_file() {
        let msg = ProbeError::OutputTooLarge { cap: 8_388_608 }.to_string();
        assert!(msg.contains("8388608"), "the message must name the cap: {msg}");
        assert!(
            msg.contains("MUSE_PROBE_MAX_OUTPUT_BYTES"),
            "it must name the knob the operator can actually turn: {msg}"
        );
        assert!(
            msg.contains("not a statement that the file's metadata is malformed"),
            "it must not read as a verdict about the file: {msg}"
        );
    }

    #[test]
    fn a_path_that_begins_with_a_dash_is_refused_before_spawn() {
        let got = reject_flag_shaped_path("-loglevel");
        match got {
            Err(ProbeError::Spawn { message, .. }) => {
                assert!(message.contains("begins with `-`"), "{message}");
            }
            other => panic!("a flag-shaped path must be refused, got {other:?}"),
        }
        // ...and an ordinary absolute path is untouched. Without this the guard
        // could be satisfied by refusing everything.
        assert!(reject_flag_shaped_path("/srv/media/Movies/-Weird- (2022).mkv").is_ok());
    }

    // --- The async entry point ---------------------------------------------
    //
    // These use STUB `ffprobe` scripts written to a temp dir. ffprobe is
    // not installed on the dev box, so a test that needed the
    // real binary would not run anywhere in this fleet — the same reason the
    // parser is a pure function. A stub cannot tell us how ffprobe behaves; it
    // tells us how OUR process handling behaves, which is what MPRB-02 changed.

    fn temp_dir_for(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muse-mprb02-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Write an executable `/bin/sh` stub standing in for `ffprobe`.
    fn stub_ffprobe(dir: &std::path::Path, body: &str) -> String {
        let path = dir.join("stub-ffprobe");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }
        path.to_string_lossy().into_owned()
    }

    /// A `ResolvedPath` for a real file under a real root, built through the
    /// guard — never by weakening the guard, which is the property that makes
    /// `ResolvedPath` worth taking as an argument at all.
    fn resolved_in(dir: &std::path::Path, name: &str) -> ResolvedPath {
        let file = dir.join(name);
        std::fs::write(&file, b"not really a video").expect("write subject file");
        crate::media::paths::PathGuard::new(vec![dir.to_str().unwrap()], false)
            .resolve(&file)
            .expect("the file is inside the root")
    }

    const STUB_JSON: &str = r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video","width":1920,"height":1080}],"format":{"format_name":"matroska,webm"}}"#;

    /// The happy path, end to end through the production async entry point:
    /// spawn, drain, exit, parse.
    #[tokio::test]
    async fn the_async_probe_parses_what_the_tool_wrote() {
        let dir = temp_dir_for("async-ok");
        let bin = stub_ffprobe(&dir, &format!("printf '%s' '{STUB_JSON}'"));
        let path = resolved_in(&dir, "a.mkv");

        let got = run_ffprobe_async(&bin, &path, Duration::from_secs(30), MAX_CAPTURED_BYTES)
            .await
            .expect("the stub's document must parse");
        assert_eq!(got.container, "matroska,webm");
        assert_eq!(got.primary_video().unwrap().width, Some(1920));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A non-zero exit is still an `ExitFailure`, not a timeout or a parse
    /// error. Without this the async path could satisfy the tests above by
    /// never distinguishing anything.
    #[tokio::test]
    async fn the_async_probe_reports_a_nonzero_exit_as_such() {
        let dir = temp_dir_for("async-exit");
        let bin = stub_ffprobe(&dir, "echo 'moov atom not found' >&2; exit 1");
        let path = resolved_in(&dir, "a.mkv");

        let got = run_ffprobe_async(&bin, &path, Duration::from_secs(30), MAX_CAPTURED_BYTES).await;
        match got {
            Err(ProbeError::ExitFailure { code, stderr }) => {
                assert_eq!(code, Some(1));
                assert!(stderr.contains("moov atom"), "{stderr}");
            }
            other => panic!("expected ExitFailure, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The child's pid, as `/proc` sees it: `None` once the entry is gone
    /// (reaped and released), `Some('Z')` while it is a zombie.
    #[cfg(target_os = "linux")]
    fn proc_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 3, after the parenthesised comm — which can itself contain
        // spaces and parentheses, so split at the LAST ')'.
        let after = stat.rsplit_once(')')?.1;
        after.split_whitespace().next()?.chars().next()
    }

    /// The timeout fires — and the corpse is collected.
    ///
    /// The kill alone is not enough. A killed child whose status is never
    /// collected stays in the process table; at 16,000 items, a per-stall leak
    /// walks into the container's pid limit, after which the worker can spawn
    /// NOTHING and the symptom looks nothing like a probe bug. So this asserts
    /// the OS's view of the pid, not our own bookkeeping.
    #[tokio::test]
    async fn an_async_timeout_kills_the_child_and_leaves_no_zombie() {
        let start = std::time::Instant::now();
        let (pid, got) = spawn_with_timeout_async_reporting_pid(
            "sleep",
            &["30".to_string()],
            Duration::from_millis(300),
            MAX_CAPTURED_BYTES,
        )
        .await;
        let waited = start.elapsed();

        assert!(
            matches!(got, Err(ProbeError::Timeout { .. })),
            "expected a timeout, got {got:?}"
        );
        assert!(
            waited >= Duration::from_millis(250) && waited < Duration::from_secs(3),
            "must stop waiting NEAR the deadline, waited {waited:?}"
        );

        let pid = pid.expect("a spawned child has a pid");
        #[cfg(target_os = "linux")]
        {
            // The assertion is that the pid is GONE, not merely "not a zombie".
            // Absence is the only state that means both things happened: a
            // living child would still be `S`, and a killed-but-uncollected one
            // would be `Z`. Checking only for `Z` would be satisfied by a stub
            // that was never killed at all.
            //
            // Polled, with a ceiling, because the timeout path kills and reaps
            // and the last of that can land microseconds after the await
            // returns; a `sleep 30` that was never killed survives thirty
            // seconds and is not rescued by a two-second poll.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let mut last = proc_state(pid);
            while last.is_some() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
                last = proc_state(pid);
            }
            assert_eq!(
                last, None,
                "pid {pid} is still in the process table as `{last:?}` — `Z` means it \
                 was killed but never reaped, anything else means it was never killed"
            );
        }
        let _ = pid;
    }

    /// A child that outruns the cap is reported AS a cap breach — the point
    /// being what it is NOT reported as.
    ///
    /// Before MPRB-02 the drain truncated silently, the prefix failed to parse,
    /// and the caller was told `MalformedOutput`: our limit, reported as the
    /// file's malformed metadata. The operator's response to those two is
    /// opposite, so the assertion below is as much about the error we must not
    /// return as the one we must.
    #[tokio::test]
    async fn an_async_flood_is_reported_as_the_cap_not_as_malformed_json() {
        let dir = temp_dir_for("async-flood");
        // Valid JSON is never reached — the flood comes first, so a passing
        // result cannot be an artefact of unparseable output.
        let bin = stub_ffprobe(&dir, "head -c 4000000 /dev/zero | tr '\\0' 'x'");
        let path = resolved_in(&dir, "a.mkv");

        let cap = 512 * 1024;
        let got = run_ffprobe_async(&bin, &path, Duration::from_secs(30), cap).await;
        match got {
            Err(ProbeError::OutputTooLarge { cap: reported }) => {
                assert_eq!(reported, cap, "the error must name the cap in force");
            }
            Err(ProbeError::MalformedOutput { message }) => panic!(
                "a truncation must not be reported as the file's parse failure: {message}"
            ),
            other => panic!("expected OutputTooLarge, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same rule on the synchronous ffprobe path, which parses JSON too and
    /// had the identical mislabelling.
    #[test]
    fn a_sync_flood_is_reported_as_the_cap_not_as_malformed_json() {
        let dir = temp_dir_for("sync-flood");
        let bin = stub_ffprobe(&dir, "head -c 4000000 /dev/zero | tr '\\0' 'x'");
        let path = resolved_in(&dir, "a.mkv");

        let cap = 512 * 1024;
        let got = run_ffprobe_with_limits(&bin, &path, Duration::from_secs(30), cap);
        assert!(
            matches!(got, Err(ProbeError::OutputTooLarge { cap: c }) if c == cap),
            "expected OutputTooLarge, got {got:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cap must not fire on ordinary output — otherwise the two tests above
    /// would be satisfied by an implementation that always reports a breach.
    #[tokio::test]
    async fn output_under_the_cap_is_not_reported_as_a_breach() {
        let dir = temp_dir_for("async-under");
        let bin = stub_ffprobe(&dir, &format!("printf '%s' '{STUB_JSON}'"));
        let path = resolved_in(&dir, "a.mkv");

        assert!(
            run_ffprobe_async(&bin, &path, Duration::from_secs(30), 512 * 1024)
                .await
                .is_ok(),
            "a small document must pass a 512 KiB cap"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The generic `spawn_with_timeout` keeps its old behaviour: it drains to
    /// EOF, lets the child exit on its own, and appends an advisory note.
    ///
    /// This is a REGRESSION guard on the refactor, not new behaviour. Foundry's
    /// encoder and the subtitle extractor judge a run by its exit status, and a
    /// child killed by SIGPIPE at the cap would report a signal exit — turning
    /// a merely verbose encode into a failed one. Changing that while "adding a
    /// cap error" would have been a silent regression in a different module.
    #[test]
    fn the_generic_spawn_still_drains_past_the_cap_and_only_annotates() {
        let args = vec![
            "-c".to_string(),
            "head -c 9437184 /dev/zero | tr '\\0' 'x'".to_string(),
        ];
        let out = spawn_with_timeout("sh", &args, Duration::from_secs(60)).expect("completes");
        assert!(out.status.success(), "the child must exit normally: {:?}", out.status);
        assert!(String::from_utf8_lossy(&out.stdout).contains("output truncated"));
    }

    // --- S130-A MPRB-03 -----------------------------------------------------
    //
    // What this item did NOT do, stated here because it is the load-bearing
    // decision: it added **no** HDR/Dolby-Vision classifier and **no** subtitle
    // codec list. `foundry::hdr` is the sole authority on the first (and is
    // what `foundry::directplay::may_delete_original` consumes to refuse
    // deleting a DV master), and `subtitles::discover::is_image_codec` on the
    // second. `media::derive` composes them; it restates neither. See that
    // module's header for the full table.

    /// A document carrying every field MPRB-03 added, in the renderings
    /// ffprobe actually mixes within one document: `level` as a bare number,
    /// `sample_rate` as a string, the rates as rationals.
    const FULL_FIELDS: &str = r#"{
        "streams": [
            {
                "index": 0,
                "codec_name": "hevc",
                "codec_type": "video",
                "profile": "Main 10",
                "width": 3840, "height": 2160,
                "pix_fmt": "yuv420p10le",
                "level": 153,
                "bits_per_raw_sample": "10",
                "r_frame_rate": "24000/1001",
                "avg_frame_rate": "24000/1001",
                "color_range": "tv",
                "color_transfer": "smpte2084",
                "color_primaries": "bt2020",
                "disposition": { "default": 1, "attached_pic": 0 }
            },
            {
                "index": 1,
                "codec_name": "dts",
                "codec_type": "audio",
                "profile": "DTS-HD MA",
                "channels": 8,
                "sample_rate": "48000",
                "channel_layout": "7.1",
                "disposition": { "default": 1, "forced": 0 },
                "tags": { "language": "eng" }
            },
            {
                "index": 2,
                "codec_name": "hdmv_pgs_subtitle",
                "codec_type": "subtitle",
                "disposition": { "default": 0, "forced": 0, "hearing_impaired": 1 },
                "tags": { "language": "eng" }
            }
        ],
        "format": { "format_name": "matroska,webm", "duration": "7200.0" }
    }"#;

    #[test]
    fn mprb03_the_new_video_fields_parse_from_the_document_we_already_fetch() {
        let p = parse_probe_json(FULL_FIELDS).expect("must parse");
        let v = p.primary_video().expect("a video stream");

        assert_eq!(v.level, Some(153), "HEVC level 5.1, in ffprobe's own units");
        assert_eq!(
            v.bits_per_raw_sample,
            Some(10),
            "the OBSERVED depth, string-rendered in this document"
        );
        // 24000/1001, not 24. A rate rounded to 24 would fail a `<= 24` device
        // ceiling by 0.0 and pass a `>= 24` one — the reason the ratio is kept
        // as a ratio.
        let fps = v.frame_rate_fps.expect("r_frame_rate parses");
        assert!((fps - 23.976_023).abs() < 1e-5, "got {fps}");
        assert!((v.avg_frame_rate_fps.unwrap() - 23.976_023).abs() < 1e-5);
        assert_eq!(v.color_range.as_deref(), Some("tv"));

        // And the argv is untouched: every field above came out of the JSON
        // ffprobe already returned. This item costs no extra I/O against a
        // 16,221-file library, and this assertion is what stops that claim
        // from quietly becoming false.
        assert_eq!(
            build_ffprobe_args("/x"),
            vec![
                "-v", "error", "-print_format", "json", "-show_format",
                "-show_streams", "-show_chapters", "--", "/x",
            ]
        );
    }

    #[test]
    fn mprb03_the_new_audio_and_subtitle_fields_parse() {
        let p = parse_probe_json(FULL_FIELDS).expect("must parse");

        let a = &p.audio[0];
        // `codec_name` is plain `dts` for both DTS and DTS-HD MA; the profile
        // is the only thing that separates them, which is why it is carried.
        assert_eq!(a.codec, "dts");
        assert_eq!(a.profile.as_deref(), Some("dts-hd ma"));
        assert_eq!(a.sample_rate_hz, Some(48_000));
        assert_eq!(a.channel_layout.as_deref(), Some("7.1"));
        assert!(a.default, "disposition.default must reach AudioStream");
        assert!(!a.forced);

        let s = &p.subtitles[0];
        assert!(
            s.hearing_impaired,
            "an SDH track must be distinguishable, or it gets auto-selected"
        );
        assert!(!s.default);
    }

    #[test]
    fn mprb03_a_real_captured_document_carries_the_new_fields_too() {
        // The synthetic fixture above proves the parser reads a shape we
        // invented. This one proves it reads the shape ffprobe on the
        // DEPLOYMENT HOST actually emitted — `level` and `color_range` were
        // sitting unread in this captured document the whole time.
        let p = parse_probe_json(LIVE_DOLBY_VISION_STREAM).expect("live capture must parse");
        let v = p.primary_video().expect("a video stream");
        assert_eq!(v.level, Some(150), "HEVC 5.0 as this real file reports it");
        assert_eq!(v.color_range.as_deref(), Some("tv"));
        // This capture carries no rate fields at all, and an absent field is
        // silent — no note, because there is nothing we failed to read.
        assert_eq!(v.frame_rate_fps, None);
        assert!(p.notes.is_empty(), "notes: {:?}", p.notes);
    }

    #[test]
    fn mprb03_as_ratio_never_divides_by_zero_and_never_returns_a_zero_rate() {
        let s = |t: &str| Some(serde_json::Value::String(t.to_string()));

        // The case this function exists for. `0/0` is what ffprobe writes for
        // a stream with no meaningful rate; as an f64 it is NaN, which
        // compares false against every ceiling AND every floor.
        assert_eq!(as_ratio(&s("0/0")), None);
        assert_eq!(as_ratio(&s("24/0")), None);
        // A real rate is not rounded.
        let ntsc = as_ratio(&s("24000/1001")).unwrap();
        assert!((ntsc - 23.976_023).abs() < 1e-5, "got {ntsc}");
        assert_eq!(as_ratio(&s("25/1")), Some(25.0));
        // Zero frames per second is not a rate.
        assert_eq!(as_ratio(&s("0/1")), None);
        // Unparseable, empty, absent, negative, wrong type.
        assert_eq!(as_ratio(&s("")), None);
        assert_eq!(as_ratio(&s("garbage")), None);
        assert_eq!(as_ratio(&s("N/A")), None);
        assert_eq!(as_ratio(&s("-24/1")), None);
        assert_eq!(as_ratio(&None), None);
        assert_eq!(as_ratio(&Some(serde_json::Value::Bool(true))), None);
        // A bare number is a value too — the function is about meaning, not
        // rendering.
        assert_eq!(as_ratio(&Some(serde_json::json!(29.97))), Some(29.97));
        assert_eq!(as_ratio(&Some(serde_json::json!(0))), None);
    }

    #[test]
    fn mprb03_an_unusable_rate_is_noted_so_it_is_not_read_as_never_looked_at() {
        let json = r#"{
            "streams": [{
                "index": 0, "codec_name": "h264", "codec_type": "video",
                "width": 1920, "height": 1080,
                "r_frame_rate": "0/0"
            }],
            "format": { "format_name": "matroska,webm" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.primary_video().unwrap().frame_rate_fps, None);
        assert!(
            p.notes.iter().any(|n| n.contains("r_frame_rate") && n.contains("0/0")),
            "a field that was present but unusable must say so, or it is \
             indistinguishable from a field nobody read: {:?}",
            p.notes
        );
        // And it changed nothing else.
        assert_eq!(p.primary_video().unwrap().width, Some(1920));
    }

    #[test]
    fn mprb03_channel_layout_keeps_ffprobes_literal_unknown() {
        // Deliberately the one place `normalize_descriptor`'s "unknown => None"
        // rule is NOT applied: "the muxer declared the positions unknown" and
        // "no layout field at all" are different facts, and a downmix decision
        // reads them differently.
        let json = r#"{
            "streams": [
                { "index": 0, "codec_name": "flac", "codec_type": "audio",
                  "channels": 6, "channel_layout": "unknown" },
                { "index": 1, "codec_name": "aac", "codec_type": "audio",
                  "channels": 2 }
            ],
            "format": { "format_name": "matroska,webm" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        assert_eq!(p.audio[0].channel_layout.as_deref(), Some("unknown"));
        assert_eq!(p.audio[1].channel_layout, None);
        // The typed count is unaffected either way, so nothing depends on
        // parsing that string.
        assert_eq!(p.audio[0].channels, Some(6));
    }

    #[test]
    fn mprb03_contradictory_colour_tags_are_noted_and_change_no_verdict() {
        let json = r#"{
            "streams": [{
                "index": 0, "codec_name": "hevc", "codec_type": "video",
                "width": 3840, "height": 2160, "pix_fmt": "yuv420p10le",
                "color_primaries": "bt2020", "color_transfer": "bt709"
            }],
            "format": { "format_name": "matroska,webm" }
        }"#;
        let p = parse_probe_json(json).unwrap();
        let v = p.primary_video().unwrap();
        assert!(
            p.notes.iter().any(|n| n.contains("bt2020") && n.contains("bt709")),
            "notes: {:?}",
            p.notes
        );
        // Both tags survive verbatim — the note describes the document, it does
        // not repair it.
        assert_eq!(v.color_primaries.as_deref(), Some("bt2020"));
        assert_eq!(v.color_transfer.as_deref(), Some("bt709"));
        // And the ONE authority on dynamic range is untouched by the note: a
        // recognised SDR transfer is still SDR. If this ever disagrees with
        // `foundry::hdr`, a second classifier has appeared.
        assert_eq!(crate::foundry::hdr::classify_hdr(v), crate::foundry::hdr::HdrVerdict::Sdr);
    }

    fn document_with_streams(n: usize) -> String {
        let streams: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    r#"{{"index":{i},"codec_name":"subrip","codec_type":"subtitle"}}"#
                )
            })
            .collect();
        format!(
            r#"{{"streams":[{}],"format":{{"format_name":"matroska,webm"}}}}"#,
            streams.join(",")
        )
    }

    #[test]
    fn mprb03_a_document_above_the_stream_cap_is_refused_not_truncated() {
        let over = parse_probe_json(&document_with_streams(MAX_STREAMS + 1));
        match over {
            Err(ProbeError::MalformedOutput { message }) => {
                assert!(
                    message.contains(&(MAX_STREAMS + 1).to_string())
                        && message.contains(&MAX_STREAMS.to_string()),
                    "the message must name both the count seen and the cap: {message}"
                );
            }
            other => panic!("expected MalformedOutput, got {other:?}"),
        }

        // Exactly at the cap is a real file, not an attack, and must parse.
        let at = parse_probe_json(&document_with_streams(MAX_STREAMS))
            .expect("a document AT the cap must still parse");
        assert_eq!(at.subtitles.len(), MAX_STREAMS);

        // And the measured worst case in this library — 42 subtitle streams on
        // one file — is nowhere near it. This is the assertion that would fail
        // if someone tightened the bound to a number a real file exceeds.
        assert!(
            MAX_STREAMS > 42 * 4,
            "the cap must stay well clear of the largest shape this library holds"
        );
    }

    #[test]
    fn mprb03_an_over_long_tag_is_cut_and_says_that_it_was_cut() {
        let long = "A".repeat(MAX_TAG_LEN * 2);
        let json = format!(
            r#"{{"streams":[{{"index":0,"codec_name":"h264","codec_type":"video",
                 "width":1920,"height":1080}}],
                "format":{{"format_name":"matroska,webm","tags":{{"title":"{long}"}}}}}}"#
        );
        let p = parse_probe_json(&json).unwrap();
        let title = p.title.expect("the title survives, truncated");
        assert_eq!(title.len(), MAX_TAG_LEN);
        assert!(
            p.notes.iter().any(|n| n.contains("title") && n.contains("truncated")),
            "a silent truncation is data that lies about being whole: {:?}",
            p.notes
        );
    }

    #[test]
    fn mprb03_truncation_cuts_on_a_char_boundary_not_mid_codepoint() {
        // A real library holds non-ASCII titles. Slicing a `String` at an
        // arbitrary byte index PANICS, which in a 16,221-file sweep is one
        // file taking down the worker.
        // A **3-byte** codepoint, chosen deliberately. This test was first
        // written with `é` (2 bytes), and mutation testing proved it BLIND:
        // `MAX_TAG_LEN` is 1024, which is even, so a run of 2-byte characters
        // puts a char boundary exactly at the cut index and the boundary walk
        // never has to do anything. Deleting the walk entirely left the test
        // green. 1024 % 3 == 1, so with a 3-byte character the cut index lands
        // one byte INSIDE a codepoint and an unguarded `s[..MAX_TAG_LEN]`
        // panics — which is the fault this test exists to catch.
        //
        // The two-byte case is kept as well, since it is the common one in a
        // Latin-script library; it just cannot carry the test on its own.
        for ch in ['\u{2603}', 'é'] {
            let long: String = std::iter::repeat(ch).take(MAX_TAG_LEN).collect();
            assert!(long.len() > MAX_TAG_LEN, "the fixture must exceed the cap");
            let json = format!(
                r#"{{"streams":[{{"index":0,"codec_name":"h264","codec_type":"video",
                     "width":1920,"height":1080}}],
                    "format":{{"format_name":"matroska,webm","tags":{{"title":"{long}"}}}}}}"#
            );
            let p = parse_probe_json(&json).expect("must not panic");
            let title = p.title.unwrap();
            assert!(title.len() <= MAX_TAG_LEN, "{ch:?}: {}", title.len());
            assert!(title.chars().all(|c| c == ch), "{ch:?}: mangled codepoint");
            // And the cut really did have to move: a whole number of 3-byte
            // characters cannot end on 1024, so the length must be short of
            // the cap rather than exactly on it.
            if ch == '\u{2603}' {
                assert_eq!(
                    title.len(),
                    MAX_TAG_LEN - 1,
                    "the cut must walk back to the nearest char boundary"
                );
            }
            assert!(!p.notes.is_empty());
        }
    }

    #[test]
    fn mprb03_the_note_list_is_bounded_and_says_how_many_it_dropped() {
        // 40 streams that each produce exactly one note, against a 32 cap.
        let streams: Vec<String> = (0..40)
            .map(|i| {
                format!(
                    r#"{{"index":{i},"codec_name":"h264","codec_type":"video",
                       "width":1920,"height":1080,"r_frame_rate":"0/0"}}"#
                )
            })
            .collect();
        let json = format!(
            r#"{{"streams":[{}],"format":{{"format_name":"matroska,webm"}}}}"#,
            streams.join(",")
        );
        let p = parse_probe_json(&json).unwrap();

        assert_eq!(p.notes.len(), MAX_NOTES, "the cap must actually bind");
        let last = p.notes.last().unwrap();
        assert!(
            last.contains("9 further"),
            "31 notes shown out of 40 means 9 not shown, and the list must say \
             so rather than just stopping: {last}"
        );
        // The earlier notes are the real ones, not all marker.
        assert!(p.notes[0].contains("r_frame_rate"));
    }

    #[test]
    fn mprb03_a_healthy_document_produces_no_notes_at_all() {
        // The negative that makes every assertion above mean something: notes
        // are an exception channel, not decoration on every parse.
        assert!(h264_mkv().notes.is_empty());
        assert!(parse_probe_json(FULL_FIELDS).unwrap().notes.is_empty());
    }

}
