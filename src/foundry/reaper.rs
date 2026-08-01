//! FOUNDRY-05 — reap `.muse-superseded` originals, and **only** through the
//! deletion gate.
//!
//! ## Why this module is the dangerous one
//!
//! Nothing in Muse permanently deletes a library file today. `forge`'s swap
//! hard-links the original to a sibling `<name>.muse-superseded` entry *before*
//! it releases the original name, so what looks like a delete in
//! `swap_verified_output` unlinks one of two names and the bytes stay reachable.
//!
//! Deleting that `.muse-superseded` entry is the first and only step in the
//! whole pipeline that destroys data. It is the step Path A's promise — "every
//! incoming title is optimized and the original removed" — actually rests on,
//! and it is the step that runs ~16,000 times.
//!
//! So this module does as little as possible and refuses on everything it
//! cannot establish:
//!
//! 1. It never computes its own opinion about safety. Every candidate goes
//!    through [`may_delete_original`], the same gate FOUNDRY-03 built, and a
//!    `Refuse` is final here.
//! 2. It re-probes **both** files at reap time rather than trusting anything
//!    recorded when the swap ran. The replacement may have been edited,
//!    truncated, or replaced by a later upgrade in the meantime.
//! 3. It is **off by default**, and even when enabled it defaults to a dry run.
//!    Turning it on is two deliberate steps, not one.
//! 4. A retention window means a bad encode is recoverable for a stated period
//!    after the swap, not only until the next reap.
//!
//! ## What it deliberately does not do
//!
//! It does not re-encode, re-verify, or repair anything. If the replacement is
//! wrong, the correct outcome is that the original is KEPT and an operator
//! looks at it — not that this module tries to fix it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::foundry::directplay::{may_delete_original, DeletionDecision, NormalizationOutcome};
use crate::foundry::probe::MediaProbe;
use crate::foundry::Foundry;

/// The extension `forge` gives a preserved original. Must match
/// `forge::SUPERSEDED_EXT`; asserted by a test rather than assumed.
pub const SUPERSEDED_EXT: &str = "muse-superseded";

/// How long a preserved original is kept before it becomes reapable.
///
/// Default **14 days**. The window is the difference between "recoverable" and
/// "recoverable until the next cron tick": a bad encode is usually noticed when
/// somebody tries to watch the film, which may be weeks after ingest.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// What the reaper decided about one preserved original.
#[derive(Debug, Clone, PartialEq)]
pub enum ReapOutcome {
    /// Deleted. Only ever reached from a gate `Allow` with mutation enabled.
    Deleted,
    /// The gate would allow it and the retention window has passed, but this
    /// was a dry run. The default.
    WouldDelete,
    /// The gate refused, with its reasons verbatim.
    GateRefused { blockers: Vec<String> },
    /// Still inside the retention window.
    TooYoung { age_secs: u64, retention_secs: u64 },
    /// The live replacement this original was superseded BY is missing.
    ///
    /// The most important refusal in the module: it means the library has the
    /// backup and nothing else, so the backup is the only copy.
    ReplacementMissing { expected: String },
    /// One of the two files could not be probed, so the comparison the gate
    /// needs could not be made. Unknown never becomes deletable.
    ProbeFailed { which: &'static str, detail: String },
    /// The two names resolve to the same inode, i.e. the swap never completed
    /// its unlink. Deleting here would delete the live file.
    SameInodeAsReplacement,
    /// The path could not be inspected at all.
    CouldNotInspect { detail: String },
}

impl ReapOutcome {
    pub fn deleted(&self) -> bool {
        matches!(self, Self::Deleted)
    }
    /// Whether this outcome represents a *safe-to-delete* judgement, whether or
    /// not the deletion was actually performed.
    pub fn would_delete(&self) -> bool {
        matches!(self, Self::Deleted | Self::WouldDelete)
    }
}

impl std::fmt::Display for ReapOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deleted => write!(f, "deleted"),
            Self::WouldDelete => write!(f, "would delete (dry run)"),
            Self::GateRefused { blockers } => {
                write!(f, "the deletion gate refused: {}", blockers.join("; "))
            }
            Self::TooYoung { age_secs, retention_secs } => write!(
                f,
                "kept: {age_secs}s old, inside the {retention_secs}s retention window"
            ),
            Self::ReplacementMissing { expected } => write!(
                f,
                "kept: the replacement {expected} does not exist, so this backup is the ONLY \
                 copy of the title"
            ),
            Self::ProbeFailed { which, detail } => {
                write!(f, "kept: the {which} could not be probed ({detail})")
            }
            Self::SameInodeAsReplacement => write!(
                f,
                "kept: the backup and the live file are the SAME inode, so the swap never \
                 released the original name — deleting would destroy the live file"
            ),
            Self::CouldNotInspect { detail } => write!(f, "kept: could not inspect ({detail})"),
        }
    }
}

