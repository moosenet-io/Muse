//! MUSEL-B1 — the walker + match + record core.
//!
//! ## What this does NOT do (important safety boundary)
//! This scanner **never creates a new `media_metadata` row**. It only ever
//! links a file it finds on disk to a title *already* cataloged in Muse's
//! DB (via `arr::ingest` or a curator) — same posture as
//! `metadata::resolve`'s `apply_enrichment` (MUSEL-A2), which is
//! additive-onto-an-existing-row only. A scanned file whose parsed
//! title/year/id tag doesn't match anything already in `media_metadata` is
//! recorded as **unmatched** (visible in the returned [`ScanReport`] and in
//! logs), never guessed at and never silently attached to the wrong title.
//! A [`ScanMatch::Tentative`] result (a `metadata::resolve::MatchConfidence
//! ::TitleSearch`-only hit — see [`DbLibraryResolver`]) is treated
//! identically to unmatched for persistence purposes: nothing is written.
//!
//! ## Why this means some real files never get a `media_files` row
//! `media_files.media_item_id` is `NOT NULL` (`migrations/0009_media_files
//! .sql`), and `media_items.media_metadata_id` is in turn `NOT NULL`
//! (`migrations/0006_media_items.sql`) — there is no schema-level "unmatched
//! file" row to attach to. Extending that schema is out of MUSEL-B1's scope
//! (not in the item's `## FILES` list). An unmatched/tentative file is
//! therefore surfaced via [`ScanReport::unmatched_paths`] (and a `tracing`
//! log) rather than a DB row — visible to an operator/future review surface,
//! per the spec's "recorded as unmatched (visible), never a wrong-confident
//! match" edge case, just not via the `media_files` table specifically.
//!
//! ## READ-ONLY
//! Every filesystem call this module makes is one of: [`std::fs::read_dir`]
//! (list a directory), [`std::fs::symlink_metadata`] (detect a symlink
//! without following it), or [`std::fs::metadata`] (stat a real file for its
//! size) via [`walk_media_files`]. Nothing here ever opens a file inside
//! the library root with write/create/append access, and nothing removes or
//! renames anything under it. Persistence (`media_items`/`media_files`/
//! `artwork_cache` rows) goes only to Muse's own Postgres database via the
//! `repo::*` modules, never back to the library filesystem.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::metadata::resolve::{self, NamedProvider, ResolveIds};
use crate::models::library::Library;
use crate::models::media_item::NewMediaItem;
use crate::models::media_metadata::MediaKind;
use crate::prowlarr::{parse_release_name, ParsedRelease};
use crate::repo;

/// Recognized media file extensions (lowercase, no leading dot). Deliberately
/// conservative and video-only — matches the set `prowlarr::parse` already
/// strips as a "known extension" plus the common remaining container types.
pub const MEDIA_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "m4v", "mov", "wmv", "ts", "webm"];

fn has_media_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| MEDIA_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// One media file found under a scan root, with its path/size/parse already
/// resolved — the unit [`LibraryResolver`] matches against.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    /// Absolute filesystem path (only ever opened read-only, and only by
    /// [`crate::library::sidecar::read_bytes`] when art is actually cached
    /// — the walk itself never opens the media file's own bytes at all).
    pub absolute_path: PathBuf,
    /// Path relative to the scan root — what gets persisted as
    /// `media_files.relative_path` / `media_items.path`.
    pub relative_path: String,
    /// `None` when the file's size couldn't be stat'd (a momentary race —
    /// logged, not fatal; see [`walk_media_files`]).
    pub size_bytes: Option<i64>,
    pub parsed: ParsedRelease,
    /// Movie vs. show, guessed from the parse: a season marker means a show,
    /// no season marker means a movie. This is the same signal
    /// `prowlarr::parse::ParsedRelease` already exposes; the scanner adds no
    /// new classification heuristic of its own.
    pub kind_guess: MediaKind,
}

/// Recursively walk `root` (READ-ONLY: `read_dir` + `symlink_metadata` +
/// `metadata` only) for files with a recognized media extension. Bounded
/// per-directory error handling: a directory that can't be listed (removed
/// mid-walk, permission race, a momentarily unavailable RO mount) is logged
/// and skipped — the walk continues with whatever else it can reach, never
/// aborting the whole pass over one bad subtree.
///
/// Symlinks are deliberately **not followed** (`symlink_metadata` rather
/// than `metadata` for the initial type check) — a library directory
/// symlinked outside `root` would otherwise let the walk escape the
/// intended read-only root, and a symlink loop would hang it. This is a
/// conservative, documented skip, not a "handled" traversal (spec edge
/// case).
pub fn walk_media_files(root: &Path) -> Vec<ScannedFile> {
    let mut out = Vec::new();
    walk_dir(root, root, &mut out);
    out
}

