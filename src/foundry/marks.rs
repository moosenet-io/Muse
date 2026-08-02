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
        if out.is_empty() {
            return (
                out,
                Some(ExpansionProblem::Unreadable {
                    path: mark.path.clone(),
                    detail,
                }),
            );
        }
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