/// One preserved original and what the reaper decided about it.
#[derive(Debug, Clone)]
pub struct ReapedFile {
    pub superseded_path: String,
    pub replacement_path: String,
    pub bytes: Option<u64>,
    pub outcome: ReapOutcome,
}

/// The whole pass.
#[derive(Debug, Clone, Default)]
pub struct ReapRun {
    pub files: Vec<ReapedFile>,
    /// True only when deletion was actually enabled. Reported on every response
    /// so a dry run can never be mistaken for a real one.
    pub mutation_enabled: bool,
    pub retention_secs: u64,
    pub bytes_reclaimed: u64,
}

impl ReapRun {
    pub fn deleted(&self) -> usize {
        self.files.iter().filter(|f| f.outcome.deleted()).count()
    }
    pub fn would_delete(&self) -> usize {
        self.files.iter().filter(|f| f.outcome.would_delete()).count()
    }
    pub fn kept(&self) -> usize {
        self.files.len() - self.would_delete()
    }
}

/// The live path a `.muse-superseded` file was superseded BY.
///
/// `forge` appends the extension, so removing it recovers the original name.
/// Returns `None` when the path does not carry the extension at all, which is
/// how a caller's own mistake surfaces rather than being reinterpreted.
pub fn replacement_of(superseded: &Path) -> Option<PathBuf> {
    let name = superseded.file_name()?.to_str()?;
    let stem = name.strip_suffix(&format!(".{SUPERSEDED_EXT}"))?;
    if stem.is_empty() {
        return None;
    }
    Some(superseded.with_file_name(stem))
}

/// Every `.muse-superseded` file under `root`, recursively.
///
/// Walks with `read_dir` rather than the library scanner because the scanner
/// deliberately ignores non-media extensions — and `.muse-superseded` is
/// non-media by design, exactly so the scanner will not index a backup as a
/// title.
pub fn find_superseded(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // A directory that cannot be listed is logged and skipped rather than
        // aborting the pass: one unreadable folder must not stop the rest.
        tracing::warn!(dir = %dir.display(), "foundry reaper: could not list directory; skipping");
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        match entry.file_type() {
            // Never follow symlinks: a link could point outside the allowed
            // roots entirely, and this module deletes what it is given.
            Ok(t) if t.is_symlink() => continue,
            Ok(t) if t.is_dir() => walk(&p, out),
            Ok(t) if t.is_file() => {
                if p.extension().and_then(|e| e.to_str()) == Some(SUPERSEDED_EXT) {
                    out.push(p);
                }
            }
            _ => {}
        }
    }
}