fn walk_dir(root: &Path, dir: &Path, out: &mut Vec<ScannedFile>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "library scan: could not list directory; skipping this subtree");
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();

        let sym_meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "library scan: could not stat entry; skipping");
                continue;
            }
        };

        if sym_meta.file_type().is_symlink() {
            tracing::debug!(path = %path.display(), "library scan: symlink not followed (documented, read-only-root safety)");
            continue;
        }

        if sym_meta.is_dir() {
            walk_dir(root, &path, out);
            continue;
        }

        if !sym_meta.is_file() || !has_media_extension(&path) {
            continue;
        }

        let size_bytes = match std::fs::metadata(&path) {
            Ok(m) => Some(m.len() as i64),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "library scan: could not stat file size; recording with unknown size");
                None
            }
        };

        let relative_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let parsed = parse_release_name(&stem);
        let kind_guess = if parsed.season.is_some() {
            MediaKind::Show
        } else {
            MediaKind::Movie
        };

        out.push(ScannedFile {
            absolute_path: path,
            relative_path,
            size_bytes,
            parsed,
            kind_guess,
        });
    }
}

/// How a [`LibraryResolver`] resolved one [`ScannedFile`] against the
/// catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanMatch {
    /// Resolved to an existing `media_metadata` row with high confidence
    /// (a `{tmdb-NNNN}`/`{tvdb-NNNN}` id tag in the path, or an exact
    /// title+year match against the catalog) — safe to record.
    Matched { media_metadata_id: i64 },
    /// Only a lowest-confidence signal fired (a free-text title search, or
    /// an id tag that didn't resolve locally) — NEVER recorded as a
    /// confident match; see the module doc.
    Tentative,
    /// Nothing resolved at all.
    Unmatched,
}

/// Matches one [`ScannedFile`] against the catalog. A trait so fixture-dir
/// tests can inject a mock (per the spec's TEST PLAN: "matches (mocked
/// resolver)") without a live DB or network.
#[async_trait]
pub trait LibraryResolver: Send + Sync {
    async fn resolve(&self, file: &ScannedFile) -> MuseResult<ScanMatch>;
}

/// Extract a `{tmdb-603}` / `{tvdb-81189}` / `{imdb-tt0133093}` id tag from
/// a path (the Radarr/Sonarr folder-naming convention — e.g. `The Matrix
/// (1999) {tmdb-603}/The.Matrix.1999.mkv`). Returns the first tag found
/// (provider name, id); `None` when no such tag is present, which is the
/// common case for a plain scene-style filename.
pub fn extract_id_tag(path_str: &str) -> Option<(&'static str, String)> {
    let lower = path_str.to_lowercase();
    for (prefix, provider) in [
        ("{tmdb-", resolve::TMDB),
        ("{tvdb-", resolve::TVDB),
        ("{imdb-", resolve::IMDB),
    ] {
        if let Some(start) = lower.find(prefix) {
            let rest = &path_str[start + prefix.len()..];
            if let Some(end) = rest.find('}') {
                let id = rest[..end].trim();
                if !id.is_empty() {
                    return Some((provider, id.to_string()));
                }
            }
        }
    }
    None
}

/// The production [`LibraryResolver`]: matches against Muse's own already
/// -cataloged `media_metadata` rows only (see the module doc — never
/// creates a row). Tries, in order:
/// 1. An id tag in the path (`{tmdb-...}`/`{tvdb-...}`) resolved against the
///    catalog via `repo::media_metadata::find_by_tmdb_id`/`find_by_tvdb_id`.
/// 2. An exact, case-insensitive title+year match via
///    `repo::media_metadata::find_by_title_year` — deterministic, not a
///    fuzzy guess, so treated as a confident [`ScanMatch::Matched`].
/// 3. If `providers` are configured and neither of the above found
///    anything, a `metadata::resolve::resolve_and_merge` title search is
///    run purely to leave a discoverable log trail ("this title looks
///    resolvable externally but isn't in the local catalog yet") — the
///    result is always [`ScanMatch::Tentative`] here (mirrors
///    `maintenance::run_metadata_resolve_pass`'s own rule: a `TitleSearch`
///    hit is never auto-persisted as authoritative), since there is no
///    local row to attach it to regardless of the provider's confidence.
pub struct DbLibraryResolver<'a> {
    pub pool: &'a PgPool,
    pub providers: &'a [NamedProvider<'a>],
}

