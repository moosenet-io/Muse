//! **The armed loop: driving Path A across many titles, with a stop condition
//! for every way it can go wrong.**
//!
//! `optimize_file` handles one file and `foundry_optimize` deliberately refuses
//! more than eight paths, because "a large run is something an operator
//! assembles deliberately, not something one request starts". This module is
//! that deliberate assembly — and the whole of its job is knowing when to stop.
//!
//! ## Why the decision is a pure function
//!
//! Every stop condition lives in [`decide_next_step`], which takes numbers and
//! returns an enum. Nothing in it touches a `Foundry`, a filesystem, or a
//! clock. That is not stylistic: this codebase has four separate cases where a
//! decision embedded in a function needing a live `Foundry` had a mutation
//! survive because nothing could reach it (`probe_order`, `probe_stop_reason`,
//! `acquire_title_lock`, `survey_truncation`). A run that decides when to stop
//! destroying-adjacent work is the last place to repeat that.
//!
//! ## The distinction this module exists to preserve
//!
//! **Finishing the work and running out of road are different facts.** Exactly
//! one [`StopReason`] — [`StopReason::NoMoreCandidates`] — means the run did
//! everything it was asked to. Every other variant means it stopped early, and
//! [`RunReport::completed`] is derived from that one variant rather than from a
//! count comparison, so a truncated run can never render as a finished one.

use std::path::PathBuf;
use std::time::Duration;

/// The safety envelope for a run. Every field is a way to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLimits {
    /// How many titles this run may ATTEMPT. Not a target — a ceiling.
    pub max_titles: usize,
    /// Stop after this many failures in a row. A single failure is a bad file;
    /// a run of them is a broken host, and continuing turns one problem into
    /// hundreds.
    pub max_consecutive_failures: u32,
    /// Stop when the work filesystem drops below this. Checked BEFORE each
    /// title, because the check is only useful while there is still room to
    /// stage the next encode.
    pub min_free_bytes: u64,
    /// Wall-clock ceiling for the whole run.
    pub deadline: Duration,
}

impl RunLimits {
    /// Deliberately small. A first live run is a canary, not a sweep, and an
    /// operator who wants more says so explicitly.
    pub fn conservative() -> Self {
        Self {
            max_titles: 10,
            max_consecutive_failures: 3,
            // 50 GiB. The work dir stages a full copy plus the encode.
            min_free_bytes: 50 * 1024 * 1024 * 1024,
            deadline: Duration::from_secs(6 * 3600),
        }
    }
}

/// What a run has done so far. Counts only; no judgement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunLedger {
    pub attempted: usize,
    pub rewritten: usize,
    pub failed: usize,
    pub skipped: usize,
    pub consecutive_failures: u32,
    pub bytes_before_total: u64,
    pub bytes_after_total: u64,
}

impl RunLedger {
    /// Disk actually freed, or 0 when the rewrites grew the library.
    ///
    /// Saturating rather than signed: a negative "reclaimed" is a number that
    /// reads as a small positive one at a glance, and this figure is quoted to
    /// operators. Growth is visible as `bytes_after_total > bytes_before_total`
    /// for anyone who looks, which is the honest place for it.
    pub fn bytes_reclaimed(&self) -> u64 {
        self.bytes_before_total
            .saturating_sub(self.bytes_after_total)
    }
}

/// Why a run stopped. **Exactly one of these means the work is done.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The candidate list was exhausted. **The only complete outcome.**
    NoMoreCandidates,
    /// The title ceiling was reached. There is more work; this run may not do
    /// it.
    BudgetReached { limit: usize },
    /// Too many failures in a row — treated as a broken host rather than a run
    /// of bad files.
    ConsecutiveFailures { count: u32, limit: u32 },
    /// The work filesystem fell below its floor.
    FreeSpaceFloor { free_bytes: u64, floor_bytes: u64 },
    /// An operator asked it to stop.
    Cancelled,
    /// Wall clock ran out.
    DeadlineReached { elapsed_secs: u64 },
    /// The deployment-level kill-switch is closed. Checked here as well as in
    /// forge so a run refuses as a whole rather than reporting N identical
    /// per-file skips.
    MutationDisabled,
}

impl StopReason {
    /// Whether this run got through everything it was given.
    ///
    /// A single `matches!` against one variant, NOT a comparison of counts
    /// against the candidate list. A count comparison would call a run
    /// "complete" whenever the numbers happened to line up — including a run
    /// cancelled on its last title.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::NoMoreCandidates)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoMoreCandidates => "no_more_candidates",
            Self::BudgetReached { .. } => "budget_reached",
            Self::ConsecutiveFailures { .. } => "consecutive_failures",
            Self::FreeSpaceFloor { .. } => "free_space_floor",
            Self::Cancelled => "cancelled",
            Self::DeadlineReached { .. } => "deadline_reached",
            Self::MutationDisabled => "mutation_disabled",
        }
    }
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMoreCandidates => write!(f, "every candidate was processed"),
            Self::BudgetReached { limit } => write!(
                f,
                "the run reached its ceiling of {limit} titles; more remain unprocessed"
            ),
            Self::ConsecutiveFailures { count, limit } => write!(
                f,
                "{count} titles failed in a row (limit {limit}) — this reads as a broken \
                 host rather than bad files, so the run stopped"
            ),
            Self::FreeSpaceFloor {
                free_bytes,
                floor_bytes,
            } => write!(
                f,
                "the work filesystem has {free_bytes} bytes free, below the {floor_bytes} \
                 floor; there is not enough room to stage another encode"
            ),
            Self::Cancelled => write!(f, "an operator stopped the run"),
            Self::DeadlineReached { elapsed_secs } => {
                write!(f, "the run hit its wall-clock limit after {elapsed_secs}s")
            }
            Self::MutationDisabled => write!(
                f,
                "MUSE_FOUNDRY_ENABLE_MUTATION is closed — this deployment cannot modify \
                 the library"
            ),
        }
    }
}

/// The next thing a run should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStep {
    /// Process one more title.
    Process,
    /// Stop, for this reason.
    Stop(StopReason),
}

/// Everything the decision needs, gathered by the caller so the decision itself
/// stays pure.
#[derive(Debug, Clone, Copy)]
pub struct RunObservation {
    pub candidates_remaining: usize,
    pub free_bytes: u64,
    pub elapsed: Duration,
    pub cancelled: bool,
    pub mutation_enabled: bool,
}

