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
//!
//! As of MPRB-06 this module also spawns `ffprobe` against files it records.
//! That does not weaken the boundary: the path is resolved through
//! [`crate::media::MediaCore`]'s **read-only** library guard (built with
//! `enable_mutation: false`, permanently), and `ffprobe` is invoked with a
//! read-only argv (`-show_streams -show_format`, no output file).
//!
//! ## MPRB-06 — what the recorded `media_info` is derived from
//! Until MPRB-06 this file wrote `media_files.media_info` as
//! `{"container": "<filename extension>"}`. That is a **claim about a file's
//! contents derived from its name**: a `.mkv` full of HEVC and a `.mkv` full of
//! MPEG-2 were indistinguishable in the database, and an `.avi` renamed to
//! `.mkv` was simply believed. Nothing here derives `media_info` from the path
//! any more — the document comes from `ffprobe`, through
//! [`crate::media::probe::run_ffprobe_async`] (MPRB-02) and
//! [`crate::media::doc::MediaInfoDoc`] (MPRB-05). The filename's extension is
//! still recorded, but *inside* that document, as the hint MPRB-05 documents it
//! to be, never as the answer.
//!
//! **The scanner probes on ARRIVAL, not on sight.** See [`probe_decision`] for
//! the rule and the reasoning; the 16,221-file catch-up belongs to the backfill
//! worker (MPRB-07) and its attempt-bounded queue, not to a rescan.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::media::doc::StoredMediaInfo;
// MPRB-07 moved these four out of this module and into `crate::media::sink`
// unchanged, when the backfill worker became the second consumer of the same
// persistence edge. See that module for why there is one definition, not two.
use crate::media::sink::{probe_write, DbProbeSink, ProbeSink, ProbeWrite};
use crate::media::MediaCore;
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