/// Age of a preserved original, measured from its **ctime**, or `None` when it
/// cannot be determined.
///
/// NOT mtime, and this is the whole point. `forge` creates the backup with
/// `hard_link`, and a hard link shares the inode — so the backup inherits the
/// ORIGINAL's mtime, which is the download date and is routinely years old.
/// Measuring retention from mtime would have made every backup instantly
/// eligible, silently reducing a 14-day window to no window at all. Raised by
/// opus and free at the FOUNDRY-05 gate.
///
/// ctime is updated when the link is created (the link count changes), so it
/// is the moment the backup came into existence. It is also not settable by
/// `touch`/`cp -p`/rsync, which mtime is — relevant because this value gates a
/// deletion.
///
/// A clock that has gone backwards yields `None` (via `duration_since`), which
/// the caller treats as "not old enough" rather than as zero age.
fn age_of(p: &Path) -> Option<Duration> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(p).ok()?;
    let ctime = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(u64::try_from(md.ctime()).ok()?))?;
    SystemTime::now().duration_since(ctime).ok()
}

/// Decide one preserved original. **Never deletes** — the decision and the
/// deletion are separate so the decision is testable without a filesystem that
/// can lose data.
/// `probe` is injected rather than taken as a `Foundry` so the DECISION is
/// testable without a live ffprobe or a configured guard. The impure boundary
/// stays at [`reap`], which passes `Foundry::probe_file`. A decision this
/// consequential should not be reachable only through a test that skips.
pub fn decide_one(
    probe: &dyn Fn(&Path) -> Result<MediaProbe, String>,
    superseded: &Path,
    retention: Duration,
) -> (ReapOutcome, Option<PathBuf>, Option<u64>) {
    let Some(replacement) = replacement_of(superseded) else {
        return (
            ReapOutcome::CouldNotInspect {
                detail: format!("{} is not a superseded name", superseded.display()),
            },
            None,
            None,
        );
    };

    let bytes = std::fs::metadata(superseded).ok().map(|m| m.len());

    // The replacement must EXIST. Without it this backup is the only copy of
    // the title, and deleting it destroys the title outright.
    if !replacement.is_file() {
        return (
            ReapOutcome::ReplacementMissing {
                expected: replacement.display().to_string(),
            },
            Some(replacement),
            bytes,
        );
    }

    // Same inode means the swap never released the original name. Deleting
    // this path would unlink the live file's only remaining name.
    match (
        crate::foundry::forge::identity_of(superseded),
        crate::foundry::forge::identity_of(&replacement),
    ) {
        (Ok(a), Ok(b)) if a == b => {
            return (ReapOutcome::SameInodeAsReplacement, Some(replacement), bytes)
        }
        (Err(e), _) | (_, Err(e)) => {
            return (
                ReapOutcome::CouldNotInspect { detail: e.to_string() },
                Some(replacement),
                bytes,
            )
        }
        _ => {}
    }

    // Retention is checked BEFORE probing: it is the cheap check, and a file
    // inside the window is not going to be deleted whatever the probes say.
    match age_of(superseded) {
        Some(age) if age < retention => {
            return (
                ReapOutcome::TooYoung {
                    age_secs: age.as_secs(),
                    retention_secs: retention.as_secs(),
                },
                Some(replacement),
                bytes,
            )
        }
        // An unreadable mtime is not "old enough".
        None => {
            return (
                ReapOutcome::CouldNotInspect {
                    detail: "the file's age could not be determined".to_string(),
                },
                Some(replacement),
                bytes,
            )
        }
        Some(_) => {}
    }

    // Re-probe BOTH, now. Nothing recorded at swap time is trusted: the
    // replacement may have been edited, truncated or replaced since.
    let source: MediaProbe = match probe(superseded) {
        Ok(p) => p,
        Err(detail) => {
            return (
                ReapOutcome::ProbeFailed { which: "preserved original", detail },
                Some(replacement),
                bytes,
            )
        }
    };
    let output: MediaProbe = match probe(&replacement) {
        Ok(p) => p,
        Err(detail) => {
            return (
                ReapOutcome::ProbeFailed { which: "replacement", detail },
                Some(replacement),
                bytes,
            )
        }
    };

    // The gate. This module contributes no opinion of its own.
    match may_delete_original(&source, &NormalizationOutcome::Verified { output }) {
        DeletionDecision::Allow { .. } => (ReapOutcome::WouldDelete, Some(replacement), bytes),
        DeletionDecision::Refuse { blockers } => (
            ReapOutcome::GateRefused {
                blockers: blockers.iter().map(|b| b.to_string()).collect(),
            },
            Some(replacement),
            bytes,
        ),
    }
}

