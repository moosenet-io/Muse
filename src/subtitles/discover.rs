//! SUBS-01 — discovery of the two LOCAL subtitle tiers: embedded streams and
//! sidecar files.
//!
//! These are the tiers Muse *inherits* rather than chooses (see the ordering
//! argument in [`super`]), and they are handled here together because they
//! share a property the provider tier does not have: they are already
//! associated with this exact file, by whoever produced or organised it.
//!
//! # Read-only, like the rest of the library surface
//!
//! Everything here is `read_dir` and `symlink_metadata`, exactly as
//! [`crate::library::sidecar`] is, and for the same reason: `MUSE_LIBRARY_ROOT`
//! is a read-only boundary in this crate. Nothing in this module creates,
//! modifies, or removes a file. Symlinked candidates are rejected rather than
//! followed, so a link inside the library cannot be used to reach a file
//! outside it.

use std::path::{Path, PathBuf};

use crate::media::probe::{MediaProbe, SubtitleStream};

use super::cues::SubtitleFormat;
use super::{AvailableSubtitle, SubtitleSource};

/// Subtitle file extensions Muse will pick up as a sidecar.
///
/// A strict subset of [`crate::safety::SIDECAR_EXTENSIONS`] — that list also
/// covers artwork and metadata, which are not subtitles. `sub`/`idx`/`sup` are
/// included even though Muse cannot re-time them: they are real subtitles the
/// operator may want to select, and listing them while marking them
/// non-shiftable is more honest than pretending they are not there.
pub const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "vtt", "ass", "ssa", "sub", "idx", "sup"];

/// Map an ffprobe subtitle codec name to the text format Muse can shift it as.
/// `None` for image-based codecs and for anything unrecognised — an unknown
/// codec is treated as non-shiftable rather than optimistically assumed to be
/// SubRip.
pub fn format_for_codec(codec: &str) -> Option<SubtitleFormat> {
    match codec.trim().to_ascii_lowercase().as_str() {
        "subrip" | "srt" => Some(SubtitleFormat::SubRip),
        "webvtt" | "vtt" => Some(SubtitleFormat::WebVtt),
        "ass" | "ssa" => Some(SubtitleFormat::AdvancedSubStation),
        // `mov_text` (MP4 timed text) is text, but its timings live in the
        // container's sample table rather than in a text body, so this crate
        // cannot shift it by rewriting a file. Non-shiftable, deliberately.
        _ => None,
    }
}

/// Whether a codec is one of the known image-based subtitle formats.
///
/// These carry bitmaps, not text. They are named so they can be marked
/// non-shiftable — handing one to [`super::cues::apply_offset`] would be a
/// category error, and the operator needs to know the timing control does not
/// apply before they reach for it.
///
/// The list itself is **not** restated here. It is
/// [`crate::media::probe::BITMAP_SUBTITLE_CODECS`], the same one
/// [`crate::foundry::directplay::direct_play_blockers`] and
/// [`crate::foundry::ladder`] consult; before `SUBCODEC-01` this module kept a
/// second, shorter copy and the two disagreed about `pgssub`/`dvdsub`/`dvbsub`.
/// "Image" here and "bitmap" there are the same rule under two names, so they
/// resolve through one symbol.
pub fn is_image_codec(codec: &str) -> bool {
    crate::media::probe::is_bitmap_subtitle_codec(codec)
}

/// Enumerate the embedded subtitle tracks in an already-taken probe.
/// **Pure** — the ffprobe call itself lives in [`crate::media::probe`].
///
/// Real files in this library carry dozens of subtitle streams — the measured
/// worst case is recorded once, at
/// [`crate::foundry::validate::SubtitleBand::Extreme`], and deliberately not
/// restated here; this module used to carry its own copy of that figure and it
/// went stale along with every other copy. So this is not a theoretical list;
/// it is routinely the largest of the three tiers, and it is the tier that is
/// already in sync by construction.
pub fn embedded_from_probe(probe: &MediaProbe) -> Vec<AvailableSubtitle> {
    probe.subtitles.iter().map(available_from_stream).collect()
}