/// **Should the run continue?**
///
/// ## The ordering is the design
///
/// Conditions are checked most-authoritative first, and the order is not
/// arbitrary:
///
/// 1. **Mutation disabled** — the deployment forbids the work entirely. Nothing
///    below matters if the run may not act at all.
/// 2. **Cancelled** — an operator's explicit instruction outranks every
///    automatic condition. Checked before the resource limits so that a stop
///    requested during a low-disk condition still reports as `Cancelled`: the
///    operator asked, and telling them the disk stopped it would be a lie about
///    who decided.
/// 3. **Deadline** and **free space** — physical limits. Free space is checked
///    before each title rather than after, because a floor is only useful while
///    there is still room to stage the next encode.
/// 4. **Consecutive failures** — a broken host.
/// 5. **Budget** — the ceiling.
/// 6. **Candidates** — and only then, having ruled out every early stop, can an
///    empty list mean the work is finished.
///
/// That last point is why `NoMoreCandidates` is checked LAST. Checked first, an
/// exhausted list would mask a cancellation or a disk floor, and the run would
/// report itself complete when it was interrupted.
pub fn decide_next_step(
    limits: &RunLimits,
    ledger: &RunLedger,
    obs: RunObservation,
) -> RunStep {
    if !obs.mutation_enabled {
        return RunStep::Stop(StopReason::MutationDisabled);
    }
    if obs.cancelled {
        return RunStep::Stop(StopReason::Cancelled);
    }
    if obs.elapsed >= limits.deadline {
        return RunStep::Stop(StopReason::DeadlineReached {
            elapsed_secs: obs.elapsed.as_secs(),
        });
    }
    if obs.free_bytes < limits.min_free_bytes {
        return RunStep::Stop(StopReason::FreeSpaceFloor {
            free_bytes: obs.free_bytes,
            floor_bytes: limits.min_free_bytes,
        });
    }
    if limits.max_consecutive_failures > 0
        && ledger.consecutive_failures >= limits.max_consecutive_failures
    {
        return RunStep::Stop(StopReason::ConsecutiveFailures {
            count: ledger.consecutive_failures,
            limit: limits.max_consecutive_failures,
        });
    }
    if ledger.attempted >= limits.max_titles {
        return RunStep::Stop(StopReason::BudgetReached {
            limit: limits.max_titles,
        });
    }
    if obs.candidates_remaining == 0 {
        return RunStep::Stop(StopReason::NoMoreCandidates);
    }
    RunStep::Process
}

/// The outcome of a whole run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub ledger: RunLedger,
    pub stop_reason: StopReason,
    /// Whether the candidate list this run was given covered the whole library.
    /// `completed()` requires BOTH this and an exhausted list.
    pub survey_complete: bool,
    /// Titles never attempted, because the run stopped first. Reported so an
    /// early stop states its own size rather than leaving the operator to
    /// subtract.
    pub candidates_unprocessed: usize,
}

impl RunReport {
    /// Whether the library was actually processed.
    ///
    /// Requires an exhausted candidate list AND a complete survey. Either alone
    /// is a half-truth: a run can exhaust a partial list without having looked
    /// at most of the library, and a complete survey means nothing if the run
    /// stopped early. See [`StopReason::is_complete`] for why the first half is
    /// not a count comparison.
    pub fn completed(&self) -> bool {
        self.stop_reason.is_complete() && self.survey_complete
    }
}

/// Which titles a run is allowed to touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidatePolicy {
    /// Only titles whose originals the deletion gate will allow to be removed.
    ///
    /// **The default, and the one worth explaining.** A full-library survey
    /// (2026-08-02, all 16,221 titles) found 3,621 that would be re-encoded, of
    /// which only 463 can ever have their original reclaimed. For the other
    /// 3,158 the rewrite leaves BOTH files on disk permanently, so Path A
    /// increases library size for them.
    ///
    /// That is a real trade rather than a strict loss — those titles still gain
    /// a direct-playable file — which is exactly why it is a policy an operator
    /// chooses rather than a rule baked in.
    ReclaimableOnly,
    /// Every title the planner says needs work, whether or not the original can
    /// be reclaimed.
    All,
}

/// Select what a run should process, from an already-surveyed library.
///
/// Pure: it takes survey outcomes and returns paths. The survey did the probing
/// and the planning; this decides only what is in scope. Keeping it separate
/// means the selection rule — the thing that decides whether 463 or 3,621
/// titles get touched — is testable without probing anything.
pub fn select_candidates(
    surveyed: &[(PathBuf, bool)],
    policy: CandidatePolicy,
) -> Vec<PathBuf> {
    surveyed
        .iter()
        .filter(|(_, reclaims_disk)| match policy {
            CandidatePolicy::ReclaimableOnly => *reclaims_disk,
            CandidatePolicy::All => true,
        })
        .map(|(p, _)| p.clone())
        .collect()
}

/// What happened to one title. The driver's view of [`ForgeStatus`], reduced to
/// the three things the ledger counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitleOutcome {
    Rewritten { bytes_before: u64, bytes_after: u64 },
    Failed,
    /// Skipped for a reason forge decided — not a failure, and specifically not
    /// counted toward the consecutive-failure limit. A run over a library where
    /// most titles are already optimal must not read as a broken host.
    Skipped,
}

/// **Drive a run to completion or to its first stop condition.**
///
/// Generic over how a title is processed and how the world is observed, so the
/// loop — including every interaction between the ledger and the stop
/// conditions — is testable without encoding a single frame. The production
/// wiring passes a closure that calls [`Foundry::optimize_file`]; the tests
/// pass one that returns canned outcomes.
///
/// Titles are processed **strictly one at a time**. Concurrency here would put
/// several multi-hour ffmpeg encodes on a shared host at once and make the
/// free-space floor meaningless, since each would stage its own copy.
pub fn drive_run<Obs, Proc>(
    limits: &RunLimits,
    candidates: &[PathBuf],
    observe: Obs,
    process: Proc,
) -> RunReport
where
    Obs: FnMut() -> RunObservation,
    Proc: FnMut(&std::path::Path) -> TitleOutcome,
{
    drive_run_with_progress(limits, candidates, observe, process, |_| {})
}

