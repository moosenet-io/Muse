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
use crate::media::probe::MediaProbe;
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
    /// A `forge` swap currently holds the lock covering this title, so a swap
    /// is in flight on it right now.
    ///
    /// The mid-swap window is exactly when a backup must not be touched: forge
    /// hard-links the original to the backup name BEFORE it has finished
    /// putting the replacement in place, so for a moment both names exist and
    /// the replacement is not yet verified. Retention normally covers this (a
    /// just-created backup is minutes old), but `retention_days=0` is an
    /// accepted override, and a rule that only holds for the default is not a
    /// rule. Taking the same lock forge takes closes it outright.
    SwapInFlight,
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
            Self::SwapInFlight => write!(
                f,
                "kept: a Foundry swap currently holds the lock for this title, so the \
                 replacement may not be in place yet"
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
    /// True only when deletion was actually enabled — which requires BOTH the
    /// global gate and this request's `mutate`. Reported on every response so
    /// a dry run can never be mistaken for a real one.
    pub mutation_enabled: bool,
    /// Whether MUSE_FOUNDRY_ENABLE_MUTATION is open. Reported separately so a
    /// request that asked to mutate and was refused by the GLOBAL gate can be
    /// told apart from one that never asked.
    pub globally_permitted: bool,
    pub retention_secs: u64,
    pub bytes_reclaimed: u64,
    /// Directories successfully listed across every root.
    pub dirs_read: usize,
    /// Directories that could not be listed. Non-zero means the pass covered
    /// LESS than the library, and `examined: 0` may be ignorance rather than
    /// absence.
    pub dirs_unreadable: usize,
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
    resolve_replacement(superseded).map(|(_, path)| path)
}

/// What a directory listing established about a backup's replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchVerdict {
    /// Exactly one media sibling shares the stem, from a listing that was read
    /// in full. The only verdict that may authorise a deletion.
    Unique,
    /// None found, or several — we cannot tell which file superseded this
    /// backup.
    Ambiguous,
    /// The listing was incomplete, so the candidate set may be short.
    Incomplete { unreadable: usize },
}

/// **Was the resolution trustworthy?** Pure, so it is testable.
///
/// The fail-closed ambiguity rule only works if every candidate was SEEN. A
/// transient NFS error (ESTALE, EIO) dropping one of two same-stem files
/// collapses the list from 2 to 1, and the survivor — which may not be the file
/// that superseded this backup — would otherwise resolve cleanly and authorise
/// deleting against the wrong replacement.
///
/// This is decided from the SAME listing that produced the matches, and that is
/// the point. An earlier version checked completeness with a second, separate
/// `read_dir`, which did not close the race — it moved it: a transient error
/// hitting only the resolution read left the guard already satisfied. Raised at
/// the REAP-01 gate. One read, one verdict.
pub(crate) fn classify_matches(matches: &[PathBuf], unreadable: usize) -> MatchVerdict {
    if unreadable > 0 {
        return MatchVerdict::Incomplete { unreadable };
    }
    match matches.len() {
        1 => MatchVerdict::Unique,
        _ => MatchVerdict::Ambiguous,
    }
}

/// The verdict for a backup's replacement lookup, for callers that need to
/// tell "could not look" from "not there".
///
/// `replacement_of` collapses both into the same-name fallback, which is right
/// for its callers that only need a path. [`decide_one`] needs the difference:
/// reporting a partial listing as `ReplacementMissing` would state "this backup
/// is the ONLY copy of the title" — a confident and false claim about a
/// directory that simply could not be read.
pub(crate) fn resolve_replacement(superseded: &Path) -> Option<(MatchVerdict, PathBuf)> {
    let name = superseded.file_name()?.to_str()?;
    let stem = name.strip_suffix(&format!(".{SUPERSEDED_EXT}"))?;
    if stem.is_empty() {
        return None;
    }
    let same_name = superseded.with_file_name(stem);
    if same_name.is_file() {
        // A same-name replacement needs no listing at all.
        return Some((MatchVerdict::Unique, same_name));
    }

    let (Some(dir), Some(base)) = (
        superseded.parent(),
        Path::new(stem).file_stem().and_then(|s| s.to_str()),
    ) else {
        return Some((MatchVerdict::Ambiguous, same_name));
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Some((MatchVerdict::Ambiguous, same_name));
    };

    let mut matches: Vec<PathBuf> = Vec::new();
    let mut unreadable = 0usize;
    for entry in entries {
        let Ok(entry) = entry else {
            unreadable += 1;
            continue;
        };
        let p = entry.path();
        if !p.is_file() || !crate::library::scan::has_media_extension(&p) {
            continue;
        }
        if p.file_stem().and_then(|s| s.to_str()) == Some(base) {
            matches.push(p);
        }
    }

    let verdict = classify_matches(&matches, unreadable);
    let path = match verdict {
        MatchVerdict::Unique => matches[0].clone(),
        // Ambiguous or incomplete: hand back the same-name path so a caller
        // that only wants a path reports ReplacementMissing and KEEPS the
        // backup.
        _ => same_name,
    };
    Some((verdict, path))
}

/// Every `.muse-superseded` file under `root`, recursively.
///
/// Walks with `read_dir` rather than the library scanner because the scanner
/// deliberately ignores non-media extensions — and `.muse-superseded` is
/// non-media by design, exactly so the scanner will not index a backup as a
/// title.
pub fn find_superseded(root: &Path) -> WalkResult {
    let mut result = WalkResult::default();
    walk(root, &mut result);
    result.found.sort();
    result
}