fn available_from_stream(stream: &SubtitleStream) -> AvailableSubtitle {
    AvailableSubtitle {
        source: SubtitleSource::Embedded {
            stream_index: stream.index,
            codec: stream.codec.clone(),
        },
        language: stream
            .language
            .as_deref()
            .map(|l| l.trim().to_ascii_lowercase())
            .filter(|l| !l.is_empty() && l != "und"),
        display: None,
        format: format_for_codec(&stream.codec),
        forced: stream.forced,
        // ffprobe exposes an SDH/hearing-impaired disposition on some files,
        // but `SubtitleStream` does not model it, so this is left false rather
        // than inferred from the title tag. An inferred-wrong flag would make
        // `select_preferred` reorder tracks on a guess.
        hearing_impaired: false,
    }
}

/// Why a persisted embedded selection no longer matches the file.
///
/// This exists because an embedded selection is stored as a stream INDEX, and
/// an index is only meaningful against one particular file. Foundry's
/// transcode path maps `-map 0:s?` and stream-copies every subtitle track, and
/// verifies count/codec/language/disposition afterwards, so a normalization
/// does not lose tracks — but a file can still be REPLACED by a quality
/// upgrade, and a different release will have a different stream layout. The
/// index alone cannot tell those cases apart; the index plus the codec and
/// language can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddedDrift {
    /// No stream at that index exists any more.
    StreamGone { stream_index: u32 },
    /// A stream exists at that index but is a different track.
    StreamChanged {
        stream_index: u32,
        expected_codec: String,
        actual_codec: String,
        expected_language: Option<String>,
        actual_language: Option<String>,
    },
}

impl std::fmt::Display for EmbeddedDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StreamGone { stream_index } => write!(
                f,
                "the selected embedded subtitle (stream {stream_index}) is no longer present in \
                 this file — it was probably replaced by a different release"
            ),
            Self::StreamChanged {
                stream_index,
                expected_codec,
                actual_codec,
                expected_language,
                actual_language,
            } => write!(
                f,
                "stream {stream_index} is no longer the subtitle that was selected (expected \
                 {expected_codec}/{}, found {actual_codec}/{}) — the file was replaced or \
                 remuxed, so the selection has been invalidated rather than silently pointed \
                 at a different track",
                expected_language.as_deref().unwrap_or("untagged"),
                actual_language.as_deref().unwrap_or("untagged"),
            ),
        }
    }
}

/// Check that a persisted embedded selection still identifies the same track.
/// **Pure.**
///
/// Returns `Ok(())` when the stream at `stream_index` still has the recorded
/// codec and language. Otherwise an [`EmbeddedDrift`] — which the caller must
/// surface, NOT silently repair by picking a nearby track. Silently
/// re-pointing a selection is how an operator ends up watching a film with
/// Hungarian subtitles they never chose.
pub fn verify_embedded_selection(
    probe: &MediaProbe,
    stream_index: u32,
    expected_codec: &str,
    expected_language: Option<&str>,
) -> Result<(), EmbeddedDrift> {
    let Some(stream) = probe.subtitles.iter().find(|s| s.index == stream_index) else {
        return Err(EmbeddedDrift::StreamGone { stream_index });
    };

    let actual_language = stream
        .language
        .as_deref()
        .map(|l| l.trim().to_ascii_lowercase())
        .filter(|l| !l.is_empty() && l != "und");
    let expected_language = expected_language
        .map(|l| l.trim().to_ascii_lowercase())
        .filter(|l| !l.is_empty() && l != "und");

    let codec_matches = stream.codec.eq_ignore_ascii_case(expected_codec.trim());
    let language_matches = match (&expected_language, &actual_language) {
        (Some(a), Some(b)) => super::language_matches(a, b),
        (None, None) => true,
        _ => false,
    };

    if codec_matches && language_matches {
        return Ok(());
    }

    Err(EmbeddedDrift::StreamChanged {
        stream_index,
        expected_codec: expected_codec.to_string(),
        actual_codec: stream.codec.clone(),
        expected_language,
        actual_language,
    })
}

/// A sidecar subtitle found beside a media file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarSubtitle {
    pub path: PathBuf,
    pub language: Option<String>,
    pub forced: bool,
    pub hearing_impaired: bool,
    pub format: Option<SubtitleFormat>,
}