/// [`drive_run`], plus a hook called with the authoritative ledger after each
/// title.
///
/// The hook exists so a status endpoint can show live progress WITHOUT keeping
/// its own running totals. An earlier version had `execute_run` sum the same
/// outcomes a second time to feed the progress snapshot; the two agreed, but
/// two hand-maintained copies of one accounting will diverge on the next edit
/// and the divergence would show up as an operator-visible wrong number during
/// a destructive run. Raised at the FOUNDRY-11 gate.
pub fn drive_run_with_progress<Obs, Proc, Prog>(
    limits: &RunLimits,
    candidates: &[PathBuf],
    mut observe: Obs,
    mut process: Proc,
    mut on_progress: Prog,
) -> RunReport
where
    Obs: FnMut() -> RunObservation,
    Proc: FnMut(&std::path::Path) -> TitleOutcome,
    Prog: FnMut(&RunLedger),
{
    let mut ledger = RunLedger::default();
    let mut next = 0usize;

    loop {
        let mut o = observe();
        // The caller reports the world; the count of work left is ours to know.
        o.candidates_remaining = candidates.len().saturating_sub(next);

        match decide_next_step(limits, &ledger, o) {
            RunStep::Stop(stop_reason) => {
                return RunReport {
                    ledger,
                    stop_reason,
                    // The driver is given a list; whether that list covered the
                    // library is the caller's fact, and `execute_run` sets it.
                    survey_complete: true,
                    candidates_unprocessed: candidates.len().saturating_sub(next),
                }
            }
            RunStep::Process => {}
        }

        let path = &candidates[next];
        next += 1;
        ledger.attempted += 1;

        match process(path) {
            TitleOutcome::Rewritten {
                bytes_before,
                bytes_after,
            } => {
                ledger.rewritten += 1;
                ledger.consecutive_failures = 0;
                ledger.bytes_before_total = ledger.bytes_before_total.saturating_add(bytes_before);
                ledger.bytes_after_total = ledger.bytes_after_total.saturating_add(bytes_after);
            }
            TitleOutcome::Failed => {
                ledger.failed += 1;
                ledger.consecutive_failures += 1;
            }
            TitleOutcome::Skipped => {
                ledger.skipped += 1;
                // Deliberately does NOT reset the failure streak. A skip is not
                // evidence the host recovered, so a run alternating
                // failure/skip/failure must still trip the limit rather than
                // having its streak wiped by files it never touched.
            }
        }

        on_progress(&ledger);
    }
}

// --- Production wiring -----------------------------------------------------

/// Shared state for a run in flight: what it has done, and the one bit an
/// operator can flip to stop it.
#[derive(Debug, Default)]
pub struct RunHandle {
    cancel: std::sync::atomic::AtomicBool,
    active: std::sync::atomic::AtomicBool,
    progress: std::sync::Mutex<Option<RunProgress>>,
}

/// A snapshot of a run, safe to serve from a status endpoint mid-run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunProgress {
    pub ledger: RunLedger,
    pub candidates_total: usize,
    pub current: Option<PathBuf>,
    /// Whether the survey that produced the candidate list was complete.
    ///
    /// A survey has its own deadline and can truncate. Exhausting a PARTIAL
    /// candidate list would otherwise report `NoMoreCandidates` and therefore
    /// `completed`, telling an operator the library was processed when most of
    /// it was never even looked at. Raised at the FOUNDRY-11 gate.
    pub survey_complete: bool,
    /// `None` while running. Present once stopped — and its presence is the
    /// only signal that the run is over, so a status reader cannot mistake a
    /// stalled run for a finished one.
    pub stop_reason: Option<StopReason>,
}

impl RunHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask a run to stop. Returns whether a run was actually in flight, so an
    /// operator is told "there was nothing to stop" rather than a bare success.
    ///
    /// **Sets the flag only when a run is actually active.** Storing it
    /// unconditionally meant an idle stop request left `cancel` latched, and
    /// the next run — started minutes or hours later by someone who never saw
    /// that request — was born cancelled and stopped on its first title with no
    /// explanation an operator could distinguish from a broken run. Raised at
    /// the FOUNDRY-11 gate.
    pub fn request_stop(&self) -> bool {
        let active = self.active.load(std::sync::atomic::Ordering::SeqCst);
        if active {
            self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        active
    }

    pub fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn snapshot(&self) -> Option<RunProgress> {
        self.progress.lock().ok().and_then(|g| g.clone())
    }

    /// Claim the run slot. Returns false when one is already in flight.
    ///
    /// Compare-and-swap rather than a read-then-write, so two requests arriving
    /// together cannot both believe they won and start two concurrent sweeps
    /// over the same library.
    fn try_claim(&self) -> bool {
        self.active
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    fn release(&self) {
        self.active
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.cancel
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Claim the slot, receiving a guard that releases it **however the run
    /// ends** — including a panic.
    ///
    /// The reason this is an RAII guard and not a matched claim/release pair:
    /// `execute_run` calls into ffmpeg wrappers, path handling and probe
    /// parsing, any of which can panic on a sufficiently strange file. A panic
    /// unwinding past a bare `release()` would leave `active` set forever, and
    /// because `spawn_blocking` catches the panic the process would survive in
    /// a state where every future run is refused `AlreadyRunning` and the
    /// status endpoint reports a phantom run in flight. Only a restart would
    /// clear it. Raised at the FOUNDRY-11 gate by two reviewers independently.
    ///
    /// Claiming here rather than inside `execute_run` also removes a
    /// time-of-check/time-of-use gap: the caller holds the slot from the moment
    /// it decides to start, so it cannot report "started" and then lose the
    /// race.
    pub fn claim(&self) -> Option<RunSlot<'_>> {
        self.try_claim().then(|| {
            // Clear any stale cancel as part of claiming, so a run can never be
            // born cancelled however the flag came to be set. `release` clears
            // it too; doing it at BOTH ends means neither is load-bearing on
            // its own.
            self.cancel
                .store(false, std::sync::atomic::Ordering::SeqCst);
            RunSlot { handle: self }
        })
    }
}

/// Proof that the caller holds the run slot, and the thing that gives it back.
///
/// Cannot be constructed outside this module, so `execute_run` taking one by
/// value is a compile-time guarantee that a run has claimed the slot exactly
/// once.
#[derive(Debug)]
pub struct RunSlot<'a> {
    handle: &'a RunHandle,
}

impl Drop for RunSlot<'_> {
    fn drop(&mut self) {
        self.handle.release();
    }
}

/// The exact phrase an operator must restate to start a run of this size.
///
/// The optimize endpoint has the operator restate a *path*, because there the
/// dangerous quantity is which file. Here it is **how many titles**, so that is
/// what gets restated: a mis-typed `max_titles` then fails closed instead of
/// starting a larger run than intended.
pub fn confirm_phrase(max_titles: usize) -> String {
    format!("run {max_titles} titles")
}

/// Check an operator's confirmation against the run they asked for.
///
/// Extracted rather than left inline in the handler for the reason this
/// codebase keeps rediscovering: a decision inside a function that needs a live
/// `AppState` is effectively untested, and this particular decision is the one
/// standing between a typo and a 16,000-title run.
pub fn check_confirm(max_titles: usize, provided: Option<&str>) -> Result<(), String> {
    let expected = confirm_phrase(max_titles);
    match provided {
        Some(c) if c == expected => Ok(()),
        _ => Err(format!(
            "confirm must restate the size of the run. Expected confirm=\"{expected}\""
        )),
    }
}

