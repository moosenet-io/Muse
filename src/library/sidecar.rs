//! MUSEL-B1 — sidecar `.nfo`/poster/fanart detection beside a media file.
//!
//! *arr-organized libraries (Radarr/Sonarr, plus most manually-curated
//! libraries) conventionally drop a handful of well-known filenames next to
//! the media file itself: `movie.nfo`/`tvshow.nfo` (or `<basename>.nfo`),
//! `poster.jpg`/`poster.png`, `fanart.jpg`/`fanart.png`, `folder.jpg` (a
//! poster fallback used by some tools), and per-season art
//! (`season01-poster.jpg` etc, beside the season folder). [`detect`] looks
//! for those beside a media file's directory and reports what it found —
//! detection + a READ-ONLY byte read, never a write.
//!
//! Every filesystem touch here is read-only: [`std::fs::read_dir`] (listing
//! a directory) and [`read_bytes`]'s `OpenOptions::new().read(true)` (never
//! `.write(true)`/`.create(true)`). Nothing in this module ever creates,
//! modifies, or removes a file inside the library.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::metadata::resolve;

use crate::error::{MuseError, MuseResult};

/// Well-known movie/show-level NFO filenames (checked case-insensitively).
const NFO_NAMES: &[&str] = &["movie.nfo", "tvshow.nfo"];
/// Well-known poster filenames, in preference order (checked
/// case-insensitively). `folder.jpg` is a poster fallback some libraries
/// (and older Kodi/XBMC-descended tooling) use in place of `poster.*`.
const POSTER_NAMES: &[&str] = &["poster.jpg", "poster.png", "folder.jpg", "folder.png"];
/// Well-known backdrop/fanart filenames, in preference order.
const FANART_NAMES: &[&str] = &["fanart.jpg", "fanart.png", "backdrop.jpg", "backdrop.png"];

/// What [`detect`] found beside one media file. Every field is an absolute
/// path into the (read-only) library — callers read the bytes themselves
/// via [`read_bytes`] only for the art they actually intend to cache, so a
/// scan pass that skips art caching (e.g. the file didn't resolve to a
/// catalog row) never touches the sidecar bytes at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SidecarArt {
    /// `movie.nfo`/`tvshow.nfo`/`<basename>.nfo`, if present.
    pub nfo_path: Option<PathBuf>,
    /// The first matching poster filename found (see [`POSTER_NAMES`]).
    pub poster_path: Option<PathBuf>,
    /// The first matching fanart/backdrop filename found (see
    /// [`FANART_NAMES`]).
    pub fanart_path: Option<PathBuf>,
    /// Per-season poster art in the media file's directory
    /// (`season##-poster.*`, case-insensitive) — a TV library convention;
    /// empty for a movie or a file whose directory has none.
    pub season_poster_paths: Vec<PathBuf>,
}

impl SidecarArt {
    /// True when nothing was found — the scanner treats this as "no local
    /// sidecar art; fall back to a provider re-fetch," never an error.
    pub fn is_empty(&self) -> bool {
        self.nfo_path.is_none()
            && self.poster_path.is_none()
            && self.fanart_path.is_none()
            && self.season_poster_paths.is_empty()
    }
}

/// Detect sidecar `.nfo`/poster/fanart/season-art files beside
/// `media_file`'s directory. READ-ONLY: only lists the directory
/// (`read_dir`) and checks each candidate's existence — never reads or
/// writes file contents. Returns an empty [`SidecarArt`] (never an error)
/// when the directory can't be listed (permissions, a momentarily
/// unavailable mount, a race where the file was removed) — a scan pass
/// treats "no sidecar art found" and "couldn't check" identically: fall
/// back to a provider re-fetch, never abort the file's scan over it.
pub fn detect(media_file: &Path) -> SidecarArt {
    let Some(dir) = media_file.parent() else {
        return SidecarArt::default();
    };

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::debug!(
                dir = %dir.display(),
                error = %e,
                "library sidecar: could not list directory; treating as no sidecar art found"
            );
            return SidecarArt::default();
        }
    };

    let stem_nfo = media_file
        .file_stem()
        .map(|s| format!("{}.nfo", s.to_string_lossy().to_lowercase()));

    let mut art = SidecarArt::default();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_lowercase()) else {
            continue;
        };

        if art.nfo_path.is_none()
            && (NFO_NAMES.contains(&name.as_str()) || stem_nfo.as_deref() == Some(name.as_str()))
        {
            art.nfo_path = Some(path.clone());
        }

        if art.poster_path.is_none() && POSTER_NAMES.contains(&name.as_str()) {
            art.poster_path = Some(path.clone());
        }

        if art.fanart_path.is_none() && FANART_NAMES.contains(&name.as_str()) {
            art.fanart_path = Some(path.clone());
        }

        if name.starts_with("season") && name.contains("poster") {
            art.season_poster_paths.push(path.clone());
        }
    }

    art.season_poster_paths.sort();
    art
}