/// Find subtitle sidecars beside `media_file`. READ-ONLY.
///
/// A directory that cannot be listed yields an empty vector, not an error —
/// the same posture [`crate::library::sidecar::detect`] takes, and for the
/// same reason: a momentarily unavailable mount must not abort a scan. That is
/// safe HERE, unlike in the provider tier, because an empty local result is
/// never presented as "no subtitles exist"; it is one tier of three, and the
/// caller reports each tier's status separately.
///
/// Only files whose stem is the media file's stem, or the stem plus
/// dot-separated tags, are considered. A directory holding two films must not
/// have one film's subtitles offered for the other.
pub fn detect_sidecars(media_file: &Path) -> Vec<SidecarSubtitle> {
    let Some(dir) = media_file.parent() else {
        return Vec::new();
    };
    let Some(media_stem) = media_file.file_stem().map(|s| s.to_string_lossy().to_lowercase()) else {
        return Vec::new();
    };

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::debug!(
                dir = %dir.display(),
                error = %e,
                "subtitles: could not list the media directory; reporting no sidecars for this tier"
            );
            return Vec::new();
        }
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Reject symlinks the same way `library::sidecar` does: `is_file()`
        // follows them, which would let a link inside the library read a file
        // outside `MUSE_LIBRARY_ROOT`.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_lowercase()) else {
            continue;
        };
        let Some(ext) = path.extension().map(|e| e.to_string_lossy().to_lowercase()) else {
            continue;
        };
        if !SUBTITLE_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }
        let Some(tags) = sidecar_tags(&name, &media_stem, &ext) else {
            continue;
        };

        found.push(SidecarSubtitle {
            path: path.clone(),
            language: tags.language,
            forced: tags.forced,
            hearing_impaired: tags.hearing_impaired,
            format: SubtitleFormat::from_extension(&ext),
        });
    }

    // `read_dir` order is unspecified; sort so the list an operator sees is
    // the same on every scan of an unchanged directory.
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

impl SidecarSubtitle {
    pub fn as_available(&self) -> AvailableSubtitle {
        AvailableSubtitle {
            source: SubtitleSource::Sidecar {
                path: self.path.to_string_lossy().into_owned(),
            },
            language: self.language.clone(),
            display: None,
            format: self.format,
            forced: self.forced,
            hearing_impaired: self.hearing_impaired,
        }
    }
}

/// What a sidecar filename's dot-separated tags say about it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SidecarTags {
    pub language: Option<String>,
    pub forced: bool,
    pub hearing_impaired: bool,
}

/// Parse `<media stem>[.tag]*.<ext>` into its tags. **Pure.**
///
/// Returns `None` when the filename does not belong to this media file at all,
/// which is what keeps one film's subtitles out of another's list in a shared
/// directory.
///
/// Recognised tags, in any order: a language code or name, `forced`, and the
/// SDH markers `sdh`/`cc`/`hi`. Unrecognised tags are ignored rather than
/// treated as a language — `Movie.en.track3.srt` is English, not `track3`.
pub fn sidecar_tags(filename: &str, media_stem: &str, ext: &str) -> Option<SidecarTags> {
    let filename = filename.to_lowercase();
    let media_stem = media_stem.to_lowercase();

    let stem = filename.strip_suffix(&format!(".{}", ext.to_lowercase()))?;

    let remainder = if stem == media_stem {
        ""
    } else {
        // Must be the media stem followed by a DOT. Without the dot check,
        // `Movie2.srt` would be accepted as a sidecar of `Movie`.
        stem.strip_prefix(&media_stem)?.strip_prefix('.')?
    };

    let mut tags = SidecarTags::default();
    for token in remainder.split('.').filter(|t| !t.is_empty()) {
        match token {
            "forced" => tags.forced = true,
            "sdh" | "cc" | "hi" => tags.hearing_impaired = true,
            other => {
                // The first token that canonicalises to a known language wins;
                // later ones do not overwrite it, so `Movie.en.fr-notes.srt`
                // stays English rather than flipping on a trailing token.
                if tags.language.is_none() && super::is_known_language_tag(other) {
                    tags.language = Some(other.to_string());
                }
            }
        }
    }

    Some(tags)
}

