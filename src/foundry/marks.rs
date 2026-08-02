//! FOUNDRY-06 — Path B's trigger: which titles the operator marked for
//! renditions.
//!
//! ## Why this module is the enforcement point
//!
//! The rendition ladder (FOUNDRY-03) has existed with **no caller**.
//! `plan_ladder` was reachable only from tests, so Path B could not run at all.
//! That was safe, but only by accident of being unwired — and "safe because
//! unfinished" stops being true the moment somebody finishes it.
//!
//! The operator's constraint, verbatim: *"we don't want to generate 4 versions
//! of everything, just items that are marked for that by the user via the
//! interface when opening a title (season or top of show directory, movie)"*.
//!
//! So this module is built so that **the only way to produce a rendition
//! candidate is a mark**. There is no function here that enumerates the
//! library, and a source-level test asserts there never is one. The absence of
//! a mark is the absence of consent, and that is enforced by there being
//! nowhere else for a candidate to come from — not by a check that could be
//! forgotten.
//!
//! ## Scopes are stored unexpanded
//!
//! A `season` mark is kept as a season, not flattened to today's episodes.
//! Expanding at mark time would silently miss an episode that arrives
//! tomorrow, which is the opposite of what marking a season means. Expansion
//! happens at RUN time, where the resulting count is visible before anything
//! encodes.

use std::path::{Path, PathBuf};

use crate::foundry::rendition::RenditionName;

/// What the operator marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkScope {
    /// One movie file.
    Movie,
    /// A season directory — every episode in it, including ones not yet there.
    Season,
    /// A whole show directory, all seasons.
    Show,
}

impl MarkScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Season => "season",
            Self::Show => "show",
        }
    }

    /// Parse the value the UI sends. Unknown scopes are REFUSED rather than
    /// defaulted: a typo'd scope that silently became `movie` would mark one
    /// file when the operator meant a whole show, and they would not find out
    /// until the renditions did not appear.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "movie" => Some(Self::Movie),
            "season" => Some(Self::Season),
            "show" => Some(Self::Show),
            _ => None,
        }
    }

    /// Whether this scope names a directory to walk rather than a single file.
    pub fn is_directory(self) -> bool {
        matches!(self, Self::Season | Self::Show)
    }
}

/// A live mark.
#[derive(Debug, Clone, PartialEq)]
pub struct RenditionMark {
    pub id: i64,
    pub scope: MarkScope,
    pub path: String,
    pub rungs: Vec<RenditionName>,
}

/// Why a mark produced no work.
#[derive(Debug, Clone, PartialEq)]
pub enum ExpansionProblem {
    /// The marked path is gone — renamed, deleted, or the mount is absent.
    ///
    /// Reported rather than skipped silently: a mark that quietly produces
    /// nothing is indistinguishable from a mark that was never made, and the
    /// operator would conclude the feature is broken.
    PathMissing { path: String },
    /// A directory scope that contains no media at all.
    NoMediaUnder { path: String },
    /// The directory could not be listed.
    Unreadable { path: String, detail: String },
    /// The marked path is itself a symlink.
    ///
    /// Refused rather than followed. `Path::exists`/`is_file` follow symlinks
    /// and `read_dir` follows a directory symlink, so a mark on a link would
    /// expand to its TARGET — encoding files outside anything the operator
    /// marked, and potentially outside the library entirely. Raised by codex
    /// at the FOUNDRY-06 gate; the symlink test only covered links found
    /// INSIDE a marked directory, not the marked path itself.
    MarkedPathIsSymlink { path: String },
    /// Partially unreadable: some media was found, but a subdirectory could
    /// not be listed, so the expansion is INCOMPLETE.
    ///
    /// Reported alongside the files rather than swallowed. Returning the
    /// partial list with no problem would be the same absence-vs-ignorance
    /// confusion this codebase has hit four times elsewhere.
    PartiallyUnreadable { path: String, detail: String },
}