/// Read a sidecar file's bytes, strictly READ-ONLY (`OpenOptions::new()
/// .read(true)` — never `.write(true)`/`.create(true)`/`.append(true)`).
/// Used by a caller that intends to cache the art bytes (see
/// `repo::artwork_cache`); detection alone ([`detect`]) never calls this.
pub fn read_bytes(path: &Path) -> MuseResult<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| MuseError::upstream(format!("library sidecar: could not open {}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| MuseError::upstream(format!("library sidecar: could not read {}: {e}", path.display())))?;
    Ok(bytes)
}

/// Best-effort content-type guess from a sidecar art path's extension, for
/// the `artwork_cache.content_type` column. Defaults to `image/jpeg` (the
/// overwhelmingly common case in practice) for an unrecognized/missing
/// extension rather than erroring — a content-type guess is advisory, not a
/// correctness gate.
pub fn guess_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()) {
        Some(ext) if ext == "png" => "image/png",
        _ => "image/jpeg",
    }
}

/// Extract a provider id embedded in a `.nfo`'s XML — the *arr suite (and
/// Kodi's own scrapers) writes the id it identified the title against right
/// into the `.nfo` it drops beside the media file, which makes a `.nfo` the
/// single highest-value sidecar for matching: it's the *arr suite's own
/// identification, not a filename guess. Recognizes, in priority order:
/// 1. `<uniqueid type="tmdb">…</uniqueid>` / `type="imdb"` / `type="tvdb"`
///    (the current Kodi/`*arr` NFO schema — any attribute order, single or
///    double quotes).
/// 2. Legacy flat tags: `<tmdbid>…</tmdbid>`, `<imdbid>…</imdbid>`, and a
///    bare `<id>…</id>` (older Kodi `movie.nfo` scraped via the TMDb
///    scraper wrote the TMDb id into a plain `<id>` tag with no `uniqueid`
///    wrapper at all — treated as a TMDb id here, the common-case
///    convention; a title where that guess is wrong just fails to resolve
///    locally, which is a safe, visible "unmatched," never a wrong-confident
///    attach — see `library::scan::DbLibraryResolver`).
///
/// Deliberately a plain substring scan, not a real XML parser — the crate's
/// existing no-new-heavy-dependency posture (see `Cargo.toml`'s comments on
/// why `image`'s dependency footprint was kept minimal) and a `.nfo`'s
/// handful of well-known tags don't need one. Returns `None` (never an
/// error) for anything that doesn't parse as UTF-8-ish text or carries none
/// of the recognized tags — a `.nfo` that doesn't yield an id simply
/// contributes nothing to matching, same as no `.nfo` at all.
pub fn extract_provider_id_from_nfo(bytes: &[u8]) -> Option<(&'static str, String)> {
    let xml = String::from_utf8_lossy(bytes);
    let xml_lower = xml.to_lowercase();

    for (attr, provider) in [("tmdb", resolve::TMDB), ("imdb", resolve::IMDB), ("tvdb", resolve::TVDB)] {
        if let Some(value) = extract_uniqueid_value(&xml, &xml_lower, attr) {
            return Some((provider, value));
        }
    }

    if let Some(value) = extract_simple_tag_value(&xml, &xml_lower, "tmdbid") {
        return Some((resolve::TMDB, value));
    }
    if let Some(value) = extract_simple_tag_value(&xml, &xml_lower, "imdbid") {
        return Some((resolve::IMDB, value));
    }
    if let Some(value) = extract_simple_tag_value(&xml, &xml_lower, "tvdbid") {
        return Some((resolve::TVDB, value));
    }
    if let Some(value) = extract_simple_tag_value(&xml, &xml_lower, "id") {
        return Some((resolve::TMDB, value));
    }

    None
}