/// Re-verify, then unlink. **The only deletion in Muse.**
///
/// The gate's verdict was formed from probes that take seconds on a 4K file.
/// In that window the replacement can be removed, renamed, or replaced by
/// another process — and if it is, this backup became the last copy of the
/// title AFTER the gate said it was safe. So the two facts that make deletion
/// survivable are re-checked immediately before the unlink, against the same
/// paths, with nothing in between:
///
/// 1. the replacement still exists, and
/// 2. it is still a DIFFERENT inode from the backup.
///
/// Cheap (two stats) next to what it prevents. Raised by opus and free at the
/// FOUNDRY-05 gate.
///
/// This is not a claim to have closed the TOCTOU race — nothing short of
/// holding the directory can — only to have narrowed it from "the length of
/// two ffprobes" to "the length of two stats".
fn delete_verified(superseded: &Path, replacement: Option<&Path>) -> ReapOutcome {
    let Some(replacement) = replacement else {
        return ReapOutcome::CouldNotInspect {
            detail: "no replacement path to re-verify against".to_string(),
        };
    };
    if !replacement.is_file() {
        return ReapOutcome::ReplacementMissing {
            expected: replacement.display().to_string(),
        };
    }
    match (
        crate::foundry::forge::identity_of(superseded),
        crate::foundry::forge::identity_of(replacement),
    ) {
        (Ok(a), Ok(b)) if a == b => return ReapOutcome::SameInodeAsReplacement,
        (Err(e), _) | (_, Err(e)) => {
            return ReapOutcome::CouldNotInspect {
                detail: format!("re-verification before deletion failed: {e}"),
            }
        }
        _ => {}
    }
    match std::fs::remove_file(superseded) {
        Ok(()) => {
            tracing::info!(
                path = %superseded.display(),
                replacement = %replacement.display(),
                "foundry reaper: deleted a preserved original after the gate allowed it \
                 and the replacement was re-verified"
            );
            ReapOutcome::Deleted
        }
        Err(e) => ReapOutcome::CouldNotInspect {
            detail: format!("deletion failed: {e}"),
        },
    }
}