/// Whether a path has an extension the scanner treats as media.
///
/// `pub` so other modules resolving a release folder to its feature file use
/// the SAME list the scanner does — two copies of this list would drift, and a
/// subtitle lookup that disagreed with the scanner about what counts as media
/// is exactly the kind of divergence nobody notices until a container is
/// missing from one of them.
pub fn has_media_extension(path: &Path) -> bool {
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
    /// Sidecar art/`.nfo` detected beside this file (`sidecar::detect`),
    /// computed once during the walk so both matching (the `.nfo`-embedded
    /// id, see `nfo_provider_id` below) and attachment
    /// (`record_matched_file`) reuse the same detection pass instead of
    /// re-listing the directory.
    pub sidecar_art: crate::library::sidecar::SidecarArt,
    /// The provider id embedded in the `.nfo` sidecar (READ-ONLY read +
    /// parse via `sidecar::extract_provider_id_from_nfo`), if one was found
    /// — `*arr`/Kodi writes its own identification into the `.nfo`, making
    /// this the highest-value sidecar signal for matching (review finding,
    /// S119b codex: detecting the `.nfo` without ever reading its embedded
    /// id left the most valuable sidecar unused). `None` when there's no
    /// `.nfo`, it has no recognized id tag, or it couldn't be read.
    pub nfo_provider_id: Option<(&'static str, String)>,
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

        // Detect + (READ-ONLY) read the `.nfo` here, during the walk, so
        // its embedded provider id (when present) is available to the
        // resolver as a matching signal, not just discovered after the
        // fact for caching.
        let sidecar_art = crate::library::sidecar::detect(&path);
        let nfo_provider_id = sidecar_art
            .nfo_path
            .as_ref()
            .and_then(|nfo_path| crate::library::sidecar::read_bytes(nfo_path).ok())
            .and_then(|bytes| crate::library::sidecar::extract_provider_id_from_nfo(&bytes));

        out.push(ScannedFile {
            absolute_path: path,
            relative_path,
            size_bytes,
            parsed,
            kind_guess,
            sidecar_art,
            nfo_provider_id,
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
        // Explicit id candidates, in priority order: the path's
        // `{tmdb-...}`/`{tvdb-...}`/`{imdb-...}` tag first (the
        // Radarr/Sonarr folder-naming convention), then the `.nfo`'s own
        // embedded id (the `*arr` suite's own identification -- the
        // strongest signal available, per the S119b codex review). Either
        // one is a caller/tooling-asserted, SPECIFIC identity claim -- see
        // the loop below for why a failure here must never fall through to
        // a title/year guess.
        let mut explicit_ids: Vec<(&'static str, String)> = Vec::new();
        if let Some(tag) = extract_id_tag(&file.relative_path) {
            explicit_ids.push(tag);
        }
        if let Some(nfo_id) = &file.nfo_provider_id {
            explicit_ids.push(nfo_id.clone());
        }

        if !explicit_ids.is_empty() {
            for (provider, id) in &explicit_ids {
                let local = match *provider {
                    resolve::TMDB => repo::media_metadata::find_by_tmdb_id(self.pool, file.kind_guess, id).await?,
                    resolve::TVDB => repo::media_metadata::find_by_tvdb_id(self.pool, file.kind_guess, id).await?,
                    _ => None,
                };
                if let Some(media_metadata_id) = local {
                    return Ok(ScanMatch::Matched { media_metadata_id });
                }
            }

            // Review finding 1 (codex): an explicit id (path tag OR .nfo)
            // is a caller-asserted, specific identity claim. If NONE of
            // the explicit candidates resolve to a local catalog row, that
            // must NOT fall through to the title/year match below --
            // doing so risks confidently attaching this file to a
            // DIFFERENT title's row (the one that happens to share a
            // title+year) even though an explicit id said otherwise. This
            // mirrors MUSEL-A2's `resolve_and_merge` rule: known-but-
            // unresolvable ids never fall back to a title guess. The
            // title/year and resolve_and_merge paths below are reachable
            // ONLY when the file carried no explicit id at all.
            tracing::info!(
                path = %file.relative_path,
                candidates = ?explicit_ids,
                "library scan: explicit id(s) (path tag and/or .nfo) did not resolve to a local catalog row; \
                 recording as unmatched rather than falling back to a title/year guess"
            );
            return Ok(ScanMatch::Unmatched);
        }

        // Review finding (codex, correctness): the confident title+year
        // match requires a REAL year, not just a title. `find_by_title_year`
        // (a shared helper other callers rely on for its own, different
        // posture) runs a TITLE-ONLY query when `year` is `None` and still
        // returns a confident id -- which here would let a file whose
        // filename simply didn't carry a parseable year attach confidently
        // to ANY same-title catalog row (a remake vs the original, a
        // different edition, ...). Deliberately NOT changing
        // `find_by_title_year`'s own behavior (other callers may want the
        // title-only fallback); this scanner call site just refuses to use
        // it. A title with no year skips straight past this branch to the
        // resolve_and_merge tentative path below (never persisted as
        // confident) / unmatched.
        if let (Some(title), Some(year)) = (&file.parsed.title, file.parsed.year) {
            if let Some(media_metadata_id) =
                repo::media_metadata::find_by_title_year(self.pool, file.kind_guess, title, Some(year)).await?
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
    /// How many `.nfo` sidecars were (re-)read and cached/attached onto a
    /// matched item this pass — the counter that makes `.nfo`
    /// detection→attachment observable in the report, not silently
    /// dropped (S119b codex review). Skips the same idempotency guard
    /// `art_cached` does: an unchanged rescan doesn't re-attach.
    pub nfo_attached: usize,
    /// MPRB-06: documents persisted this pass — `ok` **plus** `suspicious`.
    /// A suspicious result parsed, so it counts as probed for completion; it
    /// is counted again below for attention. Conflating those two questions is
    /// what makes a sweep look finished when it is not.
    pub probed: usize,
    /// Subset of [`Self::probed`]: parsed, stored, and flagged by
    /// [`crate::media::derive::suspicion`] as describing something implausible.
    pub probe_suspicious: usize,
    /// A probe that produced no document. The failure is recorded on the row
    /// (state + attempt counter) and the scan continues.
    pub probe_failed: usize,
    /// Files not probed this pass: the host cannot probe, the file is unchanged
    /// and already carries a document, the file is unchanged and left to the
    /// backfill, or its path did not resolve inside the library root.
    pub probe_skipped: usize,
    /// The probe produced a verdict and the **database** refused to record it.
    /// Deliberately not folded into [`Self::probe_failed`]: an operator's
    /// response to "this file will not parse" and "Postgres rejected a write"
    /// are entirely different, and reporting one as the other sends them at the
    /// wrong thing.
    pub probe_persist_failed: usize,
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
pub async fn scan_library(
    pool: &PgPool,
    library: &Library,
    resolver: &dyn LibraryResolver,
    media: &MediaCore,
) -> MuseResult<ScanReport> {
    let root = PathBuf::from(&library.root_folder);
    let files = walk_media_files(&root);

    // Once per pass, not once per file. MPRB-01 takes the capability snapshot at
    // construction precisely so a 16,000-item loop does not re-answer it, and a
    // per-file warning about a host-level fact is 16,000 lines of the same
    // sentence.
    if !media.can_probe() {
        tracing::warn!(
            library = %library.name,
            "library scan: ffprobe is not usable on this host — files will be recorded \
             without a media_info document (the scan itself is unaffected)"
        );
    }

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
                match record_matched_file(pool, library.id, media_metadata_id, &file, media).await {
                    Ok(outcome) => {
                        report.matched += 1;
                        report.art_cached += outcome.art_cached;
                        report.nfo_attached += outcome.nfo_attached;
                        report.probed += outcome.probe.probed;
                        report.probe_suspicious += outcome.probe.suspicious;
                        report.probe_failed += outcome.probe.failed;
                        report.probe_skipped += outcome.probe.skipped;
                        report.probe_persist_failed += outcome.probe.persist_failed;
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

/// What [`record_matched_file`] actually did this pass — separate counters
/// so [`ScanReport`] can report `.nfo` attachment independently of
/// poster/fanart caching (S119b codex review: `.nfo` detection with no
/// attachment/counter left it silently dropped).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RecordOutcome {
    /// 0-2: poster/backdrop.
    art_cached: usize,
    /// 0-1: whether the `.nfo` sidecar (if any) was (re-)attached this pass.
    nfo_attached: usize,
    /// MPRB-06: what the probe step did for this one file.
    probe: ProbeTally,
}

/// Upsert the `media_items` + `media_files` rows for a confidently-matched
/// file, then cache/attach any sidecar art + `.nfo` beside it
/// (`file.sidecar_art`, already detected during the walk — see
/// [`ScannedFile`]).
async fn record_matched_file(
    pool: &PgPool,
    library_id: i64,
    media_metadata_id: i64,
    file: &ScannedFile,
    media: &MediaCore,
) -> MuseResult<RecordOutcome> {
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

    // MPRB-06: `media_info` is NOT written here, and `upsert_scanned` no longer
    // accepts it. Until this item the two lines that stood here derived the
    // stored document from `absolute_path.extension()` — the founding defect of
    // this epic. `media_files.media_info` now has exactly one writer,
    // `repo::media_file::set_probe_result`, and exactly one source, `ffprobe`.
    let (media_file, file_changed) =
        repo::media_file::upsert_scanned(pool, media_item.id, &file.relative_path, file.size_bytes).await?;

    let mut outcome = RecordOutcome {
        probe: probe_pass(
            media,
            &DbProbeSink(pool),
            media_file.id,
            &file.relative_path,
            &file.absolute_path,
            &media_file.stored_media_info(),
            file_changed,
        )
        .await,
        ..Default::default()
    };

    // Review finding (codex, this round): sidecar attachment must NOT be
    // gated on the MEDIA FILE's own change status. A prior revision
    // skipped ALL sidecar work whenever `upsert_scanned` reported the
    // media file unchanged -- but a poster/fanart/`.nfo` can be added or
    // edited beside an already-scanned, byte-identical media file (a
    // curator drops in a better poster after the fact, `*arr` rewrites the
    // `.nfo`, etc.), and that rescan would then never pick it up. Sidecar
    // detection + attachment now ALWAYS runs for a matched file; the
    // idempotency guarantee moves down into `cache_if_changed` below,
    // which compares against what's already cached and skips only a
    // byte-identical re-write -- so an unchanged file with unchanged
    // sidecars still does zero writes, but a changed/new sidecar is
    // attached regardless of whether the media file itself moved.
    //
    // MPRB-06: the same reasoning is why the probe step above is gated on the
    // media file's change status and this one is not — a probe describes the
    // media file's own bytes, a sidecar does not.
    if let Some(poster_path) = &file.sidecar_art.poster_path {
        if cache_art(pool, media_item.id, "poster", poster_path).await {
            outcome.art_cached += 1;
        }
    }
    if let Some(fanart_path) = &file.sidecar_art.fanart_path {
        if cache_art(pool, media_item.id, "backdrop", fanart_path).await {
            outcome.art_cached += 1;
        }
    }
    // Review finding (S119b codex): the `.nfo` was detected but never
    // attached/reported -- cache it the same way poster/fanart are, under
    // its own `artwork_cache` variant, so detection -> attachment is
    // complete and observable (`ScanReport::nfo_attached`), not silently
    // dropped. Its embedded id was already consumed for MATCHING up in
    // `DbLibraryResolver::resolve`; this is the separate "attach the file
    // itself" step the review asked for.
    if let Some(nfo_path) = &file.sidecar_art.nfo_path {
        if cache_nfo(pool, media_item.id, nfo_path).await {
            outcome.nfo_attached += 1;
        }
    }

    Ok(outcome)
}

// --- MPRB-06: probing on arrival -------------------------------------------

/// What the probe step did for one file. Every field is a distinct outcome; none
/// is derived from another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ProbeTally {
    /// Documents persisted: `ok` + `suspicious`.
    probed: usize,
    /// Subset of `probed`.
    suspicious: usize,
    /// A probe with no usable answer; a failure row was written.
    failed: usize,
    /// Not probed this pass, for any of the reasons in [`ProbeDecision`], plus
    /// a path that did not resolve inside the library root.
    skipped: usize,
    /// A verdict the database refused to record.
    persist_failed: usize,
}

/// Whether this pass should probe this file.
///
/// **Pure, and deliberately so** — no filesystem, no database, no `MediaCore`.
/// The scan integration's real question is a policy question, and a policy that
/// can only be exercised through a Postgres connection is a policy nobody
/// verifies. Everything this function needs is passed in.
///
/// ## Why an unchanged file is not re-probed
/// The library is 16,221 titles on an NFS mount. A rescan is a routine
/// operation — it runs on a schedule and after every import — and probing every
/// file it walks would turn each one into 16,221 subprocess spawns and 16,221
/// full-file-header reads across the network, for a set of answers that cannot
/// have changed: `upsert_scanned` reports `file_changed == false` only when the
/// row's `size_bytes` still matches what is on disk, and a file whose bytes are
/// the same has the same codecs.
///
/// So this is an **arrival trigger**: probe what is new, and probe what changed.
///
/// ## Why an unchanged, never-probed file is left alone rather than probed here
/// It is not ignored — it is somebody else's job, and that job already exists.
/// [`crate::repo::media_file::list_needing_probe`] (MPRB-05) is an
/// attempt-bounded, keyset-paginated queue over exactly these rows, and the
/// backfill worker (MPRB-07) drains it at a rate an operator controls. Probing
/// them from the scanner instead would put an unbounded, unresumable,
/// unthrottled 16,221-file sweep inside a pass whose failure mode is a wedged
/// NFS mount — and it would do it *twice*, since the backfill queue would still
/// contain the rows the scan had not reached yet. One rule, one home: the
/// catch-up sweep has a home, and it is not here.
///
/// ## Ordering
/// Capability is checked first so a host without `ffprobe` produces one clear
/// answer for every file rather than four different ones, and the answer is
/// "this host cannot", not "this file did not need it".
fn probe_decision(can_probe: bool, file_changed: bool, stored: &StoredMediaInfo) -> ProbeDecision {
    if !can_probe {
        return ProbeDecision::NoCapability;
    }
    if file_changed {
        // New row, or the bytes moved. Any document already stored describes
        // contents that no longer exist.
        return ProbeDecision::Probe;
    }
    // `needs_probe()` is MPRB-05's rule, called rather than restated: absent and
    // legacy rows need one, and a document written by a NEWER binary must not be
    // re-probed because doing so would DOWNGRADE the row.
    if stored.needs_probe() {
        return ProbeDecision::DeferredToBackfill;
    }
    ProbeDecision::UpToDate
}

/// The outcome of [`probe_decision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeDecision {
    /// New or changed: probe it now.
    Probe,
    /// This host has no usable `ffprobe`. Degrade — the scan still records the
    /// file, it simply records no document (Module Contract §2).
    NoCapability,
    /// Unchanged, and already carrying a document. Nothing to do, no writes.
    UpToDate,
    /// Unchanged, and carrying no document this binary understands. The
    /// backfill's queue owns it.
    DeferredToBackfill,
}

impl ProbeDecision {
    /// The `reason` field of the skip log, so an operator reading the log sees
    /// which of the three skips happened.
    fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::NoCapability => "no_ffprobe_on_this_host",
            Self::UpToDate => "unchanged_and_already_probed",
            Self::DeferredToBackfill => "unchanged_left_to_the_backfill",
        }
    }
}

/// Probe one file and persist the result, or explain why neither happened.
///
/// Never returns an error: a probe failure, a path that will not resolve and a
/// refused write are all recorded in the [`ProbeTally`] and logged. **A file
/// that cannot be probed must not fail the scan** — the scan's job is to record
/// what is on disk, and it did.
async fn probe_pass(
    media: &MediaCore,
    sink: &(dyn ProbeSink + Send + Sync),
    media_file_id: i64,
    relative_path: &str,
    absolute_path: &Path,
    stored: &StoredMediaInfo,
    file_changed: bool,
) -> ProbeTally {
    let mut tally = ProbeTally::default();

    let decision = probe_decision(media.can_probe(), file_changed, stored);
    if decision != ProbeDecision::Probe {
        tally.skipped += 1;
        tracing::debug!(
            path = %relative_path,
            reason = decision.as_str(),
            "library scan: not probing this file"
        );
        return tally;
    }

    // Resolved through the media core's READ-ONLY library guard, never by
    // handing `run_ffprobe_async` a raw path: `ResolvedPath` is the type that
    // says this file is inside `MUSE_LIBRARY_ROOT`, and the scanner is not
    // exempt from proving it.
    let resolved = match media.library_guard().resolve(absolute_path) {
        Ok(resolved) => resolved,
        Err(e) => {
            // NOT persisted as a probe failure. Nothing was observed about the
            // file, and burning one of its bounded `probe_attempts` on a
            // configuration fault would eventually exhaust the backfill's
            // retries for a file that is perfectly readable.
            tally.skipped += 1;
            tracing::warn!(
                path = %relative_path,
                error = %e,
                "library scan: a walked file did not resolve inside MUSE_LIBRARY_ROOT; not probing it"
            );
            return tally;
        }
    };

    let result = media.probe_async(&resolved).await;
    let write = probe_write(&result);

    match &write {
        ProbeWrite::Document { suspicion, .. } => {
            if let Some(reason) = *suspicion {
                tracing::info!(
                    path = %relative_path,
                    suspicion = reason,
                    "library scan: probed, and the result describes something implausible"
                );
            }
        }
        ProbeWrite::Failure { error } => {
            tracing::warn!(
                path = %relative_path,
                error = %error,
                retryable = error.is_retryable(),
                "library scan: probe produced no usable answer; recording the failure and continuing"
            );
        }
    }

    // Counted from what was WRITTEN, not from what was observed. `probed` is
    // reported as documents persisted; incrementing it before the write and
    // leaving it standing when the write is refused would make the counter a
    // claim about the database that the database never agreed to.
    match sink.record(media_file_id, relative_path, &write).await {
        Ok(()) => match &write {
            ProbeWrite::Document { suspicion, .. } => {
                tally.probed += 1;
                if suspicion.is_some() {
                    tally.suspicious += 1;
                }
            }
            ProbeWrite::Failure { .. } => tally.failed += 1,
        },
        Err(e) => {
            tally.persist_failed += 1;
            tracing::warn!(
                path = %relative_path,
                error = %e,
                "library scan: could not record the probe outcome; the file is still recorded"
            );
        }
    }

    tally
}

/// Read (READ-ONLY) + cache the `.nfo` sidecar into `artwork_cache` under
/// variant `"nfo"` — same table/`entity_kind` convention [`cache_art`] uses
/// for poster/fanart, so a matched item's `.nfo` is attached and later
/// retrievable the same way. Best-effort, same posture as [`cache_art`]: a
/// read or DB failure is logged and treated as "not attached this pass,"
/// never a scan-aborting error.
async fn cache_nfo(pool: &PgPool, media_item_id: i64, nfo_path: &Path) -> bool {
    let bytes = match crate::library::sidecar::read_bytes(nfo_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(path = %nfo_path.display(), error = %e, "library scan: could not read .nfo sidecar; skipping attachment");
            return false;
        }
    };

    cache_if_changed(pool, media_item_id, "nfo", "application/xml", &bytes).await
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

    cache_if_changed(pool, media_item_id, variant, content_type, &bytes).await
}

/// The per-sidecar idempotency guard (codex review, this round): looks up
/// whatever is already cached for `(media_item_id, variant)` and, if its
/// bytes are byte-for-byte identical to `bytes`, skips the write entirely
/// (returns `false` — "not (re-)attached this pass," the sidecar was
/// already current). Otherwise (no existing row, a lookup failure, or
/// genuinely different bytes) writes via `artwork_cache::store_bytes` and
/// returns whether that write succeeded. This is what makes sidecar
/// attachment idempotent WITHOUT tying it to the media file's own
/// size-guard: a newly-added or edited sidecar is written even when the
/// media file next to it hasn't changed at all, while re-scanning an
/// unchanged sidecar is still a true no-op (no DB write).
async fn cache_if_changed(pool: &PgPool, media_item_id: i64, variant: &str, content_type: &str, bytes: &[u8]) -> bool {
    match repo::artwork_cache::get(pool, "media_item", media_item_id, variant).await {
        Ok(Some(existing)) if existing.bytes.as_deref() == Some(bytes) => {
            tracing::debug!(
                media_item_id,
                variant,
                "library scan: sidecar unchanged since the last cache; skipping the write"
            );
            return false;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                media_item_id,
                variant,
                error = %e,
                "library scan: could not check the existing cached sidecar; attempting to (re-)write it anyway"
            );
        }
    }

    match repo::artwork_cache::store_bytes(pool, "media_item", media_item_id, variant, None, content_type, bytes, None)
        .await
    {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(media_item_id, variant, error = %e, "library scan: could not cache/attach sidecar; skipping");
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

    // Built ONCE for the whole run, not once per library: construction takes the
    // host capability snapshot, which costs three bounded subprocess spawns
    // (CAPDET-01), and the answer is a property of the host, not of a library.
    let media = MediaCore::from_config(config);
    if media.library_guard_is_inert() {
        tracing::warn!(
            "library scan: the media core's library guard is inert — no walked file will \
             resolve, so nothing will be probed this run"
        );
    }

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
        match scan_library(pool, &library, &resolver, &media).await {
            Ok(report) => {
                tracing::info!(
                    library = %library.name,
                    scanned = report.scanned,
                    matched = report.matched,
                    tentative = report.tentative,
                    unmatched = report.unmatched,
                    errors = report.errors,
                    art_cached = report.art_cached,
                    nfo_attached = report.nfo_attached,
                    probed = report.probed,
                    probe_suspicious = report.probe_suspicious,
                    probe_failed = report.probe_failed,
                    probe_skipped = report.probe_skipped,
                    probe_persist_failed = report.probe_persist_failed,
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

    /// A binary name that cannot exist on any host — same device
    /// `crate::media`'s own tests use.
    const ABSENT_BIN: &str = "muse-scan-no-such-ffprobe-xyzzy";

    /// A [`MediaCore`] that cannot probe, for the DB-gated tests below.
    ///
    /// Those tests predate MPRB-06 and assert the walk/match/sidecar behaviour;
    /// they are given a core that degrades so their subject stays what it was.
    /// The probe path itself is exercised **without** a database, further down,
    /// against a real `ffprobe`-shaped subprocess — see
    /// [`probe_pass_persists_what_the_file_says_not_what_it_is_called`].
    fn no_probe_core() -> MediaCore {
        MediaCore::from_config(&crate::config::Config {
            probe_ffprobe_bin: Some(ABSENT_BIN.to_string()),
            ffmpeg_path: ABSENT_BIN.to_string(),
            foundry_handbrake_bin: Some(ABSENT_BIN.to_string()),
            ..Default::default()
        })
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
        // WEAKTEST-01 (Plane MUSE #131 sweep): `!files.is_empty()` did not
        // establish that the read-only pass below actually covers EVERY
        // fixture file — truncating the walker to a single result left this
        // test green (four sibling tests caught it, this one did not). The
        // fixture contains exactly two media files (plus .nfo/.jpg/.txt that
        // must NOT be walked), so the count is asserted exactly.
        assert_eq!(
            files.len(),
            2,
            "the walk must return both fixture media files (and no sidecars): {:?}",
            files.iter().map(|f| f.absolute_path.clone()).collect::<Vec<_>>()
        );
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

            // Review finding (codex, "airtight" round): the banned-pattern
            // set was too short-listed. Broadened to comprehensively cover
            // every std/library write-shaped filesystem entry point, not
            // just the handful this module happened to need to avoid so
            // far. `fs::` (unqualified) patterns already match the
            // `std::fs::`-qualified spelling too since this is a plain
            // substring search (`"fs::write("` is a substring of
            // `"std::fs::write("`), but a few fully-qualified forms are
            // listed explicitly anyway for defense-in-depth/readability.
            // `"fs::symlink("` (not bare `"symlink"`) deliberately keeps
            // its trailing `(` so it doesn't false-positive against this
            // module's own legitimate, read-only `symlink_metadata` calls.
            for banned in [
                "File::create",
                ".write(true)",
                ".create(true)",
                ".create_new(true)",
                ".append(true)",
                ".truncate(true)",
                "fs::write(",
                "std::fs::write",
                "fs::remove_file",
                "fs::remove_dir",
                "fs::remove_dir_all",
                "fs::rename",
                "fs::copy(",
                "fs::hard_link",
                "fs::soft_link",
                "fs::symlink(",
                "fs::set_permissions",
                "fs::create_dir",
                "fs::create_dir_all",
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

        let report = scan_library(&pool, &library, &AlwaysErrorsResolver, &no_probe_core())
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

        let report = scan_library(&pool, &library, &resolver, &no_probe_core()).await.expect("first scan_library pass");
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
        // MPRB-06: a freshly scanned row carries NO document. Before this item it
        // carried `{"container": "mkv"}` — a `Legacy` document derived from the
        // filename. The scan core here cannot probe, so the honest state is
        // "not probed yet", which is what the backfill queue looks for.
        assert_eq!(
            files[0].stored_media_info(),
            StoredMediaInfo::Absent,
            "the scanner must never mint a document from the filename's extension"
        );
        assert_eq!(files[0].media_info_version, None);
        assert_eq!(report.probed, 0);
        assert_eq!(
            report.probe_skipped, 1,
            "a host that cannot probe still scans; it records no document"
        );

        let art = repo::artwork_cache::get(&pool, "media_item", items[0].id, "poster")
            .await
            .expect("query artwork_cache")
            .expect("poster should be cached");
        assert_eq!(art.bytes.as_deref(), Some(&b"fixture-poster-bytes"[..]));
        assert_eq!(art.content_type.as_deref(), Some("image/jpeg"));

        // Idempotent re-scan: no duplicate media_items/media_files rows, AND
        // (review finding 3, codex) no re-caching of unchanged sidecar art.
        let report2 = scan_library(&pool, &library, &resolver, &no_probe_core()).await.expect("second scan_library pass");
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

        // Review finding (codex, "sidecar must not depend on the media file
        // changing" round): a NEW poster + `.nfo` dropped in beside an
        // already-scanned, byte-identical media file must still be
        // attached on the next rescan -- sidecar attachment must not hinge
        // on `media_files`' own size-guard. Neither sidecar existed on the
        // first two scans above, so a fresh scan finding them now proves
        // detection isn't gated on the media file having changed.
        fs::write(movie_dir.join("fanart.jpg"), b"newly-added-fanart-bytes").unwrap();
        fs::write(movie_dir.join("movie.nfo"), b"<movie><title>added later</title></movie>").unwrap();

        let report3 = scan_library(&pool, &library, &resolver, &no_probe_core()).await.expect("third scan_library pass");
        assert_eq!(report3.matched, 1);
        assert_eq!(
            report3.art_cached, 1,
            "a newly-added fanart beside an unchanged media file must still be attached on rescan"
        );
        assert_eq!(
            report3.nfo_attached, 1,
            "a newly-added .nfo beside an unchanged media file must still be attached on rescan"
        );

        let fanart = repo::artwork_cache::get(&pool, "media_item", items[0].id, "backdrop")
            .await
            .expect("query artwork_cache for the newly-added fanart")
            .expect("fanart should now be cached");
        assert_eq!(fanart.bytes.as_deref(), Some(&b"newly-added-fanart-bytes"[..]));

        let nfo = repo::artwork_cache::get(&pool, "media_item", items[0].id, "nfo")
            .await
            .expect("query artwork_cache for the newly-added nfo")
            .expect("nfo should now be cached");
        assert_eq!(nfo.bytes.as_deref(), Some(&b"<movie><title>added later</title></movie>"[..]));

        // And re-scanning again with nothing further changed is back to a
        // full no-op, including for the sidecars just attached above.
        let report4 = scan_library(&pool, &library, &resolver, &no_probe_core()).await.expect("fourth scan_library pass");
        assert_eq!(report4.matched, 1);
        assert_eq!(report4.art_cached, 0, "unchanged sidecars (poster+fanart) must not be re-cached again");
        assert_eq!(report4.nfo_attached, 0, "an unchanged .nfo must not be re-attached again");

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
        let report = scan_library(&pool, &library, &resolver, &no_probe_core()).await.expect("scan_library pass");

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

    /// S119b codex review, test (a): a `.nfo` with an embedded
    /// `<uniqueid type="tmdb">` id, beside a file with NO path id tag and a
    /// filename that does NOT title/year-match the seeded row, still
    /// matches via the `.nfo`'s id — proving the `.nfo`'s embedded id is
    /// actually consumed for matching, not just detected/cached.
    #[tokio::test]
    async fn nfo_embedded_tmdb_id_matches_a_file_with_no_path_id_tag() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set -- skipping \
                 nfo_embedded_tmdb_id_matches_a_file_with_no_path_id_tag \
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
        // Deliberately does NOT match the seeded row's title -- the only
        // way this can match is via the .nfo's embedded id.
        let filename_title = format!("Totally Different Filename Title {suffix}");
        let catalog_title = format!("MUSEL-B1 Nfo Id Match Test {suffix}");
        let tmdb_id = format!("nfo-id-match-tmdb-{suffix}");

        let dir = unique_dir(&format!("nfo-id-match-{suffix}"));
        // No {tmdb-...}/{tvdb-...}/{imdb-...} tag anywhere in this path.
        let movie_dir = dir.join(format!("{filename_title} (2019)"));
        fs::create_dir_all(&movie_dir).unwrap();
        let media_path = movie_dir.join(format!("{filename_title}.2019.1080p.WEB-DL.x264-GRP.mkv"));
        fs::write(&media_path, b"fixture bytes").unwrap();
        fs::write(
            movie_dir.join("movie.nfo"),
            format!(r#"<movie><uniqueid type="tmdb">{tmdb_id}</uniqueid></movie>"#),
        )
        .unwrap();

        let library = repo::library::create(
            &pool,
            &crate::models::library::NewLibrary {
                name: format!("MUSEL-B1 Nfo Id Match Library {suffix}"),
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
                tmdb_id: Some(tmdb_id),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: catalog_title,
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: None,
                year: Some(2019),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("seed the media_metadata row the .nfo's tmdb id points at");

        let resolver = DbLibraryResolver { pool: &pool, providers: &[] };
        let report = scan_library(&pool, &library, &resolver, &no_probe_core()).await.expect("scan_library pass");

        assert_eq!(report.scanned, 1);
        assert_eq!(
            report.matched, 1,
            "the .nfo's embedded tmdb id must match even though the filename's own title doesn't"
        );
        // Test (b): the .nfo present + attached is reflected in the report.
        assert_eq!(report.nfo_attached, 1, ".nfo attachment must be observable in the ScanReport counter");

        let items = repo::media_item::list_by_library(&pool, library.id).await.expect("list media_items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].media_metadata_id, metadata.id);

        let nfo_art = repo::artwork_cache::get(&pool, "media_item", items[0].id, "nfo")
            .await
            .expect("query artwork_cache for the nfo variant")
            .expect(".nfo should have been cached/attached under the 'nfo' variant");
        assert!(nfo_art.bytes.is_some(), "the cached .nfo row should carry the actual .nfo bytes");
        assert_eq!(nfo_art.content_type.as_deref(), Some("application/xml"));

        fs::remove_dir_all(&dir).ok();
    }

    /// S119b codex review, test (c): a `.nfo` embeds an id that does NOT
    /// resolve locally, and the file's own filename WOULD otherwise exactly
    /// title/year-match a different, already-cataloged row — the resolver
    /// must still refuse the confident title/year attach (same "an
    /// explicit id that fails must not fall through" rule as the path-tag
    /// case, now via the .nfo).
    #[tokio::test]
    async fn nfo_embedded_id_that_fails_to_resolve_does_not_fall_back_to_a_title_year_match() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set -- skipping \
                 nfo_embedded_id_that_fails_to_resolve_does_not_fall_back_to_a_title_year_match \
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
        let title = format!("MUSEL-B1 Nfo Id Failure Precedence Test {suffix}");

        let dir = unique_dir(&format!("nfo-id-failure-precedence-{suffix}"));
        // No id tag in the path itself -- but the .nfo carries one that
        // won't resolve. The filename's title+year DOES exactly match the
        // seeded row below; the resolver must still not attach it.
        let movie_dir = dir.join(format!("{title} (2022)"));
        fs::create_dir_all(&movie_dir).unwrap();
        let media_path = movie_dir.join(format!("{title}.2022.1080p.BluRay.x264-GRP.mkv"));
        fs::write(&media_path, b"fixture bytes").unwrap();
        fs::write(
            movie_dir.join("movie.nfo"),
            br#"<movie><uniqueid type="tmdb">99999999-does-not-exist</uniqueid></movie>"#,
        )
        .unwrap();

        let library = repo::library::create(
            &pool,
            &crate::models::library::NewLibrary {
                name: format!("MUSEL-B1 Nfo Id Failure Precedence Library {suffix}"),
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
                tmdb_id: Some(format!("nfo-id-failure-precedence-tmdb-{suffix}")),
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
                year: Some(2022),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("seed a media_metadata row whose title+year exactly matches the fixture file");

        let resolver = DbLibraryResolver { pool: &pool, providers: &[] };
        let report = scan_library(&pool, &library, &resolver, &no_probe_core()).await.expect("scan_library pass");

        assert_eq!(report.scanned, 1);
        assert_eq!(
            report.matched, 0,
            "an unresolvable .nfo-embedded id must never fall back to a confident title/year attach"
        );
        assert_eq!(report.unmatched, 1);
        assert_eq!(report.nfo_attached, 0, "an unmatched file's .nfo is never attached (no media_item to attach it to)");

        let items = repo::media_item::list_by_library(&pool, library.id).await.expect("list media_items");
        assert!(items.is_empty(), "no media_items row should be created for the unresolved nfo-id file");

        fs::remove_dir_all(&dir).ok();
    }

    /// Review finding (codex, correctness): a file whose filename parses a
    /// title but NO year (`ParsedRelease.year == None`) must NOT confidently
    /// attach via a title-ONLY match, even when a catalog row with that
    /// exact title exists (any year) — `repo::media_metadata::
    /// find_by_title_year` runs a title-only query and returns a confident
    /// id when `year` is `None`, which this scanner call site must refuse
    /// to use (a remake vs. the original, a different edition, etc. would
    /// otherwise attach wrong-confidently). No id tag, no providers
    /// configured -> the file should end up unmatched, never `Matched`,
    /// and no `media_items` row created.
    #[tokio::test]
    async fn title_with_no_year_does_not_confidently_attach_via_title_only_match() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set -- skipping \
                 title_with_no_year_does_not_confidently_attach_via_title_only_match \
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
        let title = format!("MUSEL-B1 No Year Title Only Test {suffix}");

        let dir = unique_dir(&format!("no-year-title-only-{suffix}"));
        let movie_dir = dir.join(&title);
        fs::create_dir_all(&movie_dir).unwrap();
        // Deliberately no 4-digit year token anywhere in this filename --
        // `parse_release_name` will parse a title but leave `year: None`.
        let media_path = movie_dir.join(format!("{title}.1080p.BluRay.x264-GRP.mkv"));
        fs::write(&media_path, b"fixture bytes").unwrap();

        let library = repo::library::create(
            &pool,
            &crate::models::library::NewLibrary {
                name: format!("MUSEL-B1 No Year Title Only Library {suffix}"),
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
                tmdb_id: Some(format!("no-year-title-only-tmdb-{suffix}")),
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
                // Any year on the catalog row -- the fixture file's own
                // parse has NO year at all, so this must never be treated
                // as a match regardless of what this row's year is.
                year: Some(1999),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("seed a media_metadata row with the exact same title (but the fixture file has no parsed year)");

        // Sanity precondition: confirm the fixture filename really did
        // parse with no year, so this test is actually exercising the
        // no-year path and not accidentally matching some other way.
        let files = walk_media_files(&dir);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].parsed.title.as_deref(), Some(title.as_str()));
        assert_eq!(files[0].parsed.year, None, "precondition: this fixture filename must parse with no year");

        let resolver = DbLibraryResolver { pool: &pool, providers: &[] };
        let report = scan_library(&pool, &library, &resolver, &no_probe_core()).await.expect("scan_library pass");

        assert_eq!(report.scanned, 1);
        assert_eq!(
            report.matched, 0,
            "a title-only match (no parsed year) must never confidently attach, even with an exact-title catalog row"
        );

        let items = repo::media_item::list_by_library(&pool, library.id).await.expect("list media_items");
        assert!(items.is_empty(), "no media_items row should be created for a title-only (no-year) match");

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

    // --- MPRB-06: the probe step ------------------------------------------
    //
    // Everything below runs WITHOUT a database. `probe_pass` is the whole
    // production probe step; only its final write is behind the `ProbeSink`
    // trait, so these tests drive the real decision, the real subprocess, the
    // real parser and the real classification, and inspect what the production
    // code asked the database to store. `MUSE_TEST_DATABASE_URL` is not set on
    // the build host (MUSE #130), so anything that needed a pool would report
    // `ok` while executing nothing.

    mod probe_step {
        use super::*;
        use crate::media::probe::{MediaProbe, ProbeError};
        use crate::media::derive::Suspicion;
        use crate::media::doc::{MediaInfoDoc, StoredProbeState};
        use crate::media::probe::{parse_probe_json, ProbeState};

        /// The committed golden corpus (MPRB-04): real `ffprobe` output from the
        /// real library, scrubbed. Used here so "what the file says" is not a
        /// document this test wrote for itself.
        fn golden(name: &str) -> PathBuf {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden/probe")
                .join(format!("{name}.json"))
        }

        fn golden_probe(name: &str) -> MediaProbe {
            parse_probe_json(&fs::read_to_string(golden(name)).expect("read the golden fixture"))
                .expect("the golden fixture must parse")
        }

        /// A `StoredMediaInfo::V1` built from a real probe, with the variant
        /// asserted rather than assumed: a fixture that silently landed in
        /// `UnknownVersion` would make every "already probed" test below pass for
        /// the wrong reason.
        fn v1_of(name: &str) -> StoredMediaInfo {
            let json = MediaInfoDoc::new(golden_probe(name), "Movie.mkv")
                .to_json()
                .expect("the document must serialise");
            let stored = StoredMediaInfo::from_json(Some(&json));
            assert!(
                matches!(stored, StoredMediaInfo::V1(_)),
                "fixture is not a v1 document: {stored:?}"
            );
            stored
        }

        fn legacy() -> StoredMediaInfo {
            // Built by parsing rather than by `json!`, so this fixture does not
            // trip the guard below that forbids the scanner from constructing a
            // container document.
            let value: serde_json::Value =
                serde_json::from_str(r#"{"container":"mkv"}"#).expect("fixture parses");
            let stored = StoredMediaInfo::from_json(Some(&value));
            assert!(matches!(stored, StoredMediaInfo::Legacy(_)), "{stored:?}");
            stored
        }

        fn newer_binarys_document() -> StoredMediaInfo {
            let stored = StoredMediaInfo::from_json(Some(&serde_json::json!({
                "schema_version": 2, "probe": {}
            })));
            assert!(
                matches!(stored, StoredMediaInfo::UnknownVersion { version: 2 }),
                "{stored:?}"
            );
            stored
        }

        // --- the rule, exercised without a filesystem or a database ---------

        #[test]
        fn a_new_or_changed_file_is_always_probed() {
            for stored in [
                StoredMediaInfo::from_json(None),
                legacy(),
                v1_of("dv_hdr_hevc_4k"),
                newer_binarys_document(),
            ] {
                assert_eq!(
                    probe_decision(true, true, &stored),
                    ProbeDecision::Probe,
                    "a file whose bytes moved must be re-probed whatever the row held: {stored:?}"
                );
            }
        }

        #[test]
        fn an_unchanged_file_that_already_has_a_document_is_not_reprobed() {
            assert_eq!(
                probe_decision(true, false, &v1_of("dv_hdr_hevc_4k")),
                ProbeDecision::UpToDate,
                "a rescan of 16,221 unchanged files must not spawn 16,221 probes"
            );
        }

        #[test]
        fn an_unchanged_unprobed_file_is_left_to_the_backfills_bounded_queue() {
            for stored in [StoredMediaInfo::from_json(None), legacy()] {
                assert_eq!(
                    probe_decision(true, false, &stored),
                    ProbeDecision::DeferredToBackfill,
                    "{stored:?}"
                );
            }
        }

        #[test]
        fn an_unchanged_row_written_by_a_newer_binary_is_never_downgraded() {
            assert_eq!(
                probe_decision(true, false, &newer_binarys_document()),
                ProbeDecision::UpToDate,
                "re-probing a document a newer binary wrote would replace a richer \
                 answer with a poorer one"
            );
        }

        #[test]
        fn a_host_without_ffprobe_probes_nothing_and_says_which_it_is() {
            for changed in [true, false] {
                for stored in [
                    StoredMediaInfo::from_json(None),
                    legacy(),
                    v1_of("dv_hdr_hevc_4k"),
                    newer_binarys_document(),
                ] {
                    assert_eq!(
                        probe_decision(false, changed, &stored),
                        ProbeDecision::NoCapability,
                        "changed={changed} stored={stored:?}"
                    );
                }
            }
            assert_eq!(
                ProbeDecision::NoCapability.as_str(),
                "no_ffprobe_on_this_host"
            );
        }

        // --- the step, exercised against a real subprocess -------------------

        /// What the production code asked the database to store.
        #[derive(Debug)]
        enum Recorded {
            Document {
                media_file_id: i64,
                relative_path: String,
                probe: MediaProbe,
                suspicion: Option<String>,
            },
            Failure {
                media_file_id: i64,
                error: ProbeError,
            },
        }

        #[derive(Default)]
        struct RecordingSink {
            writes: Mutex<Vec<Recorded>>,
            /// When set, every write is refused — the DB-said-no path.
            refuse: bool,
        }

        #[async_trait]
        impl ProbeSink for RecordingSink {
            async fn record(
                &self,
                media_file_id: i64,
                relative_path: &str,
                write: &ProbeWrite<'_>,
            ) -> MuseResult<()> {
                self.writes.lock().unwrap().push(match write {
                    ProbeWrite::Document { probe, suspicion } => Recorded::Document {
                        media_file_id,
                        relative_path: relative_path.to_string(),
                        probe: (*probe).clone(),
                        suspicion: (*suspicion).map(str::to_string),
                    },
                    ProbeWrite::Failure { error } => Recorded::Failure {
                        media_file_id,
                        error: (*error).clone(),
                    },
                });
                if self.refuse {
                    return Err(crate::error::MuseError::Internal(anyhow::anyhow!(
                        "the database refused this write"
                    )));
                }
                Ok(())
            }
        }

        /// An executable `/bin/sh` stub standing in for `ffprobe` that **records
        /// every probe invocation**, so "this pass did not probe" is a claim
        /// about a process rather than about a counter.
        ///
        /// `-version` is answered like a real `ffprobe` and deliberately NOT
        /// counted: `MediaCore::from_config` runs the capability detection
        /// (CAPDET-01) at construction, and counting that spawn would have made
        /// the marker read 1 for a pass that never probed anything — a
        /// zero-spawn assertion is worthless if something else spawns.
        fn stub_ffprobe(dir: &Path, body: &str) -> (String, PathBuf) {
            let marker = dir.join("spawns");
            let path = dir.join("stub-ffprobe");
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\ncase \"$1\" in -version) echo 'ffprobe version 5.1.9'; exit 0;; esac\necho spawned >> '{}'\n{body}\n",
                    marker.display()
                ),
            )
            .expect("write the stub");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            }

            // Wait until the stub is actually executable before returning it.
            //
            // This is not defensive padding — it was added after the mutation
            // battery caught the failure. A freshly written script is racy to
            // exec in a multi-threaded test binary: another thread forking while
            // our write fd is still open leaves the child holding it, and the
            // exec fails with ETXTBSY. `MediaCore::from_config` runs its
            // capability detection at construction, so a lost race turned
            // `can_probe()` into `false` and the test's subject silently became
            // the degradation path instead of the probe path — a real result
            // replaced by a different real result, which is exactly the kind of
            // environmental blindness that makes a mutation look survivable.
            for attempt in 0..100 {
                match std::process::Command::new(&path).arg("-version").output() {
                    Ok(out) if out.status.success() => break,
                    other => {
                        assert!(attempt < 99, "the stub never became executable: {other:?}");
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            }

            (path.to_string_lossy().into_owned(), marker)
        }

        fn spawn_count(marker: &Path) -> usize {
            fs::read_to_string(marker).map(|s| s.lines().count()).unwrap_or(0)
        }

        /// A `MediaCore` rooted at `dir`, probing via `bin`.
        ///
        /// Asserts the capability snapshot came out `true`: every test using this
        /// helper is about what happens when a probe RUNS, and a core that
        /// quietly could not probe would send them all down the degradation path
        /// while still executing. The assertion makes that a named failure rather
        /// than a confusing one.
        fn core_rooted(dir: &Path, bin: &str) -> MediaCore {
            let core = MediaCore::from_config(&crate::config::Config {
                probe_ffprobe_bin: Some(bin.to_string()),
                library_root: Some(dir.to_string_lossy().into_owned()),
                ffmpeg_path: ABSENT_BIN.to_string(),
                foundry_handbrake_bin: Some(ABSENT_BIN.to_string()),
                ..Default::default()
            });
            assert!(
                core.can_probe(),
                "the stub must be detected as a usable ffprobe, or this test is \
                 exercising the degradation path by accident"
            );
            core
        }

        /// **The founding defect, and its fix.**
        ///
        /// The subject file is named `Feature.mkv`. What it actually contains is
        /// the real library's MSMPEG4v2-in-AVI capture. Before MPRB-06 the row
        /// would have recorded `{"container": "mkv"}` — the name. It now records
        /// what the tool read: an `avi` container and an `msmpeg4v2` video
        /// stream, contradicting the extension outright.
        #[tokio::test]
        async fn probe_pass_persists_what_the_file_says_not_what_it_is_called() {
            let dir = unique_dir("probe-contents-not-name");
            let (bin, marker) = stub_ffprobe(
                &dir,
                &format!("cat '{}'", golden("legacy_msmpeg4v2_avi").display()),
            );
            let subject = dir.join("Feature.mkv");
            fs::write(&subject, b"not really a video").unwrap();
            let sink = RecordingSink::default();

            let tally = probe_pass(
                &core_rooted(&dir, &bin),
                &sink,
                77,
                "Feature.mkv",
                &subject,
                &StoredMediaInfo::from_json(None),
                true,
            )
            .await;

            assert_eq!(spawn_count(&marker), 1, "exactly one probe for one new file");
            assert_eq!(
                tally,
                ProbeTally { probed: 1, ..Default::default() },
                "a clean probe is one stored document and nothing else"
            );

            let writes = sink.writes.lock().unwrap();
            let [Recorded::Document { media_file_id, relative_path, probe, suspicion }] =
                &writes[..]
            else {
                panic!("expected exactly one stored document, got {writes:?}");
            };
            assert_eq!(*media_file_id, 77);
            assert_eq!(relative_path, "Feature.mkv");
            assert_eq!(*suspicion, None);
            assert_eq!(
                probe.container, "avi",
                "the stored container must come from the file, not from `.mkv`"
            );
            assert_eq!(
                probe.primary_video().map(|v| v.codec.as_str()),
                Some("msmpeg4v2"),
                "a codec is a fact about bytes, and the filename cannot supply it"
            );

            // The extension is still recorded — inside the document, as MPRB-05's
            // hint, next to the contradicting truth rather than instead of it.
            let doc = MediaInfoDoc::new(probe.clone(), relative_path);
            assert_eq!(doc.file_extension.as_deref(), Some("mkv"));
            assert_ne!(
                doc.probe.container, "mkv",
                "recording what the filename claims must not become believing it"
            );

            fs::remove_dir_all(&dir).ok();
        }

        /// An unchanged file is not merely "not counted" — `ffprobe` is not run.
        /// The marker file is what makes that a statement about a process.
        #[tokio::test]
        async fn an_unchanged_already_probed_file_spawns_nothing_and_writes_nothing() {
            let dir = unique_dir("probe-unchanged");
            let (bin, marker) = stub_ffprobe(
                &dir,
                &format!("cat '{}'", golden("dv_hdr_hevc_4k").display()),
            );
            let subject = dir.join("Feature.mkv");
            fs::write(&subject, b"not really a video").unwrap();
            let sink = RecordingSink::default();

            let tally = probe_pass(
                &core_rooted(&dir, &bin),
                &sink,
                1,
                "Feature.mkv",
                &subject,
                &v1_of("dv_hdr_hevc_4k"),
                false,
            )
            .await;

            assert_eq!(
                spawn_count(&marker),
                0,
                "a rescan of an unchanged, already-probed file must not touch the mount"
            );
            assert!(sink.writes.lock().unwrap().is_empty(), "and must not write");
            assert_eq!(tally, ProbeTally { skipped: 1, ..Default::default() });

            fs::remove_dir_all(&dir).ok();
        }

        /// A failure is classified by MPRB-02's taxonomy, reached through
        /// MPRB-05's wrapper. This module writes no second classification, and
        /// the assertion is against the typed value rather than a spelling.
        #[tokio::test]
        async fn a_failed_probe_is_recorded_with_mprb02s_classification() {
            let dir = unique_dir("probe-exit-failure");
            let (bin, marker) = stub_ffprobe(&dir, "echo 'moov atom not found' >&2; exit 1");
            let subject = dir.join("Broken.mkv");
            fs::write(&subject, b"truncated").unwrap();
            let sink = RecordingSink::default();

            let tally = probe_pass(
                &core_rooted(&dir, &bin),
                &sink,
                42,
                "Broken.mkv",
                &subject,
                &StoredMediaInfo::from_json(None),
                true,
            )
            .await;

            assert_eq!(spawn_count(&marker), 1);
            assert_eq!(
                tally,
                ProbeTally { failed: 1, ..Default::default() },
                "a failure is a failure, never a stored document"
            );

            let writes = sink.writes.lock().unwrap();
            let [Recorded::Failure { media_file_id, error }] = &writes[..] else {
                panic!("expected exactly one recorded failure, got {writes:?}");
            };
            assert_eq!(*media_file_id, 42);
            assert_eq!(
                StoredProbeState::from_error(error),
                StoredProbeState::Failed(ProbeState::ProbeFailed),
                "ffprobe answered and the answer was unusable — that is a statement \
                 about the file, not about the host"
            );
            assert!(!error.is_retryable(), "and re-running it says the same thing");

            fs::remove_dir_all(&dir).ok();
        }

        /// The other side of the taxonomy, so the test above cannot pass by
        /// classifying everything as `probe_failed`.
        #[tokio::test]
        async fn a_missing_binary_is_recorded_as_unreadable_not_as_a_broken_file() {
            let dir = unique_dir("probe-tool-missing");
            let subject = dir.join("Fine.mkv");
            fs::write(&subject, b"perfectly readable").unwrap();
            let sink = RecordingSink::default();

            // A core whose capability snapshot says it CAN probe, pointed at a
            // binary that is gone by the time the probe runs: the state
            // `can_probe()` alone cannot describe.
            let (bin, _marker) = stub_ffprobe(&dir, "cat /dev/null");
            let core = core_rooted(&dir, &bin);
            assert!(core.can_probe(), "the snapshot must have said yes for this test to mean anything");
            fs::remove_file(&bin).unwrap();

            let tally = probe_pass(
                &core,
                &sink,
                9,
                "Fine.mkv",
                &subject,
                &StoredMediaInfo::from_json(None),
                true,
            )
            .await;

            assert_eq!(tally, ProbeTally { failed: 1, ..Default::default() });
            let writes = sink.writes.lock().unwrap();
            let [Recorded::Failure { error, .. }] = &writes[..] else {
                panic!("expected one recorded failure, got {writes:?}");
            };
            assert_eq!(
                StoredProbeState::from_error(error),
                StoredProbeState::Failed(ProbeState::Unreadable),
                "blaming the operator's media for a missing tool is the one thing \
                 ProbeError::ToolMissing exists to prevent"
            );
            assert!(error.is_retryable());

            fs::remove_dir_all(&dir).ok();
        }

        /// A parsed-but-implausible result is still stored, and stored labelled —
        /// with MPRB-03's description, not one this module invented.
        #[tokio::test]
        async fn a_suspicious_result_is_stored_and_carries_mprb03s_label() {
            let dir = unique_dir("probe-suspicious");
            const ZERO_DURATION: &str = r#"{"streams":[{"index":0,"codec_name":"h264","codec_type":"video","width":1920,"height":1080}],"format":{"format_name":"matroska,webm","duration":"0.000000"}}"#;
            let (bin, _marker) = stub_ffprobe(&dir, &format!("printf '%s' '{ZERO_DURATION}'"));
            let subject = dir.join("Zero.mkv");
            fs::write(&subject, b"zero").unwrap();
            let sink = RecordingSink::default();

            let tally = probe_pass(
                &core_rooted(&dir, &bin),
                &sink,
                5,
                "Zero.mkv",
                &subject,
                &StoredMediaInfo::from_json(None),
                true,
            )
            .await;

            assert_eq!(
                tally,
                ProbeTally { probed: 1, suspicious: 1, ..Default::default() },
                "suspicious counts as probed for completion AND as needing attention"
            );
            let writes = sink.writes.lock().unwrap();
            let [Recorded::Document { suspicion, .. }] = &writes[..] else {
                panic!("a suspicious result must still be stored, got {writes:?}");
            };
            assert_eq!(
                suspicion.as_deref(),
                Some(Suspicion::ZeroDuration.as_str()),
                "the label is MPRB-03's, passed through"
            );

            fs::remove_dir_all(&dir).ok();
        }

        /// A walked file that will not resolve inside `MUSE_LIBRARY_ROOT` is a
        /// configuration fault. It is skipped — not spawned against, and not
        /// written as a probe failure, which would spend one of the file's
        /// bounded `probe_attempts` on something that is not wrong with the file.
        #[tokio::test]
        async fn a_file_outside_the_library_root_is_skipped_not_recorded_as_a_failure() {
            let dir = unique_dir("probe-outside-root");
            let elsewhere = unique_dir("probe-outside-root-other");
            let (bin, marker) = stub_ffprobe(&dir, "cat /dev/null");
            let subject = elsewhere.join("Feature.mkv");
            fs::write(&subject, b"outside").unwrap();
            let sink = RecordingSink::default();

            let tally = probe_pass(
                &core_rooted(&dir, &bin),
                &sink,
                3,
                "Feature.mkv",
                &subject,
                &StoredMediaInfo::from_json(None),
                true,
            )
            .await;

            assert_eq!(tally, ProbeTally { skipped: 1, ..Default::default() });
            assert_eq!(spawn_count(&marker), 0);
            assert!(sink.writes.lock().unwrap().is_empty());

            fs::remove_dir_all(&dir).ok();
            fs::remove_dir_all(&elsewhere).ok();
        }

        /// A host with no `ffprobe` still scans. It records no document, it does
        /// not spawn, and it does not fail.
        #[tokio::test]
        async fn a_host_without_ffprobe_degrades_rather_than_failing_the_scan() {
            let dir = unique_dir("probe-no-capability");
            let subject = dir.join("Feature.mkv");
            fs::write(&subject, b"unprobeable here").unwrap();
            let sink = RecordingSink::default();
            let core = MediaCore::from_config(&crate::config::Config {
                probe_ffprobe_bin: Some(ABSENT_BIN.to_string()),
                library_root: Some(dir.to_string_lossy().into_owned()),
                ffmpeg_path: ABSENT_BIN.to_string(),
                foundry_handbrake_bin: Some(ABSENT_BIN.to_string()),
                ..Default::default()
            });
            assert!(!core.can_probe());

            let tally = probe_pass(
                &core,
                &sink,
                11,
                "Feature.mkv",
                &subject,
                &StoredMediaInfo::from_json(None),
                true,
            )
            .await;

            assert_eq!(tally, ProbeTally { skipped: 1, ..Default::default() });
            assert!(sink.writes.lock().unwrap().is_empty());

            fs::remove_dir_all(&dir).ok();
        }

        /// A verdict the database refused is not a stored document. `probed` is
        /// reported as "documents persisted"; counting an observation the write
        /// never landed would make that number a claim about Postgres that
        /// Postgres never agreed to.
        #[tokio::test]
        async fn a_refused_write_is_counted_as_a_refused_write_not_as_a_document() {
            let dir = unique_dir("probe-refused-write");
            let (bin, _marker) = stub_ffprobe(
                &dir,
                &format!("cat '{}'", golden("dv_hdr_hevc_4k").display()),
            );
            let subject = dir.join("Feature.mkv");
            fs::write(&subject, b"fine").unwrap();
            let sink = RecordingSink { refuse: true, ..Default::default() };

            let tally = probe_pass(
                &core_rooted(&dir, &bin),
                &sink,
                8,
                "Feature.mkv",
                &subject,
                &StoredMediaInfo::from_json(None),
                true,
            )
            .await;

            assert_eq!(
                tally,
                ProbeTally { persist_failed: 1, ..Default::default() },
                "no `probed`, and the failure is reported as a WRITE failure, which is \
                 a different thing from a file that will not parse"
            );
            assert_eq!(sink.writes.lock().unwrap().len(), 1, "it was attempted");

            fs::remove_dir_all(&dir).ok();
        }

        /// The extension-derived write cannot come back without changing a
        /// signature: `upsert_scanned` no longer takes a `media_info` at all, and
        /// nothing in the scanner derives a container from a path.
        #[test]
        fn the_scanner_no_longer_derives_a_stored_document_from_a_filename() {
            let me = include_str!("scan.rs");
            for banned in [
                concat!("json!({ \"container\"", ":"),
                concat!("json!({\"container\"", ":"),
            ] {
                assert!(
                    !me.contains(banned),
                    "the scanner must not construct a container document: {banned}"
                );
            }
            let repo_src = include_str!("../repo/media_file.rs");
            let signature = repo_src
                .split("pub async fn upsert_scanned(")
                .nth(1)
                .expect("upsert_scanned must exist")
                .split(')')
                .next()
                .unwrap();
            assert!(
                !signature.contains("media_info"),
                "upsert_scanned must not accept a media_info again: {signature}"
            );
        }
    }
}