/// Read a sidecar subtitle's text. READ-ONLY, with the same symlink
/// defence-in-depth [`crate::library::sidecar::read_bytes`] applies.
pub fn read_sidecar(path: &Path) -> crate::error::MuseResult<String> {
    let bytes = crate::library::sidecar::read_bytes(path)?;
    super::wyzie::decode_subtitle_body(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foundry::probe::MediaProbe;
    use std::fs;

    fn unique_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("muse-subs-test-{name}-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    fn probe_with(subtitles: Vec<SubtitleStream>) -> MediaProbe {
        MediaProbe {
            container: "matroska,webm".into(),
            duration_secs: Some(7200.0),
            format_bitrate_bps: None,
            size_bytes: None,
            video: vec![],
            audio: vec![],
            subtitles,
            attachments: vec![],
            data_stream_count: 0,
            unindexed_stream_count: 0,
            chapter_count: 0,
            title: None,
            other_stream_count: 0,
            notes: Vec::new(),
        }
    }

    fn stream(index: u32, codec: &str, language: Option<&str>) -> SubtitleStream {
        SubtitleStream {
            index,
            codec: codec.into(),
            language: language.map(str::to_string),
            forced: false,
            default: false,
            ..Default::default()
        }
    }

    // ---------- embedded ----------

    #[test]
    fn embedded_streams_become_the_top_preference_tier() {
        let probe = probe_with(vec![
            stream(2, "subrip", Some("eng")),
            stream(3, "ass", Some("jpn")),
        ]);
        let available = embedded_from_probe(&probe);
        assert_eq!(available.len(), 2);
        for sub in &available {
            assert_eq!(
                sub.source.preference_rank(),
                0,
                "embedded is the first tier — it shipped with this exact encode"
            );
        }
        assert_eq!(available[0].language.as_deref(), Some("eng"));
        assert_eq!(available[0].format, Some(SubtitleFormat::SubRip));
        assert_eq!(available[1].format, Some(SubtitleFormat::AdvancedSubStation));
    }

    #[test]
    fn a_file_with_many_subtitle_streams_yields_all_of_them() {
        // 42 here is "many", not a claim about the library — the library's
        // measured maximum has one home (foundry::validate::SubtitleBand::Extreme,
        // asserted in media::probe_golden). What is under test is that NONE are
        // dropped, whatever the count.
        let streams: Vec<SubtitleStream> = (0..42).map(|i| stream(i, "subrip", Some("eng"))).collect();
        let available = embedded_from_probe(&probe_with(streams));
        assert_eq!(available.len(), 42);
    }

    #[test]
    fn an_image_based_embedded_track_is_listed_but_marked_unshiftable() {
        // Listing it is honest; claiming Muse can re-time it is not.
        let probe = probe_with(vec![stream(4, "hdmv_pgs_subtitle", Some("eng"))]);
        let available = embedded_from_probe(&probe);
        assert_eq!(available.len(), 1, "an image subtitle is still a real subtitle");
        assert!(!available[0].is_shiftable());
        // SUBCODEC-01: the full alias set, from a literal — `is_image_codec`
        // now resolves through `media::probe::BITMAP_SUBTITLE_CODECS`, and this
        // module must not grow a second, shorter copy again.
        for codec in [
            "hdmv_pgs_subtitle",
            "dvd_subtitle",
            "dvb_subtitle",
            "xsub",
            "pgssub",
            "dvdsub",
            "dvbsub",
        ] {
            assert!(is_image_codec(codec), "{codec} is bitmap");
            assert!(is_image_codec(&codec.to_ascii_uppercase()), "{codec} uppercase");
        }
        assert!(!is_image_codec("subrip"));
        assert!(!is_image_codec("webvtt"));
    }

    #[test]
    fn an_unknown_codec_is_treated_as_unshiftable_rather_than_assumed_to_be_srt() {
        assert_eq!(format_for_codec("subrip"), Some(SubtitleFormat::SubRip));
        assert_eq!(format_for_codec("ASS"), Some(SubtitleFormat::AdvancedSubStation));
        assert_eq!(format_for_codec("webvtt"), Some(SubtitleFormat::WebVtt));
        assert_eq!(format_for_codec("mov_text"), None);
        assert_eq!(format_for_codec("something_new"), None);
        assert_eq!(format_for_codec(""), None);
    }

    #[test]
    fn an_untagged_or_und_embedded_language_is_none_not_a_literal_und() {
        let probe = probe_with(vec![stream(1, "subrip", Some("und")), stream(2, "subrip", None)]);
        let available = embedded_from_probe(&probe);
        assert_eq!(available[0].language, None, "`und` is not a language");
        assert_eq!(available[1].language, None);
    }

    // ---------- embedded drift ----------

    #[test]
    fn a_matching_embedded_selection_verifies_clean() {
        let probe = probe_with(vec![stream(2, "subrip", Some("eng"))]);
        assert!(verify_embedded_selection(&probe, 2, "subrip", Some("eng")).is_ok());
        // The 2/3-letter split must not read as drift.
        assert!(verify_embedded_selection(&probe, 2, "subrip", Some("en")).is_ok());
    }

    #[test]
    fn a_replaced_file_invalidates_the_selection_rather_than_silently_repointing_it() {
        // The safety-critical case: after an upgrade, stream 2 is a different
        // track. Muse must NOT quietly serve it as the operator's choice.
        let probe = probe_with(vec![stream(2, "subrip", Some("hun"))]);
        let err = verify_embedded_selection(&probe, 2, "subrip", Some("eng")).unwrap_err();
        assert!(matches!(err, EmbeddedDrift::StreamChanged { .. }), "got {err:?}");
        assert!(err.to_string().contains("invalidated"));

        let probe = probe_with(vec![stream(9, "subrip", Some("eng"))]);
        let err = verify_embedded_selection(&probe, 2, "subrip", Some("eng")).unwrap_err();
        assert!(matches!(err, EmbeddedDrift::StreamGone { .. }), "got {err:?}");
    }

    #[test]
    fn a_codec_change_at_the_same_index_is_drift() {
        let probe = probe_with(vec![stream(2, "ass", Some("eng"))]);
        assert!(verify_embedded_selection(&probe, 2, "subrip", Some("eng")).is_err());
    }

    #[test]
    fn an_untagged_selection_verifies_only_against_an_untagged_stream() {
        let probe = probe_with(vec![stream(2, "subrip", None)]);
        assert!(verify_embedded_selection(&probe, 2, "subrip", None).is_ok());
        assert!(
            verify_embedded_selection(&probe, 2, "subrip", Some("eng")).is_err(),
            "an untagged stream must not satisfy a selection that recorded a language"
        );
    }

    // ---------- sidecar filename tags ----------

    #[test]
    fn sidecar_tags_read_language_forced_and_sdh_in_any_order() {
        let stem = "the.martian.2015.1080p";
        assert_eq!(
            sidecar_tags(&format!("{stem}.en.srt"), stem, "srt").unwrap(),
            SidecarTags {
                language: Some("en".into()),
                forced: false,
                hearing_impaired: false,
            }
        );
        assert_eq!(
            sidecar_tags(&format!("{stem}.en.forced.srt"), stem, "srt").unwrap(),
            SidecarTags {
                language: Some("en".into()),
                forced: true,
                hearing_impaired: false,
            }
        );
        assert_eq!(
            sidecar_tags(&format!("{stem}.forced.eng.srt"), stem, "srt").unwrap().forced,
            true
        );
        assert!(sidecar_tags(&format!("{stem}.en.sdh.srt"), stem, "srt").unwrap().hearing_impaired);
        assert!(sidecar_tags(&format!("{stem}.english.cc.srt"), stem, "srt").unwrap().hearing_impaired);
    }

    #[test]
    fn an_untagged_sidecar_is_accepted_with_no_language_rather_than_rejected() {
        let stem = "movie";
        let tags = sidecar_tags("movie.srt", stem, "srt").unwrap();
        assert_eq!(tags.language, None);
    }

    #[test]
    fn a_sidecar_belonging_to_a_different_film_in_the_same_directory_is_rejected() {
        // Without the dot boundary, `Movie2.en.srt` would be offered as a
        // subtitle for `Movie`.
        assert!(sidecar_tags("movie2.en.srt", "movie", "srt").is_none());
        assert!(sidecar_tags("other.film.en.srt", "movie", "srt").is_none());
        assert!(sidecar_tags("movie.en.srt", "movie", "srt").is_some());
    }

    #[test]
    fn an_unrecognised_filename_token_is_not_mistaken_for_a_language() {
        let stem = "movie";
        let tags = sidecar_tags("movie.track3.srt", stem, "srt").unwrap();
        assert_eq!(tags.language, None, "`track3` is not a language");
        let tags = sidecar_tags("movie.en.v2.srt", stem, "srt").unwrap();
        assert_eq!(tags.language.as_deref(), Some("en"));
        let tags = sidecar_tags("movie.v2.en.srt", stem, "srt").unwrap();
        assert_eq!(tags.language.as_deref(), Some("en"), "a junk token must not shadow a real one");
    }

    #[test]
    fn the_first_language_tag_wins_and_a_later_one_does_not_overwrite_it() {
        let tags = sidecar_tags("movie.en.fr.srt", "movie", "srt").unwrap();
        assert_eq!(tags.language.as_deref(), Some("en"));
    }

    // ---------- sidecar filesystem detection ----------

    #[test]
    fn detects_subtitle_sidecars_beside_a_media_file_and_ignores_unrelated_files() {
        let dir = unique_dir("detect");
        let media = dir.join("The.Martian.2015.1080p.mkv");
        fs::write(&media, b"not real video").unwrap();
        fs::write(dir.join("The.Martian.2015.1080p.en.srt"), b"1\n").unwrap();
        fs::write(dir.join("The.Martian.2015.1080p.fr.forced.ass"), b"x").unwrap();
        // Not subtitles, and not ours:
        fs::write(dir.join("poster.jpg"), b"x").unwrap();
        fs::write(dir.join("movie.nfo"), b"x").unwrap();
        fs::write(dir.join("Another.Film.en.srt"), b"x").unwrap();

        let found = detect_sidecars(&media);
        assert_eq!(found.len(), 2, "found: {found:?}");
        assert!(found.iter().all(|s| s.path.parent() == Some(dir.as_path())));
        assert!(found.iter().any(|s| s.language.as_deref() == Some("en")));
        let forced = found.iter().find(|s| s.forced).unwrap();
        assert_eq!(forced.format, Some(SubtitleFormat::AdvancedSubStation));

        // Sidecars are the SECOND tier, always.
        for s in &found {
            assert_eq!(s.as_available().source.preference_rank(), 1);
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_image_based_sidecar_is_listed_but_not_shiftable() {
        let dir = unique_dir("image-sidecar");
        let media = dir.join("Movie.mkv");
        fs::write(&media, b"x").unwrap();
        fs::write(dir.join("Movie.en.sub"), b"x").unwrap();

        let found = detect_sidecars(&media);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].format, None);
        assert!(!found[0].as_available().is_shiftable());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn a_symlinked_sidecar_is_rejected_not_followed_outside_the_library() {
        let dir = unique_dir("symlink");
        let outside = unique_dir("symlink-outside");
        fs::write(outside.join("real.srt"), b"outside-the-library").unwrap();
        let media = dir.join("Movie.mkv");
        fs::write(&media, b"x").unwrap();
        std::os::unix::fs::symlink(outside.join("real.srt"), dir.join("Movie.en.srt")).unwrap();

        assert!(
            detect_sidecars(&media).is_empty(),
            "a symlinked sidecar must not be followed out of the library root"
        );

        fs::remove_dir_all(&dir).ok();
        fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn sidecar_detection_returns_a_declared_sorted_order_not_read_dir_order() {
        // `read_dir` order is unspecified. Asserting only that two consecutive
        // calls agree is NOT enough — an earlier version of this test did
        // exactly that and passed with the sort removed, because read_dir
        // happens to be stable for an unchanged directory on this filesystem.
        // The list an operator sees must be in a DECLARED order.
        let dir = unique_dir("determinism");
        let media = dir.join("Movie.mkv");
        fs::write(&media, b"x").unwrap();
        // Written in an order that is not the sorted order, so a pass-through
        // of creation/read_dir order would be visible.
        for lang in ["ja", "fr", "en", "es", "de"] {
            fs::write(dir.join(format!("Movie.{lang}.srt")), b"x").unwrap();
        }

        let found = detect_sidecars(&media);
        assert_eq!(found.len(), 5);

        let paths: Vec<PathBuf> = found.iter().map(|s| s.path.clone()).collect();
        let mut expected = paths.clone();
        expected.sort();
        assert_eq!(paths, expected, "sidecars must be returned in sorted path order");

        // And, still, stable across repeats.
        for _ in 0..5 {
            assert_eq!(detect_sidecars(&media), found);
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_directory_yields_an_empty_tier_not_a_panic() {
        let missing = std::env::temp_dir().join("muse-subs-does-not-exist").join("Movie.mkv");
        assert!(detect_sidecars(&missing).is_empty());
    }

    #[test]
    fn reading_a_sidecar_refuses_binary_content() {
        let dir = unique_dir("read");
        let path = dir.join("Movie.en.srt");
        fs::write(&path, b"1\n00:00:20,000 --> 00:00:24,400\nHi\n").unwrap();
        assert!(read_sidecar(&path).unwrap().contains("00:00:20,000"));

        let binary = dir.join("Movie.fr.srt");
        fs::write(&binary, [0u8, 1, 2, 3]).unwrap();
        assert!(
            read_sidecar(&binary).is_err(),
            "binary content must be refused, not parsed into garbage cues"
        );

        fs::remove_dir_all(&dir).ok();
    }
}