/// Run one reap pass over the configured allowed roots.
///
/// `mutate` is the second of the two deliberate steps: with it false — the
/// default — every allowed candidate is reported as [`ReapOutcome::WouldDelete`]
/// and nothing is removed.
pub fn reap(foundry: &Foundry, retention: Duration, mutate: bool) -> ReapRun {
    // The Foundry's OWN config, not one rebuilt by the caller: rebuilding it
    // is a second place the allowed-roots list could be derived, and the
    // reaper must walk exactly the roots the guard would permit.
    let cfg = foundry.config();
    let mut run = ReapRun {
        mutation_enabled: mutate,
        retention_secs: retention.as_secs(),
        ..Default::default()
    };

    let probe = |p: &Path| foundry.probe_file(p);
    for root in &cfg.allowed_roots {
        for superseded in find_superseded(root) {
            let (outcome, replacement, bytes) = decide_one(&probe, &superseded, retention);

            let outcome = if mutate && outcome == ReapOutcome::WouldDelete {
                let deleted = delete_verified(&superseded, replacement.as_deref());
                if deleted == ReapOutcome::Deleted {
                    run.bytes_reclaimed = run.bytes_reclaimed.saturating_add(bytes.unwrap_or(0));
                }
                deleted
            } else {
                outcome
            };

            run.files.push(ReapedFile {
                superseded_path: superseded.display().to_string(),
                replacement_path: replacement
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                bytes,
                outcome,
            });
        }
    }

    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("muse-reaper-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("scratch");
        d
    }

    /// The extension must match the one `forge` actually writes. Two copies of
    /// a magic string is one rename away from a reaper that silently finds
    /// nothing — which would look exactly like "there is nothing to reap".
    #[test]
    fn the_superseded_extension_matches_the_one_forge_writes() {
        let forge_src = include_str!("forge.rs");
        assert!(
            forge_src.contains(&format!("const SUPERSEDED_EXT: &str = \"{SUPERSEDED_EXT}\"")),
            "forge's SUPERSEDED_EXT no longer matches the reaper's"
        );
    }

    #[test]
    fn the_replacement_is_the_name_without_the_extension() {
        assert_eq!(
            replacement_of(Path::new("/lib/Movie/Movie.mkv.muse-superseded")),
            Some(PathBuf::from("/lib/Movie/Movie.mkv"))
        );
        // Not a superseded name at all.
        assert_eq!(replacement_of(Path::new("/lib/Movie/Movie.mkv")), None);
        // Degenerate: nothing left after stripping.
        assert_eq!(replacement_of(Path::new("/lib/.muse-superseded")), None);
    }

    #[test]
    fn the_walk_finds_superseded_files_and_ignores_everything_else() {
        let d = tmp("walk");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a.mkv"), b"x").unwrap();
        fs::write(d.join("a.mkv.muse-superseded"), b"x").unwrap();
        fs::write(d.join("sub").join("b.avi.muse-superseded"), b"x").unwrap();
        fs::write(d.join("sub").join("b.avi"), b"x").unwrap();
        let found = find_superseded(&d);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|p| p.extension().unwrap() == SUPERSEDED_EXT));
        let _ = fs::remove_dir_all(&d);
    }

    /// Symlinks are never followed, and never collected.
    ///
    /// This module DELETES what the walk hands it. A symlink inside an allowed
    /// root can name a file outside every allowed root, so following one would
    /// let the reaper destroy something the path guard would have refused to
    /// even read. Caught by mutation testing: removing the symlink check left
    /// every other test green.
    #[test]
    fn the_walk_never_follows_or_collects_a_symlink() {
        let outside = tmp("symlink-outside");
        let inside = tmp("symlink-inside");
        // A real backup outside the root, and a link to it from inside.
        let target = outside.join("Elsewhere.mkv.muse-superseded");
        fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, inside.join("Linked.mkv.muse-superseded")).unwrap();
        // ...and a symlinked DIRECTORY, which would otherwise be descended.
        std::os::unix::fs::symlink(&outside, inside.join("subdir")).unwrap();

        let found = find_superseded(&inside);
        assert!(
            found.is_empty(),
            "a symlink must never be collected for deletion: {found:?}"
        );
        assert!(target.exists(), "the target must be untouched");
        let _ = fs::remove_dir_all(&inside);
        let _ = fs::remove_dir_all(&outside);
    }

    /// The single most important refusal: if the replacement is gone, the
    /// backup is the ONLY copy of the title.
    #[test]
    fn a_backup_whose_replacement_is_missing_is_never_deleted() {
        let d = tmp("orphan");
        let sup = d.join("Movie.mkv.muse-superseded");
        fs::write(&sup, b"x").unwrap();
        // Deliberately do NOT create Movie.mkv.
        let (outcome, _, _) = decide_one(&no_probe, &sup, Duration::ZERO);
        assert!(
            matches!(outcome, ReapOutcome::ReplacementMissing { .. }),
            "got {outcome:?}"
        );
        assert!(!outcome.would_delete());
        let _ = fs::remove_dir_all(&d);
    }

    /// A backup that is still the same inode as the live file means the swap
    /// never released the original name. Deleting would destroy the live file.
    #[test]
    fn a_backup_still_hardlinked_to_the_live_file_is_never_deleted() {
        let d = tmp("sameinode");
        let live = d.join("Movie.mkv");
        let sup = d.join("Movie.mkv.muse-superseded");
        fs::write(&live, b"x").unwrap();
        fs::hard_link(&live, &sup).unwrap();
        let (outcome, _, _) = decide_one(&no_probe, &sup, Duration::ZERO);
        assert_eq!(outcome, ReapOutcome::SameInodeAsReplacement);
        assert!(!outcome.would_delete());
        let _ = fs::remove_dir_all(&d);
    }

    /// Retention is a real gate, not a label.
    #[test]
    fn a_backup_inside_the_retention_window_is_kept() {
        let d = tmp("young");
        let live = d.join("Movie.mkv");
        let sup = d.join("Movie.mkv.muse-superseded");
        fs::write(&live, b"x").unwrap();
        fs::write(&sup, b"y").unwrap();
        let (outcome, _, _) = decide_one(&no_probe, &sup, Duration::from_secs(3600));
        assert!(matches!(outcome, ReapOutcome::TooYoung { .. }), "got {outcome:?}");
        assert!(!outcome.would_delete());
        let _ = fs::remove_dir_all(&d);
    }

    /// An unprobeable file is never deletable. These fixtures are not media, so
    /// ffprobe fails on them — which is exactly the case being asserted.
    #[test]
    fn a_file_that_cannot_be_probed_is_kept_not_deleted() {
        let d = tmp("unprobeable");
        let live = d.join("Movie.mkv");
        let sup = d.join("Movie.mkv.muse-superseded");
        fs::write(&live, b"not media").unwrap();
        fs::write(&sup, b"not media either").unwrap();
        let (outcome, _, _) = decide_one(&no_probe, &sup, Duration::ZERO);
        assert!(
            matches!(
                outcome,
                ReapOutcome::ProbeFailed { .. } | ReapOutcome::CouldNotInspect { .. }
            ),
            "an unprobeable pair must never be deletable, got {outcome:?}"
        );
        assert!(!outcome.would_delete());
        let _ = fs::remove_dir_all(&d);
    }

    /// The mutate branch, tested for real rather than re-implemented.
    ///
    /// Opus caught the first version at the gate: it never called the deletion
    /// path at all, it restated the condition in the test and asserted on
    /// that. It would have passed with the real branch broken, and left the
    /// only code that destroys data uncovered.
    #[test]
    fn the_delete_path_removes_the_backup_and_leaves_the_replacement() {
        let d = tmp("delete");
        let live = d.join("Movie.mkv");
        let sup = d.join("Movie.mkv.muse-superseded");
        fs::write(&live, b"the replacement").unwrap();
        fs::write(&sup, b"the original").unwrap();

        assert_eq!(delete_verified(&sup, Some(&live)), ReapOutcome::Deleted);
        assert!(!sup.exists(), "the backup must be gone");
        assert!(live.exists(), "the replacement must be untouched");
        assert_eq!(fs::read(&live).unwrap(), b"the replacement");
        let _ = fs::remove_dir_all(&d);
    }

    /// The race the re-verification exists for: the replacement disappears
    /// between the gate allowing and the unlink. Deleting then would destroy
    /// the last copy of the title.
    #[test]
    fn a_replacement_that_vanishes_after_the_gate_allowed_stops_the_deletion() {
        let d = tmp("race");
        let live = d.join("Movie.mkv");
        let sup = d.join("Movie.mkv.muse-superseded");
        fs::write(&live, b"x").unwrap();
        fs::write(&sup, b"the only copy now").unwrap();

        // Simulate the window: the gate has said WouldDelete, and now the
        // replacement goes away before the unlink.
        fs::remove_file(&live).unwrap();

        let outcome = delete_verified(&sup, Some(&live));
        assert!(
            matches!(outcome, ReapOutcome::ReplacementMissing { .. }),
            "got {outcome:?}"
        );
        assert!(sup.exists(), "the last copy must survive");
        let _ = fs::remove_dir_all(&d);
    }

    /// The other half of the race: the replacement is replaced by a hard link
    /// to the backup itself, so unlinking would destroy the live file.
    #[test]
    fn a_replacement_that_becomes_the_same_inode_stops_the_deletion() {
        let d = tmp("race-inode");
        let live = d.join("Movie.mkv");
        let sup = d.join("Movie.mkv.muse-superseded");
        fs::write(&sup, b"x").unwrap();
        fs::hard_link(&sup, &live).unwrap();

        assert_eq!(
            delete_verified(&sup, Some(&live)),
            ReapOutcome::SameInodeAsReplacement
        );
        assert!(sup.exists());
        let _ = fs::remove_dir_all(&d);
    }

    /// Retention must be measured from a clock the hard link does NOT inherit.
    ///
    /// `forge` creates the backup with `hard_link`, which shares the inode, so
    /// the backup carries the ORIGINAL's mtime — the download date, routinely
    /// years old. Measuring from mtime silently reduced a 14-day window to no
    /// window at all. This fixture reproduces that exactly: an old mtime on a
    /// freshly-linked file.
    #[test]
    fn retention_is_not_defeated_by_the_backups_inherited_mtime() {
        let d = tmp("mtime");
        let live = d.join("Movie.mkv");
        fs::write(&live, b"x").unwrap();

        // Backdate the original the way a real download would be.
        let old = SystemTime::now() - Duration::from_secs(365 * 24 * 60 * 60);
        let ft = fs::FileTimes::new().set_modified(old).set_accessed(old);
        fs::File::options()
            .write(true)
            .open(&live)
            .unwrap()
            .set_times(ft)
            .unwrap();

        // Now make the backup the way forge does: a hard link.
        let sup = d.join("Movie.mkv.muse-superseded");
        fs::hard_link(&live, &sup).unwrap();

        // The inherited mtime IS a year old...
        let inherited = fs::metadata(&sup).unwrap().modified().unwrap();
        assert!(
            SystemTime::now().duration_since(inherited).unwrap() > Duration::from_secs(300 * 86_400),
            "fixture must actually have an ancient mtime"
        );

        // ...but the backup was created just now, so retention must hold it.
        let age = age_of(&sup).expect("ctime must be readable");
        assert!(
            age < Duration::from_secs(3600),
            "retention must measure the BACKUP's age ({age:?}), not the inherited mtime"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// The module must contain exactly ONE deletion call site, and it must be
    /// reachable only from the mutate branch. A second one added later would
    /// bypass the gate entirely.
    #[test]
    fn there_is_exactly_one_deletion_call_site_in_this_module() {
        let src = include_str!("reaper.rs");
        let body = src.split("#[cfg(test)]").next().expect("a non-test body");
        assert_eq!(
            body.matches("remove_file").count(),
            1,
            "the reaper must have exactly one deletion call site"
        );
        assert_eq!(body.matches("remove_dir_all").count(), 0);
        // ...and it must sit behind both the mutate flag and a WouldDelete
        // verdict, which are checked together.
        assert!(
            body.contains("if mutate && outcome == ReapOutcome::WouldDelete"),
            "the deletion must be gated on BOTH mutation being enabled and the gate allowing"
        );
    }

    /// The reaper must never form its own opinion about safety.
    #[test]
    fn the_module_defers_to_the_deletion_gate_rather_than_judging_for_itself() {
        let src = include_str!("reaper.rs");
        let body = src.split("#[cfg(test)]").next().expect("a non-test body");
        assert!(body.contains("may_delete_original"), "the gate must be consulted");
        // No local re-derivation of the things the gate checks.
        for forbidden in ["classify_hdr", "classify_dolby_vision", "undetectable_formats"] {
            assert!(
                !body.contains(forbidden),
                "the reaper must not re-derive {forbidden}; that is the gate's job"
            );
        }
    }

    /// A prober that always fails — the honest default for these fixtures,
    /// which are not media. Any test needing a successful probe says so.
    fn no_probe(_: &Path) -> Result<MediaProbe, String> {
        Err("test prober: not media".to_string())
    }
}
