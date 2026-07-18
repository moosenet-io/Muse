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
}