#[async_trait]
impl<'a> LibraryResolver for DbLibraryResolver<'a> {
    async fn resolve(&self, file: &ScannedFile) -> MuseResult<ScanMatch> {
        if let Some((provider, id)) = extract_id_tag(&file.relative_path) {
            let local = match provider {
                resolve::TMDB => repo::media_metadata::find_by_tmdb_id(self.pool, file.kind_guess, &id).await?,
                resolve::TVDB => repo::media_metadata::find_by_tvdb_id(self.pool, file.kind_guess, &id).await?,
                _ => None,
            };
            // Review finding 1 (codex): an explicit id tag in the path is a
            // caller-asserted, specific identity claim. If it doesn't
            // resolve to a local catalog row, that must NOT fall through
            // to the title/year match below -- doing so risks confidently
            // attaching this file to a DIFFERENT title's row (the one that
            // happens to share a title+year) even though the path itself
            // named a specific id that isn't cataloged. This mirrors
            // MUSEL-A2's `resolve_and_merge` rule: known-but-unresolvable
            // ids never fall back to a title guess. The title/year and
            // resolve_and_merge paths below are reachable ONLY when the
            // path carried no explicit id tag at all.
            return Ok(match local {
                Some(media_metadata_id) => ScanMatch::Matched { media_metadata_id },
                None => {
                    tracing::info!(
                        path = %file.relative_path,
                        provider,
                        id,
                        "library scan: explicit id tag did not resolve to a local catalog row; \
                         recording as unmatched rather than falling back to a title/year guess"
                    );
                    ScanMatch::Unmatched
                }
            });
        }

        if let Some(title) = &file.parsed.title {
            if let Some(media_metadata_id) =
                repo::media_metadata::find_by_title_year(self.pool, file.kind_guess, title, file.parsed.year).await?
            {
                return Ok(ScanMatch::Matched { media_metadata_id });
            }
        }

        if !self.providers.is_empty() {
            if let Some(title) = &file.parsed.title {
                let ids = ResolveIds::new().with_title(title.clone());
                let resolve_kind = match file.kind_guess {
                    MediaKind::Movie => crate::metadata::MediaKind::Movie,
                    MediaKind::Show => crate::metadata::MediaKind::Series,
                };
                match resolve::resolve_and_merge(&ids, resolve_kind, self.providers).await {
                    Ok(Some(resolved)) => {
                        // Whether this came back MatchConfidence::Id or
                        // ::TitleSearch, there is still no LOCAL catalog
                        // row to attach it to -- always Tentative here.
                        let _ = resolved.confidence; // documented, see fn doc
                        tracing::info!(
                            path = %file.relative_path,
                            "library scan: title search found an external candidate with no matching \
                             local catalog row yet; recording as tentative, never auto-matched"
                        );
                        return Ok(ScanMatch::Tentative);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(path = %file.relative_path, error = %e, "library scan: title-search fallback failed; treating as unmatched");
                    }
                }
            }
        }

        Ok(ScanMatch::Unmatched)
    }
}

/// Tally + detail of one scan pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanReport {
    pub scanned: usize,
    pub matched: usize,
    pub tentative: usize,
    pub unmatched: usize,
    /// A file whose size/parse couldn't be established, or whose DB write
    /// failed — logged and skipped, never aborting the rest of the pass.
    pub errors: usize,
    /// Relative paths of files that ended up `Tentative` or `Unmatched` —
    /// the "recorded as unmatched (visible)" surface the spec's edge case
    /// asks for, since there's no schema-level row to attach them to (see
    /// the module doc).
    pub unmatched_paths: Vec<String>,
    pub art_cached: usize,
}

/// Walk `library.root_folder` and record what's found. Non-blocking per
/// file: a single file's resolve/persist failure increments
/// `ScanReport::errors` and the pass continues (same posture as
/// `maintenance::run_metadata_resolve_pass`). Idempotent — re-scanning an
/// unchanged file is a clean no-op (see
/// `repo::media_file::upsert_scanned`'s size guard).
///
/// Callers that only want the pure walk+parse+match behavior (e.g. a
/// fixture-dir test with a mocked [`LibraryResolver`] and no DB writes
/// intended) should call [`walk_media_files`] + `resolver.resolve` directly
/// rather than this DB-writing entry point; `scan_library` is the full,
/// DB-gated production path.
pub async fn scan_library(pool: &PgPool, library: &Library, resolver: &dyn LibraryResolver) -> MuseResult<ScanReport> {
    let root = PathBuf::from(&library.root_folder);
    let files = walk_media_files(&root);

    let mut report = ScanReport {
        scanned: files.len(),
        ..Default::default()
    };

    for file in files {
        let scan_match = match resolver.resolve(&file).await {
            Ok(m) => m,
            Err(e) => {
                report.errors += 1;
                tracing::warn!(path = %file.relative_path, error = %e, "library scan: resolver failed for this file; skipping");
                continue;
            }
        };

        match scan_match {
            ScanMatch::Matched { media_metadata_id } => {
                match record_matched_file(pool, library.id, media_metadata_id, &file).await {
                    Ok(art_cached) => {
                        report.matched += 1;
                        report.art_cached += art_cached;
                    }
                    Err(e) => {
                        report.errors += 1;
                        tracing::warn!(path = %file.relative_path, error = %e, "library scan: failed to record matched file; skipping");
                    }
                }
            }
            ScanMatch::Tentative => {
                report.tentative += 1;
                report.unmatched_paths.push(file.relative_path.clone());
                tracing::info!(path = %file.relative_path, "library scan: tentative match only; not recorded (see module doc)");
            }
            ScanMatch::Unmatched => {
                report.unmatched += 1;
                report.unmatched_paths.push(file.relative_path.clone());
                tracing::debug!(path = %file.relative_path, "library scan: no match found; file left unrecorded");
            }
        }
    }

    Ok(report)
}