/// The process-wide run slot.
///
/// A global rather than a field on `AppState`, because the constraint it
/// enforces IS process-global: there is one work dir and one library, and two
/// concurrent sweeps would race for both. Threading a handle through every
/// `AppState` construction site would let a test or a future caller create a
/// second one and quietly defeat the guard, which is the opposite of what this
/// is for.
pub fn global_handle() -> &'static RunHandle {
    static HANDLE: std::sync::OnceLock<RunHandle> = std::sync::OnceLock::new();
    HANDLE.get_or_init(RunHandle::new)
}

/// Why a run could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStartRefusal {
    /// Another run is in flight. Two concurrent sweeps would race for the work
    /// dir and make the free-space floor meaningless.
    AlreadyRunning,
}

/// Stamp the CALLER's survey-completeness onto the driver's report.
///
/// [`drive_run_with_progress`] is handed a candidate list and cannot know
/// whether that list covered the library, so it always reports
/// `survey_complete: true`; only `execute_run`'s caller knows. That single
/// assignment used to live inline in [`execute_run`], which needs a live
/// `Foundry` and is therefore unreachable from any test — replacing it with
/// `= true` left the whole suite green (FSURV-02 S1).
///
/// This is the seam, and it is deliberately the smallest one that works: a
/// pure function over an already-built report, with `execute_run` otherwise
/// unchanged. What it protects is [`RunReport::completed`], which is what
/// tells an operator a 16,221-title sweep is finished — a truncated survey
/// that reads as a complete run would say "done" about a library that was
/// never fully examined.
pub fn finish_report(mut report: RunReport, survey_complete: bool) -> RunReport {
    report.survey_complete = survey_complete;
    report
}