impl std::fmt::Display for ExpansionProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PathMissing { path } => write!(
                f,
                "the marked path {path} no longer exists — the title was renamed, removed, \
                 or its mount is absent; the mark produced nothing"
            ),
            Self::NoMediaUnder { path } => {
                write!(f, "{path} contains no media files, so the mark produced nothing")
            }
            Self::MarkedPathIsSymlink { path } => write!(
                f,
                "the marked path {path} is a symlink — refusing to follow it, because it \
                 would expand to its target and encode files that were never marked"
            ),
            Self::PartiallyUnreadable { path, detail } => write!(
                f,
                "{path} was only PARTLY listed ({detail}) — the files found are real but \
                 the expansion is incomplete, so titles may be missing"
            ),
            Self::Unreadable { path, detail } => write!(
                f,
                "{path} could not be listed ({detail}) — this is NOT a statement that it \
                 holds no media"
            ),
        }
    }
}

/// Expand ONE mark to the files it covers, at run time.
///
/// Deliberately takes a single mark: the count of files a mark implies is
/// visible to the caller before anything encodes, and a mark that expands to
/// four hundred episodes is something the operator should see rather than
/// discover.
///
/// Never walks anything but the marked path. A `Movie` mark yields exactly its
/// own file; a directory scope yields the media beneath it and nothing else.
pub fn expand(mark: &RenditionMark) -> (Vec<PathBuf>, Option<ExpansionProblem>) {
    let path = Path::new(&mark.path);

    // symlink_metadata does NOT follow. Checked FIRST, because every test
    // below (`exists`, `is_file`, `read_dir`) does follow, so a marked symlink
    // would silently expand to its target.
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            return (
                Vec::new(),
                Some(ExpansionProblem::MarkedPathIsSymlink {
                    path: mark.path.clone(),
                }),
            )
        }
        Ok(_) => {}
        Err(_) => {
            return (
                Vec::new(),
                Some(ExpansionProblem::PathMissing {
                    path: mark.path.clone(),
                }),
            )
        }
    }

    if !path.exists() {
        return (
            Vec::new(),
            Some(ExpansionProblem::PathMissing {
                path: mark.path.clone(),
            }),
        );
    }

    if !mark.scope.is_directory() {
        // A movie mark is its own file. If it points at a directory anyway,
        // that is a mis-scoped mark and is reported rather than silently
        // treated as a season.
        return if path.is_file() {
            (vec![path.to_path_buf()], None)
        } else {
            (
                Vec::new(),
                Some(ExpansionProblem::NoMediaUnder {
                    path: mark.path.clone(),
                }),
            )
        };
    }

    let mut out = Vec::new();
    let mut unreadable: Option<String> = None;
    walk_media(path, &mut out, &mut unreadable);
    out.sort();

    if let Some(detail) = unreadable {
        // Both cases are reported. Returning a partial list with NO problem
        // would say "here is what is under this mark" when part of it could
        // not be read — the same absence-vs-ignorance confusion fixed four
        // times elsewhere in this codebase. Opus, FOUNDRY-06 gate.
        return (
            out.clone(),
            Some(if out.is_empty() {
                ExpansionProblem::Unreadable { path: mark.path.clone(), detail }
            } else {
                ExpansionProblem::PartiallyUnreadable { path: mark.path.clone(), detail }
            }),
        );
    }
    if out.is_empty() {
        return (
            out,
            Some(ExpansionProblem::NoMediaUnder {
                path: mark.path.clone(),
            }),
        );
    }
    (out, None)
}