/// Find `<uniqueid type="{provider_attr}" ...>VALUE</uniqueid>` (attribute
/// order/quoting-tolerant): locates `type="{provider_attr}"` (or
/// single-quoted), then the `>` that closes that opening tag, then the text
/// up to the next `<`.
fn extract_uniqueid_value(xml: &str, xml_lower: &str, provider_attr: &str) -> Option<String> {
    let needle_dq = format!("type=\"{provider_attr}\"");
    let needle_sq = format!("type='{provider_attr}'");
    let attr_pos = xml_lower.find(&needle_dq).or_else(|| xml_lower.find(&needle_sq))?;

    let tail = &xml[attr_pos..];
    let gt = tail.find('>')?;
    let value_start = &tail[gt + 1..];
    let lt = value_start.find('<')?;
    let value = value_start[..lt].trim();

    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Find `<{tag}>VALUE</{tag}>` (case-insensitive on the tag name).
fn extract_simple_tag_value(xml: &str, xml_lower: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");

    let open_pos = xml_lower.find(&open)?;
    let value_start = open_pos + open.len();
    let rel_close = xml_lower[value_start..].find(&close)?;
    let value = xml[value_start..value_start + rel_close].trim();

    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("muse-sidecar-test-{name}-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    #[test]
    fn detects_movie_nfo_and_poster_and_fanart() {
        let dir = unique_dir("basic");
        let media = dir.join("The.Matrix.1999.1080p.BluRay.x264-GRP.mkv");
        fs::write(&media, b"not a real video").unwrap();
        fs::write(dir.join("movie.nfo"), b"<movie></movie>").unwrap();
        fs::write(dir.join("poster.jpg"), b"fake-jpeg-bytes").unwrap();
        fs::write(dir.join("fanart.jpg"), b"fake-jpeg-bytes-2").unwrap();

        let art = detect(&media);
        assert_eq!(art.nfo_path, Some(dir.join("movie.nfo")));
        assert_eq!(art.poster_path, Some(dir.join("poster.jpg")));
        assert_eq!(art.fanart_path, Some(dir.join("fanart.jpg")));
        assert!(!art.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_basename_nfo_when_no_movie_tvshow_nfo_present() {
        let dir = unique_dir("basename-nfo");
        let media = dir.join("Some.Show.S01E01.mkv");
        fs::write(&media, b"x").unwrap();
        fs::write(dir.join("some.show.s01e01.nfo"), b"<episodedetails/>").unwrap();

        let art = detect(&media);
        assert_eq!(art.nfo_path, Some(dir.join("some.show.s01e01.nfo")));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn falls_back_to_folder_jpg_as_a_poster() {
        let dir = unique_dir("folder-jpg");
        let media = dir.join("Movie.Name.2020.mkv");
        fs::write(&media, b"x").unwrap();
        fs::write(dir.join("folder.jpg"), b"x").unwrap();

        let art = detect(&media);
        assert_eq!(art.poster_path, Some(dir.join("folder.jpg")));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_season_poster_art() {
        let dir = unique_dir("season-poster");
        let media = dir.join("Show.S02E01.mkv");
        fs::write(&media, b"x").unwrap();
        fs::write(dir.join("season02-poster.jpg"), b"x").unwrap();
        fs::write(dir.join("Season01-poster.jpg"), b"x").unwrap();

        let art = detect(&media);
        assert_eq!(art.season_poster_paths.len(), 2);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_sidecar_art_present_is_a_clean_empty_result_not_an_error() {
        let dir = unique_dir("empty");
        let media = dir.join("Movie.mkv");
        fs::write(&media, b"x").unwrap();

        let art = detect(&media);
        assert!(art.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unreadable_directory_is_a_clean_empty_result_not_a_panic() {
        let dir = std::env::temp_dir().join("muse-sidecar-test-does-not-exist-at-all");
        let media = dir.join("Movie.mkv");

        let art = detect(&media);
        assert!(art.is_empty());
    }

    #[test]
    fn read_bytes_reads_exactly_what_was_written_read_only() {
        let dir = unique_dir("read-bytes");
        let poster = dir.join("poster.jpg");
        fs::write(&poster, b"some-jpeg-bytes").unwrap();

        let bytes = read_bytes(&poster).expect("read_bytes should succeed");
        assert_eq!(bytes, b"some-jpeg-bytes");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_bytes_on_a_missing_file_is_a_typed_error_not_a_panic() {
        let missing = std::env::temp_dir().join("muse-sidecar-does-not-exist.jpg");
        assert!(read_bytes(&missing).is_err());
    }

    #[test]
    fn guess_content_type_defaults_to_jpeg() {
        assert_eq!(guess_content_type(Path::new("poster.jpg")), "image/jpeg");
        assert_eq!(guess_content_type(Path::new("poster.png")), "image/png");
        assert_eq!(guess_content_type(Path::new("poster")), "image/jpeg");
    }

    #[test]
    fn extract_provider_id_from_nfo_reads_a_uniqueid_tmdb_tag() {
        let nfo = br#"<movie><uniqueid type="tmdb" default="true">603</uniqueid></movie>"#;
        assert_eq!(extract_provider_id_from_nfo(nfo), Some((resolve::TMDB, "603".to_string())));
    }

    #[test]
    fn extract_provider_id_from_nfo_reads_uniqueid_imdb_and_tvdb() {
        let imdb_nfo = br#"<movie><uniqueid type="imdb">tt0133093</uniqueid></movie>"#;
        assert_eq!(
            extract_provider_id_from_nfo(imdb_nfo),
            Some((resolve::IMDB, "tt0133093".to_string()))
        );

        let tvdb_nfo = br#"<tvshow><uniqueid type='tvdb'>81189</uniqueid></tvshow>"#;
        assert_eq!(extract_provider_id_from_nfo(tvdb_nfo), Some((resolve::TVDB, "81189".to_string())));
    }

    #[test]
    fn extract_provider_id_from_nfo_prefers_tmdb_over_imdb_over_tvdb() {
        let nfo = br#"<movie>
            <uniqueid type="tvdb">999</uniqueid>
            <uniqueid type="imdb">tt0133093</uniqueid>
            <uniqueid type="tmdb">603</uniqueid>
        </movie>"#;
        assert_eq!(extract_provider_id_from_nfo(nfo), Some((resolve::TMDB, "603".to_string())));
    }

    #[test]
    fn extract_provider_id_from_nfo_falls_back_to_legacy_flat_tags() {
        let tmdbid_nfo = br#"<movie><tmdbid>603</tmdbid></movie>"#;
        assert_eq!(
            extract_provider_id_from_nfo(tmdbid_nfo),
            Some((resolve::TMDB, "603".to_string()))
        );

        let imdbid_nfo = br#"<movie><imdbid>tt0133093</imdbid></movie>"#;
        assert_eq!(
            extract_provider_id_from_nfo(imdbid_nfo),
            Some((resolve::IMDB, "tt0133093".to_string()))
        );

        // Oldest legacy shape: a bare <id> with no uniqueid/tmdbid wrapper
        // at all -- treated as a TMDb id (documented convention).
        let bare_id_nfo = br#"<movie><id>603</id></movie>"#;
        assert_eq!(extract_provider_id_from_nfo(bare_id_nfo), Some((resolve::TMDB, "603".to_string())));
    }

    #[test]
    fn extract_provider_id_from_nfo_returns_none_for_no_recognized_tag() {
        let nfo = br#"<movie><title>Some Movie</title></movie>"#;
        assert_eq!(extract_provider_id_from_nfo(nfo), None);
    }

    #[test]
    fn extract_provider_id_from_nfo_returns_none_for_garbage_bytes() {
        let garbage: &[u8] = &[0xff, 0xfe, 0x00, 0x01, 0x02];
        assert_eq!(extract_provider_id_from_nfo(garbage), None);
    }
}