/// **Execute a run against a real `Foundry`.** Blocking; callers put it on a
/// thread that may block.
///
/// This is the only function here that touches the world, and it is
/// deliberately thin: it supplies `observe` and `process` to [`drive_run`] and
/// does no deciding of its own.
pub fn execute_run(
    foundry: &crate::foundry::Foundry,
    policy: &crate::foundry::policy::TranscodePolicy,
    candidates: &[PathBuf],
    limits: &RunLimits,
    work_dir: Option<&std::path::Path>,
    // The slot IS the handle. Taking them as two parameters let a caller claim
    // one handle and pass another — including claiming a fresh local
    // `RunHandle` and running concurrently with the endpoint-managed global
    // one, which is exactly the concurrency this guard exists to prevent.
    // Raised at the FOUNDRY-11 gate. Now there is one parameter and no way for
    // them to disagree.
    slot: RunSlot<'_>,
    // `survey_complete`: whether the survey that produced these candidates was
    // itself complete. A truncated survey means the list is partial, so
    // exhausting it is NOT the same as having processed the library.
    survey_complete: bool,
) -> RunReport {
    let handle = slot.handle;
    let started = std::time::Instant::now();
    let mutation_enabled = foundry.mutation_enabled();

    // `if let Ok` rather than `.unwrap()`: a poisoned lock (some earlier panic
    // while holding it) must not take down the run as well.
    if let Ok(mut g) = handle.progress.lock() {
        *g = Some(RunProgress {
            ledger: RunLedger::default(),
            candidates_total: candidates.len(),
            current: None,
            survey_complete,
            stop_reason: None,
        });
    }

    let report = drive_run_with_progress(
        limits,
        candidates,
        || RunObservation {
            candidates_remaining: 0, // the driver fills this in
            // An unreadable work dir counts as ZERO free, so the floor trips.
            // "Cannot tell" must never read as "plenty of room".
            free_bytes: work_dir
                .and_then(crate::foundry::validate::free_bytes_for)
                .unwrap_or(0),
            elapsed: started.elapsed(),
            cancelled: handle.cancel.load(std::sync::atomic::Ordering::SeqCst),
            mutation_enabled,
        },
        |path| {
            if let Ok(mut g) = handle.progress.lock() {
                if let Some(p) = g.as_mut() {
                    p.current = Some(path.to_path_buf());
                }
            }

            let outcome = match foundry.optimize_file(path, policy) {
                crate::foundry::forge::ForgeStatus::Rewritten(rec) => {
                    tracing::info!(
                        path = %path.display(),
                        bytes_before = rec.bytes_before,
                        bytes_after = rec.bytes_after,
                        "foundry run: rewrote title"
                    );
                    TitleOutcome::Rewritten {
                        bytes_before: rec.bytes_before,
                        bytes_after: rec.bytes_after,
                    }
                }
                crate::foundry::forge::ForgeStatus::Skipped { reason } => {
                    tracing::info!(path = %path.display(), %reason, "foundry run: skipped title");
                    TitleOutcome::Skipped
                }
                crate::foundry::forge::ForgeStatus::Failed { reason } => {
                    tracing::warn!(path = %path.display(), %reason, "foundry run: title FAILED");
                    TitleOutcome::Failed
                }
            };

            if let Ok(mut g) = handle.progress.lock() {
                if let Some(p) = g.as_mut() {
                    p.current = None;
                }
            }
            outcome
        },
        |ledger| {
            // ONE source of truth: the driver's own ledger, copied verbatim.
            if let Ok(mut g) = handle.progress.lock() {
                if let Some(p) = g.as_mut() {
                    p.ledger = ledger.clone();
                }
            }
        },
    );

    let report = finish_report(report, survey_complete);

    if let Ok(mut g) = handle.progress.lock() {
        if let Some(p) = g.as_mut() {
            p.ledger = report.ledger.clone();
            p.current = None;
            // Read back from the report rather than stamping `survey_complete`
            // a second time: two hand-maintained copies of one fact diverge on
            // the next edit, and this one decides whether the operator is told
            // the library is done.
            p.survey_complete = report.survey_complete;
            p.stop_reason = Some(report.stop_reason.clone());
        }
    }
    tracing::info!(
        stop_reason = report.stop_reason.as_str(),
        completed = report.completed(),
        attempted = report.ledger.attempted,
        rewritten = report.ledger.rewritten,
        failed = report.ledger.failed,
        skipped = report.ledger.skipped,
        unprocessed = report.candidates_unprocessed,
        "foundry run: finished"
    );
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(remaining: usize) -> RunObservation {
        RunObservation {
            candidates_remaining: remaining,
            free_bytes: u64::MAX,
            elapsed: Duration::ZERO,
            cancelled: false,
            mutation_enabled: true,
        }
    }

    fn limits() -> RunLimits {
        RunLimits {
            max_titles: 10,
            max_consecutive_failures: 3,
            min_free_bytes: 1000,
            deadline: Duration::from_secs(100),
        }
    }

    #[test]
    fn a_healthy_run_with_work_left_continues() {
        assert_eq!(
            decide_next_step(&limits(), &RunLedger::default(), obs(5)),
            RunStep::Process
        );
    }

    #[test]
    fn an_exhausted_candidate_list_is_the_one_complete_outcome() {
        let step = decide_next_step(&limits(), &RunLedger::default(), obs(0));
        assert_eq!(step, RunStep::Stop(StopReason::NoMoreCandidates));
        match step {
            RunStep::Stop(r) => assert!(r.is_complete()),
            _ => panic!(),
        }
    }

    #[test]
    fn the_title_ceiling_stops_the_run_and_is_not_a_completion() {
        let ledger = RunLedger {
            attempted: 10,
            ..Default::default()
        };
        let step = decide_next_step(&limits(), &ledger, obs(500));
        assert_eq!(
            step,
            RunStep::Stop(StopReason::BudgetReached { limit: 10 })
        );
        match step {
            RunStep::Stop(r) => assert!(
                !r.is_complete(),
                "hitting the ceiling leaves work undone and must never read as finished"
            ),
            _ => panic!(),
        }
    }

    #[test]
    fn failures_in_a_row_stop_the_run() {
        let ledger = RunLedger {
            consecutive_failures: 3,
            ..Default::default()
        };
        assert_eq!(
            decide_next_step(&limits(), &ledger, obs(5)),
            RunStep::Stop(StopReason::ConsecutiveFailures { count: 3, limit: 3 })
        );
    }

    #[test]
    fn scattered_failures_do_not_stop_the_run() {
        // The counter is CONSECUTIVE. A run that fails one file in ten is
        // processing a library with some bad files, not falling over.
        let ledger = RunLedger {
            attempted: 5,
            failed: 4,
            consecutive_failures: 1,
            ..Default::default()
        };
        assert_eq!(decide_next_step(&limits(), &ledger, obs(5)), RunStep::Process);
    }

    #[test]
    fn a_zero_failure_limit_disables_the_check_rather_than_stopping_instantly() {
        // Guards against the off-by-one where `0 >= 0` stops a run that has not
        // failed at all.
        let l = RunLimits {
            max_consecutive_failures: 0,
            ..limits()
        };
        assert_eq!(
            decide_next_step(&l, &RunLedger::default(), obs(5)),
            RunStep::Process
        );
    }

    #[test]
    fn a_full_disk_stops_the_run_before_the_next_title() {
        let o = RunObservation {
            free_bytes: 999,
            ..obs(5)
        };
        assert_eq!(
            decide_next_step(&limits(), &RunLedger::default(), o),
            RunStep::Stop(StopReason::FreeSpaceFloor {
                free_bytes: 999,
                floor_bytes: 1000
            })
        );
    }

    #[test]
    fn exactly_the_floor_is_still_enough() {
        let o = RunObservation {
            free_bytes: 1000,
            ..obs(5)
        };
        assert_eq!(
            decide_next_step(&limits(), &RunLedger::default(), o),
            RunStep::Process
        );
    }

    #[test]
    fn the_deadline_stops_the_run() {
        let o = RunObservation {
            elapsed: Duration::from_secs(100),
            ..obs(5)
        };
        assert_eq!(
            decide_next_step(&limits(), &RunLedger::default(), o),
            RunStep::Stop(StopReason::DeadlineReached { elapsed_secs: 100 })
        );
    }

    #[test]
    fn a_cancelled_run_stops() {
        let o = RunObservation {
            cancelled: true,
            ..obs(5)
        };
        assert_eq!(
            decide_next_step(&limits(), &RunLedger::default(), o),
            RunStep::Stop(StopReason::Cancelled)
        );
    }

    /// The operator asked. Saying the disk stopped it would misattribute the
    /// decision — see the ordering note on `decide_next_step`.
    #[test]
    fn cancellation_outranks_the_resource_limits() {
        let o = RunObservation {
            cancelled: true,
            free_bytes: 0,
            elapsed: Duration::from_secs(9999),
            ..obs(5)
        };
        assert_eq!(
            decide_next_step(&limits(), &RunLedger::default(), o),
            RunStep::Stop(StopReason::Cancelled)
        );
    }

    /// The kill-switch outranks everything, including a cancellation: a run
    /// that may not act at all never started.
    #[test]
    fn a_closed_kill_switch_outranks_every_other_condition() {
        let o = RunObservation {
            mutation_enabled: false,
            cancelled: true,
            free_bytes: 0,
            ..obs(0)
        };
        assert_eq!(
            decide_next_step(&limits(), &RunLedger::default(), o),
            RunStep::Stop(StopReason::MutationDisabled)
        );
    }

    /// **The bug this ordering exists to prevent.**
    ///
    /// With the candidate check first, a run that was cancelled — or that hit
    /// its disk floor — on its very last title would report `NoMoreCandidates`
    /// and therefore `completed`. The interruption would vanish.
    #[test]
    fn an_early_stop_on_the_last_title_is_not_reported_as_completion() {
        for (label, o) in [
            (
                "cancelled",
                RunObservation {
                    cancelled: true,
                    ..obs(0)
                },
            ),
            (
                "disk floor",
                RunObservation {
                    free_bytes: 0,
                    ..obs(0)
                },
            ),
            (
                "deadline",
                RunObservation {
                    elapsed: Duration::from_secs(9999),
                    ..obs(0)
                },
            ),
        ] {
            match decide_next_step(&limits(), &RunLedger::default(), o) {
                RunStep::Stop(r) => assert!(
                    !r.is_complete(),
                    "{label}: an exhausted list must not mask an early stop"
                ),
                RunStep::Process => panic!("{label}: expected a stop"),
            }
        }
    }

    #[test]
    fn a_report_is_complete_only_when_its_stop_reason_is() {
        // A ledger whose counts look finished cannot make a cancelled run read
        // as complete: `completed()` reads the reason, not the numbers.
        let report = RunReport {
            ledger: RunLedger {
                attempted: 10,
                rewritten: 10,
                ..Default::default()
            },
            stop_reason: StopReason::Cancelled,
            survey_complete: true,
            candidates_unprocessed: 0,
        };
        assert!(!report.completed());
    }

    #[test]
    fn reclaimed_bytes_never_go_negative_when_a_rewrite_grew_the_file() {
        let ledger = RunLedger {
            bytes_before_total: 100,
            bytes_after_total: 250,
            ..Default::default()
        };
        assert_eq!(ledger.bytes_reclaimed(), 0);
    }

    #[test]
    fn reclaimed_bytes_are_the_difference_when_the_run_shrank_the_library() {
        let ledger = RunLedger {
            bytes_before_total: 1000,
            bytes_after_total: 400,
            ..Default::default()
        };
        assert_eq!(ledger.bytes_reclaimed(), 600);
    }

    #[test]
    fn every_stop_reason_renders_as_something_an_operator_can_act_on() {
        for r in [
            StopReason::NoMoreCandidates,
            StopReason::BudgetReached { limit: 10 },
            StopReason::ConsecutiveFailures { count: 3, limit: 3 },
            StopReason::FreeSpaceFloor {
                free_bytes: 1,
                floor_bytes: 2,
            },
            StopReason::Cancelled,
            StopReason::DeadlineReached { elapsed_secs: 5 },
            StopReason::MutationDisabled,
        ] {
            let text = r.to_string();
            assert!(text.len() > 20, "{r:?} renders as {text:?}");
            assert!(!r.as_str().is_empty());
        }
    }

    fn surveyed() -> Vec<(PathBuf, bool)> {
        vec![
            (PathBuf::from("/srv/media/reclaims-a.mkv"), true),
            (PathBuf::from("/srv/media/keeps-original-b.avi"), false),
            (PathBuf::from("/srv/media/reclaims-c.mp4"), true),
            (PathBuf::from("/srv/media/keeps-original-d.avi"), false),
        ]
    }

    #[test]
    fn the_default_policy_admits_only_titles_that_reclaim_their_original() {
        let picked = select_candidates(&surveyed(), CandidatePolicy::ReclaimableOnly);
        assert_eq!(
            picked,
            vec![
                PathBuf::from("/srv/media/reclaims-a.mkv"),
                PathBuf::from("/srv/media/reclaims-c.mp4"),
            ],
            "a run that leaves both copies on disk is opt-in, not the default"
        );
    }

    #[test]
    fn the_all_policy_admits_everything_the_planner_would_touch() {
        assert_eq!(select_candidates(&surveyed(), CandidatePolicy::All).len(), 4);
    }

    /// The selection must key on the per-title fact, not on list position or
    /// count — a filter that returned the first N would pass a count assertion
    /// while touching the wrong titles.
    #[test]
    fn selection_picks_the_reclaimable_titles_not_merely_the_right_number() {
        let inverted: Vec<(PathBuf, bool)> = surveyed()
            .into_iter()
            .map(|(p, r)| (p, !r))
            .collect();
        let picked = select_candidates(&inverted, CandidatePolicy::ReclaimableOnly);
        assert_eq!(
            picked,
            vec![
                PathBuf::from("/srv/media/keeps-original-b.avi"),
                PathBuf::from("/srv/media/keeps-original-d.avi"),
            ]
        );
    }

    #[test]
    fn a_library_where_nothing_reclaims_selects_nothing() {
        let none: Vec<(PathBuf, bool)> =
            surveyed().into_iter().map(|(p, _)| (p, false)).collect();
        assert!(select_candidates(&none, CandidatePolicy::ReclaimableOnly).is_empty());
    }

    // --- the driver loop ---------------------------------------------------

    fn paths(n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| PathBuf::from(format!("/srv/media/t{i}.mkv")))
            .collect()
    }

    fn healthy() -> RunObservation {
        RunObservation {
            candidates_remaining: 0, // overwritten by the driver
            free_bytes: u64::MAX,
            elapsed: Duration::ZERO,
            cancelled: false,
            mutation_enabled: true,
        }
    }

    #[test]
    fn a_clean_run_processes_every_candidate_and_reports_complete() {
        let seen = std::cell::RefCell::new(Vec::new());
        let report = drive_run(&limits(), &paths(5), healthy, |p| {
            seen.borrow_mut().push(p.to_path_buf());
            TitleOutcome::Rewritten {
                bytes_before: 100,
                bytes_after: 40,
            }
        });

        assert_eq!(seen.borrow().len(), 5);
        assert_eq!(seen.borrow()[0], PathBuf::from("/srv/media/t0.mkv"));
        assert!(report.completed());
        assert_eq!(report.stop_reason, StopReason::NoMoreCandidates);
        assert_eq!(report.ledger.rewritten, 5);
        assert_eq!(report.ledger.bytes_reclaimed(), 300);
        assert_eq!(report.candidates_unprocessed, 0);
    }

    #[test]
    fn the_budget_stops_the_loop_and_the_remainder_is_reported() {
        let calls = std::cell::Cell::new(0);
        let l = RunLimits {
            max_titles: 3,
            ..limits()
        };
        let report = drive_run(&l, &paths(10), healthy, |_| {
            calls.set(calls.get() + 1);
            TitleOutcome::Rewritten {
                bytes_before: 10,
                bytes_after: 5,
            }
        });

        assert_eq!(calls.get(), 3, "the ceiling must stop the loop, not the report");
        assert!(!report.completed());
        assert_eq!(report.candidates_unprocessed, 7);
    }

    #[test]
    fn a_broken_host_stops_the_run_after_the_failure_limit() {
        let calls = std::cell::Cell::new(0);
        let report = drive_run(&limits(), &paths(50), healthy, |_| {
            calls.set(calls.get() + 1);
            TitleOutcome::Failed
        });

        assert_eq!(calls.get(), 3, "3 failures in a row, then stop");
        assert_eq!(
            report.stop_reason,
            StopReason::ConsecutiveFailures { count: 3, limit: 3 }
        );
        assert!(!report.completed());
        assert_eq!(report.candidates_unprocessed, 47);
    }

    #[test]
    fn a_success_clears_the_failure_streak() {
        // fail, fail, succeed, fail, fail — never three in a row, so the run
        // finishes. Without the reset it would stop at the fourth.
        let n = std::cell::Cell::new(0);
        let report = drive_run(&limits(), &paths(5), healthy, |_| {
            let i = n.get();
            n.set(i + 1);
            if i == 2 {
                TitleOutcome::Rewritten {
                    bytes_before: 10,
                    bytes_after: 1,
                }
            } else {
                TitleOutcome::Failed
            }
        });

        assert!(report.completed(), "got {:?}", report.stop_reason);
        assert_eq!(report.ledger.failed, 4);
        assert_eq!(report.ledger.rewritten, 1);
    }

    /// A skip is not evidence the host recovered. Alternating failures and
    /// skips must still trip the limit.
    #[test]
    fn a_skip_does_not_clear_the_failure_streak() {
        let n = std::cell::Cell::new(0);
        let report = drive_run(&limits(), &paths(20), healthy, |_| {
            let i = n.get();
            n.set(i + 1);
            if i % 2 == 0 {
                TitleOutcome::Failed
            } else {
                TitleOutcome::Skipped
            }
        });

        assert_eq!(
            report.stop_reason,
            StopReason::ConsecutiveFailures { count: 3, limit: 3 },
            "skips must not launder a failing host into a healthy-looking run"
        );
    }

    /// A library that is mostly already-optimal is the NORMAL case — 77.2% of
    /// this one. Skips must never look like a broken host.
    #[test]
    fn a_run_of_pure_skips_completes_rather_than_tripping_the_failure_limit() {
        let report = drive_run(&limits(), &paths(9), healthy, |_| TitleOutcome::Skipped);
        assert!(report.completed());
        assert_eq!(report.ledger.skipped, 9);
        assert_eq!(report.ledger.failed, 0);
    }

    #[test]
    fn cancelling_mid_run_stops_it_and_reports_the_unprocessed_remainder() {
        let n = std::cell::Cell::new(0);
        let report = drive_run(
            &limits(),
            &paths(10),
            || RunObservation {
                cancelled: n.get() >= 4,
                ..healthy()
            },
            |_| {
                n.set(n.get() + 1);
                TitleOutcome::Rewritten {
                    bytes_before: 10,
                    bytes_after: 2,
                }
            },
        );

        assert_eq!(report.stop_reason, StopReason::Cancelled);
        assert!(!report.completed());
        assert_eq!(report.ledger.rewritten, 4);
        assert_eq!(report.candidates_unprocessed, 6);
    }

    #[test]
    fn a_filling_disk_stops_the_run_before_the_next_title() {
        let n = std::cell::Cell::new(0u64);
        let report = drive_run(
            &limits(),
            &paths(10),
            || RunObservation {
                // Plenty of room for two titles, then below the floor.
                free_bytes: if n.get() < 2 { 100_000 } else { 999 },
                ..healthy()
            },
            |_| {
                n.set(n.get() + 1);
                TitleOutcome::Rewritten {
                    bytes_before: 10,
                    bytes_after: 2,
                }
            },
        );

        assert!(matches!(
            report.stop_reason,
            StopReason::FreeSpaceFloor { .. }
        ));
        assert_eq!(report.ledger.attempted, 2);
    }

    #[test]
    fn a_closed_kill_switch_processes_nothing_at_all() {
        let calls = std::cell::Cell::new(0);
        let report = drive_run(
            &limits(),
            &paths(10),
            || RunObservation {
                mutation_enabled: false,
                ..healthy()
            },
            |_| {
                calls.set(calls.get() + 1);
                TitleOutcome::Rewritten {
                    bytes_before: 1,
                    bytes_after: 1,
                }
            },
        );

        assert_eq!(calls.get(), 0, "not one file may be touched");
        assert_eq!(report.stop_reason, StopReason::MutationDisabled);
        assert_eq!(report.candidates_unprocessed, 10);
    }

    #[test]
    fn an_empty_candidate_list_completes_without_processing_anything() {
        let report = drive_run(&limits(), &[], healthy, |_| {
            panic!("nothing to process")
        });
        assert!(report.completed());
        assert_eq!(report.ledger.attempted, 0);
    }

    /// A rewrite that GREW the file is still counted honestly: the totals move,
    /// and `bytes_reclaimed` floors at zero rather than wrapping.
    #[test]
    fn a_run_that_grew_the_library_reports_zero_reclaimed_not_a_huge_number() {
        let report = drive_run(&limits(), &paths(3), healthy, |_| TitleOutcome::Rewritten {
            bytes_before: 10,
            bytes_after: 90,
        });
        assert_eq!(report.ledger.bytes_reclaimed(), 0);
        assert_eq!(report.ledger.bytes_before_total, 30);
        assert_eq!(report.ledger.bytes_after_total, 270);
    }

    // --- the handle --------------------------------------------------------

    #[test]
    fn only_one_run_may_claim_the_slot() {
        let h = RunHandle::new();
        assert!(h.try_claim(), "the first claim wins");
        assert!(
            !h.try_claim(),
            "a second concurrent sweep would race for the work dir and make the \
             free-space floor meaningless"
        );
        h.release();
        assert!(h.try_claim(), "the slot is reusable once released");
    }

    #[test]
    fn stopping_when_nothing_runs_says_so_rather_than_reporting_success() {
        let h = RunHandle::new();
        assert!(!h.request_stop(), "there was no run to stop");
    }

    #[test]
    fn stopping_an_active_run_reports_that_it_was_active() {
        let h = RunHandle::new();
        assert!(h.try_claim());
        assert!(h.request_stop());
    }

    #[test]
    fn releasing_clears_the_cancel_flag_so_the_next_run_is_not_born_cancelled() {
        let h = RunHandle::new();
        assert!(h.try_claim());
        h.request_stop();
        h.release();
        assert!(h.try_claim());
        assert!(
            !h.cancel.load(std::sync::atomic::Ordering::SeqCst),
            "a stale cancel would make the next run stop immediately with no \
             explanation an operator could distinguish from a broken run"
        );
    }

    /// **The bug two reviewers found independently.**
    ///
    /// `execute_run` calls into ffmpeg wrappers, path handling and probe
    /// parsing, any of which can panic on a strange enough file. With a bare
    /// `release()` at the end, an unwind would leave `active` set forever —
    /// and because `spawn_blocking` catches the panic, the process would
    /// survive in a state where every later run is refused and the status
    /// endpoint reports a phantom run. Only a restart would clear it.
    #[test]
    fn a_panic_while_holding_the_slot_still_releases_it() {
        let handle = RunHandle::new();

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = handle.claim().expect("the slot is free");
            assert!(handle.is_active());
            panic!("an encode blew up");
        }));

        assert!(panicked.is_err(), "the panic must actually have happened");
        assert!(
            !handle.is_active(),
            "the slot must be released on unwind, or every future run is refused \
             until the process restarts"
        );
        assert!(handle.claim().is_some(), "and the next run can claim it");
    }

    #[test]
    fn the_guard_releases_the_slot_on_a_normal_return_too() {
        let handle = RunHandle::new();
        {
            let _slot = handle.claim().expect("free");
            assert!(handle.is_active());
        }
        assert!(!handle.is_active());
    }

    #[test]
    fn only_one_guard_can_exist_at_a_time() {
        let handle = RunHandle::new();
        let first = handle.claim();
        assert!(first.is_some());
        assert!(
            handle.claim().is_none(),
            "a second concurrent sweep would race for the work dir"
        );
        drop(first);
        assert!(handle.claim().is_some());
    }

    /// A cancel requested during one run must not silently kill the next one.
    #[test]
    fn a_guard_release_clears_a_cancel_so_the_next_run_is_not_born_cancelled() {
        let handle = RunHandle::new();
        {
            let _slot = handle.claim().expect("free");
            handle.request_stop();
        }
        let _next = handle.claim().expect("free again");
        assert!(!handle.cancel.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn a_fresh_handle_is_not_active_and_has_no_progress() {
        let h = RunHandle::new();
        assert!(!h.is_active());
        assert!(h.snapshot().is_none());
    }

    // --- the confirmation gate ---------------------------------------------

    #[test]
    fn the_right_phrase_confirms_the_run() {
        assert!(check_confirm(10, Some("run 10 titles")).is_ok());
    }

    #[test]
    fn a_missing_confirmation_refuses_and_says_what_was_needed() {
        let err = check_confirm(10, None).unwrap_err();
        assert!(err.contains("run 10 titles"), "got {err:?}");
    }

    /// **The typo case this gate exists for.** An operator who meant 10 and
    /// typed 100 has a confirmation that no longer matches, so the larger run
    /// cannot start by accident.
    #[test]
    fn a_confirmation_for_a_different_size_refuses() {
        assert!(check_confirm(100, Some("run 10 titles")).is_err());
        assert!(check_confirm(10, Some("run 100 titles")).is_err());
    }

    #[test]
    fn a_vague_confirmation_does_not_pass() {
        for c in ["yes", "run", "confirm", "run titles", "RUN 10 TITLES", " run 10 titles"] {
            assert!(
                check_confirm(10, Some(c)).is_err(),
                "{c:?} must not confirm a destructive run"
            );
        }
    }

    #[test]
    fn the_phrase_names_the_actual_size() {
        assert_eq!(confirm_phrase(463), "run 463 titles");
        assert!(check_confirm(463, Some(&confirm_phrase(463))).is_ok());
    }

    /// **A truncated survey must not produce a "completed" run.**
    ///
    /// The survey has its own deadline and can stop early. Exhausting a PARTIAL
    /// candidate list still yields `NoMoreCandidates`, so without this the run
    /// would tell an operator the library was processed when most of it was
    /// never looked at. Raised at the FOUNDRY-11 gate.
    #[test]
    fn exhausting_a_partial_candidate_list_is_not_a_completed_run() {
        let report = RunReport {
            ledger: RunLedger::default(),
            stop_reason: StopReason::NoMoreCandidates,
            survey_complete: false,
            candidates_unprocessed: 0,
        };
        assert!(
            !report.completed(),
            "the list was exhausted but the library was never fully surveyed"
        );

        let complete = RunReport {
            survey_complete: true,
            ..report
        };
        assert!(complete.completed());
    }

    /// **The caller's answer must actually reach the report.**
    ///
    /// FSURV-02 S1. The test above proves the RULE — a report with
    /// `survey_complete: false` is not complete. It says nothing about whether
    /// the false ever gets there, because the assignment lived inside
    /// `execute_run`, which needs a live `Foundry` and no test can call. So
    /// `report.survey_complete = true` in place of the caller's value survived
    /// the whole suite: the rule was guarded and the wiring was not.
    ///
    /// `drive_run` always yields `survey_complete: true` — that is the input
    /// this has to overwrite, and it is why a mutation to `= true` is
    /// invisible to any test that only checks the happy case.
    #[test]
    fn the_callers_survey_completeness_overrides_the_drivers_optimistic_default() {
        let from_driver = drive_run(
            &limits(),
            &[PathBuf::from("/a.mkv")],
            || obs(0),
            |_| TitleOutcome::Skipped,
        );
        assert!(
            from_driver.survey_complete,
            "the driver cannot know, so it reports true and the caller corrects it"
        );

        let truncated = finish_report(from_driver.clone(), false);
        assert!(
            !truncated.survey_complete,
            "a truncated survey must survive the stamp"
        );
        assert!(
            !truncated.completed(),
            "and it must make the run read as NOT completed — the whole point"
        );
        // Nothing else may be rewritten on the way through.
        assert_eq!(truncated.ledger, from_driver.ledger);
        assert_eq!(truncated.stop_reason, from_driver.stop_reason);

        let whole = finish_report(from_driver.clone(), true);
        assert!(whole.survey_complete && whole.completed());
    }

    /// `execute_run` must pass the CALLER's value, not a literal.
    ///
    /// This is deliberately the weaker guard class, and it is worth naming
    /// why. `execute_run` needs a live `Foundry`, so the one line that binds
    /// the caller's `survey_complete` to [`finish_report`] cannot be reached
    /// from any test — substituting `true` for the parameter there still
    /// leaves the suite green (verified). A source-text assertion proves the
    /// wiring is WRITTEN; the test above proves the function it wires to
    /// BEHAVES. Neither replaces the other, and covering the remaining line
    /// properly would mean restructuring `execute_run`, which is a bigger
    /// change than this defect justifies.
    #[test]
    fn execute_run_hands_finish_report_the_callers_own_answer() {
        let body = include_str!("run.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("a non-test body");
        assert!(
            body.contains("finish_report(report, survey_complete)"),
            "a literal here would report a truncated survey as a completed library sweep"
        );
        assert!(
            body.contains("p.survey_complete = report.survey_complete;"),
            "the progress snapshot must read the report back rather than keep a \
             second copy of the same fact"
        );
    }

    /// **An idle stop request must not poison the next run.**
    ///
    /// Storing `cancel` unconditionally meant a stop sent while nothing was
    /// running stayed latched, and a run started hours later by someone who
    /// never saw that request died on its first title. Raised at the
    /// FOUNDRY-11 gate.
    #[test]
    fn a_stop_request_while_idle_does_not_cancel_the_next_run() {
        let handle = RunHandle::new();
        assert!(!handle.request_stop(), "nothing was running");

        let _slot = handle.claim().expect("the slot is free");
        assert!(
            !handle.cancel.load(std::sync::atomic::Ordering::SeqCst),
            "the next run must not be born cancelled by a stop nobody remembers"
        );
    }

    #[test]
    fn a_stop_request_during_a_run_still_cancels_that_run() {
        let handle = RunHandle::new();
        let _slot = handle.claim().expect("free");
        assert!(handle.request_stop());
        assert!(handle.cancel.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn the_conservative_default_is_actually_conservative() {
        let l = RunLimits::conservative();
        assert!(
            l.max_titles <= 25,
            "a first live run is a canary, not a sweep"
        );
        assert!(l.max_consecutive_failures >= 1);
        assert!(l.min_free_bytes > 0);
        assert!(l.deadline > Duration::ZERO);
    }
}