fn walk_media(dir: &Path, out: &mut Vec<PathBuf>, unreadable: &mut Option<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            *unreadable = Some(e.to_string());
            return;
        }
    };
    for entry in entries.flatten() {
        let p = entry.path();
        match entry.file_type() {
            // Never follow symlinks: a link inside a marked season could point
            // anywhere, and a rendition run would then encode files the
            // operator never marked.
            Ok(t) if t.is_symlink() => continue,
            Ok(t) if t.is_dir() => walk_media(&p, out, unreadable),
            Ok(t) if t.is_file() => {
                if crate::library::scan::has_media_extension(&p) {
                    out.push(p);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("muse-marks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("scratch");
        d
    }

    fn mark(scope: MarkScope, path: &Path) -> RenditionMark {
        RenditionMark {
            id: 1,
            scope,
            path: path.display().to_string(),
            rungs: vec![RenditionName::Tv],
        }
    }

    /// THE constraint this whole module exists for.
    ///
    /// The operator's requirement is that renditions are produced only for
    /// marked titles, never library-wide. That is enforced structurally: there
    /// is no function here that can produce a candidate from anything but a
    /// mark. A check could be forgotten; an absent capability cannot.
    #[test]
    fn nothing_in_this_module_can_enumerate_the_library() {
        let body = include_str!("marks.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("a non-test body");
        assert!(
            !body.contains("walk_media_files"),
            "the library scanner must never be reachable from here — a rendition run must \\
             not be able to produce a candidate list that nobody marked"
        );
        assert!(
            !body.contains("library_root"),
            "the library root must not be readable from here for the same reason"
        );
        // Every walk must start from a mark's own path.
        assert!(
            body.contains("pub fn expand(mark: &RenditionMark)"),
            "expansion must take a MARK, so a candidate cannot exist without one"
        );
    }

    #[test]
    fn a_movie_mark_expands_to_exactly_its_own_file() {
        let d = tmp("movie");
        let f = d.join("Movie.mkv");
        fs::write(&f, b"x").unwrap();
        let (files, problem) = expand(&mark(MarkScope::Movie, &f));
        assert_eq!(files, vec![f]);
        assert_eq!(problem, None);
        let _ = fs::remove_dir_all(&d);
    }

    /// A season mark covers episodes that are not there yet — which is why the
    /// scope is stored unexpanded and walked at RUN time.
    #[test]
    fn a_season_mark_expands_to_the_episodes_present_when_it_runs() {
        let d = tmp("season");
        fs::write(d.join("S01E01.mkv"), b"x").unwrap();
        fs::write(d.join("S01E02.mkv"), b"x").unwrap();
        fs::write(d.join("poster.jpg"), b"x").unwrap();
        let (files, problem) = expand(&mark(MarkScope::Season, &d));
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(files.iter().all(|p| p.extension().unwrap() == "mkv"));
        assert_eq!(problem, None);

        // An episode arriving later is picked up, because nothing was
        // flattened at mark time.
        fs::write(d.join("S01E03.mkv"), b"x").unwrap();
        let (later, _) = expand(&mark(MarkScope::Season, &d));
        assert_eq!(later.len(), 3, "a later episode must be covered by the same mark");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_show_mark_covers_every_season_beneath_it() {
        let d = tmp("show");
        for s in ["Season 01", "Season 02"] {
            fs::create_dir_all(d.join(s)).unwrap();
            fs::write(d.join(s).join("ep.mkv"), b"x").unwrap();
        }
        let (files, _) = expand(&mark(MarkScope::Show, &d));
        assert_eq!(files.len(), 2);
        let _ = fs::remove_dir_all(&d);
    }

    /// A mark that produces nothing must SAY so. Silently yielding an empty
    /// list is indistinguishable from a mark that was never made, and the
    /// operator would conclude the feature is broken.
    #[test]
    fn a_mark_whose_path_is_gone_reports_it_rather_than_yielding_nothing() {
        let (files, problem) = expand(&RenditionMark {
            id: 1,
            scope: MarkScope::Show,
            path: "/nonexistent-muse-mark".into(),
            rungs: vec![RenditionName::Tv],
        });
        assert!(files.is_empty());
        assert!(matches!(problem, Some(ExpansionProblem::PathMissing { .. })), "{problem:?}");
    }

    #[test]
    fn a_directory_with_no_media_reports_that_rather_than_succeeding_emptily() {
        let d = tmp("empty");
        fs::write(d.join("readme.txt"), b"x").unwrap();
        let (files, problem) = expand(&mark(MarkScope::Season, &d));
        assert!(files.is_empty());
        assert!(matches!(problem, Some(ExpansionProblem::NoMediaUnder { .. })), "{problem:?}");
        let _ = fs::remove_dir_all(&d);
    }

    /// Symlinks are not followed: a link inside a marked season could point
    /// anywhere, and following it would encode files nobody marked.
    #[test]
    fn a_symlink_inside_a_marked_season_is_not_followed() {
        let outside = tmp("mark-outside");
        let inside = tmp("mark-inside");
        fs::write(outside.join("Elsewhere.mkv"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, inside.join("linked")).unwrap();
        std::os::unix::fs::symlink(outside.join("Elsewhere.mkv"), inside.join("Link.mkv")).unwrap();
        fs::write(inside.join("Real.mkv"), b"x").unwrap();

        let (files, _) = expand(&mark(MarkScope::Season, &inside));
        assert_eq!(files.len(), 1, "only the real file: {files:?}");
        assert!(files[0].ends_with("Real.mkv"));
        let _ = fs::remove_dir_all(&inside);
        let _ = fs::remove_dir_all(&outside);
    }

    /// The structural guarantee, asserted across BOTH modules that could
    /// break it.
    ///
    /// FOUNDRY-06 proved this module cannot enumerate the library. But the
    /// endpoints and the repo layer are where a future change would most
    /// naturally reach for "all titles" — so the guarantee is only real if it
    /// holds there too.
    #[test]
    fn neither_the_repo_nor_the_endpoints_can_produce_an_unmarked_candidate() {
        let repo = include_str!("../repo/rendition_mark.rs");
        let repo_body = repo.split("#[cfg(test)]").next().unwrap_or(repo);
        assert!(
            !repo_body.contains("walk_media_files") && !repo_body.contains("library_root"),
            "the marks repo must not be able to list the library"
        );
        // Every SELECT must itself be scoped to live marks. Counting
        // occurrences was too weak — the UPDATE's own `revoked_at IS NULL`
        // satisfied the count while the SELECT had none, so a mutation that
        // made live() return REVOKED marks survived. Each SELECT is now
        // inspected individually.
        for (i, chunk) in repo_body.match_indices("SELECT") {
            let stmt_end = repo_body[i..]
                .find("\")")
                .map(|e| i + e)
                .unwrap_or(repo_body.len());
            let stmt = &repo_body[i..stmt_end];
            let _ = chunk;
            assert!(
                stmt.contains("revoked_at IS NULL"),
                "a read that is not scoped to LIVE marks would resurrect consent the \
                 operator withdrew. Offending statement: {stmt}"
            );
        }

        // And the plan endpoint must derive candidates from marks alone.
        let dash = include_str!("../web/dashboard.rs");
        let plan_start = dash
            .find("pub async fn foundry_renditions_plan")
            .expect("the plan endpoint exists");
        let plan = &dash[plan_start..plan_start + 2500];
        assert!(
            plan.contains("rendition_mark::live"),
            "the plan must read marks"
        );
        assert!(
            !plan.contains("walk_media_files"),
            "the plan must NOT be able to enumerate the library — that is the whole \
             constraint: renditions only for marked titles, never library-wide"
        );
    }

    /// A mark on a SYMLINK must be refused, not followed.
    ///
    /// Codex caught this at the gate. `Path::exists`, `is_file` and `read_dir`
    /// all follow symlinks, so a marked link would expand to its target —
    /// encoding files never marked, potentially outside the library entirely.
    /// The earlier symlink test covered links found INSIDE a marked directory
    /// and named the property too broadly.
    #[test]
    fn a_mark_on_a_symlink_is_refused_rather_than_followed() {
        let outside = tmp("sym-target");
        let inside = tmp("sym-mark");
        fs::write(outside.join("Elsewhere.mkv"), b"x").unwrap();

        // A marked DIRECTORY symlink.
        let dir_link = inside.join("linked-season");
        std::os::unix::fs::symlink(&outside, &dir_link).unwrap();
        let (files, problem) = expand(&mark(MarkScope::Season, &dir_link));
        assert!(files.is_empty(), "must not expand through the link: {files:?}");
        assert!(
            matches!(problem, Some(ExpansionProblem::MarkedPathIsSymlink { .. })),
            "{problem:?}"
        );

        // A marked FILE symlink.
        let file_link = inside.join("linked-movie.mkv");
        std::os::unix::fs::symlink(outside.join("Elsewhere.mkv"), &file_link).unwrap();
        let (files, problem) = expand(&mark(MarkScope::Movie, &file_link));
        assert!(files.is_empty(), "{files:?}");
        assert!(
            matches!(problem, Some(ExpansionProblem::MarkedPathIsSymlink { .. })),
            "{problem:?}"
        );

        let _ = fs::remove_dir_all(&inside);
        let _ = fs::remove_dir_all(&outside);
    }

    /// A partially-readable directory must report BOTH the files it found and
    /// the fact that it could not read everything.
    ///
    /// Returning the partial list with no problem would say "here is what is
    /// under this mark" when part of it was never seen — the same
    /// absence-vs-ignorance confusion fixed four times elsewhere. Opus.
    #[test]
    fn a_partially_unreadable_mark_reports_both_the_files_and_the_gap() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("partial");
        fs::write(d.join("Visible.mkv"), b"x").unwrap();
        let blocked = d.join("blocked");
        fs::create_dir_all(&blocked).unwrap();
        fs::write(blocked.join("Hidden.mkv"), b"x").unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();

        // Probe root-ness INDEPENDENTLY of the code under test.
        //
        // This was `problem.is_none()`, which is exactly the observable a
        // broken implementation produces — so a mutation that reported the
        // partial read as complete made the test conclude "must be root" and
        // return early, passing vacuously. The skip-condition and the
        // failure-condition were the same thing. Caught by that mutation
        // surviving.
        let running_as_root = fs::read_dir(&blocked).is_ok();

        let (files, problem) = expand(&mark(MarkScope::Show, &d));
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&d);

        if running_as_root {
            return; // root reads anything; the permission bit does not apply
        }
        assert!(!files.is_empty(), "the readable file must still be returned");
        assert!(
            matches!(problem, Some(ExpansionProblem::PartiallyUnreadable { .. })),
            "an incomplete expansion must SAY it is incomplete: {problem:?}"
        );
    }

    /// The wholly-unreadable case, which had no test at all. Opus.
    #[test]
    fn a_wholly_unreadable_mark_reports_unreadable_not_empty() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("unreadable");
        fs::write(d.join("Hidden.mkv"), b"x").unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o000)).unwrap();

        // Independent probe, for the same reason as above.
        let running_as_root = fs::read_dir(&d).is_ok();
        let (files, problem) = expand(&mark(MarkScope::Season, &d));
        let _ = &files;
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&d);

        if running_as_root {
            return;
        }
        assert!(
            matches!(problem, Some(ExpansionProblem::Unreadable { .. })),
            "must NOT read as 'contains no media': {problem:?}"
        );
    }

    /// An unknown scope is refused, not defaulted. A typo that silently became
    /// `movie` would mark one file when the operator meant a whole show.
    #[test]
    fn an_unknown_scope_is_refused_rather_than_defaulted() {
        assert_eq!(MarkScope::parse("movie"), Some(MarkScope::Movie));
        assert_eq!(MarkScope::parse("  SEASON "), Some(MarkScope::Season));
        assert_eq!(MarkScope::parse("show"), Some(MarkScope::Show));
        assert_eq!(MarkScope::parse("boxset"), None);
        assert_eq!(MarkScope::parse(""), None);
    }
}