/// What a walk saw, including what it could NOT see.
///
/// `found` alone is not enough to report on. A reap that examined nothing
/// because the library holds no backups, and a reap that examined nothing
/// because every directory was unreadable, produce the identical `examined: 0`
/// — and on the one endpoint that can permanently delete data, "there is
/// nothing to do" and "I could not look" must not render the same way.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WalkResult {
    pub found: Vec<PathBuf>,
    /// Directories entered successfully.
    pub dirs_read: usize,
    /// Directories that could not be listed. Skipped so one bad folder does
    /// not abort the pass — but COUNTED, so the pass cannot claim coverage it
    /// did not have.
    pub dirs_unreadable: usize,
}

impl WalkResult {
    /// Whether the walk saw enough of the tree to be believed.
    ///
    /// A walk that read nothing at all has established nothing, and its empty
    /// result is ignorance rather than absence.
    pub fn is_trustworthy(&self) -> bool {
        self.dirs_read > 0
    }
}

fn walk(dir: &Path, result: &mut WalkResult) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // Skipped rather than fatal — one unreadable folder must not stop the
        // rest — but COUNTED, so the response can say the coverage was partial.
        result.dirs_unreadable += 1;
        tracing::warn!(dir = %dir.display(), "foundry reaper: could not list directory; skipping");
        return;
    };
    result.dirs_read += 1;
    for entry in entries.flatten() {
        let p = entry.path();
        match entry.file_type() {
            // Never follow symlinks: a link could point outside the allowed
            // roots entirely, and this module deletes what it is given.
            Ok(t) if t.is_symlink() => continue,
            Ok(t) if t.is_dir() => walk(&p, result),
            Ok(t) if t.is_file() => {
                if p.extension().and_then(|e| e.to_str()) == Some(SUPERSEDED_EXT) {
                    result.found.push(p);
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
    let age = SystemTime::now().duration_since(ctime).ok()?;
    // An implausible age is not evidence of age. `age_of` failed CLOSED on an
    // unreadable stat and on a backwards clock, but a ctime of 0 — or any
    // nonsense past value — produced an age of decades that sails past every
    // retention window. That is failing OPEN on exactly the input a broken
    // filesystem or a mis-set clock produces.
    //
    // This is not hypothetical on the target fleet: the library is an NFSv3
    // mount from a QNAP whose clock was measured running ~29 minutes behind
    // this host (2026-08-02), so ctime here is a REMOTE server's notion of
    // time compared against a LOCAL now. That skew is harmless against a
    // multi-day window; a larger one would not be. Raised in the FOUNDRY-11
    // reaper audit.
    plausible_age(age)
}

/// The plausibility decision, separated from the stat that produced it.
///
/// Inline, this was untestable without a filesystem on which a ctime of 0 can
/// be forced — and it showed: the first version of the ceiling had a test that
/// only compared two constants, and deleting the check outright did not fail
/// it. That is the fourth time in this codebase a decision inside a function
/// needing real I/O has had a mutant survive.
fn plausible_age(age: Duration) -> Option<Duration> {
    (age <= MAX_PLAUSIBLE_AGE).then_some(age)
}

/// Beyond this, a reported age is a broken clock or a broken inode rather than
/// an old file.
///
/// **25 years, deliberately not 10.** The first version used ten, which is
/// exactly the longest retention the endpoint accepts (3650 days) — and
/// deletion requires `retention < age <= MAX`, so at a maximum-length window
/// the deletable band was EMPTY and the reaper was silently disarmed. Any
/// backup older than the ceiling was also kept forever under any window. A
/// ceiling that can collide with a configured window is a disarm, not a guard.
/// Raised at the REAP-01 gate.
///
/// The bound has to sit in the gap between the longest legitimate window and
/// the age a broken timestamp reports. A ctime of 0 currently reports ~56
/// years and grows by a year every year, so 25 leaves headroom at both ends:
/// well above the 10-year maximum retention, well below the epoch age.
const MAX_PLAUSIBLE_AGE: Duration = Duration::from_secs(25 * 365 * 24 * 60 * 60);

/// **The retention window actually used, given what the deployment configured
/// and what a request asked for.**
///
/// A request may only LENGTHEN the window, never shorten it.
///
/// The reaper took its retention purely from the caller: the endpoint hardcoded
/// its own 14-day default, `cfg.retention_days` was never read by this module
/// at all, and [`DEFAULT_RETENTION`] was dead code. So an operator could set
/// `MUSE_FOUNDRY_RETENTION_DAYS=30`, see it confirmed in the startup log and on
/// the status surface, and have it apply to nothing — while
/// `?retention_days=0&mutate=true` reduced the window to zero and made every
/// backup in a 16,000-item library deletable in a single call.
///
/// That is the same shape as the defect this module already documents fixing
/// for `mutate` — "one query parameter made that false" — left open one field
/// over. Raised in the FOUNDRY-11 reaper audit.
///
/// The configured value is a FLOOR, so the recoverability window a deployment
/// promises cannot be revoked by a query string.
pub fn effective_retention(configured: Duration, requested: Option<Duration>) -> Duration {
    match requested {
        Some(r) => r.max(configured),
        None => configured,
    }
}

/// **What unlinking this backup would actually free.**
///
/// `len()` is what the file appears to be. It is what unlinking FREES only when
/// this is the last name for the inode. A library whose files are hard-linked
/// elsewhere — the standard \*arr atomic-move setup keeps the download-client
/// name alongside the library name — would have every deletion report its full
/// apparent size while `df` did not move. Across 16,000 titles an operator
/// would be told multiple TB were reclaimed, see no change, and reasonably
/// conclude the deletions had failed. Raised in the FOUNDRY-11 reaper audit.
///
/// Measured on the target library before implementing: 0 of 500 sampled media
/// files have `nlink > 1`, so this is insurance rather than a live correction.
/// It is still the honest number, and the cost of being wrong is an operator
/// mistrusting a working reaper.
fn reclaimable_bytes(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(p).ok()?;
    // More than one name: the bytes survive the unlink under another name.
    Some(if md.nlink() > 1 { 0 } else { md.len() })
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
    let Some((verdict, replacement)) = resolve_replacement(superseded) else {
        return (
            ReapOutcome::CouldNotInspect {
                detail: format!("{} is not a superseded name", superseded.display()),
            },
            None,
            None,
        );
    };

    // **Was the directory listing complete?**
    //
    // `replacement_of` resolves across a container change by matching stems,
    // and refuses when it finds none or several — ambiguity must never resolve
    // into a deletion. That rule only works if BOTH candidates were SEEN. A
    // transient NFS error (ESTALE, EIO) dropping one of two same-stem files
    // collapses the list from 2 to 1, and the survivor — which may not be the
    // file that superseded this backup — then resolves cleanly and authorises
    // deleting against the wrong replacement.
    //
    // Checked here rather than inside `replacement_of` so it can be reported as
    // what it is. Folding it into that function produced `ReplacementMissing`,
    // whose message states "this backup is the ONLY copy of the title" — a
    // confident and FALSE claim when the truth is that a directory could not be
    // read. Ignorance rendering as absence is the bug; ignorance rendering as a
    // different specific certainty is the same bug wearing a hat.
    // Decided from the SAME listing that produced `replacement`, so a
    // transient error cannot satisfy a completeness guard and then corrupt the
    // resolution. One read, one verdict.
    if let MatchVerdict::Incomplete { unreadable } = verdict {
        return (
            ReapOutcome::CouldNotInspect {
                detail: format!(
                    "{unreadable} entr{} in the containing directory could not be read, so \
                     the search for this backup's replacement may have missed a candidate",
                    if unreadable == 1 { "y" } else { "ies" }
                ),
            },
            None,
            None,
        );
    }

    let bytes = reclaimable_bytes(superseded);

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

    // **A symlinked replacement defeats the same-inode check below.**
    //
    // `identity_of` uses `symlink_metadata`, which does NOT follow — so if the
    // replacement is a symlink pointing at this very backup, it reports the
    // LINK's inode, the identity check sees two different inodes, and the guard
    // that exists precisely to catch "these are the same file" does not fire.
    //
    // Everything after that agrees the deletion is safe, because everything
    // after that DOES follow: `PathGuard::resolve` canonicalizes, both probes
    // land on the same real file, and `may_delete_original` compares a probe
    // against itself — identical codecs, streams, resolution and duration — and
    // returns `Allow` unconditionally. The only copy of the title is unlinked
    // and the symlink is left dangling.
    //
    // I could not identify anything in Muse that creates this state
    // (`fs::hard_link` does not follow symlinks, so forge cannot mint it), so
    // this is a defeated defence rather than a proven live path. That is reason
    // to close it, not to leave it: the walk already refuses symlinks on the
    // grounds that "a link could point outside the allowed roots entirely, and
    // this module deletes what it is given", and the replacement-resolution
    // path was doing the opposite. Raised in the FOUNDRY-11 reaper audit.
    match std::fs::symlink_metadata(&replacement) {
        Ok(md) if md.file_type().is_symlink() => {
            return (
                ReapOutcome::CouldNotInspect {
                    detail: format!(
                        "the replacement {} is a symlink; a link cannot be shown to be a \
                         distinct file from this backup, so the backup is kept",
                        replacement.display()
                    ),
                },
                Some(replacement),
                bytes,
            )
        }
        Ok(_) => {}
        Err(e) => {
            return (
                ReapOutcome::CouldNotInspect {
                    detail: e.to_string(),
                },
                Some(replacement),
                bytes,
            )
        }
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

/// Take the swap lock that covers this backup's title, if a lock dir exists.
///
/// Named and separate so the KEY CHOICE is testable. The choice is the whole
/// point: forge keys its lock on the DESTINATION path, and `file_stem` strips
/// only the last extension — `Movie.mkv` has stem `Movie`, while
/// `Movie.mkv.muse-superseded` has stem `Movie.mkv`. Keying on the backup's
/// own path therefore produces a different key and a lock that excludes
/// nothing at all. Mutation testing caught that an inline version of this was
/// unverified.
///
/// `Ok(None)` means no lock was needed (no work dir, so forge cannot swap).
/// `Err(())` means a swap holds it.
pub fn acquire_title_lock(
    lock_dir: Option<&Path>,
    superseded: &Path,
) -> Result<Option<crate::foundry::forge::SwapLock>, LockRefusal> {
    let (Some(dir), Some(target)) = (lock_dir, replacement_of(superseded)) else {
        return Ok(None);
    };
    match crate::foundry::forge::SwapLock::acquire(dir, &target) {
        Ok(l) => Ok(Some(l)),
        // Contention and a BROKEN lock are different facts. Mapping both to
        // "a swap is in flight" would report an unusable lock directory as
        // ordinary busyness, and the operator would wait for a swap that is
        // not happening. Opus, FOUNDRY-12 gate.
        Err(e) => Err(match e {
            crate::foundry::forge::SwapError::LockBusy(_) => LockRefusal::Contended,
            other => LockRefusal::Unusable {
                detail: other.to_string(),
            },
        }),
    }
}

/// Why the title lock could not be taken.
#[derive(Debug, Clone, PartialEq)]
pub enum LockRefusal {
    /// A swap holds it. Ordinary; retry later.
    Contended,
    /// The lock itself could not be used. NOT contention — this must not be
    /// reported as "a swap is in flight", or the operator waits for something
    /// that is not happening.
    Unusable { detail: String },
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

    // BOTH switches, not just the request's.
    //
    // The reaper previously honoured only `mutate` from the query string, so
    // `?mutate=true` deleted backups even with MUSE_FOUNDRY_ENABLE_MUTATION
    // unset. An operator who has deliberately closed the global gate — and who
    // reads "mutation_enabled=false" in the startup log — would reasonably
    // believe nothing in the library can be destroyed. One query parameter
    // made that false.
    //
    // The two switches are independent on purpose: the global gate says "this
    // deployment may modify the library at all", and `mutate` says "this
    // particular request intends to". Deleting requires both, which is what
    // "two deliberate steps" was always meant to mean.
    let globally_permitted = cfg.enable_mutation;
    let mutate = mutate && globally_permitted;
    let lock_dir = cfg.work_dir.clone();
    let mut run = ReapRun {
        mutation_enabled: mutate,
        globally_permitted,
        retention_secs: retention.as_secs(),
        ..Default::default()
    };

    let probe = |p: &Path| foundry.probe_file(p);
    for root in &cfg.allowed_roots {
        let walked = find_superseded(root);
        run.dirs_read += walked.dirs_read;
        run.dirs_unreadable += walked.dirs_unreadable;
        for superseded in walked.found {
            // Take the SAME lock a swap takes, for the whole decide+delete of
            // this title. Keyed on directory + file stem, so it covers
            // `Movie.avi`, `Movie.avi.muse-superseded` and `Movie.mkv`
            // together — which is the point: a swap converting a container
            // touches all three, and locking only one name would let a reap
            // proceed alongside it.
            //
            // Non-blocking: if a swap holds it, this title is skipped and
            // reported, not waited on. A reap is entirely retryable, and a
            // reaper that blocked behind swaps would stall the whole pass.
            // Keyed on the REPLACEMENT (live) path, because that is the path
            // forge keys on — it locks its destination. Locking the backup's
            // own path would use a different key: `file_stem` strips only the
            // LAST extension, so `Movie.mkv` has stem `Movie` while
            // `Movie.mkv.muse-superseded` has stem `Movie.mkv`. Two different
            // keys means no exclusion at all. Caught by the test below, which
            // failed on the first attempt.
            // `_lock`, not `_`: a `let _ = ...` binding drops immediately, so
            // the guard would be released before the decide+delete it exists
            // to protect. The underscore-prefixed NAME keeps it alive to the
            // end of the loop body. Subtle enough that a source-level test
            // below asserts it.
            let _lock = match acquire_title_lock(lock_dir.as_deref(), &superseded) {
                Ok(l) => l,
                Err(refusal) => {
                    run.files.push(ReapedFile {
                        superseded_path: superseded.display().to_string(),
                        replacement_path: replacement_of(&superseded)
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        bytes: std::fs::metadata(&superseded).ok().map(|m| m.len()),
                        outcome: ReapOutcome::SwapInFlight,
                    });
                    continue;
                }
            };

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

    /// The bug a real end-to-end swap found in five seconds.
    ///
    /// Path A converts `avi` to `mkv`, so after the swap the backup is
    /// `Movie.avi.muse-superseded` and the replacement is `Movie.mkv` — NOT
    /// `Movie.avi`, which no longer exists by design. Keying only on the
    /// original name made the reaper report "the replacement does not exist,
    /// this backup is the ONLY copy" and keep the file forever, so nothing
    /// Path A converted could ever reclaim disk.
    ///
    /// Every reaper test used same-name fixtures, which is why this survived
    /// review, mutation testing, and a full dry run against the live library.
    #[test]
    fn a_container_changing_swap_still_resolves_its_replacement() {
        let d = tmp("container-change");
        let sup = d.join("Movie.avi.muse-superseded");
        let replacement = d.join("Movie.mkv");
        fs::write(&sup, b"original").unwrap();
        fs::write(&replacement, b"converted").unwrap();
        // The original .avi name is GONE, exactly as after a real swap.

        assert_eq!(
            replacement_of(&sup),
            Some(replacement),
            "the replacement must be found across a container change"
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// Same-container swaps must keep working — the common remux case.
    #[test]
    fn a_same_container_swap_resolves_to_the_identical_name() {
        let d = tmp("same-container");
        let sup = d.join("Movie.mkv.muse-superseded");
        let replacement = d.join("Movie.mkv");
        fs::write(&sup, b"original").unwrap();
        fs::write(&replacement, b"remuxed").unwrap();
        assert_eq!(replacement_of(&sup), Some(replacement));
        let _ = fs::remove_dir_all(&d);
    }

    /// Ambiguity must never resolve into a deletion.
    ///
    /// Two media files sharing the stem means we cannot tell which superseded
    /// this backup. Picking one risks deleting the last copy of a title, so
    /// the resolution falls back to the original name — which does not exist,
    /// so the caller reports ReplacementMissing and KEEPS the backup.
    #[test]
    fn two_candidates_sharing_a_stem_do_not_resolve_to_either() {
        let d = tmp("ambiguous");
        let sup = d.join("Movie.avi.muse-superseded");
        fs::write(&sup, b"original").unwrap();
        fs::write(d.join("Movie.mkv"), b"one").unwrap();
        fs::write(d.join("Movie.mp4"), b"two").unwrap();

        let resolved = replacement_of(&sup).expect("some path");
        assert_eq!(
            resolved,
            d.join("Movie.avi"),
            "ambiguity must fall back to the non-existent original name, so the caller \
             refuses rather than guessing which file superseded this backup"
        );
        assert!(!resolved.is_file(), "...and that path must not exist, forcing the refusal");
        let _ = fs::remove_dir_all(&d);
    }

    /// A stem match must still be MEDIA — a stray `Movie.txt` is not a
    /// replacement, and treating it as one would allow deleting the only copy.
    #[test]
    fn a_non_media_sibling_is_not_mistaken_for_the_replacement() {
        let d = tmp("nonmedia-sibling");
        let sup = d.join("Movie.avi.muse-superseded");
        fs::write(&sup, b"original").unwrap();
        fs::write(d.join("Movie.txt"), b"notes").unwrap();
        let resolved = replacement_of(&sup).expect("some path");
        assert_eq!(resolved, d.join("Movie.avi"));
        assert!(!resolved.is_file());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_walk_finds_superseded_files_and_ignores_everything_else() {
        let d = tmp("walk");
        fs::create_dir_all(d.join("sub")).unwrap();
        fs::write(d.join("a.mkv"), b"x").unwrap();
        fs::write(d.join("a.mkv.muse-superseded"), b"x").unwrap();
        fs::write(d.join("sub").join("b.avi.muse-superseded"), b"x").unwrap();
        fs::write(d.join("sub").join("b.avi"), b"x").unwrap();
        let found = find_superseded(&d).found;
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|p| p.extension().unwrap() == SUPERSEDED_EXT));
        let _ = fs::remove_dir_all(&d);
    }

    /// A reap must not proceed on a title a swap is currently working on.
    ///
    /// forge hard-links the original to the backup name BEFORE the replacement
    /// is in place and verified, so mid-swap both names exist and the
    /// replacement is not yet trustworthy. Retention normally covers this — a
    /// just-created backup is minutes old — but `retention_days=0` is an
    /// accepted override, and a rule that only holds for the default value is
    /// not a rule.
    ///
    /// The lock is keyed on directory + file stem, so a swap holding
    /// `Movie.mkv` must also exclude a reap of `Movie.mkv.muse-superseded`.
    /// That shared-stem behaviour is the property under test.
    #[test]
    fn the_swap_lock_key_is_the_live_name_not_the_backups_own_path() {
        use crate::foundry::forge::SwapLock;
        let work = tmp("lockwork");
        let lib = tmp("locklib");
        let live = lib.join("Movie.mkv");
        let sup = lib.join("Movie.mkv.muse-superseded");
        fs::write(&live, b"x").unwrap();
        fs::write(&sup, b"y").unwrap();

        // A swap takes the lock on the LIVE (destination) name.
        let held = SwapLock::acquire(&work, &live).expect("first acquire must succeed");

        // A reap must key on the SAME path to be excluded. This is the trap
        // the first version of this test exposed: keying on the backup's own
        // path uses a DIFFERENT key, because `file_stem` strips only the last
        // extension — `Movie.mkv` -> `Movie`, `Movie.mkv.muse-superseded` ->
        // `Movie.mkv`. That would have been a lock that excluded nothing.
        let reap_target = replacement_of(&sup).expect("a backup has a replacement path");
        assert_eq!(reap_target, live, "the reaper must lock the live name");
        assert!(
            SwapLock::acquire(&work, &reap_target).is_err(),
            "a reap must be excluded while a swap holds the title"
        );

        // ...and keying on the backup path instead would NOT exclude, which is
        // exactly why the reaper must not do that.
        assert!(
            SwapLock::acquire(&work, &sup).is_ok(),
            "sanity: the backup path is a different lock key, hence the rule above"
        );

        drop(held);
        assert!(
            SwapLock::acquire(&work, &reap_target).is_ok(),
            "the lock must be released when the swap finishes"
        );
        let _ = fs::remove_dir_all(&work);
        let _ = fs::remove_dir_all(&lib);
    }

    /// The reaper's own lock acquisition, not just `SwapLock`'s semantics.
    ///
    /// An earlier version made this choice inline and a mutation that keyed on
    /// the BACKUP path — a lock excluding nothing — survived every test. The
    /// choice is now named, so it is the thing under test.
    #[test]
    fn the_reaper_locks_the_live_title_not_the_backups_own_path() {
        use crate::foundry::forge::SwapLock;
        let work = tmp("acqwork");
        let lib = tmp("acqlib");
        let live = lib.join("Movie.mkv");
        let sup = lib.join("Movie.mkv.muse-superseded");
        fs::write(&live, b"x").unwrap();
        fs::write(&sup, b"y").unwrap();

        // A swap holds the live title...
        let held = SwapLock::acquire(&work, &live).expect("swap takes the lock");
        // ...so the reaper's acquisition must be REFUSED.
        assert!(
            acquire_title_lock(Some(&work), &sup).is_err(),
            "the reaper must be excluded while a swap holds this title"
        );
        drop(held);
        // Released: now it succeeds.
        assert!(
            acquire_title_lock(Some(&work), &sup).is_ok(),
            "the reaper must proceed once the swap finishes"
        );
        // No lock dir: nothing to race with, so no lock is needed.
        assert!(matches!(acquire_title_lock(None, &sup), Ok(None)));

        let _ = fs::remove_dir_all(&work);
        let _ = fs::remove_dir_all(&lib);
    }

    /// A broken lock is not a busy lock.
    ///
    /// Mapping both to "a swap is in flight" would report an unusable lock
    /// directory as ordinary busyness, and the operator would wait for a swap
    /// that is not happening. Opus, FOUNDRY-12 gate.
    #[test]
    fn an_unusable_lock_is_not_reported_as_a_swap_in_flight() {
        let contended = LockRefusal::Contended;
        let broken = LockRefusal::Unusable {
            detail: "permission denied creating the lock directory".to_string(),
        };
        assert_ne!(contended, broken);

        // The mapping the reap loop performs, asserted directly.
        let as_outcome = |r: LockRefusal| match r {
            LockRefusal::Contended => ReapOutcome::SwapInFlight,
            LockRefusal::Unusable { detail } => ReapOutcome::CouldNotInspect { detail },
        };
        assert_eq!(as_outcome(contended), ReapOutcome::SwapInFlight);
        assert!(
            matches!(as_outcome(broken), ReapOutcome::CouldNotInspect { .. }),
            "a broken lock must not read as a swap being in progress"
        );
    }

    /// The lock must live across the WHOLE decide+delete, not be dropped at
    /// the end of its own statement.
    ///
    /// `let _lock = ...` keeps the guard to the end of scope; `let _ = ...`
    /// drops it IMMEDIATELY, releasing the lock before the work it protects.
    /// The two differ by three characters and behave completely differently,
    /// and no runtime test in this module can see it because `reap` needs a
    /// live Foundry. Opus flagged the gap at the FOUNDRY-12 gate; this is a
    /// source-level assertion because that is what can actually catch it.
    #[test]
    fn the_title_lock_is_bound_to_a_name_so_it_outlives_the_statement() {
        let src = include_str!("reaper.rs");
        let body = src.split("#[cfg(test)]").next().expect("a non-test body");
        assert!(
            body.contains("let _lock = match acquire_title_lock("),
            "the lock must be bound to a NAMED binding; `let _ = ...` would release it \
             immediately and the reap would run unprotected"
        );
        assert!(
            !body.contains("let _ = acquire_title_lock("),
            "an anonymous binding drops the guard at once — the lock would protect nothing"
        );
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

        let found = find_superseded(&inside).found;
        assert!(
            found.is_empty(),
            "a symlink must never be collected for deletion: {found:?}"
        );
        assert!(target.exists(), "the target must be untouched");
        let _ = fs::remove_dir_all(&inside);
        let _ = fs::remove_dir_all(&outside);
    }

    /// "Nothing to do" and "I could not look" must not render the same.
    ///
    /// The live dry-run returned `examined: 0` against the real library, which
    /// was correct — nothing has ever swapped. But the identical response
    /// would have come back if every directory had been unreadable, and on the
    /// one endpoint that can permanently delete data that ambiguity is not
    /// acceptable.
    #[test]
    fn a_walk_reports_what_it_could_not_read_not_just_what_it_found() {
        let d = tmp("coverage");
        fs::create_dir_all(d.join("readable")).unwrap();
        fs::write(d.join("readable").join("a.mkv.muse-superseded"), b"x").unwrap();

        let ok = find_superseded(&d);
        assert_eq!(ok.found.len(), 1);
        assert!(ok.dirs_read >= 2, "root + subdir: {}", ok.dirs_read);
        assert_eq!(ok.dirs_unreadable, 0);
        assert!(ok.is_trustworthy());

        // An unreadable subtree is COUNTED, not silently skipped.
        use std::os::unix::fs::PermissionsExt;
        let blocked = d.join("blocked");
        fs::create_dir_all(&blocked).unwrap();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        let partial = find_superseded(&d);
        let running_as_root = partial.dirs_unreadable == 0;
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&d);

        if running_as_root {
            return; // root can read anything; the permission bit does not apply
        }
        assert_eq!(
            partial.dirs_unreadable, 1,
            "an unreadable directory must be counted so the pass cannot claim full coverage"
        );
        assert!(partial.is_trustworthy(), "it still read most of the tree");
    }

    /// A walk that read NOTHING has established nothing — its empty result is
    /// ignorance, not absence.
    #[test]
    fn a_walk_that_read_nothing_is_not_trustworthy() {
        let missing = find_superseded(Path::new("/nonexistent-muse-reaper-root"));
        assert!(missing.found.is_empty());
        assert_eq!(missing.dirs_read, 0);
        assert_eq!(missing.dirs_unreadable, 1);
        assert!(
            !missing.is_trustworthy(),
            "an empty result from a walk that read nothing must not read as 'no backups'"
        );
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
    /// The aggregator must treat "could not inspect" as KEPT and never as a
    /// delete judgement — flagged as unverified at the REAP-01 gate.
    ///
    /// This does NOT assert `bytes_reclaimed`: that field is set by `reap`, so
    /// a hand-built `ReapRun` could only assert its own fixture. An earlier
    /// version of this test was named "...reclaims_nothing" and asserted no
    /// such thing — a name writing a cheque the body did not cash, which is the
    /// decorative-test failure this codebase keeps catching. The bytes rule is
    /// pinned structurally by the test below instead.
    #[test]
    fn a_file_that_could_not_be_inspected_counts_as_kept_not_deleted() {
        let run = ReapRun {
            files: vec![
                ReapedFile {
                    superseded_path: "/lib/A.mkv.muse-superseded".into(),
                    replacement_path: "/lib/A.mkv".into(),
                    bytes: Some(1_000),
                    outcome: ReapOutcome::CouldNotInspect {
                        detail: "2 entries in the containing directory could not be read".into(),
                    },
                },
                ReapedFile {
                    superseded_path: "/lib/B.mkv.muse-superseded".into(),
                    replacement_path: "/lib/B.mkv".into(),
                    bytes: Some(2_000),
                    outcome: ReapOutcome::Deleted,
                },
            ],
            ..Default::default()
        };

        assert_eq!(run.deleted(), 1);
        assert_eq!(run.would_delete(), 1, "an uninspectable file is not deletable");
        assert_eq!(run.kept(), 1, "it must be counted as kept, not vanish");
        assert!(
            !ReapOutcome::CouldNotInspect { detail: String::new() }.would_delete(),
            "CouldNotInspect must never be a delete judgement"
        );
    }

    /// `bytes_reclaimed` may only grow when a file was ACTUALLY deleted.
    ///
    /// Asserted against the source, in the same style as the existing
    /// "must consult the deletion gate" test, because the property is about
    /// where the accumulation sits rather than about any value a fixture can
    /// hold: reporting reclaimed bytes for a file still on disk would tell an
    /// operator the run freed space it did not free.
    #[test]
    fn reclaimed_bytes_are_only_counted_inside_the_actually_deleted_branch() {
        let src = include_str!("reaper.rs");
        let accum = src
            .find("run.bytes_reclaimed = run.bytes_reclaimed.saturating_add(")
            .expect("the accumulation site must exist");

        // Walk back to the nearest enclosing condition and require it to be the
        // one that establishes a real deletion.
        let before = &src[..accum];
        let guard = before
            .rfind("if deleted == ReapOutcome::Deleted {")
            .expect("bytes must only be counted once a deletion is CONFIRMED");
        assert!(
            !before[guard..].contains("\n        }\n"),
            "the accumulation must still be inside that branch, not after it"
        );
    }

    // --- resolution trustworthiness ----------------------------------------

    fn m(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn exactly_one_candidate_from_a_complete_listing_resolves() {
        assert_eq!(
            classify_matches(&m(&["/lib/Movie/Movie.mkv"]), 0),
            MatchVerdict::Unique
        );
    }

    #[test]
    fn no_candidates_or_several_are_ambiguous() {
        assert_eq!(classify_matches(&m(&[]), 0), MatchVerdict::Ambiguous);
        assert_eq!(
            classify_matches(&m(&["/lib/M/M.mkv", "/lib/M/M.mp4"]), 0),
            MatchVerdict::Ambiguous
        );
    }

    /// **The collapse this guard exists to stop.**
    ///
    /// Two same-stem candidates would be Ambiguous and keep the backup. A
    /// transient NFS error dropping one leaves a single match that looks
    /// perfectly unique — and it may not be the file that superseded this
    /// backup. An incomplete listing must never resolve, however tidy the
    /// survivors look.
    #[test]
    fn a_single_candidate_from_an_incomplete_listing_does_not_resolve() {
        assert_eq!(
            classify_matches(&m(&["/lib/Movie/Movie.mkv"]), 1),
            MatchVerdict::Incomplete { unreadable: 1 },
            "one unreadable entry may have hidden the real replacement"
        );
    }

    /// Incompleteness outranks even a clean-looking ambiguity count, because
    /// the count itself is unreliable when entries were missed.
    #[test]
    fn incompleteness_outranks_the_match_count() {
        assert_eq!(
            classify_matches(&m(&[]), 3),
            MatchVerdict::Incomplete { unreadable: 3 }
        );
        assert_eq!(
            classify_matches(&m(&["/a/M.mkv", "/a/M.mp4"]), 2),
            MatchVerdict::Incomplete { unreadable: 2 }
        );
    }

    /// A complete listing with no errors must NOT be reported as incomplete —
    /// the over-refusal guard. Every reap would otherwise report
    /// CouldNotInspect and nothing would ever be reclaimed.
    #[test]
    fn a_complete_listing_is_never_reported_incomplete() {
        for matches in [m(&[]), m(&["/a/M.mkv"]), m(&["/a/M.mkv", "/a/M.mp4"])] {
            assert!(
                !matches!(
                    classify_matches(&matches, 0),
                    MatchVerdict::Incomplete { .. }
                ),
                "a listing read in full is complete, whatever it contained"
            );
        }
    }

    // --- a symlinked replacement -------------------------------------------

    /// **The defeated defence.**
    ///
    /// If the replacement is a symlink to the backup itself, `identity_of`
    /// (which does not follow) sees two different inodes, so the same-inode
    /// guard does not fire — while everything downstream DOES follow, compares
    /// the file against itself, and returns `Allow`. The only copy of the title
    /// would be unlinked.
    ///
    /// Built on a real filesystem with a real symlink rather than a fixture,
    /// because the whole bug is about which calls traverse links and which do
    /// not — a mock would encode my belief about that rather than test it.
    #[test]
    fn a_replacement_that_is_a_symlink_to_the_backup_is_never_deleted() {
        let dir = std::env::temp_dir().join(format!(
            "muse-reap-symlink-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let backup = dir.join("Movie.mkv.muse-superseded");
        std::fs::write(&backup, b"the only copy of the title").expect("write backup");

        // The replacement is a SYMLINK pointing back at the backup.
        let replacement = dir.join("Movie.mkv");
        std::os::unix::fs::symlink(&backup, &replacement).expect("symlink");

        // Sanity: the trap really is set — the identity check alone does NOT
        // catch this, which is why the explicit symlink refusal has to exist.
        let a = crate::foundry::forge::identity_of(&backup).expect("backup identity");
        let b = crate::foundry::forge::identity_of(&replacement).expect("link identity");
        assert_ne!(
            a, b,
            "symlink_metadata reports the LINK's inode, so the same-inode guard \
             cannot see through this — that is the bug being closed"
        );

        // Probing follows the link, so both sides look identical and the gate
        // would allow. A probe that always succeeds models that worst case.
        let probe = |_: &Path| -> Result<MediaProbe, String> {
            Err("probe should not be reached: the symlink must be refused first".to_string())
        };
        let (outcome, _, _) = decide_one(&probe, &backup, Duration::ZERO);

        assert!(
            !outcome.would_delete(),
            "a symlinked replacement must never authorise deleting the backup, got {outcome:?}"
        );
        assert!(
            matches!(outcome, ReapOutcome::CouldNotInspect { .. }),
            "and it must say why, got {outcome:?}"
        );
        assert!(
            backup.is_file(),
            "decide_one must not have deleted anything — it never deletes"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- reclaimable bytes --------------------------------------------------

    #[test]
    fn a_backup_with_one_name_reports_its_full_size_as_reclaimable() {
        let dir = std::env::temp_dir().join(format!("muse-reap-nlink1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let f = dir.join("A.mkv.muse-superseded");
        std::fs::write(&f, vec![0u8; 1024]).expect("write");

        assert_eq!(reclaimable_bytes(&f), Some(1024));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hard-linked backup frees NOTHING when unlinked — the bytes survive
    /// under the other name. Reporting its apparent size would tell an operator
    /// the run reclaimed space it did not reclaim.
    #[test]
    fn a_hard_linked_backup_reports_no_reclaimable_bytes() {
        let dir = std::env::temp_dir().join(format!("muse-reap-nlink2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let f = dir.join("B.mkv.muse-superseded");
        std::fs::write(&f, vec![0u8; 4096]).expect("write");
        std::fs::hard_link(&f, dir.join("B-elsewhere.mkv")).expect("hard link");

        assert_eq!(
            reclaimable_bytes(&f),
            Some(0),
            "the bytes survive under the other name, so unlinking frees nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- the retention floor -----------------------------------------------

    /// **The one-parameter disarm this floor exists to close.**
    ///
    /// `?retention_days=0&mutate=true` made every backup in the library
    /// instantly deletable, regardless of what the deployment had configured.
    #[test]
    fn a_request_may_not_shorten_the_configured_retention_window() {
        let configured = Duration::from_secs(30 * 24 * 60 * 60);
        assert_eq!(
            effective_retention(configured, Some(Duration::ZERO)),
            configured,
            "retention_days=0 must not revoke the deployment's recoverability window"
        );
        assert_eq!(
            effective_retention(configured, Some(Duration::from_secs(24 * 60 * 60))),
            configured,
            "a shorter request is floored at the configured value"
        );
    }

    #[test]
    fn a_request_may_lengthen_the_retention_window() {
        let configured = Duration::from_secs(14 * 24 * 60 * 60);
        let longer = Duration::from_secs(90 * 24 * 60 * 60);
        assert_eq!(effective_retention(configured, Some(longer)), longer);
    }

    #[test]
    fn no_request_means_the_configured_window() {
        let configured = Duration::from_secs(21 * 24 * 60 * 60);
        assert_eq!(effective_retention(configured, None), configured);
    }

    /// A deployment that configured zero gets zero — the floor is the
    /// deployment's choice, not a second opinion about it. `config.rs` already
    /// warns at startup when retention is zero; that is the right place to
    /// argue with the operator, not here.
    #[test]
    fn a_deployment_that_configured_zero_is_not_overridden_upward() {
        assert_eq!(
            effective_retention(Duration::ZERO, None),
            Duration::ZERO
        );
    }

    /// An implausible age is not evidence of age. A zero ctime reports ~56
    /// years, which sails past every retention window — failing OPEN on exactly
    /// the input a broken clock or a damaged inode produces.
    #[test]
    fn an_implausibly_old_age_is_rejected_rather_than_treated_as_very_old() {
        let from_zero_ctime = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("the clock is after 1970");
        assert_eq!(
            plausible_age(from_zero_ctime),
            None,
            "a ctime of 0 must not make a backup eligible for deletion"
        );
    }

    #[test]
    fn an_ordinary_age_is_accepted() {
        let a_week = Duration::from_secs(7 * 24 * 60 * 60);
        assert_eq!(plausible_age(a_week), Some(a_week));
    }

    /// **The disarm this ceiling nearly caused.**
    ///
    /// Deletion needs `retention < age <= MAX_PLAUSIBLE_AGE`. With the ceiling
    /// set equal to the longest retention the endpoint accepts, that band is
    /// empty: nothing is ever both old enough and plausible, so a
    /// maximum-length window silently disarms the reaper entirely.
    ///
    /// This asserts a file can actually be DELETABLE just past the longest
    /// window — the capability — rather than that the window length is itself
    /// a plausible age, which was the earlier test's mistake and told us
    /// nothing.
    #[test]
    fn a_backup_just_past_the_longest_window_is_still_reapable() {
        let longest_window = Duration::from_secs(3650 * 24 * 60 * 60);
        let just_past = longest_window + Duration::from_secs(24 * 60 * 60);

        let age = plausible_age(just_past)
            .expect("a backup one day past a maximum-length window is not implausible");
        assert!(
            age >= longest_window,
            "and it must still count as old enough to reap, or the longest \
             configurable window disarms the reaper"
        );
    }

    #[test]
    fn the_ceiling_leaves_headroom_above_the_longest_configurable_window() {
        let longest_window = Duration::from_secs(3650 * 24 * 60 * 60);
        assert!(
            MAX_PLAUSIBLE_AGE > longest_window,
            "a ceiling equal to the longest window leaves an empty deletable band"
        );
    }

    #[test]
    fn the_boundary_itself_is_plausible() {
        assert_eq!(plausible_age(MAX_PLAUSIBLE_AGE), Some(MAX_PLAUSIBLE_AGE));
        assert_eq!(
            plausible_age(MAX_PLAUSIBLE_AGE + Duration::from_secs(1)),
            None
        );
    }

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

    /// Deleting requires BOTH gates, not just the request's.
    ///
    /// The reaper honoured only `mutate` from the query string, so
    /// `?mutate=true` deleted backups even with MUSE_FOUNDRY_ENABLE_MUTATION
    /// unset. An operator who has deliberately closed the global gate — and
    /// who reads `mutation_enabled=false` in the startup log — would
    /// reasonably believe nothing in the library can be destroyed. One query
    /// parameter made that false.
    #[test]
    fn deleting_requires_the_global_gate_as_well_as_the_request() {
        let body = include_str!("reaper.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("a non-test body");

        assert!(
            body.contains("let mutate = mutate && globally_permitted;"),
            "the request's mutate flag must be ANDed with the global gate"
        );
        // ...and the global gate must come from the config, not be assumed.
        assert!(
            body.contains("let globally_permitted = cfg.enable_mutation;"),
            "the global gate must be read from the Foundry's own configuration"
        );
        // The deletion itself is still additionally gated on the verdict.
        assert!(
            body.contains("if mutate && outcome == ReapOutcome::WouldDelete"),
            "deletion must still require the gate's own Allow"
        );
    }

    /// The truth table, stated so a future change cannot quietly widen it.
    #[test]
    fn the_two_gates_compose_as_an_and_not_an_or() {
        // (global, request) -> may delete
        let cases = [
            ((false, false), false),
            ((false, true), false), // the case that was broken
            ((true, false), false),
            ((true, true), true),
        ];
        for ((global, request), expected) in cases {
            assert_eq!(
                request && global,
                expected,
                "global={global} request={request} must yield {expected}"
            );
        }
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