/// Upsert the `media_items` + `media_files` rows for a confidently-matched
/// file, then detect + cache any sidecar art beside it. Returns how many
/// art variants were cached (0-2: poster/backdrop).
async fn record_matched_file(
    pool: &PgPool,
    library_id: i64,
    media_metadata_id: i64,
    file: &ScannedFile,
) -> MuseResult<usize> {
    let media_item = repo::media_item::upsert(
        pool,
        &NewMediaItem {
            library_id,
            media_metadata_id,
            path: file.relative_path.clone(),
            monitored: false,
            quality_profile_id: None,
            minimum_availability: None,
            plex_rating_key: None,
            added_at: Some(chrono::Utc::now()),
        },
    )
    .await?;

    let container = file
        .absolute_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());
    let media_info = container.map(|c| serde_json::json!({ "container": c }));

    let (_media_file, file_changed) =
        repo::media_file::upsert_scanned(pool, media_item.id, &file.relative_path, file.size_bytes, media_info)
            .await?;

    // Review finding 3 (codex): an idempotent rescan of an unchanged file
    // (the size guard says nothing changed) must be a full no-op, not just
    // "no duplicate media_files row" -- re-reading + re-writing the same
    // sidecar art bytes into `artwork_cache` on every pass is unnecessary
    // I/O and DB churn for a file that hasn't moved. Only (re-)cache art
    // when `upsert_scanned` actually recorded a change (first sighting, or
    // a genuine size change).
    if !file_changed {
        return Ok(0);
    }

    let art = crate::library::sidecar::detect(&file.absolute_path);
    let mut cached = 0usize;

    if let Some(poster_path) = &art.poster_path {
        if cache_art(pool, media_item.id, "poster", poster_path).await {
            cached += 1;
        }
    }
    if let Some(fanart_path) = &art.fanart_path {
        if cache_art(pool, media_item.id, "backdrop", fanart_path).await {
            cached += 1;
        }
    }

    Ok(cached)
}

/// Read (READ-ONLY) + cache one sidecar art file into `artwork_cache`,
/// keyed the same way `web::artwork::art_handler` already reads it
/// (`entity_kind = "media_item"`). Best-effort: a read or DB failure is
/// logged and treated as "no art cached this pass," never propagated as a
/// scan-aborting error (art is enrichment, not the file record itself).
async fn cache_art(pool: &PgPool, media_item_id: i64, variant: &str, art_path: &Path) -> bool {
    let bytes = match crate::library::sidecar::read_bytes(art_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(path = %art_path.display(), error = %e, "library scan: could not read sidecar art; skipping");
            return false;
        }
    };
    let content_type = crate::library::sidecar::guess_content_type(art_path);

    match repo::artwork_cache::store_bytes(pool, "media_item", media_item_id, variant, None, content_type, &bytes, None)
        .await
    {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(media_item_id, variant, error = %e, "library scan: could not cache sidecar art; skipping");
            false
        }
    }
}

/// Whether `candidate` (a `libraries.root_folder`) is contained within
/// `root` (`MUSE_LIBRARY_ROOT`) — **path-component-aware**, not a raw
/// string prefix (review finding 2, codex): `str::starts_with` would
/// wrongly treat `/mnt/library2` as inside `/mnt/library`, since
/// `"library2"` textually starts with `"library"`. [`Path::starts_with`]
/// compares whole path components instead, so that case is correctly
/// rejected while `/mnt/library/Movies` is correctly accepted.
///
/// Trailing slashes are trimmed first so `/mnt/library` and `/mnt/library/`
/// compare equal. When both paths actually exist on disk, canonicalizes
/// first (resolves `..`/symlinks) for the most accurate answer; falls back
/// to a purely lexical, still component-aware comparison when either side
/// doesn't exist yet (a not-yet-mounted `MUSE_LIBRARY_ROOT`, or a
/// fixture-only path in a test with no real mount) — this function must
/// stay correct without a live QNAP mount, same posture as the rest of
/// MUSEL-B1's fixture-dir test suite.
fn path_is_within_root(candidate: &str, root: &str) -> bool {
    let candidate_norm = candidate.trim_end_matches('/');
    let root_norm = root.trim_end_matches('/');

    if let (Ok(candidate_canon), Ok(root_canon)) =
        (std::fs::canonicalize(candidate_norm), std::fs::canonicalize(root_norm))
    {
        return candidate_canon.starts_with(&root_canon);
    }

    Path::new(candidate_norm).starts_with(Path::new(root_norm))
}

/// Top-level entry point: clean no-op when `MUSE_LIBRARY_ROOT`
/// (`config.library_root`) is unset (the spec's inert-when-unmounted
/// requirement — see MUSEL-B0). Scans every enabled `libraries` row whose
/// `root_folder` is inside `config.library_root`. `providers` are the same
/// configured `NamedProvider`s the maintenance pass (MUSEL-A2) already
/// builds; pass an empty slice to disable the title-search fallback signal
/// entirely.
pub async fn run_scan(
    pool: &PgPool,
    config: &crate::config::Config,
    providers: &[NamedProvider<'_>],
) -> MuseResult<Vec<ScanReport>> {
    let Some(library_root) = config.library_root.as_deref() else {
        tracing::debug!("library scan: MUSE_LIBRARY_ROOT unset; clean no-op");
        return Ok(Vec::new());
    };

    let libraries = repo::library::list(pool).await?;
    let mut reports = Vec::new();

    for library in libraries {
        if !library.enabled {
            continue;
        }
        if !path_is_within_root(&library.root_folder, library_root) {
            tracing::debug!(
                library = %library.name,
                root_folder = %library.root_folder,
                library_root,
                "library scan: library's root_folder is outside the configured MUSE_LIBRARY_ROOT; skipping"
            );
            continue;
        }

        let resolver = DbLibraryResolver { pool, providers };
        match scan_library(pool, &library, &resolver).await {
            Ok(report) => {
                tracing::info!(
                    library = %library.name,
                    scanned = report.scanned,
                    matched = report.matched,
                    tentative = report.tentative,
                    unmatched = report.unmatched,
                    errors = report.errors,
                    art_cached = report.art_cached,
                    "library scan: pass complete"
                );
                reports.push(report);
            }
            Err(e) => {
                tracing::warn!(library = %library.name, error = %e, "library scan: pass failed for this library; continuing with the rest");
            }
        }
    }

    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    fn unique_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("muse-library-scan-test-{name}-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    fn checksum_tree(root: &Path) -> Vec<(String, u64, Vec<u8>)> {
        // A small "did anything change" fingerprint over the whole fixture
        // tree: relative path + size + full bytes for every file, sorted so
        // comparison is order-independent. Good enough for a fixture tree
        // small enough for a unit test (spec: "assert the fixture dir +
        // files are byte-for-byte unchanged").
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64, Vec<u8>)>) {
            for entry in fs::read_dir(dir).expect("read_dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else if path.is_file() {
                    let bytes = fs::read(&path).expect("read fixture file");
                    let rel = path.strip_prefix(root).unwrap().to_string_lossy().to_string();
                    out.push((rel, bytes.len() as u64, bytes));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// A resolver that always returns a fixed [`ScanMatch`], recording
    /// every file it was asked about — used to test `walk_media_files` +
    /// parsing without any DB/network involved (spec: "matches (mocked
    /// resolver)").
    struct MockResolver {
        result: ScanMatch,
        seen: Mutex<Vec<String>>,
    }

    impl MockResolver {
        fn always(result: ScanMatch) -> Self {
            Self {
                result,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LibraryResolver for MockResolver {
        async fn resolve(&self, file: &ScannedFile) -> MuseResult<ScanMatch> {
            self.seen.lock().unwrap().push(file.relative_path.clone());
            Ok(self.result.clone())
        }
    }

    fn build_fixture_library(dir: &Path) {
        fs::create_dir_all(dir.join("Movies/The Matrix (1999)")).unwrap();
        fs::write(
            dir.join("Movies/The Matrix (1999)/The.Matrix.1999.1080p.BluRay.x264-GRP.mkv"),
            b"not a real video, fixture only",
        )
        .unwrap();
        fs::write(dir.join("Movies/The Matrix (1999)/movie.nfo"), b"<movie/>").unwrap();
        fs::write(dir.join("Movies/The Matrix (1999)/poster.jpg"), b"fake-poster-bytes").unwrap();

        fs::create_dir_all(dir.join("TV/Some Show/Season 01")).unwrap();
        fs::write(
            dir.join("TV/Some Show/Season 01/Some.Show.S01E01.720p.WEB-DL.x264-TEAM.mkv"),
            b"not a real video either",
        )
        .unwrap();

        // A non-media file that must be ignored by the walk.
        fs::write(dir.join("Movies/The Matrix (1999)/readme.txt"), b"ignore me").unwrap();
    }

    #[test]
    fn walk_finds_media_files_and_parses_them() {
        let dir = unique_dir("walk-basic");
        build_fixture_library(&dir);

        let files = walk_media_files(&dir);
        assert_eq!(files.len(), 2, "should find exactly the two media files, not readme.txt");

        let movie = files
            .iter()
            .find(|f| f.relative_path.ends_with(".mkv") && f.relative_path.contains("Matrix"))
            .expect("movie file found");
        assert_eq!(movie.parsed.title.as_deref(), Some("The Matrix"));
        assert_eq!(movie.parsed.year, Some(1999));
        assert_eq!(movie.kind_guess, MediaKind::Movie);
        assert!(movie.size_bytes.unwrap() > 0);

        let episode = files
            .iter()
            .find(|f| f.relative_path.contains("Some.Show"))
            .expect("episode file found");
        assert_eq!(episode.parsed.season, Some(1));
        assert_eq!(episode.parsed.episode, Some(1));
        assert_eq!(episode.kind_guess, MediaKind::Show);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walk_ignores_non_media_files() {
        let dir = unique_dir("walk-ignore");
        build_fixture_library(&dir);

        let files = walk_media_files(&dir);
        assert!(!files.iter().any(|f| f.relative_path.ends_with(".txt")));
        assert!(!files.iter().any(|f| f.relative_path.ends_with(".nfo")));
        assert!(!files.iter().any(|f| f.relative_path.ends_with(".jpg")));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walk_of_a_missing_root_returns_empty_not_a_panic() {
        let missing = std::env::temp_dir().join("muse-library-scan-does-not-exist-at-all");
        let files = walk_media_files(&missing);
        assert!(files.is_empty());
    }

    #[test]
    fn walk_does_not_follow_symlinks() {
        let dir = unique_dir("walk-symlink");
        let outside = unique_dir("walk-symlink-outside");
        fs::write(outside.join("secret.mkv"), b"outside the root").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, dir.join("escaped")).ok();
        }

        let files = walk_media_files(&dir);
        assert!(
            files.is_empty() || !files.iter().any(|f| f.relative_path.contains("secret")),
            "a symlinked-in file from outside the root must never be walked"
        );

        fs::remove_dir_all(&dir).ok();
        fs::remove_dir_all(&outside).ok();
    }

    #[tokio::test]
    async fn mocked_matched_resolver_is_seen_for_every_scanned_file() {
        let dir = unique_dir("mock-matched");
        build_fixture_library(&dir);

        let files = walk_media_files(&dir);
        let resolver = MockResolver::always(ScanMatch::Matched { media_metadata_id: 1 });
        for f in &files {
            let m = resolver.resolve(f).await.unwrap();
            assert_eq!(m, ScanMatch::Matched { media_metadata_id: 1 });
        }
        assert_eq!(resolver.seen.lock().unwrap().len(), 2);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sidecar_art_is_detected_for_a_matched_file() {
        let dir = unique_dir("mock-sidecar");
        build_fixture_library(&dir);

        let files = walk_media_files(&dir);
        let movie = files
            .iter()
            .find(|f| f.relative_path.contains("Matrix"))
            .expect("movie file found");

        let art = crate::library::sidecar::detect(&movie.absolute_path);
        assert!(art.nfo_path.is_some());
        assert!(art.poster_path.is_some());

        fs::remove_dir_all(&dir).ok();
    }

    /// Behavioral read-only proof (spec: "assert the fixture dir + files
    /// are byte-for-byte unchanged"): checksum the whole fixture tree,
    /// run a full walk + sidecar detect + read_bytes pass over every file
    /// in it, then checksum again and assert nothing moved.
    #[test]
    fn fixture_scan_leaves_the_library_byte_for_byte_unchanged() {
        let dir = unique_dir("read-only-proof");
        build_fixture_library(&dir);

        let before = checksum_tree(&dir);

        let files = walk_media_files(&dir);
        assert!(!files.is_empty());
        for f in &files {
            let art = crate::library::sidecar::detect(&f.absolute_path);
            if let Some(poster) = &art.poster_path {
                let _ = crate::library::sidecar::read_bytes(poster);
            }
            if let Some(nfo) = &art.nfo_path {
                let _ = crate::library::sidecar::read_bytes(nfo);
            }
        }

        let after = checksum_tree(&dir);
        assert_eq!(before, after, "a scan pass must never modify a single byte inside the library root");

        fs::remove_dir_all(&dir).ok();
    }

    /// Structural read-only proof (spec: "the module has no write/create/
    /// remove call") — greps this module's own source (plus the sidecar
    /// module it delegates file reads to) for any `fs`/`File` write-shaped
    /// call. A cheap, durable regression guard: if a future edit adds
    /// `File::create`/`.write(true)`/`fs::remove_*`/`fs::rename` into
    /// either module, this test fails the build rather than silently
    /// regressing the read-only guarantee.
    #[test]
    fn no_write_create_remove_calls_in_the_scan_and_sidecar_source() {
        // Review finding 4 (codex): scan the WHOLE `src/library/` production
        // source directory, not just `scan.rs`+`sidecar.rs` by name -- so a
        // future file added to this module (or a helper `mod.rs` picks up)
        // that touches the library filesystem is caught by this guard too,
        // without needing to remember to extend a hardcoded path list.
        let library_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/library");
        let mut rs_files: Vec<PathBuf> = fs::read_dir(library_dir)
            .expect("read_dir the library module's own source directory")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
            .collect();
        rs_files.sort();
        assert!(
            rs_files.len() >= 3,
            "sanity check: expected to find at least mod.rs/scan.rs/sidecar.rs under {library_dir}, found {rs_files:?}"
        );

        for path in rs_files {
            let source = fs::read_to_string(&path).expect("read own source for the read-only structural proof");
            // Only scan the module body above its own `#[cfg(test)]` block --
            // this very test's banned-string list otherwise self-triggers
            // (it necessarily contains the literal strings it's checking
            // for). Strip line comments too so an unrelated doc-comment
            // mention (e.g. ".write(true)" in a module doc a few lines up)
            // doesn't false-positive either.
            let production_code = source.split("#[cfg(test)]").next().unwrap_or(&source);
            let code_only: String = production_code
                .lines()
                .map(|line| line.split("//").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");

            for banned in [
                "File::create",
                ".write(true)",
                ".create(true)",
                ".append(true)",
                "fs::remove_file",
                "fs::remove_dir",
                "fs::rename",
                "fs::write(",
                "fs::copy(",
                "fs::set_permissions",
            ] {
                assert!(
                    !code_only.contains(banned),
                    "found a write-shaped filesystem call `{banned}` in {} — the library module must stay \
                     strictly read-only on the library filesystem",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn extract_id_tag_parses_the_arr_bracket_convention() {
        assert_eq!(
            extract_id_tag("The Matrix (1999) {tmdb-603}/The.Matrix.1999.mkv"),
            Some((resolve::TMDB, "603".to_string()))
        );
        assert_eq!(
            extract_id_tag("Some Show {tvdb-81189}/Season 01/S01E01.mkv"),
            Some((resolve::TVDB, "81189".to_string()))
        );
        assert_eq!(
            extract_id_tag("Movie {imdb-tt0133093}/movie.mkv"),
            Some((resolve::IMDB, "tt0133093".to_string()))
        );
        assert_eq!(extract_id_tag("Plain.Scene.Name.2020.mkv"), None);
    }

    /// A resolver that errors for every file, without ever touching a real
    /// DB (a lazily-connected pool to a bogus URL is enough since a
    /// resolver-level error means `scan_library` never issues a query for
    /// that file at all) -- proves a per-file failure is isolated
    /// (`ScanReport::errors` increments, the pass keeps going) rather than
    /// aborting the whole scan.
    struct AlwaysErrorsResolver;

    #[async_trait]
    impl LibraryResolver for AlwaysErrorsResolver {
        async fn resolve(&self, _file: &ScannedFile) -> MuseResult<ScanMatch> {
            Err(crate::error::MuseError::upstream("simulated resolver failure"))
        }
    }

    #[tokio::test]
    async fn scan_library_isolates_a_per_file_resolver_failure_and_continues() {
        let dir = unique_dir("resolver-error");
        build_fixture_library(&dir);

        // A lazily-connected pool: never actually dials out because this
        // resolver errors before `scan_library` would issue any query.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://muse-library-test-does-not-need-to-resolve/db")
            .expect("connect_lazy should never itself fail to construct");

        let library = Library {
            id: 1,
            name: "fixture".to_string(),
            kind: crate::models::library::LibraryKind::Movie,
            root_folder: dir.to_string_lossy().to_string(),
            source_arr_name: None,
            source_arr_url: None,
            enabled: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let report = scan_library(&pool, &library, &AlwaysErrorsResolver)
            .await
            .expect("scan_library itself must not error even though every file's resolver call does");

        assert_eq!(report.scanned, 2);
        assert_eq!(report.errors, 2, "both files' resolver failures should be isolated, not fatal");
        assert_eq!(report.matched, 0);

        fs::remove_dir_all(&dir).ok();
    }

    /// MUSEL-B1 acceptance spine: the DB-gated end-to-end round trip.
    /// Gated on `MUSE_TEST_DATABASE_URL`, same skip-cleanly-when-unset
    /// posture as every other live-DB test in this crate (see
    /// `repo::media_metadata::tests` for the identical pattern) -- this is
    /// NOT required for the default `cargo test` run.
    ///
    /// Seeds a `libraries` row + a `media_metadata` row (as arr ingest
    /// would), builds a fixture tree whose title+year exactly matches the
    /// seeded metadata, scans it with the real `DbLibraryResolver`, and
    /// asserts: a `media_items` row was created, a `media_files` row
    /// records the right size, the sidecar poster got cached into
    /// `artwork_cache` under `entity_kind = "media_item"`, and a second
    /// scan pass is idempotent (no duplicate rows, no re-caching).
    #[tokio::test]
    async fn scan_library_end_to_end_matches_records_and_caches_art_idempotently() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set -- skipping \
                 scan_library_end_to_end_matches_records_and_caches_art_idempotently \
                 (expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use uuid::Uuid;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();
        let title = format!("MUSEL-B1 Scan Test Movie {suffix}");

        let dir = unique_dir(&format!("e2e-{suffix}"));
        let movie_dir = dir.join(format!("{title} (2021)"));
        fs::create_dir_all(&movie_dir).unwrap();
        let media_path = movie_dir.join(format!("{title}.2021.1080p.BluRay.x264-GRP.mkv"));
        let contents = b"fixture video bytes, not a real movie file";
        fs::write(&media_path, contents).unwrap();
        fs::write(movie_dir.join("poster.jpg"), b"fixture-poster-bytes").unwrap();

        let library = repo::library::create(
            &pool,
            &crate::models::library::NewLibrary {
                name: format!("MUSEL-B1 Test Library {suffix}"),
                kind: crate::models::library::LibraryKind::Movie,
                root_folder: dir.to_string_lossy().to_string(),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("seed library row");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &crate::models::media_metadata::NewMediaMetadata {
                kind: crate::models::media_metadata::MediaKind::Movie,
                tmdb_id: Some(format!("muselb1-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: title.clone(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: None,
                year: Some(2021),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("seed media_metadata row");

        let resolver = DbLibraryResolver { pool: &pool, providers: &[] };

        let report = scan_library(&pool, &library, &resolver).await.expect("first scan_library pass");
        assert_eq!(report.scanned, 1);
        assert_eq!(report.matched, 1, "the fixture's title+year should exactly match the seeded metadata row");
        assert_eq!(report.art_cached, 1, "the sidecar poster should have been cached");

        let items = repo::media_item::list_by_library(&pool, library.id).await.expect("list media_items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].media_metadata_id, metadata.id);

        let files = repo::media_file::list_by_media_item(&pool, items[0].id)
            .await
            .expect("list media_files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].size_bytes, Some(contents.len() as i64));

        let art = repo::artwork_cache::get(&pool, "media_item", items[0].id, "poster")
            .await
            .expect("query artwork_cache")
            .expect("poster should be cached");
        assert_eq!(art.bytes.as_deref(), Some(&b"fixture-poster-bytes"[..]));
        assert_eq!(art.content_type.as_deref(), Some("image/jpeg"));

        // Idempotent re-scan: no duplicate media_items/media_files rows, AND
        // (review finding 3, codex) no re-caching of unchanged sidecar art.
        let report2 = scan_library(&pool, &library, &resolver).await.expect("second scan_library pass");
        assert_eq!(report2.matched, 1);
        assert_eq!(
            report2.art_cached, 0,
            "an unchanged rescan (size guard says nothing changed) must not re-cache sidecar art"
        );
        let items_again = repo::media_item::list_by_library(&pool, library.id).await.expect("list media_items again");
        assert_eq!(items_again.len(), 1, "re-scanning must not create a duplicate media_items row");
        let files_again = repo::media_file::list_by_media_item(&pool, items[0].id)
            .await
            .expect("list media_files again");
        assert_eq!(files_again.len(), 1, "re-scanning an unchanged file must not create a duplicate media_files row");

        fs::remove_dir_all(&dir).ok();
    }

    /// Review finding 1 (codex) regression test: a file whose path carries
    /// an explicit `{tmdb-...}` id tag that does NOT resolve locally must
    /// stay unmatched even when its parsed title+year *would* exactly match
    /// a different, already-cataloged row -- the explicit id tag must not
    /// be silently discarded in favor of a title/year guess that could
    /// attach the file to the wrong title's row.
    #[tokio::test]
    async fn explicit_id_tag_that_fails_to_resolve_does_not_fall_back_to_a_title_year_match() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set -- skipping \
                 explicit_id_tag_that_fails_to_resolve_does_not_fall_back_to_a_title_year_match \
                 (expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use uuid::Uuid;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();
        let title = format!("MUSEL-B1 Id Tag Precedence Test {suffix}");

        let dir = unique_dir(&format!("id-tag-precedence-{suffix}"));
        // The folder name carries an explicit tmdb id tag that is NOT the
        // one seeded below, but the filename's parsed title+year DOES
        // exactly match the seeded row -- the resolver must still refuse
        // to attach it.
        let movie_dir = dir.join(format!("{title} (2021) {{tmdb-99999999-does-not-exist}}"));
        fs::create_dir_all(&movie_dir).unwrap();
        let media_path = movie_dir.join(format!("{title}.2021.1080p.BluRay.x264-GRP.mkv"));
        fs::write(&media_path, b"fixture bytes").unwrap();

        let library = repo::library::create(
            &pool,
            &crate::models::library::NewLibrary {
                name: format!("MUSEL-B1 Id Tag Precedence Library {suffix}"),
                kind: crate::models::library::LibraryKind::Movie,
                root_folder: dir.to_string_lossy().to_string(),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("seed library row");

        repo::media_metadata::upsert_by_tmdb(
            &pool,
            &crate::models::media_metadata::NewMediaMetadata {
                kind: crate::models::media_metadata::MediaKind::Movie,
                tmdb_id: Some(format!("id-tag-precedence-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: title.clone(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: None,
                year: Some(2021),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("seed a media_metadata row whose title+year exactly matches the fixture file");

        let resolver = DbLibraryResolver { pool: &pool, providers: &[] };
        let report = scan_library(&pool, &library, &resolver).await.expect("scan_library pass");

        assert_eq!(report.scanned, 1);
        assert_eq!(
            report.matched, 0,
            "an unresolvable explicit id tag must never fall back to a confident title/year attach"
        );
        assert_eq!(report.unmatched, 1);

        let items = repo::media_item::list_by_library(&pool, library.id).await.expect("list media_items");
        assert!(items.is_empty(), "no media_items row should be created for the unresolved id-tagged file");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_is_within_root_is_component_aware_not_a_string_prefix() {
        // Review finding 2 (codex): `/mnt/library2` must NOT be treated as
        // inside `/mnt/library` just because the string "library2" starts
        // with "library" -- these paths don't exist on disk in a test
        // environment, so this also exercises the non-canonicalized,
        // lexical-but-component-aware fallback path.
        assert!(!path_is_within_root("/mnt/library2", "/mnt/library"));
        assert!(!path_is_within_root("/mnt/library-other", "/mnt/library"));
        assert!(path_is_within_root("/mnt/library/Movies", "/mnt/library"));
        assert!(path_is_within_root("/mnt/library", "/mnt/library"));
        // Trailing-slash normalization on either side.
        assert!(path_is_within_root("/mnt/library/", "/mnt/library"));
        assert!(path_is_within_root("/mnt/library/Movies", "/mnt/library/"));
    }

    #[test]
    fn path_is_within_root_works_for_real_directories_too() {
        // When both sides actually exist, the canonicalizing branch runs
        // instead of the lexical fallback -- prove it agrees.
        let root = unique_dir("containment-root");
        let child = root.join("Movies");
        fs::create_dir_all(&child).unwrap();
        let sibling = unique_dir("containment-sibling");

        assert!(path_is_within_root(&child.to_string_lossy(), &root.to_string_lossy()));
        assert!(!path_is_within_root(&sibling.to_string_lossy(), &root.to_string_lossy()));

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&sibling).ok();
    }
}
