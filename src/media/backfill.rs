//! S130-A `MPRB-07` (Plane MUSE #144) — the resumable, rate-limited probe
//! **backfill worker**: the thing that drains the queue MPRB-05 built and
//! MPRB-06 deliberately did not sweep.
//!
//! # What this exists to finish
//!
//! MPRB-06 closed the epic's founding defect — `media_info` can no longer be
//! derived from a filename, and new or changed files are probed on arrival. It
//! left the **16,221 files already in the library**, which have no document and
//! are not new and have not changed, to this item. It left them here rather than
//! sweeping them in the scan for three stated reasons: a scan-time sweep would be
//! unbounded, unresumable, and would duplicate a queue that already existed.
//! This module is the answer to all three, and it **calls** that queue rather
//! than writing a second one.
//!
//! # What it calls, and does not re-implement
//!
//! | Job | Function called | Owner |
//! |---|---|---|
//! | The queue | [`crate::repo::media_file::list_needing_probe`] | MPRB-05 — already attempt-bounded and keyset-paginated |
//! | Running `ffprobe` | [`crate::media::MediaCore::probe_async`] → `run_ffprobe_async` | MPRB-02 — already has the timeout, reap, output cap and argv `--` guard |
//! | Staying inside the library | [`crate::media::MediaCore::library_guard`] | MPRB-01 — read-only guard, `resolve` refuses anything outside `MUSE_LIBRARY_ROOT` |
//! | Persisting a result | [`crate::media::sink::ProbeSink`] → `set_probe_result` / `set_probe_error` | MPRB-05 — the only writers of `media_info` |
//! | Deciding what to write | [`crate::media::sink::probe_write`] | MPRB-06 — one definition, shared |
//! | Classifying a failure | [`crate::media::probe::ProbeError::is_retryable`] | MPRB-02 |
//! | Measuring what is left | [`crate::repo::media_file::probe_progress`] | MPRB-05 |
//!
//! **There is no `match` over `ProbeError` anywhere in this module.** The one
//! classification call is `is_retryable()`, and the stored state comes from
//! `StoredProbeState::from_error` inside MPRB-05's writer. A second `match` here
//! would be free to drift from the first the moment a variant is added.
//!
//! # Resumable, and how that is actually achieved
//!
//! The cursor is **not** a saved offset and there is no checkpoint file to lose.
//! [`crate::repo::media_file::list_needing_probe`]'s predicate is *"has no v1
//! document and has attempts left"*, so a file leaves the queue the moment its
//! result is persisted. A run that dies after 9,000 files leaves 9,000 rows that
//! the next run's very first page no longer selects. The in-run `after_id`
//! keyset cursor exists to make one **pass** monotonic (so a page cannot be
//! re-read and a failed row cannot be retried inside the same run); durability
//! across restarts is a property of the DB predicate, not of anything this
//! module remembers.
//!
//! One consequence is load-bearing: the cursor advances past **every** row the
//! run considered, including skipped and failed ones. A cursor that only
//! advanced on success would re-read the same failing row forever.
//! [`CursorGuard`] fails the run rather than looping if a page ever comes back
//! that does not advance it.
//!
//! # Rate-limited, and the limit reaches the loop
//!
//! The spec's figure was 30/min, chosen when the sweep's cost was unknown. It is
//! now measured — **~0.17s per probe, ~46 minutes for a full sweep** — so 30/min
//! is deliberate conservatism against a shared NFS mount and a live serving
//! path, not a hard bound; [`crate::config::Config::probe_backfill_rate_per_min`]
//! is how an operator changes it.
//!
//! A configured limit that never reaches the loop is the failure MPRB-02 already
//! paid for, so the pacing is a **collaborator**, not a `tokio::time::sleep` call
//! buried in the loop body: [`Pacer`] is passed in, and the tests assert on the
//! actual [`Duration`] the loop asks for, per probe. See
//! `the_configured_rate_reaches_the_probe_loop`.
//!
//! # Degrades, never fails
//!
//! `can_probe() == false` produces an inert, reported run —
//! [`HaltReason::NoFfprobeOnThisHost`], zero counters, no error. So does an inert
//! library guard. [`run_backfill`] never returns `Err`: a queue failure is a
//! recorded halt reason, because the operator's question is "what happened", and
//! a `500` with no counters does not answer it.
//!
//! # Metrics that are honest
//!
//! [`BackfillReport`] reports what happened — considered, probed, suspicious,
//! failed (split retryable/terminal), exhausted, persist-failed, skipped — plus
//! `remaining`, which is a **measured** `SELECT` over `media_files`, not a
//! projection. **There is deliberately no ETA and no completion estimate.** An
//! estimate computed from an average rate is a fabricated measurement, and this
//! epic has caught that shape repeatedly (`predicted_deletion_refusals` was wrong
//! by a factor of twenty). A test asserts no such field is ever serialised.
//!
//! # Testable without a database
//!
//! Every rule — the rate, the attempt policy, the cursor, the halt conditions,
//! the arithmetic — runs against fakes in this module's tests. The DB sits behind
//! [`ProbeQueue`], [`RootLookup`] and `ProbeSink`, each a thin dispatch onto a
//! function that already exists. That is the same shape MPRB-08 (pure core, a
//! lookup behind the pool), MPRB-09 (zero aggregation in the query) and MPRB-06
//! (a two-arm `ProbeSink`) used, and for the same reason: with no
//! `MUSE_TEST_DATABASE_URL` on the build host (MUSE #130), a rule expressed
//! behind a pool is a rule nobody verifies.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::media::probe::{MediaProbe, ProbeError};
use crate::media::sink::{probe_write, ProbeSink, ProbeWrite};
use crate::media::MediaCore;
use crate::models::media_file::MediaFile;
use crate::repo;
use crate::repo::library::MediaItemLocation;

// --- configuration ---------------------------------------------------------

/// Probes per minute when `MUSE_PROBE_BACKFILL_RATE_PER_MIN` is unset. See the
/// module doc for why this number is conservatism rather than a measurement.
pub const DEFAULT_RATE_PER_MIN: u32 = 30;
/// Rows per keyset page when `MUSE_PROBE_BACKFILL_BATCH` is unset. One page is
/// one round trip; at 30 probes/min a 200-row page is ~7 minutes of work, so the
/// queue read is negligible either way and a smaller page only adds round trips.
pub const DEFAULT_BATCH_SIZE: i64 = 200;
/// Failed attempts after which a file leaves the queue for good, when
/// `MUSE_PROBE_BACKFILL_MAX_ATTEMPTS` is unset.
pub const DEFAULT_MAX_ATTEMPTS: i32 = 3;
/// Files per run when `MUSE_PROBE_BACKFILL_MAX_FILES` is unset. `0` means "the
/// whole queue": the run is already bounded by the queue emptying, by the
/// per-probe timeout (MPRB-02) and by the halt conditions below.
pub const DEFAULT_MAX_FILES_PER_RUN: u64 = 0;

/// Bounds on the configured rate.
///
/// The floor is **1, not 0**, and that is the whole point of clamping this one:
/// `MUSE_PROBE_BACKFILL_RATE_PER_MIN=0` would divide by zero if taken literally
/// and would mean "never probe" if taken charitably — and a worker that silently
/// never probes is indistinguishable from a finished backfill. The ceiling is
/// 60,000/min (1 kHz), far above the ~350/min the measured 0.17s per probe can
/// physically reach, so it never constrains a real operator; it exists so
/// "unlimited" cannot be spelled as a number.
const MIN_RATE_PER_MIN: u32 = 1;
const MAX_RATE_PER_MIN: u32 = 60_000;

/// Bounds on the page size. A page of 0 would fetch nothing forever and read as
/// an empty queue.
const MIN_BATCH_SIZE: i64 = 1;
const MAX_BATCH_SIZE: i64 = 5_000;

/// Bounds on the attempt budget. A budget of 0 empties the queue permanently
/// (`probe_attempts < 0` matches nothing) — which, again, looks exactly like a
/// finished backfill.
const MIN_MAX_ATTEMPTS: i32 = 1;
const MAX_MAX_ATTEMPTS: i32 = 100;

/// Consecutive **retryable** failures after which the run halts.
///
/// This is what `is_retryable()` buys at the run level, and it is not
/// decoration. `ToolMissing` and `Spawn` are retryable *for the file* — they say
/// nothing bad about it — but they are host-level faults, and grinding on
/// through 16,221 perfectly readable files would burn one of each file's three
/// bounded attempts on a fault that has nothing to do with them. Three runs of
/// that and the entire library is `permanently_failed` with no bad file
/// anywhere. Terminal failures do **not** trip this: `ExitFailure` on ten files
/// in a row is ten broken files, which is exactly what the queue is for.
const HOST_FAULT_HALT_STREAK: u32 = 10;

/// The resolved, clamped knobs one run operates under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BackfillConfig {
    pub rate_per_min: u32,
    pub batch_size: i64,
    pub max_attempts: i32,
    /// `0` = the whole queue.
    pub max_files: u64,
}

impl Default for BackfillConfig {
    fn default() -> Self {
        Self {
            rate_per_min: DEFAULT_RATE_PER_MIN,
            batch_size: DEFAULT_BATCH_SIZE,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_files: DEFAULT_MAX_FILES_PER_RUN,
        }
    }
}

impl BackfillConfig {
    /// Resolve from the crate's single env door, clamping every value.
    ///
    /// Out-of-range values **clamp** rather than fall back to the default, the
    /// same rule `MediaCore` applies to the probe timeout: an operator who asked
    /// for more gets as much as is allowed, instead of silently getting the
    /// value they were trying to change.
    pub fn resolve(cfg: &crate::config::Config) -> Self {
        Self {
            rate_per_min: cfg
                .probe_backfill_rate_per_min
                .map(|v| v.clamp(MIN_RATE_PER_MIN, MAX_RATE_PER_MIN))
                .unwrap_or(DEFAULT_RATE_PER_MIN),
            batch_size: cfg
                .probe_backfill_batch
                .map(|v| v.clamp(MIN_BATCH_SIZE, MAX_BATCH_SIZE))
                .unwrap_or(DEFAULT_BATCH_SIZE),
            max_attempts: cfg
                .probe_backfill_max_attempts
                .map(|v| v.clamp(MIN_MAX_ATTEMPTS, MAX_MAX_ATTEMPTS))
                .unwrap_or(DEFAULT_MAX_ATTEMPTS),
            max_files: cfg
                .probe_backfill_max_files
                .unwrap_or(DEFAULT_MAX_FILES_PER_RUN),
        }
    }

    /// The minimum wall-clock spacing between two probes.
    ///
    /// Integer nanoseconds, not `from_secs_f64`: the rate is clamped to at least
    /// 1, so this cannot divide by zero, and an exact integer keeps 30/min
    /// exactly 2s rather than 1.9999999s.
    pub fn probe_interval(&self) -> Duration {
        Duration::from_nanos(60_000_000_000 / u64::from(self.rate_per_min.max(MIN_RATE_PER_MIN)))
    }
}

/// How long to wait after a probe that took `elapsed`, to honour `interval`.
///
/// Pure, and separate from the loop, because this is the arithmetic that decides
/// whether the configured rate means anything. A probe that already took longer
/// than the interval waits zero — the limiter's job is to stop the worker going
/// *faster* than the rate, not to slow a slow mount down further.
pub fn delay_after_probe(interval: Duration, elapsed: Duration) -> Duration {
    interval.saturating_sub(elapsed)
}

// --- the attempt policy ----------------------------------------------------

/// What one failure means for the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDisposition {
    /// The failure says nothing about the file (`ToolMissing`, `Spawn`,
    /// `Timeout`). It stays in the queue until its attempt budget runs out.
    Retryable,
    /// The failure is a property of the file (`ExitFailure`, `MalformedOutput`,
    /// `NoStreams`, `OutputTooLarge`) — re-running will say it again.
    Terminal,
    /// Enough consecutive retryable failures that this is a host fault, not a
    /// library of broken files. Stop, before the whole library's attempt budget
    /// is spent on it.
    HaltHostFault,
}

/// Tracks the consecutive-retryable-failure streak across one run.
///
/// Deliberately a `struct` with a reset, not a counter inlined in the loop: the
/// reset on success is the part that is easy to omit and impossible to notice,
/// and it is what makes the streak mean "the host is wedged **now**" rather than
/// "ten retryable failures happened at some point".
#[derive(Debug, Clone, Copy)]
pub struct FailurePolicy {
    consecutive_retryable: u32,
    halt_after: u32,
}

impl Default for FailurePolicy {
    fn default() -> Self {
        Self {
            consecutive_retryable: 0,
            halt_after: HOST_FAULT_HALT_STREAK,
        }
    }
}

impl FailurePolicy {
    pub fn with_halt_after(halt_after: u32) -> Self {
        Self {
            consecutive_retryable: 0,
            halt_after,
        }
    }

    /// Classify one failure. **`is_retryable()` is called, never restated** —
    /// there is no `match` over `ProbeError` in this module.
    pub fn on_failure(&mut self, error: &ProbeError) -> FailureDisposition {
        if !error.is_retryable() {
            // A broken file is not evidence about the host, so it neither trips
            // nor resets the streak: ten bad files interleaved with host stalls
            // must not mask the stalls.
            return FailureDisposition::Terminal;
        }
        self.consecutive_retryable = self.consecutive_retryable.saturating_add(1);
        if self.consecutive_retryable >= self.halt_after {
            FailureDisposition::HaltHostFault
        } else {
            FailureDisposition::Retryable
        }
    }

    /// A probe that produced a document. The host is evidently fine.
    pub fn on_success(&mut self) {
        self.consecutive_retryable = 0;
    }

    pub fn consecutive_retryable(&self) -> u32 {
        self.consecutive_retryable
    }
}

/// Whether the failure just recorded was this file's **last** allowed attempt.
///
/// `attempts_before` is `media_files.probe_attempts` as the queue read it;
/// `set_probe_error` increments it by exactly one. Reported so an operator can
/// see files leaving the queue for good in the run that lost them, rather than
/// inferring it later from a `permanently_failed` count that names nobody.
pub fn attempt_budget_exhausted(attempts_before: i32, max_attempts: i32) -> bool {
    attempts_before.saturating_add(1) >= max_attempts
}

// --- the cursor ------------------------------------------------------------

/// The in-run keyset cursor, with the one guard that makes the loop terminate.
///
/// A queue page that does not advance the cursor would be re-read forever. The
/// real query (`id > $1 ORDER BY id`) cannot produce one — but "cannot" is what
/// an unbounded loop is always built on, and this worker runs unattended for
/// hours against a network mount.
#[derive(Debug, Clone, Copy, Default)]
pub struct CursorGuard {
    after_id: i64,
}

impl CursorGuard {
    pub fn after_id(&self) -> i64 {
        self.after_id
    }

    /// Advance to `id`. Returns `false` when the id does not move the cursor
    /// forward — the caller must halt rather than continue.
    ///
    /// **`<=`, not `<`.** Standing still is the failure this guard exists for:
    /// a page whose last row repeats the cursor would be fetched again with the
    /// same `after_id` and returned again, forever. Equality is a stall, not
    /// progress. It is also what the real predicate already says — the query is
    /// `id > $1`, so a row equal to the cursor is a row the queue promised not
    /// to return.
    pub fn advance(&mut self, id: i64) -> bool {
        if id <= self.after_id {
            return false;
        }
        self.after_id = id;
        true
    }
}

// --- the report ------------------------------------------------------------

/// Why a run stopped early. `None` on the report means it drained the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HaltReason {
    /// `can_probe()` is false. The worker is inert and says so.
    NoFfprobeOnThisHost,
    /// `MUSE_LIBRARY_ROOT` is unset or did not resolve, so nothing can be
    /// resolved, so nothing can be probed.
    LibraryGuardInert,
    /// `MUSE_PROBE_BACKFILL_MAX_FILES` reached. Not a fault — the next run
    /// resumes where this one stopped, because the rows it probed have left the
    /// queue.
    MaxFilesReached,
    /// Consecutive retryable failures: see [`HOST_FAULT_HALT_STREAK`].
    HostFaultStreak,
    /// The queue query failed.
    QueueUnavailable,
    /// The library-root lookup failed.
    RootLookupUnavailable,
    /// A page came back that did not advance the keyset cursor. Never expected;
    /// halting is how the loop stays bounded if it ever is.
    CursorStalled,
}

impl HaltReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoFfprobeOnThisHost => "no_ffprobe_on_this_host",
            Self::LibraryGuardInert => "library_guard_inert",
            Self::MaxFilesReached => "max_files_reached",
            Self::HostFaultStreak => "host_fault_streak",
            Self::QueueUnavailable => "queue_unavailable",
            Self::RootLookupUnavailable => "root_lookup_unavailable",
            Self::CursorStalled => "cursor_stalled",
        }
    }
}

/// What one run did. Counts of events that happened — **no estimate, no ETA**.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BackfillReport {
    /// Rows taken off the queue and considered. Every one of them lands in
    /// exactly one of the outcome counters below; a test asserts that.
    pub considered: u64,
    /// Documents persisted: `ok` + `suspicious`.
    pub probed: u64,
    /// Subset of [`Self::probed`].
    pub suspicious: u64,
    /// A failure was recorded and the file stays in the queue.
    pub failed_retryable: u64,
    /// A failure was recorded and re-running would say the same thing.
    pub failed_terminal: u64,
    /// Subset of the two `failed_*` counters: this failure was the file's last
    /// allowed attempt, so it has left the queue for good.
    pub exhausted: u64,
    /// The probe produced a verdict and the **database** refused to record it.
    /// Deliberately not folded into a failure count: a Postgres outage is not a
    /// broken library, and the row was not touched.
    pub persist_failed: u64,
    /// Not probed: no library row to build an absolute path from, or a path that
    /// would not resolve inside `MUSE_LIBRARY_ROOT`. **No attempt was burned** —
    /// nothing was observed about the file, and spending one of its bounded
    /// attempts on a configuration fault is how a readable file becomes
    /// permanently failed.
    pub skipped_unresolved: u64,
    /// Keyset pages fetched.
    pub pages: u64,
    /// The cursor this run reached — the highest `media_files.id` it considered.
    /// Reported because it is the evidence of forward progress, not because the
    /// next run needs it (it does not; see the module doc on resumption).
    pub last_id: i64,
    pub halted: Option<HaltReason>,
    /// Files still in the queue, **measured** by
    /// [`crate::repo::media_file::probe_progress`] after the run, or `None` when
    /// that measurement was not taken or failed. Never inferred from the
    /// counters above.
    pub remaining: Option<i64>,
    /// Unprobed and out of attempts, measured in the same round trip. Stated
    /// beside `remaining` because a queue that is empty while this is nonzero is
    /// finished, not complete.
    pub permanently_failed: Option<i64>,
    /// The rate the run actually paced itself at. Reported so an operator
    /// reading a slow run does not have to guess whether their env var landed.
    pub rate_per_min: u32,
    /// Wall clock, measured. Present so a run can be compared against the
    /// measured 0.17s/probe — and deliberately NOT turned into a completion
    /// estimate here.
    pub elapsed_ms: u64,
}

impl BackfillReport {
    /// Every considered row lands in exactly one outcome bucket. `suspicious`
    /// and `exhausted` are subsets and are excluded.
    pub fn accounted(&self) -> u64 {
        self.probed
            + self.failed_retryable
            + self.failed_terminal
            + self.persist_failed
            + self.skipped_unresolved
    }

    /// True when every row this run considered was accounted for. A false here
    /// means a counter was missed on some path — which is how a report starts
    /// describing a run that did not happen.
    pub fn is_balanced(&self) -> bool {
        self.considered == self.accounted()
    }
}

/// Files still in the queue, from a measured [`crate::repo::media_file::ProbeProgress`].
///
/// `unprobed` counts rows with no `media_info_version`, and `permanently_failed`
/// is the subset of those that are out of attempts — exactly the rows
/// `list_needing_probe` excludes. So the queue is the difference, and this is
/// arithmetic over two measured counts, not a projection. Saturating, so a
/// concurrent write between the two aggregates cannot produce a negative
/// "remaining".
pub fn queue_remaining(progress: &repo::media_file::ProbeProgress) -> i64 {
    progress.unprobed.saturating_sub(progress.permanently_failed).max(0)
}

// --- the collaborators (everything that needs the world) -------------------

/// The queue and the progress measurement — the two DB reads this worker makes.
///
/// Both are one call onto a function MPRB-05 already wrote. Nothing here decides
/// anything.
#[async_trait]
pub(crate) trait ProbeQueue {
    async fn next_page(
        &self,
        after_id: i64,
        limit: i64,
        max_attempts: i32,
    ) -> MuseResult<Vec<MediaFile>>;

    async fn progress(&self, max_attempts: i32) -> MuseResult<repo::media_file::ProbeProgress>;
}

/// `media_item_id` → where its files live.
#[async_trait]
pub(crate) trait RootLookup {
    async fn locations(
        &self,
        media_item_ids: &[i64],
    ) -> MuseResult<HashMap<i64, MediaItemLocation>>;
}

/// Resolving a path inside the library and running `ffprobe` on it.
///
/// Takes the **candidates** [`candidate_paths`] produced, in order, rather than
/// one absolute path: which of them is the file is a filesystem question, and
/// the filesystem lives behind this trait. See [`candidate_paths`].
#[async_trait]
pub(crate) trait FileProber {
    async fn probe(&self, candidates: &[PathBuf]) -> ProbeOutcome;
}

/// What [`FileProber`] came back with.
#[derive(Debug)]
pub(crate) enum ProbeOutcome {
    /// `ffprobe` ran (or failed to). Either way something is known about the
    /// file and MPRB-05 gets told.
    Attempted(Result<MediaProbe, ProbeError>),
    /// The path never resolved inside `MUSE_LIBRARY_ROOT`. **Nothing** is known
    /// about the file, so nothing is written and no attempt is burned.
    Unresolved(String),
}

/// The rate limiter's effect on the world.
///
/// A trait rather than a `sleep` in the loop body so a test can assert on the
/// [`Duration`] the loop actually asks for. See the module doc.
#[async_trait]
pub(crate) trait Pacer {
    async fn pace(&self, delay: Duration);
}

// --- rebuilding the absolute path (MPRB-10) --------------------------------

/// Where a `media_files` row's bytes might be, in the order to try them.
///
/// # The claim this replaces, and the measurement that killed it
///
/// MPRB-07 rebuilt the absolute path as `libraries.root_folder /
/// media_files.relative_path`, on the stated belief that `relative_path` is
/// always relative to the library root because that is how
/// `library::scan::walk_media_files` forms it (`strip_prefix(root)`).
///
/// That belief was never executed against the live database. MPRB-10 did, over
/// all 12,873 rows, `stat`-ing every reconstructed path on the host that holds
/// the mount:
///
/// | reconstruction | rows whose file exists |
/// |---|---:|
/// | `root_folder / relative_path` (MPRB-07's, alone) | **1,260 (9.8%)** |
/// | `rebased item folder / relative_path` (alone) | 11,447 (88.9%) |
/// | either, this function | **12,705 (98.7%)** |
///
/// # Because there are two writers of `relative_path`, with two conventions
///
/// * `library::scan` writes it relative to the **library root**, item folder
///   included: `"Veronica Mars/Season 1/…mkv"`. 1,258 rows.
/// * `arr::ingest` copies Radarr/Sonarr's `relativePath`, which those services
///   define relative to the **item's own folder**, item folder excluded:
///   `"…mkv"` for a movie, `"Season 1/…mkv"` for an episode. 11,615 rows.
///
/// One column, two meanings, and nothing in the schema distinguishes them. So
/// this function does not guess: it offers both, and the prober takes whichever
/// resolves. Only 2 of 12,873 rows had both candidates exist; the scan
/// convention is offered first so that tie is resolved deterministically, and
/// this is the entirety of the ambiguity in the live table.
///
/// # Rebasing the item folder
///
/// `media_items.path` is absolute in **its source's** namespace: Radarr reports
/// `/media/Movies/…` for what Muse mounts at `/srv/media/Movies/…`. The portable
/// part is the suffix below the library's own directory name, so the rebase
/// finds the library root's final component in the item path and re-roots what
/// follows onto `root_folder`. An item path already inside `root_folder` is
/// used as-is. Anything else yields no second candidate rather than a guess.
///
/// Neither candidate is trusted: both are handed to
/// `MediaCore::library_guard().resolve`, which canonicalises and confines them
/// to `MUSE_LIBRARY_ROOT`. A `..` smuggled into `media_items.path` or
/// `relative_path` cannot escape by being offered here.
pub(crate) fn candidate_paths(location: &MediaItemLocation, relative_path: &str) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);
    out.push(PathBuf::from(&location.root_folder).join(relative_path));

    if let Some(folder) = rebase_item_folder(&location.root_folder, location.item_path.as_deref()) {
        let candidate = folder.join(relative_path);
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// Re-root an item folder recorded in another namespace onto this library root.
///
/// `None` when there is nothing to rebase (no item path) or no shared component
/// to rebase on — an unrecognisable path produces no candidate, never a
/// fabricated one.
pub(crate) fn rebase_item_folder(root_folder: &str, item_path: Option<&str>) -> Option<PathBuf> {
    let item_path = item_path.map(str::trim)?;
    let root = root_folder.trim_end_matches('/');
    if root.is_empty() {
        return None;
    }

    // Already in Muse's namespace: nothing to rebase.
    if item_path == root || item_path.starts_with(&format!("{root}/")) {
        return Some(PathBuf::from(item_path));
    }

    // The library's own directory name is the join point between the two
    // namespaces (`/media/TV Shows/X` ↔ `/srv/media/TV Shows/X`). The FIRST
    // occurrence, not the last: a library named `Movies` holding an item folder
    // called `Movies.2019` would otherwise rebase onto the item's own name.
    let name = root.rsplit('/').next().filter(|n| !n.is_empty())?;
    let needle = format!("/{name}/");
    let at = item_path.find(&needle)?;
    let suffix = &item_path[at + needle.len()..];
    if suffix.is_empty() {
        return None;
    }
    Some(PathBuf::from(root).join(suffix))
}

/// Production: the queue and progress functions MPRB-05 wrote.
pub(crate) struct DbProbeQueue<'a>(pub(crate) &'a PgPool);

#[async_trait]
impl ProbeQueue for DbProbeQueue<'_> {
    async fn next_page(
        &self,
        after_id: i64,
        limit: i64,
        max_attempts: i32,
    ) -> MuseResult<Vec<MediaFile>> {
        repo::media_file::list_needing_probe(self.0, after_id, limit, max_attempts).await
    }

    async fn progress(&self, max_attempts: i32) -> MuseResult<repo::media_file::ProbeProgress> {
        repo::media_file::probe_progress(self.0, max_attempts).await
    }
}

/// Production: one `IN` lookup, no logic.
pub(crate) struct DbRootLookup<'a>(pub(crate) &'a PgPool);

#[async_trait]
impl RootLookup for DbRootLookup<'_> {
    async fn locations(
        &self,
        media_item_ids: &[i64],
    ) -> MuseResult<HashMap<i64, MediaItemLocation>> {
        repo::library::locations_for_media_items(self.0, media_item_ids).await
    }
}

/// Production: MPRB-01's read-only guard, then MPRB-02's bounded spawn. Exactly
/// the path MPRB-06 probes on, and for the same reason — `ResolvedPath` is the
/// type that says a file is inside `MUSE_LIBRARY_ROOT`, and the backfill is not
/// exempt from proving it for a path it rebuilt from a database row.
pub(crate) struct MediaCoreProber<'a>(pub(crate) &'a MediaCore);

#[async_trait]
impl FileProber for MediaCoreProber<'_> {
    async fn probe(&self, candidates: &[PathBuf]) -> ProbeOutcome {
        let mut refusals: Vec<String> = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            match self.0.library_guard().resolve(candidate) {
                // The first candidate that is a real file inside the library
                // wins. `resolve` is what decides that — this loop never calls
                // `exists()` itself, so a path that resolves but escapes the
                // root is refused here exactly as it was before MPRB-10.
                Ok(resolved) => return ProbeOutcome::Attempted(self.0.probe_async(&resolved).await),
                Err(e) => refusals.push(format!("{}: {e}", candidate.display())),
            }
        }
        // Every candidate refused. Report ALL of them: "no such file" against
        // one reconstruction, when a second was tried and also failed, is not
        // the diagnostic an operator needs.
        ProbeOutcome::Unresolved(if refusals.is_empty() {
            "no candidate path could be built for this row".to_string()
        } else {
            refusals.join("; ")
        })
    }
}

/// Production: real time.
pub(crate) struct SleepPacer;

#[async_trait]
impl Pacer for SleepPacer {
    async fn pace(&self, delay: Duration) {
        if delay.is_zero() {
            return;
        }
        tokio::time::sleep(delay).await;
    }
}

// --- the loop --------------------------------------------------------------

/// Drain the probe backfill queue. **Never returns `Err`** — see [`HaltReason`].
///
/// Generic over every edge that touches the world, so this whole function runs
/// for real in the tests below with no database, no filesystem and no clock.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_backfill(
    queue: &(dyn ProbeQueue + Send + Sync),
    roots: &(dyn RootLookup + Send + Sync),
    prober: &(dyn FileProber + Send + Sync),
    sink: &(dyn ProbeSink + Send + Sync),
    pacer: &(dyn Pacer + Send + Sync),
    config: BackfillConfig,
) -> BackfillReport {
    let started = Instant::now();
    let interval = config.probe_interval();
    let mut report = BackfillReport {
        rate_per_min: config.rate_per_min,
        ..Default::default()
    };
    let mut cursor = CursorGuard::default();
    let mut policy = FailurePolicy::default();

    'pages: loop {
        let page = match queue
            .next_page(cursor.after_id(), config.batch_size, config.max_attempts)
            .await
        {
            Ok(page) => page,
            Err(e) => {
                tracing::warn!(error = %e, after_id = cursor.after_id(), "probe backfill: queue read failed; halting this run");
                report.halted = Some(HaltReason::QueueUnavailable);
                break 'pages;
            }
        };

        if page.is_empty() {
            break 'pages;
        }
        report.pages += 1;

        // One lookup per page, not per file: the whole page usually belongs to a
        // handful of libraries, and 200 round trips to answer the same question
        // is the cost this batching exists to avoid.
        let item_ids: Vec<i64> = page.iter().map(|f| f.media_item_id).collect();
        let root_map = match roots.locations(&item_ids).await {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(error = %e, "probe backfill: library root lookup failed; halting this run");
                report.halted = Some(HaltReason::RootLookupUnavailable);
                break 'pages;
            }
        };

        for file in page {
            if config.max_files > 0 && report.considered >= config.max_files {
                report.halted = Some(HaltReason::MaxFilesReached);
                break 'pages;
            }
            if !cursor.advance(file.id) {
                tracing::error!(
                    id = file.id,
                    after_id = cursor.after_id(),
                    "probe backfill: the queue returned a row that does not advance the cursor; halting rather than re-reading it forever"
                );
                report.halted = Some(HaltReason::CursorStalled);
                break 'pages;
            }
            report.considered += 1;
            report.last_id = cursor.after_id();

            let Some(location) = root_map.get(&file.media_item_id) else {
                // No library row: the absolute path cannot be rebuilt, so
                // nothing was observed and nothing is written.
                report.skipped_unresolved += 1;
                tracing::warn!(
                    id = file.id,
                    media_item_id = file.media_item_id,
                    "probe backfill: no library root for this file's media item; not probing it"
                );
                continue;
            };

            let candidates = candidate_paths(location, &file.relative_path);
            let probe_started = Instant::now();
            let outcome = prober.probe(&candidates).await;

            let result = match outcome {
                ProbeOutcome::Attempted(result) => result,
                ProbeOutcome::Unresolved(reason) => {
                    report.skipped_unresolved += 1;
                    tracing::warn!(
                        id = file.id,
                        path = %file.relative_path,
                        error = %reason,
                        "probe backfill: this file did not resolve inside MUSE_LIBRARY_ROOT; not probing it"
                    );
                    continue;
                }
            };

            let write = probe_write(&result);
            let disposition = match &write {
                ProbeWrite::Document { .. } => {
                    policy.on_success();
                    None
                }
                ProbeWrite::Failure { error } => {
                    let disposition = policy.on_failure(error);
                    tracing::warn!(
                        id = file.id,
                        path = %file.relative_path,
                        error = %error,
                        retryable = error.is_retryable(),
                        "probe backfill: probe produced no usable answer; recording the failure"
                    );
                    Some(disposition)
                }
            };

            // Counted from what was WRITTEN, not from what was observed — the
            // rule MPRB-06 established. A counter incremented before the write
            // and left standing when the write is refused is a claim about the
            // database that the database never agreed to.
            match sink.record(file.id, &file.relative_path, &write).await {
                Ok(()) => match (&write, disposition) {
                    (ProbeWrite::Document { suspicion, .. }, _) => {
                        report.probed += 1;
                        if suspicion.is_some() {
                            report.suspicious += 1;
                        }
                    }
                    (ProbeWrite::Failure { .. }, Some(disposition)) => {
                        if disposition == FailureDisposition::Terminal {
                            report.failed_terminal += 1;
                        } else {
                            report.failed_retryable += 1;
                        }
                        if attempt_budget_exhausted(file.probe_attempts, config.max_attempts) {
                            report.exhausted += 1;
                        }
                    }
                    // Unreachable by construction: `disposition` is `Some`
                    // exactly when `write` is a `Failure`. Counted as a failure
                    // rather than silently dropped so the report stays balanced
                    // if that ever stops being true.
                    //
                    // DISCLOSED: no test kills a mutation of this line, and no
                    // test can — reaching it requires violating the invariant
                    // twenty lines above. It is defensive, it is unreachable,
                    // and it is not covered. `unreachable!()` was declined: a
                    // panic here would take an eight-hour sweep down to protect
                    // a counter.
                    (ProbeWrite::Failure { .. }, None) => report.failed_retryable += 1,
                },
                Err(e) => {
                    report.persist_failed += 1;
                    tracing::warn!(
                        id = file.id,
                        error = %e,
                        "probe backfill: could not record the probe outcome"
                    );
                }
            }

            // Paced per probe, and only for files that actually spawned one: a
            // skipped row costs the mount nothing, and making it wait would slow
            // the drain for no reason.
            pacer
                .pace(delay_after_probe(interval, probe_started.elapsed()))
                .await;

            if disposition == Some(FailureDisposition::HaltHostFault) {
                tracing::error!(
                    consecutive = policy.consecutive_retryable(),
                    "probe backfill: too many consecutive retryable failures — this is a host fault, \
                     not a library of broken files. Halting before the whole library's attempt \
                     budget is spent on it."
                );
                report.halted = Some(HaltReason::HostFaultStreak);
                break 'pages;
            }
        }
    }

    // Measured, once, at the end. `None` when it could not be taken — an
    // unavailable measurement is reported as absent, never as zero.
    match queue.progress(config.max_attempts).await {
        Ok(progress) => {
            report.remaining = Some(queue_remaining(&progress));
            report.permanently_failed = Some(progress.permanently_failed);
        }
        Err(e) => {
            tracing::warn!(error = %e, "probe backfill: could not measure what is left; reporting it as unknown");
        }
    }

    report.elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    report
}

/// Production wiring: the four DB/host edges, and the degrade checks that keep a
/// host without `ffprobe` inert and reportable rather than erroring.
pub async fn run_from_pool(
    pool: &PgPool,
    media: &MediaCore,
    config: BackfillConfig,
) -> BackfillReport {
    // An inert run is still a RUN, and it is exported as one. An operator whose
    // host lost `ffprobe` must not see the backfill simply vanish from
    // `muse_probe_backfill_runs_total` — silence is the one signal that reads
    // identically to "nobody asked for a run", which is the opposite of what
    // happened. It exports with `remaining` absent, so nothing claims the queue
    // was measured.
    let inert = |reason: HaltReason| {
        let report = BackfillReport {
            halted: Some(reason),
            rate_per_min: config.rate_per_min,
            ..Default::default()
        };
        crate::metrics::record_probe_backfill(&report);
        report
    };

    if !media.can_probe() {
        tracing::warn!(
            "probe backfill: ffprobe is not usable on this host — the worker is inert this run \
             (Module Contract §2: an absent backend capability leaves the module inert, never broken)"
        );
        return inert(HaltReason::NoFfprobeOnThisHost);
    }
    if media.library_guard_is_inert() {
        tracing::warn!(
            "probe backfill: the media core's library guard is inert (MUSE_LIBRARY_ROOT unset or \
             unresolvable) — no file would resolve, so the worker is inert this run"
        );
        return inert(HaltReason::LibraryGuardInert);
    }

    let report = run_backfill(
        &DbProbeQueue(pool),
        &DbRootLookup(pool),
        &MediaCoreProber(media),
        &crate::media::sink::DbProbeSink(pool),
        &SleepPacer,
        config,
    )
    .await;

    crate::metrics::record_probe_backfill(&report);
    report
}

// --- the run gate (one run at a time, and what the last one did) -----------

/// Whether a run is in flight, and the last finished run's report.
///
/// A type rather than a bare pair of statics so the tests below drive **this**
/// logic instead of a process-global that other tests share. MPRB-06 was bitten
/// by exactly that: a fixture racing another test in a multithreaded binary
/// silently changed which path was under test while still passing.
#[derive(Debug, Default)]
pub struct RunGate {
    inner: Mutex<GateState>,
}

#[derive(Debug, Default)]
struct GateState {
    running: bool,
    last: Option<BackfillReport>,
}

/// Held for the duration of a run; releases the gate when dropped, including on
/// a panic. A boolean cleared at the end of the happy path is a boolean that
/// stays set forever the first time something unwinds.
#[derive(Debug)]
pub struct RunPermit<'a> {
    gate: &'a RunGate,
}

impl RunGate {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(GateState {
                running: false,
                last: None,
            }),
        }
    }

    /// Claim the gate, or `None` when a run is already in flight.
    pub fn try_begin(&self) -> Option<RunPermit<'_>> {
        let mut state = self.lock();
        if state.running {
            return None;
        }
        state.running = true;
        Some(RunPermit { gate: self })
    }

    pub fn is_running(&self) -> bool {
        self.lock().running
    }

    /// The last finished run's report, if there has been one.
    pub fn last_report(&self) -> Option<BackfillReport> {
        self.lock().last.clone()
    }

    fn finish(&self, report: Option<BackfillReport>) {
        let mut state = self.lock();
        state.running = false;
        if let Some(report) = report {
            state.last = Some(report);
        }
    }

    /// A poisoned mutex must not take the backfill's status surface with it: the
    /// guarded state is two plain values, and there is no invariant a panicking
    /// holder could have left broken.
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl RunPermit<'_> {
    /// Record the run's outcome and release the gate.
    pub fn complete(self, report: BackfillReport) {
        self.gate.finish(Some(report));
        std::mem::forget(self);
    }
}

impl Drop for RunPermit<'_> {
    fn drop(&mut self) {
        // Reached only when a run ended WITHOUT a report — a panic, or an early
        // return. The gate reopens; the last report is left as it was, because
        // this run has nothing truthful to say about itself.
        self.gate.finish(None);
    }
}

/// The process-wide gate the ops handler uses.
pub fn global_gate() -> &'static RunGate {
    static GATE: std::sync::OnceLock<RunGate> = std::sync::OnceLock::new();
    GATE.get_or_init(RunGate::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::probe::parse_probe_json;
    use std::sync::Arc;

    // ---- fixtures ---------------------------------------------------------

    /// A real probe from the committed golden corpus (MPRB-04) — so "what the
    /// file says" is not a document these tests wrote for themselves.
    fn golden_probe() -> MediaProbe {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden/probe/dv_hdr_hevc_4k.json");
        parse_probe_json(&std::fs::read_to_string(path).expect("read the golden fixture"))
            .expect("the golden fixture must parse")
    }

    fn a_file(id: i64, media_item_id: i64, attempts: i32) -> MediaFile {
        MediaFile {
            id,
            media_item_id,
            relative_path: format!("Movie {id}/Movie {id}.mkv"),
            original_file_path: None,
            size_bytes: Some(1),
            date_added: None,
            scene_name: None,
            media_info: None,
            media_info_version: None,
            probed_at: None,
            probe_state: None,
            probe_error: None,
            probe_attempts: attempts,
            release_group: None,
            edition: None,
            languages: Vec::new(),
            subtitles: Vec::new(),
            indexer_flags: 0,
            release_type: crate::models::media_file::ReleaseTypeKind::Single,
            quality_tier_id: None,
            revision_version: 0,
            revision_real: 0,
            revision_is_repack: false,
            created_at: chrono::Utc::now(),
        }
    }

    /// A queue whose pages are served from a list of rows, applying the SAME
    /// `id > after_id` keyset rule the real query applies — so a resumption
    /// assertion is about the rule, not about a fake that always returns
    /// everything.
    struct FakeQueue {
        rows: Mutex<Vec<MediaFile>>,
        /// Every `(after_id, limit)` the loop asked for, in order.
        asked: Mutex<Vec<(i64, i64)>>,
        progress: repo::media_file::ProbeProgress,
        fail_after_pages: Option<u64>,
        /// The end-of-run `probe_progress` measurement is unavailable. Added
        /// because a mutation survived: setting `remaining = Some(0)` on that
        /// failure changed nothing any test observed.
        fail_progress: bool,
    }

    impl FakeQueue {
        fn new(rows: Vec<MediaFile>) -> Self {
            Self {
                rows: Mutex::new(rows),
                asked: Mutex::new(Vec::new()),
                progress: repo::media_file::ProbeProgress::default(),
                fail_after_pages: None,
                fail_progress: false,
            }
        }

        fn asked(&self) -> Vec<(i64, i64)> {
            self.asked.lock().unwrap().clone()
        }

        /// Simulate the effect of a persisted result: the row leaves the queue.
        fn drop_rows(&self, ids: &[i64]) {
            self.rows.lock().unwrap().retain(|r| !ids.contains(&r.id));
        }
    }

    #[async_trait]
    impl ProbeQueue for FakeQueue {
        async fn next_page(
            &self,
            after_id: i64,
            limit: i64,
            _max_attempts: i32,
        ) -> MuseResult<Vec<MediaFile>> {
            self.asked.lock().unwrap().push((after_id, limit));
            yield_to_the_timer().await;
            if let Some(n) = self.fail_after_pages {
                if self.asked.lock().unwrap().len() as u64 > n {
                    return Err(crate::error::MuseError::Config("queue is down".into()));
                }
            }
            let rows = self.rows.lock().unwrap();
            Ok(rows
                .iter()
                .filter(|r| r.id > after_id)
                .take(limit as usize)
                .cloned()
                .collect())
        }

        async fn progress(
            &self,
            _max_attempts: i32,
        ) -> MuseResult<repo::media_file::ProbeProgress> {
            if self.fail_progress {
                return Err(crate::error::MuseError::Config("progress is down".into()));
            }
            Ok(self.progress.clone())
        }
    }

    /// Every media item lives in one library at `/library`.
    struct FakeRoots {
        /// Item ids deliberately WITHOUT a library row.
        missing: Vec<i64>,
        fail: bool,
        /// `media_items.path` for every item, when the fixture wants the
        /// second (arr-convention) candidate to exist. `None` is the shape of
        /// a scan-created item and yields one candidate.
        item_path: Option<String>,
    }

    impl FakeRoots {
        fn all() -> Self {
            Self {
                missing: Vec::new(),
                fail: false,
                item_path: None,
            }
        }
    }

    #[async_trait]
    impl RootLookup for FakeRoots {
        async fn locations(&self, ids: &[i64]) -> MuseResult<HashMap<i64, MediaItemLocation>> {
            if self.fail {
                return Err(crate::error::MuseError::Config("libraries are down".into()));
            }
            Ok(ids
                .iter()
                .filter(|id| !self.missing.contains(id))
                .map(|id| {
                    (
                        *id,
                        MediaItemLocation {
                            root_folder: "/library".to_string(),
                            item_path: self.item_path.clone(),
                        },
                    )
                })
                .collect())
        }
    }

    /// Serves a scripted outcome per call and records the absolute paths it was
    /// handed.
    struct ScriptedProber {
        script: Mutex<Vec<ProbeOutcome>>,
        default_ok: bool,
        seen: Mutex<Vec<PathBuf>>,
        /// One entry per call: the WHOLE candidate list the loop offered, in
        /// order. `seen` flattens these; this keeps the grouping, which is the
        /// only way to assert what `candidate_paths` actually handed over.
        offered: Mutex<Vec<Vec<PathBuf>>>,
    }

    impl ScriptedProber {
        fn always_ok() -> Self {
            Self {
                script: Mutex::new(Vec::new()),
                default_ok: true,
                seen: Mutex::new(Vec::new()),
                offered: Mutex::new(Vec::new()),
            }
        }

        fn scripted(outcomes: Vec<ProbeOutcome>) -> Self {
            Self {
                script: Mutex::new(outcomes),
                default_ok: true,
                seen: Mutex::new(Vec::new()),
                offered: Mutex::new(Vec::new()),
            }
        }

        fn seen(&self) -> Vec<PathBuf> {
            self.seen.lock().unwrap().clone()
        }

        fn offered(&self) -> Vec<Vec<PathBuf>> {
            self.offered.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl FileProber for ScriptedProber {
        async fn probe(&self, candidates: &[PathBuf]) -> ProbeOutcome {
            self.offered.lock().unwrap().push(candidates.to_vec());
            self.seen.lock().unwrap().extend(candidates.iter().cloned());
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                assert!(self.default_ok, "the script ran out");
                return ProbeOutcome::Attempted(Ok(golden_probe()));
            }
            script.remove(0)
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Recorded {
        Document { id: i64, suspicious: bool },
        Failure { id: i64, error: String },
    }

    struct RecordingSink {
        seen: Mutex<Vec<Recorded>>,
        refuse: bool,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                refuse: false,
            }
        }

        fn seen(&self) -> Vec<Recorded> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ProbeSink for RecordingSink {
        async fn record(
            &self,
            media_file_id: i64,
            _relative_path: &str,
            write: &ProbeWrite<'_>,
        ) -> MuseResult<()> {
            if self.refuse {
                return Err(crate::error::MuseError::Config("the database said no".into()));
            }
            self.seen.lock().unwrap().push(match write {
                ProbeWrite::Document { suspicion, .. } => Recorded::Document {
                    id: media_file_id,
                    suspicious: suspicion.is_some(),
                },
                ProbeWrite::Failure { error } => Recorded::Failure {
                    id: media_file_id,
                    error: error.to_string(),
                },
            });
            Ok(())
        }
    }

    /// Records every delay the loop asks for, and waits for none of them — so
    /// the rate is asserted on rather than slept through, and a 30/min test does
    /// not take two seconds per file.
    #[derive(Default)]
    struct RecordingPacer {
        delays: Mutex<Vec<Duration>>,
    }

    impl RecordingPacer {
        fn delays(&self) -> Vec<Duration> {
            self.delays.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Pacer for RecordingPacer {
        async fn pace(&self, delay: Duration) {
            self.delays.lock().unwrap().push(delay);
            yield_to_the_timer().await;
        }
    }

    /// **The anti-hang bound below does not work without this, and that was
    /// found by mutation, not by reasoning.**
    ///
    /// `tokio::time::timeout(d, f)` only fires when `f` returns `Pending` — the
    /// timer is checked between polls, and a future that never yields is never
    /// interrupted. Every collaborator in these tests is a fake that completes
    /// immediately, so a loop that fails to terminate is a tight loop of
    /// always-ready `await`s: it wedges the whole test binary at 100% CPU and
    /// reports nothing, which is precisely the failure mode the bound exists to
    /// prevent. (Observed: mutating `CursorGuard::advance` back to `<` spun the
    /// suite for twenty minutes with the timeout in place and no output.)
    ///
    /// One yield per loop iteration hands control back to the runtime, so the
    /// timer fires, `timeout` returns `Err`, and the wedge becomes a test
    /// FAILURE with a message. It is in the two collaborators every iteration
    /// passes through — the pacer and the queue read.
    async fn yield_to_the_timer() {
        tokio::task::yield_now().await;
    }

    /// Every loop test runs under a wall-clock bound: a regression that wedges
    /// the loop must FAIL, not hang the suite with no output. See
    /// [`yield_to_the_timer`] for why the bound needs the fakes' cooperation.
    async fn bounded<F: std::future::Future<Output = BackfillReport>>(f: F) -> BackfillReport {
        tokio::time::timeout(Duration::from_secs(20), f)
            .await
            .expect("the backfill loop must terminate; it did not")
    }

    // ---- the rate limiter -------------------------------------------------

    #[test]
    fn the_default_rate_is_thirty_a_minute_and_that_is_two_seconds_apart() {
        let config = BackfillConfig::default();
        assert_eq!(config.rate_per_min, DEFAULT_RATE_PER_MIN);
        assert_eq!(config.probe_interval(), Duration::from_secs(2));
    }

    #[test]
    fn a_faster_configured_rate_is_a_shorter_interval() {
        let fast = BackfillConfig {
            rate_per_min: 120,
            ..Default::default()
        };
        assert_eq!(fast.probe_interval(), Duration::from_millis(500));
        assert!(
            fast.probe_interval() < BackfillConfig::default().probe_interval(),
            "a higher rate must not produce a longer interval"
        );
    }

    #[test]
    fn a_zero_rate_cannot_be_configured_and_never_divides_by_zero() {
        let cfg = crate::config::Config {
            probe_backfill_rate_per_min: Some(0),
            ..Default::default()
        };
        let resolved = BackfillConfig::resolve(&cfg);
        assert_eq!(
            resolved.rate_per_min, MIN_RATE_PER_MIN,
            "0/min would be a worker that never probes, which is indistinguishable \
             from a finished backfill"
        );
        assert_eq!(resolved.probe_interval(), Duration::from_secs(60));
    }

    #[test]
    fn out_of_range_knobs_clamp_rather_than_fall_back_to_the_default() {
        let cfg = crate::config::Config {
            probe_backfill_rate_per_min: Some(u32::MAX),
            probe_backfill_batch: Some(1_000_000),
            probe_backfill_max_attempts: Some(0),
            ..Default::default()
        };
        let resolved = BackfillConfig::resolve(&cfg);
        assert_eq!(resolved.rate_per_min, MAX_RATE_PER_MIN);
        assert_eq!(resolved.batch_size, MAX_BATCH_SIZE);
        assert_eq!(
            resolved.max_attempts, MIN_MAX_ATTEMPTS,
            "0 attempts would empty the queue permanently"
        );
        assert_ne!(resolved.rate_per_min, DEFAULT_RATE_PER_MIN);
    }

    #[test]
    fn unset_knobs_are_the_compiled_defaults() {
        assert_eq!(
            BackfillConfig::resolve(&crate::config::Config::default()),
            BackfillConfig::default()
        );
    }

    #[test]
    fn a_probe_slower_than_the_interval_waits_no_longer() {
        assert_eq!(
            delay_after_probe(Duration::from_secs(2), Duration::from_secs(5)),
            Duration::ZERO,
            "the limiter stops the worker going FASTER than the rate; it does not \
             slow a slow mount down further"
        );
    }

    #[test]
    fn a_fast_probe_waits_out_the_rest_of_the_interval() {
        assert_eq!(
            delay_after_probe(Duration::from_secs(2), Duration::from_millis(170)),
            Duration::from_millis(1_830),
            "0.17s is the MEASURED per-probe cost; at 30/min the rest of the \
             interval is what the limiter is for"
        );
    }

    /// The mutation MPRB-02 paid for: a configured limit that never reaches the
    /// work. Asserts on the delay the loop ACTUALLY asked for, per probe.
    #[tokio::test]
    async fn the_configured_rate_reaches_the_probe_loop() {
        let queue = FakeQueue::new((1..=3).map(|id| a_file(id, id, 0)).collect());
        let pacer = RecordingPacer::default();
        let config = BackfillConfig {
            rate_per_min: 60, // one second apart
            ..Default::default()
        };

        let report = bounded(run_backfill(
            &queue,
            &FakeRoots::all(),
            &ScriptedProber::always_ok(),
            &RecordingSink::new(),
            &pacer,
            config,
        ))
        .await;

        assert_eq!(report.probed, 3);
        let delays = pacer.delays();
        assert_eq!(delays.len(), 3, "one pace per probe, not one per page");
        for delay in &delays {
            assert!(
                *delay > Duration::from_millis(900) && *delay <= Duration::from_secs(1),
                "the loop must pace at the CONFIGURED interval (1s), got {delay:?} — a \
                 rate that does not reach the loop is not a rate"
            );
        }
        assert_eq!(report.rate_per_min, 60);
    }

    /// The control for the test above: a DIFFERENT configured rate must produce
    /// a different delay. Without this, a loop hardcoding one interval would
    /// pass the assertion above forever.
    #[tokio::test]
    async fn a_different_configured_rate_produces_a_different_delay() {
        let pacer = RecordingPacer::default();
        let report = bounded(run_backfill(
            &FakeQueue::new(vec![a_file(1, 1, 0)]),
            &FakeRoots::all(),
            &ScriptedProber::always_ok(),
            &RecordingSink::new(),
            &pacer,
            BackfillConfig {
                rate_per_min: 6, // ten seconds apart
                ..Default::default()
            },
        ))
        .await;

        assert_eq!(report.probed, 1);
        let delay = pacer.delays()[0];
        assert!(
            delay > Duration::from_secs(9) && delay <= Duration::from_secs(10),
            "6/min must pace at ~10s, got {delay:?}"
        );
    }

    #[tokio::test]
    async fn a_skipped_file_is_not_paced_for() {
        let pacer = RecordingPacer::default();
        let report = bounded(run_backfill(
            &FakeQueue::new(vec![a_file(1, 1, 0), a_file(2, 2, 0)]),
            &FakeRoots {
                missing: vec![2],
                fail: false,
                item_path: None,
            },
            &ScriptedProber::always_ok(),
            &RecordingSink::new(),
            &pacer,
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(report.probed, 1);
        assert_eq!(report.skipped_unresolved, 1);
        assert_eq!(
            pacer.delays().len(),
            1,
            "a file that never touched the mount must not consume rate budget"
        );
    }

    /// The sibling of the test above, and it exists because a mutation SURVIVED.
    ///
    /// There are **two** skip paths — a file with no library row (above) and a
    /// file whose rebuilt path will not resolve inside `MUSE_LIBRARY_ROOT`
    /// (here) — and the first test only covered the first. Making the *second*
    /// one consume rate budget changed nothing that any test observed. That is
    /// "unreached by any test", not "called but not covered", and the fix is a
    /// test, not an assertion.
    #[tokio::test]
    async fn a_file_that_does_not_resolve_is_not_paced_for_either() {
        let pacer = RecordingPacer::default();
        let report = bounded(run_backfill(
            &FakeQueue::new(vec![a_file(1, 1, 0), a_file(2, 2, 0)]),
            &FakeRoots::all(),
            &ScriptedProber::scripted(vec![
                ProbeOutcome::Unresolved("outside every allowed root".into()),
                ProbeOutcome::Attempted(Ok(golden_probe())),
            ]),
            &RecordingSink::new(),
            &pacer,
            BackfillConfig {
                batch_size: 10,
                ..Default::default()
            },
        ))
        .await;

        assert_eq!(report.skipped_unresolved, 1, "fixture check: the first file must be the unresolved one");
        assert_eq!(report.probed, 1);
        assert_eq!(
            pacer.delays().len(),
            1,
            "a path that never resolved never touched the mount, so it must not \
             consume rate budget — the same rule as the missing-library-row skip"
        );
    }

    // ---- resumption -------------------------------------------------------

    #[tokio::test]
    async fn the_queue_is_read_by_keyset_cursor_and_never_re_reads_a_page() {
        let queue = FakeQueue::new((1..=5).map(|id| a_file(id, id, 0)).collect());
        let report = bounded(run_backfill(
            &queue,
            &FakeRoots::all(),
            &ScriptedProber::always_ok(),
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig {
                batch_size: 2,
                ..Default::default()
            },
        ))
        .await;

        assert_eq!(report.probed, 5);
        assert_eq!(report.pages, 3);
        assert_eq!(
            queue.asked(),
            vec![(0, 2), (2, 2), (4, 2), (5, 2)],
            "each page must resume from the last id of the previous one — an OFFSET \
             would re-read rows, and a cursor that did not advance would loop"
        );
        assert_eq!(report.last_id, 5);
    }

    /// The property the 16,221-file library actually needs: a run that dies
    /// part-way does not start over.
    #[tokio::test]
    async fn a_restart_resumes_rather_than_repeating() {
        let rows: Vec<MediaFile> = (1..=6).map(|id| a_file(id, id, 0)).collect();
        let queue = FakeQueue::new(rows);
        let first_sink = RecordingSink::new();

        // A run that stops after three files, exactly as a restart would.
        let first = bounded(run_backfill(
            &queue,
            &FakeRoots::all(),
            &ScriptedProber::always_ok(),
            &first_sink,
            &RecordingPacer::default(),
            BackfillConfig {
                max_files: 3,
                batch_size: 10,
                ..Default::default()
            },
        ))
        .await;
        assert_eq!(first.probed, 3);
        assert_eq!(first.halted, Some(HaltReason::MaxFilesReached));
        assert_eq!(first.last_id, 3);

        // Persisting a result removes the row from the queue's predicate — that
        // is what makes the NEXT run resume without remembering anything.
        queue.drop_rows(&[1, 2, 3]);

        let second_prober = ScriptedProber::always_ok();
        let second = bounded(run_backfill(
            &queue,
            &FakeRoots::all(),
            &second_prober,
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig {
                batch_size: 10,
                ..Default::default()
            },
        ))
        .await;

        assert_eq!(second.probed, 3, "the second run must do the REMAINING work");
        let reprobed: Vec<_> = second_prober
            .seen()
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|p| p.contains("Movie 1/") || p.contains("Movie 2/") || p.contains("Movie 3/"))
            .collect();
        assert!(
            reprobed.is_empty(),
            "a restart re-probed files the first run had already done: {reprobed:?}"
        );
        assert_eq!(second.last_id, 6);
    }

    #[tokio::test]
    async fn a_failed_file_is_not_retried_inside_the_same_run() {
        let queue = FakeQueue::new(vec![a_file(1, 1, 0), a_file(2, 2, 0)]);
        let prober = ScriptedProber::scripted(vec![
            ProbeOutcome::Attempted(Err(ProbeError::NoStreams)),
            ProbeOutcome::Attempted(Ok(golden_probe())),
        ]);

        let report = bounded(run_backfill(
            &queue,
            &FakeRoots::all(),
            &prober,
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig {
                batch_size: 10,
                ..Default::default()
            },
        ))
        .await;

        assert_eq!(report.failed_terminal, 1);
        assert_eq!(report.probed, 1);
        assert_eq!(
            prober.seen().len(),
            2,
            "the cursor must advance past a FAILED row too; a cursor that only \
             advanced on success would grind on the same file forever"
        );
    }

    #[tokio::test]
    async fn a_queue_that_does_not_advance_the_cursor_halts_instead_of_looping() {
        /// Always returns the same row, ignoring `after_id` — the shape that
        /// turns a keyset loop into an infinite one.
        struct StuckQueue;

        #[async_trait]
        impl ProbeQueue for StuckQueue {
            async fn next_page(&self, _a: i64, _l: i64, _m: i32) -> MuseResult<Vec<MediaFile>> {
                // See `yield_to_the_timer`: without this, a cursor regression
                // spins this loop forever and `bounded` never fires.
                yield_to_the_timer().await;
                Ok(vec![a_file(7, 7, 0)])
            }
            async fn progress(&self, _m: i32) -> MuseResult<repo::media_file::ProbeProgress> {
                Ok(repo::media_file::ProbeProgress::default())
            }
        }

        let report = bounded(run_backfill(
            &StuckQueue,
            &FakeRoots::all(),
            &ScriptedProber::always_ok(),
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(report.halted, Some(HaltReason::CursorStalled));
        assert_eq!(report.considered, 1, "it must stop on the SECOND sighting");
    }

    #[test]
    fn the_cursor_refuses_to_go_backwards_or_stand_still() {
        let mut cursor = CursorGuard::default();
        assert!(cursor.advance(5));
        assert_eq!(cursor.after_id(), 5);
        assert!(!cursor.advance(5), "the same id twice must not advance");
        assert!(!cursor.advance(4), "a lower id must not advance");
        assert_eq!(cursor.after_id(), 5, "a refused advance must not move it");
        assert!(cursor.advance(6));
    }

    // ---- the attempt policy ----------------------------------------------

    #[test]
    fn retryable_and_terminal_come_from_is_retryable_not_from_a_second_match() {
        let mut policy = FailurePolicy::default();
        for error in [
            ProbeError::ToolMissing {
                binary: "ffprobe".into(),
            },
            ProbeError::Spawn {
                binary: "ffprobe".into(),
                message: "no".into(),
            },
            ProbeError::Timeout { secs: 120 },
        ] {
            assert!(error.is_retryable(), "fixture check: {error}");
            assert_eq!(policy.on_failure(&error), FailureDisposition::Retryable);
            policy.on_success();
        }
        for error in [
            ProbeError::ExitFailure {
                code: Some(1),
                stderr: String::new(),
            },
            ProbeError::MalformedOutput {
                message: "not json".into(),
            },
            ProbeError::NoStreams,
            ProbeError::OutputTooLarge { cap: 1 },
        ] {
            assert!(!error.is_retryable(), "fixture check: {error}");
            assert_eq!(policy.on_failure(&error), FailureDisposition::Terminal);
        }
    }

    #[test]
    fn a_streak_of_retryable_failures_halts_and_a_success_resets_it() {
        let mut policy = FailurePolicy::with_halt_after(3);
        let stall = ProbeError::Timeout { secs: 120 };

        assert_eq!(policy.on_failure(&stall), FailureDisposition::Retryable);
        assert_eq!(policy.on_failure(&stall), FailureDisposition::Retryable);
        policy.on_success();
        assert_eq!(
            policy.consecutive_retryable(),
            0,
            "a probe that worked is evidence the host is fine"
        );
        assert_eq!(policy.on_failure(&stall), FailureDisposition::Retryable);
        assert_eq!(policy.on_failure(&stall), FailureDisposition::Retryable);
        assert_eq!(policy.on_failure(&stall), FailureDisposition::HaltHostFault);
    }

    #[test]
    fn terminal_failures_never_trip_the_host_fault_halt() {
        let mut policy = FailurePolicy::with_halt_after(3);
        for _ in 0..20 {
            assert_eq!(
                policy.on_failure(&ProbeError::NoStreams),
                FailureDisposition::Terminal,
                "twenty broken files in a row is a library, not a wedged host — \
                 halting there would stop the very sweep that finds them"
            );
        }
    }

    #[tokio::test]
    async fn a_wedged_host_halts_the_run_instead_of_burning_the_librarys_attempts() {
        let queue = FakeQueue::new((1..=200).map(|id| a_file(id, id, 0)).collect());
        let prober = ScriptedProber::scripted(
            (0..200)
                .map(|_| {
                    ProbeOutcome::Attempted(Err(ProbeError::ToolMissing {
                        binary: "ffprobe".into(),
                    }))
                })
                .collect(),
        );

        let report = bounded(run_backfill(
            &queue,
            &FakeRoots::all(),
            &prober,
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig {
                batch_size: 500,
                ..Default::default()
            },
        ))
        .await;

        assert_eq!(report.halted, Some(HaltReason::HostFaultStreak));
        assert_eq!(
            report.considered, HOST_FAULT_HALT_STREAK as u64,
            "it must stop AT the streak, not sweep 200 readable files into \
             permanently-failed on a host fault"
        );
    }

    #[test]
    fn the_last_allowed_attempt_is_reported_as_exhausted() {
        // `set_probe_error` increments by one, so attempts_before == 2 with a
        // budget of 3 is the last one.
        assert!(attempt_budget_exhausted(2, 3));
        assert!(!attempt_budget_exhausted(1, 3));
        assert!(attempt_budget_exhausted(9, 3), "already over budget");
        assert!(attempt_budget_exhausted(i32::MAX, 3), "must not overflow");
    }

    #[tokio::test]
    async fn a_file_on_its_last_attempt_is_counted_as_exhausted() {
        let queue = FakeQueue::new(vec![a_file(1, 1, 2), a_file(2, 2, 0)]);
        let prober = ScriptedProber::scripted(vec![
            ProbeOutcome::Attempted(Err(ProbeError::Timeout { secs: 120 })),
            ProbeOutcome::Attempted(Err(ProbeError::Timeout { secs: 120 })),
        ]);

        let report = bounded(run_backfill(
            &queue,
            &FakeRoots::all(),
            &prober,
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig {
                max_attempts: 3,
                batch_size: 10,
                ..Default::default()
            },
        ))
        .await;

        assert_eq!(report.failed_retryable, 2);
        assert_eq!(
            report.exhausted, 1,
            "only the file that was already on attempt 2 of 3 leaves the queue"
        );
    }

    // ---- what gets written -----------------------------------------------

    #[tokio::test]
    async fn a_document_is_written_through_mprb05s_writer_and_a_failure_through_the_other() {
        let sink = RecordingSink::new();
        let prober = ScriptedProber::scripted(vec![
            ProbeOutcome::Attempted(Ok(golden_probe())),
            ProbeOutcome::Attempted(Err(ProbeError::NoStreams)),
        ]);

        let report = bounded(run_backfill(
            &FakeQueue::new(vec![a_file(1, 1, 0), a_file(2, 2, 0)]),
            &FakeRoots::all(),
            &prober,
            &sink,
            &RecordingPacer::default(),
            BackfillConfig {
                batch_size: 10,
                ..Default::default()
            },
        ))
        .await;

        let seen = sink.seen();
        assert_eq!(
            seen[0],
            Recorded::Document {
                id: 1,
                suspicious: false
            }
        );
        assert!(matches!(seen[1], Recorded::Failure { id: 2, .. }));
        assert_eq!(report.probed, 1);
        assert_eq!(report.failed_terminal, 1);
    }

    #[tokio::test]
    async fn a_refused_write_is_persist_failed_and_never_counted_as_probed() {
        let mut sink = RecordingSink::new();
        sink.refuse = true;

        let report = bounded(run_backfill(
            &FakeQueue::new(vec![a_file(1, 1, 0)]),
            &FakeRoots::all(),
            &ScriptedProber::always_ok(),
            &sink,
            &RecordingPacer::default(),
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(report.probed, 0, "a counter must not claim a write the database refused");
        assert_eq!(report.persist_failed, 1);
        assert!(report.is_balanced());
    }

    #[tokio::test]
    async fn the_absolute_path_is_the_librarys_root_joined_to_the_stored_relative_path() {
        let prober = ScriptedProber::always_ok();
        bounded(run_backfill(
            &FakeQueue::new(vec![a_file(42, 7, 0)]),
            &FakeRoots::all(),
            &prober,
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(
            prober.seen(),
            vec![PathBuf::from("/library/Movie 42/Movie 42.mkv")],
            "an item with no recorded folder has exactly one candidate: the \
             library root joined to relative_path, which is how the SCANNER \
             forms it. MPRB-10 note — this was believed to be the only \
             reconstruction; against the live database it is the right one for \
             1,258 of 12,873 rows. See `candidate_paths`."
        );
    }

    #[tokio::test]
    async fn a_path_that_will_not_resolve_is_skipped_without_burning_an_attempt() {
        let sink = RecordingSink::new();
        let report = bounded(run_backfill(
            &FakeQueue::new(vec![a_file(1, 1, 0)]),
            &FakeRoots::all(),
            &ScriptedProber::scripted(vec![ProbeOutcome::Unresolved(
                "outside every allowed root".into(),
            )]),
            &sink,
            &RecordingPacer::default(),
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(report.skipped_unresolved, 1);
        assert!(
            sink.seen().is_empty(),
            "nothing was observed about the file, so nothing may be written — a \
             configuration fault must not spend a readable file's attempts"
        );
    }

    #[tokio::test]
    async fn a_suspicious_result_is_still_stored_and_counted_as_probed() {
        // A suspicious verdict comes from MPRB-03's `suspicion`, reached through
        // `probe_write`; this asserts the backfill carries it, not that it
        // re-decides it.
        let mut probe = golden_probe();
        probe.duration_secs = Some(0.0);
        assert!(
            crate::media::derive::suspicion(&probe).is_some(),
            "fixture check: this probe must actually be suspicious, or the test \
             below asserts nothing"
        );

        let sink = RecordingSink::new();
        let report = bounded(run_backfill(
            &FakeQueue::new(vec![a_file(1, 1, 0)]),
            &FakeRoots::all(),
            &ScriptedProber::scripted(vec![ProbeOutcome::Attempted(Ok(probe))]),
            &sink,
            &RecordingPacer::default(),
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(report.probed, 1);
        assert_eq!(report.suspicious, 1);
        assert_eq!(
            sink.seen(),
            vec![Recorded::Document {
                id: 1,
                suspicious: true
            }]
        );
    }

    // ---- degrade ----------------------------------------------------------

    #[tokio::test]
    async fn a_queue_failure_is_a_reported_halt_not_an_error() {
        let mut queue = FakeQueue::new(vec![a_file(1, 1, 0)]);
        queue.fail_after_pages = Some(0);

        let report = bounded(run_backfill(
            &queue,
            &FakeRoots::all(),
            &ScriptedProber::always_ok(),
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(report.halted, Some(HaltReason::QueueUnavailable));
        assert_eq!(report.considered, 0);
    }

    #[tokio::test]
    async fn a_root_lookup_failure_is_a_reported_halt_not_an_error() {
        let report = bounded(run_backfill(
            &FakeQueue::new(vec![a_file(1, 1, 0)]),
            &FakeRoots {
                missing: Vec::new(),
                fail: true,
                item_path: None,
            },
            &ScriptedProber::always_ok(),
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(report.halted, Some(HaltReason::RootLookupUnavailable));
    }

    #[tokio::test]
    async fn an_empty_queue_is_a_clean_complete_run() {
        let report = bounded(run_backfill(
            &FakeQueue::new(Vec::new()),
            &FakeRoots::all(),
            &ScriptedProber::always_ok(),
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(report.halted, None);
        assert_eq!(report.considered, 0);
        assert_eq!(report.pages, 0);
        assert!(report.is_balanced());
    }

    #[tokio::test]
    async fn a_host_without_ffprobe_leaves_the_worker_inert_and_reportable() {
        // A MediaCore built against a binary that cannot exist: `can_probe()`
        // is false, and this must be a REPORT, not an error and not a panic.
        const ABSENT: &str = "muse-mprb07-no-such-ffprobe-xyzzy";
        let cfg = crate::config::Config {
            probe_ffprobe_bin: Some(ABSENT.to_string()),
            ffmpeg_path: ABSENT.to_string(),
            foundry_handbrake_bin: Some(ABSENT.to_string()),
            library_root: None,
            ..Default::default()
        };
        let media = MediaCore::from_config(&cfg);
        assert!(!media.can_probe(), "fixture check: this host must not be able to probe");

        // A pool that is never dialled: the degrade check short-circuits before
        // any query, and this test proves it by using a pool that would fail.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://user:pass@127.0.0.1:1/muse_test_lazy")
            .expect("connect_lazy never fails synchronously");

        let report = tokio::time::timeout(
            Duration::from_secs(20),
            run_from_pool(&pool, &media, BackfillConfig::default()),
        )
        .await
        .expect("the inert path must return immediately, never dial, never hang");

        assert_eq!(report.halted, Some(HaltReason::NoFfprobeOnThisHost));
        assert_eq!(report.considered, 0);
        assert_eq!(report.remaining, None, "an unmeasured value is absent, never zero");
    }

    // ---- the report -------------------------------------------------------

    #[test]
    fn remaining_is_measured_arithmetic_over_two_counts_never_a_projection() {
        let progress = repo::media_file::ProbeProgress {
            total: 16_221,
            probed: 10_000,
            unprobed: 6_221,
            permanently_failed: 21,
            ..Default::default()
        };
        assert_eq!(queue_remaining(&progress), 6_200);
    }

    #[test]
    fn remaining_never_goes_negative_when_the_two_counts_race() {
        let progress = repo::media_file::ProbeProgress {
            unprobed: 3,
            permanently_failed: 9,
            ..Default::default()
        };
        assert_eq!(queue_remaining(&progress), 0);
    }

    /// Added because a mutation SURVIVED: nothing observed the difference
    /// between "the measurement could not be taken" and "the queue is empty".
    /// That distinction is the whole reason `remaining` is an `Option`, and an
    /// operator told the backfill is finished when it is merely unmeasured is
    /// the one wrong answer this field must never give.
    #[tokio::test]
    async fn an_unmeasurable_remaining_is_absent_never_zero() {
        let mut queue = FakeQueue::new(vec![a_file(1, 1, 0)]);
        queue.fail_progress = true;

        let report = bounded(run_backfill(
            &queue,
            &FakeRoots::all(),
            &ScriptedProber::always_ok(),
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(report.probed, 1, "fixture check: the run itself must have succeeded");
        assert_eq!(
            report.remaining, None,
            "a measurement that could not be taken must be reported as ABSENT — \
             `Some(0)` reads as a drained queue"
        );
        assert_eq!(report.permanently_failed, None);
    }

    /// The control for [`BackfillReport::is_balanced`]. Every other test asserts
    /// it is TRUE; without this one a guard hardcoded to `true` would satisfy
    /// all of them, which is exactly what a surviving mutation showed.
    #[test]
    fn the_balance_guard_can_actually_fail() {
        let missed_a_counter = BackfillReport {
            considered: 3,
            probed: 2,
            ..Default::default()
        };
        assert_eq!(missed_a_counter.accounted(), 2);
        assert!(
            !missed_a_counter.is_balanced(),
            "a row that landed in no bucket must be visible, or the guard is decoration"
        );

        let balanced = BackfillReport {
            considered: 3,
            probed: 1,
            failed_terminal: 1,
            skipped_unresolved: 1,
            ..Default::default()
        };
        assert!(balanced.is_balanced());
        // The two subset counters must NOT be double-counted into the total.
        let with_subsets = BackfillReport {
            suspicious: 1,
            exhausted: 1,
            ..balanced
        };
        assert!(
            with_subsets.is_balanced(),
            "`suspicious` and `exhausted` are subsets; counting them again would \
             make every honest run look unbalanced"
        );
    }

    #[tokio::test]
    async fn every_considered_row_lands_in_exactly_one_outcome_bucket() {
        let queue = FakeQueue::new(vec![
            a_file(1, 1, 0),
            a_file(2, 2, 0),
            a_file(3, 3, 0),
            a_file(4, 4, 0), // no library root
        ]);
        let prober = ScriptedProber::scripted(vec![
            ProbeOutcome::Attempted(Ok(golden_probe())),
            ProbeOutcome::Attempted(Err(ProbeError::NoStreams)),
            ProbeOutcome::Attempted(Err(ProbeError::Timeout { secs: 120 })),
        ]);

        let report = bounded(run_backfill(
            &queue,
            &FakeRoots {
                missing: vec![4],
                fail: false,
                item_path: None,
            },
            &prober,
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig {
                batch_size: 10,
                ..Default::default()
            },
        ))
        .await;

        assert_eq!(report.considered, 4);
        assert_eq!(report.probed, 1);
        assert_eq!(report.failed_terminal, 1);
        assert_eq!(report.failed_retryable, 1);
        assert_eq!(report.skipped_unresolved, 1);
        assert!(
            report.is_balanced(),
            "a row that lands in no bucket is a run the report cannot describe: {report:?}"
        );
    }

    /// The trap this epic keeps paying for: a fabricated measurement. The report
    /// says what happened; it never says when the sweep will finish.
    #[test]
    fn the_report_carries_no_eta_and_no_completion_estimate() {
        let json = serde_json::to_value(BackfillReport {
            considered: 10,
            probed: 9,
            elapsed_ms: 1_234,
            remaining: Some(16_000),
            ..Default::default()
        })
        .expect("the report serialises");
        let keys: Vec<String> = json
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect();

        for forbidden in ["eta", "estimate", "projected", "finish", "completion", "percent"] {
            assert!(
                !keys.iter().any(|k| k.contains(forbidden)),
                "the report grew a `{forbidden}`-shaped field: {keys:?}. An estimate \
                 computed from an average is a fabricated measurement."
            );
        }
        // And the fields that ARE there are the measured ones.
        assert!(keys.contains(&"remaining".to_string()));
        assert!(keys.contains(&"elapsed_ms".to_string()));
    }

    // ---- the run gate -----------------------------------------------------

    #[test]
    fn a_second_run_cannot_start_while_one_is_in_flight() {
        let gate = RunGate::new();
        let first = gate.try_begin().expect("the first run claims the gate");
        assert!(gate.is_running());
        assert!(
            gate.try_begin().is_none(),
            "two concurrent sweeps would double the load on the mount and \
             interleave two cursors over the same queue"
        );
        first.complete(BackfillReport {
            probed: 7,
            ..Default::default()
        });
        assert!(!gate.is_running());
        assert_eq!(gate.last_report().expect("a report").probed, 7);
        assert!(gate.try_begin().is_some(), "the gate must reopen");
    }

    #[test]
    fn a_run_that_never_reports_still_releases_the_gate() {
        let gate = RunGate::new();
        {
            let _permit = gate.try_begin().expect("claimed");
            assert!(gate.is_running());
        } // dropped without `complete` — a panic, or an early return
        assert!(
            !gate.is_running(),
            "a boolean cleared only on the happy path stays set forever the first \
             time something unwinds"
        );
        assert!(
            gate.last_report().is_none(),
            "a run with nothing truthful to say about itself must not leave a report"
        );
    }

    #[test]
    fn the_gate_reports_the_LAST_completed_run_not_the_first() {
        let gate = RunGate::new();
        gate.try_begin().expect("first").complete(BackfillReport {
            probed: 1,
            ..Default::default()
        });
        gate.try_begin().expect("second").complete(BackfillReport {
            probed: 2,
            ..Default::default()
        });
        assert_eq!(gate.last_report().expect("a report").probed, 2);
    }

    #[test]
    fn the_gate_is_safe_to_share_across_threads() {
        let gate = Arc::new(RunGate::new());
        let mut handles = Vec::new();
        let claimed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..8 {
            let gate = Arc::clone(&gate);
            let claimed = Arc::clone(&claimed);
            handles.push(std::thread::spawn(move || {
                if let Some(permit) = gate.try_begin() {
                    claimed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    permit.complete(BackfillReport::default());
                }
            }));
        }
        for h in handles {
            h.join().expect("no thread may panic");
        }
        let n = claimed.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            (1..=8).contains(&n),
            "at least one thread must have run, and none may have panicked"
        );
        assert!(!gate.is_running());
    }

    // ---- MPRB-10: rebuilding the absolute path ----------------------------
    //
    // These are the rules that decide, per row, which bytes get probed. They
    // were the difference between a backfill that could reach 1,260 of the
    // live library's 12,873 rows and one that reaches 12,705 — measured by
    // `stat`-ing every reconstruction against the real mount, not reasoned
    // about. See `candidate_paths`.

    fn at(root: &str, item: Option<&str>) -> MediaItemLocation {
        MediaItemLocation {
            root_folder: root.to_string(),
            item_path: item.map(str::to_string),
        }
    }

    #[test]
    fn the_scan_convention_is_offered_first() {
        // `library::scan` writes `relative_path` relative to the library ROOT,
        // item folder included. 1,258 live rows.
        let candidates = candidate_paths(
            &at("/srv/media/TV Shows", None),
            "Veronica Mars/Season 1/e08.mkv",
        );
        assert_eq!(
            candidates,
            vec![PathBuf::from("/srv/media/TV Shows/Veronica Mars/Season 1/e08.mkv")],
            "with no item path there is exactly one candidate, and it is the \
             root-relative one MPRB-07 built"
        );
    }

    #[test]
    fn the_arr_convention_is_offered_as_a_second_candidate() {
        // `arr::ingest` copies Radarr/Sonarr's `relativePath`, which excludes
        // the item folder — and records `media_items.path` in the ARR's
        // namespace (`/media/…`), not Muse's (`/srv/media/…`). 11,615 live rows.
        let candidates = candidate_paths(
            &at("/srv/media/Movies", Some("/media/Movies/1984")),
            "1984.avi",
        );
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/srv/media/Movies/1984.avi"),
                PathBuf::from("/srv/media/Movies/1984/1984.avi"),
            ],
            "MPRB-07's reconstruction is candidate 1 and misses; the rebased \
             item folder is candidate 2 and is where the file actually is"
        );
    }

    #[test]
    fn an_item_path_already_in_muses_namespace_is_not_rebased_twice() {
        assert_eq!(
            rebase_item_folder("/srv/media/Movies", Some("/srv/media/Movies/1984")),
            Some(PathBuf::from("/srv/media/Movies/1984")),
        );
    }

    #[test]
    fn a_library_name_that_also_appears_in_the_item_folder_rebases_on_the_first() {
        // A library called `Movies` holding a folder ALSO called `Movies`.
        //
        // FIXTURE NOTE — this test asserted nothing until the mutation sweep
        // said so. It first used `/media/Movies/Movies.2019`, which contains
        // `/Movies/` exactly ONCE (`Movies.2019` has no trailing slash), so
        // `find` and `rfind` return the same index and the `rfind` mutation
        // survived. The name has to be a whole PATH COMPONENT for there to be
        // two occurrences at all.
        assert_eq!(
            rebase_item_folder("/srv/media/Movies", Some("/media/Movies/Movies/2019")),
            Some(PathBuf::from("/srv/media/Movies/Movies/2019")),
            "matching the LAST occurrence would drop the item folder and \
             address /srv/media/Movies/2019, which is a different directory"
        );
    }

    #[test]
    fn an_unrecognisable_item_path_yields_no_second_candidate_rather_than_a_guess() {
        assert_eq!(rebase_item_folder("/srv/media/Movies", None), None);
        assert_eq!(
            rebase_item_folder("/srv/media/Movies", Some("   ")),
            None,
            "a blank item path names nothing"
        );
        assert_eq!(
            rebase_item_folder("/srv/media/Movies", Some("/elsewhere/Films/1984")),
            None,
            "no shared component means no rebase — a fabricated path is worse \
             than an unresolved row"
        );
        assert_eq!(
            rebase_item_folder("/srv/media/Movies", Some("/media/Movies")),
            None,
            "the library root itself is not an item folder"
        );
        // FIXTURE NOTE — the line above does NOT reach the empty-suffix guard:
        // `/media/Movies` has no trailing slash, so the `/Movies/` search fails
        // first and `?` returns. The mutation that deletes the guard survived
        // until this second case, which HAS the trailing slash, was added.
        assert_eq!(
            rebase_item_folder("/srv/media/Movies", Some("/media/Movies/")),
            None,
            "an item path that reduces to the library root with nothing after \
             it names no item folder"
        );
        assert_eq!(
            rebase_item_folder("", Some("/media/Movies/1984")),
            None,
            "an empty root has no name to join on"
        );
    }

    #[test]
    fn a_padded_item_path_is_trimmed_before_it_is_rebased() {
        // Untrimmed, the trailing space survives into the suffix and addresses
        // `…/1984 ` — a directory that does not exist. (This is also the
        // fixture that makes the `trim` observable at all: the blank-path case
        // above is satisfied with or without it.)
        assert_eq!(
            rebase_item_folder("/srv/media/Movies", Some(" /media/Movies/1984 ")),
            Some(PathBuf::from("/srv/media/Movies/1984")),
        );
    }

    #[test]
    fn identical_candidates_are_offered_once() {
        let candidates = candidate_paths(
            &at("/srv/media/Movies", Some("/srv/media/Movies")),
            "1984.avi",
        );
        assert_eq!(
            candidates,
            vec![PathBuf::from("/srv/media/Movies/1984.avi")],
            "an item path that reduces to the root must not make the prober \
             stat the same path twice"
        );
    }

    #[test]
    fn a_traversal_in_either_input_is_still_offered_for_the_guard_to_refuse() {
        // `candidate_paths` deliberately does NOT sanitise: confinement is
        // `PathGuard::resolve`'s job and there must be exactly one of it. What
        // this asserts is that nothing here silently DROPS such a path, which
        // would hide it from the guard's refusal.
        let candidates = candidate_paths(&at("/srv/media/Movies", None), "../../etc/shadow");
        assert_eq!(
            candidates,
            vec![PathBuf::from("/srv/media/Movies/../../etc/shadow")]
        );
    }

    #[tokio::test]
    async fn both_candidates_reach_the_prober_in_order() {
        // The loop-level assertion: the rule above is not decoration, it is
        // what the production path hands to `FileProber`.
        let queue = FakeQueue::new(vec![a_file(42, 42, 0)]);
        let prober = ScriptedProber::always_ok();

        let report = bounded(run_backfill(
            &queue,
            &FakeRoots {
                missing: Vec::new(),
                fail: false,
                item_path: Some("/media/library/Movie 42".to_string()),
            },
            &prober,
            &RecordingSink::new(),
            &RecordingPacer::default(),
            BackfillConfig::default(),
        ))
        .await;

        assert_eq!(report.probed, 1);
        assert_eq!(
            prober.offered(),
            vec![vec![
                PathBuf::from("/library/Movie 42/Movie 42.mkv"),
                PathBuf::from("/library/Movie 42/Movie 42/Movie 42.mkv"),
            ]],
            "one call, both candidates, scan convention first"
        );
    }

    #[test]
    fn an_item_path_inside_a_root_whose_own_name_repeats_is_not_re_rooted() {
        // The early "already in Muse's namespace" return is load-bearing here
        // and nowhere else: the library's final component also appears EARLIER
        // in its own root, so re-rooting on the first occurrence would splice
        // the name in twice and address a directory that does not exist.
        assert_eq!(
            rebase_item_folder("/srv/Movies/Movies", Some("/srv/Movies/Movies/1984")),
            Some(PathBuf::from("/srv/Movies/Movies/1984")),
        );
    }

    // ---- MPRB-10: the production prober, against a real filesystem ---------
    //
    // `candidate_paths` decides WHAT to offer; `MediaCoreProber` decides which
    // offer is the file. That second decision is a filesystem question, so
    // these tests use a real temp root and the real `PathGuard` — the fakes
    // above cannot reach it, and it is the half that was wrong in production.

    fn probe_core(root: &std::path::Path) -> MediaCore {
        MediaCore::from_config(&crate::config::Config {
            // Absent on purpose: a spawn failure is an `Attempted(Err(..))`,
            // which is exactly what distinguishes "we found the file and ran
            // at it" from "we never found the file".
            probe_ffprobe_bin: Some("muse-mprb10-no-such-ffprobe".to_string()),
            ffmpeg_path: "muse-mprb10-no-such-ffmpeg".to_string(),
            foundry_handbrake_bin: Some("muse-mprb10-no-such-handbrake".to_string()),
            library_root: Some(root.to_string_lossy().to_string()),
            ..Default::default()
        })
    }

    fn mprb10_temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muse-mprb10-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    #[tokio::test]
    async fn the_prober_falls_through_to_the_second_candidate_when_the_first_is_not_there() {
        let root = mprb10_temp_root("fallthrough");
        std::fs::create_dir_all(root.join("Movies/1984")).expect("create item folder");
        std::fs::write(root.join("Movies/1984/1984.avi"), b"x").expect("write the file");

        let core = probe_core(&root);
        let prober = MediaCoreProber(&core);
        let outcome = prober
            .probe(&[
                root.join("Movies/1984.avi"),      // MPRB-07's reconstruction
                root.join("Movies/1984/1984.avi"), // where the file actually is
            ])
            .await;

        assert!(
            matches!(outcome, ProbeOutcome::Attempted(_)),
            "the second candidate exists, so the row must be PROBED, not skipped \
             as unresolved: {outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn the_prober_takes_the_first_candidate_that_resolves_and_stops() {
        let root = mprb10_temp_root("first-wins");
        std::fs::create_dir_all(root.join("Movies/1984")).expect("create item folder");
        std::fs::write(root.join("Movies/1984.avi"), b"x").expect("write candidate 1");

        let core = probe_core(&root);
        // Candidate 2 does not exist; if the loop did not stop at the first
        // resolution it would fall through to it and report Unresolved.
        let outcome = MediaCoreProber(&core)
            .probe(&[
                root.join("Movies/1984.avi"),
                root.join("Movies/1984/1984.avi"),
            ])
            .await;

        assert!(
            matches!(outcome, ProbeOutcome::Attempted(_)),
            "first resolving candidate wins: {outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Writes the argv it was handed to `marker`, then fails. The ONLY way to
    /// observe WHICH candidate was probed when several of them exist: with an
    /// absent binary every candidate produces the same `Attempted(Err(..))`,
    /// which is why the "first resolving candidate wins" mutation survived the
    /// first sweep.
    fn fake_ffprobe(dir: &std::path::Path, marker: &std::path::Path) -> PathBuf {
        let bin = dir.join("fake-ffprobe.sh");
        // `-version` must SUCCEED. It is the capability snapshot's probe, and
        // the same shape `media::mod`'s `stub_bin` uses; a script that fails it
        // still gets spawned for the probe, but the marker-empty failure this
        // fixture first produced came from getting the shell quoting wrong, so
        // this follows a form already known to work in this crate.
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = \"-version\" ]; then\n    echo 'ffprobe version fake'; exit 0\n  fi\ndone\nfor a in \"$@\"; do echo \"$a\" >> '{}'; done\nexit 1\n",
                marker.display()
            ),
        )
        .expect("write the fake ffprobe");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        bin
    }

    #[tokio::test]
    async fn the_candidate_that_gets_probed_is_the_first_one_that_resolves() {
        let root = mprb10_temp_root("which-one");
        let tools = mprb10_temp_root("which-one-tools");
        let marker = tools.join("probed.txt");

        // BOTH candidates exist and are different files. Only an observation of
        // the argv can tell which was chosen.
        std::fs::create_dir_all(root.join("Movies/1984")).expect("item folder");
        std::fs::write(root.join("Movies/1984.avi"), b"first").expect("candidate 1");
        std::fs::write(root.join("Movies/1984/1984.avi"), b"second").expect("candidate 2");

        let core = MediaCore::from_config(&crate::config::Config {
            probe_ffprobe_bin: Some(
                fake_ffprobe(&tools, &marker).to_string_lossy().to_string(),
            ),
            ffmpeg_path: "muse-mprb10-no-such-ffmpeg".to_string(),
            foundry_handbrake_bin: Some("muse-mprb10-no-such-handbrake".to_string()),
            library_root: Some(root.to_string_lossy().to_string()),
            ..Default::default()
        });
        // Construction takes the capability snapshot, which spawns the binary.
        // Start counting after it.
        let _ = std::fs::remove_file(&marker);

        let outcome = MediaCoreProber(&core)
            .probe(&[
                root.join("Movies/1984.avi"),
                root.join("Movies/1984/1984.avi"),
            ])
            .await;
        assert!(matches!(outcome, ProbeOutcome::Attempted(_)), "{outcome:?}");

        let argv = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(
            argv.contains("Movies/1984.avi") && !argv.contains("Movies/1984/1984.avi"),
            "exactly ONE candidate may be probed, and it must be the first that \
             resolved — probing the last would mean every arr-convention row \
             pays two stats and every scan-convention row is read from the \
             wrong place. argv seen: {argv}"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&tools);
    }

    #[tokio::test]
    async fn when_no_candidate_resolves_every_one_of_them_is_named() {
        let root = mprb10_temp_root("all-refused");
        let core = probe_core(&root);
        let outcome = MediaCoreProber(&core)
            .probe(&[
                root.join("Movies/1984.avi"),
                root.join("Movies/1984/1984.avi"),
            ])
            .await;

        match outcome {
            ProbeOutcome::Unresolved(reason) => {
                assert!(
                    reason.contains("Movies/1984.avi") && reason.contains("Movies/1984/1984.avi"),
                    "'no such file' against ONE reconstruction, when a second was \
                     tried and also failed, is not the diagnostic an operator \
                     needs: {reason}"
                );
            }
            other => panic!("no candidate exists, so nothing was observed: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_candidate_outside_the_library_root_is_still_refused_by_the_guard() {
        // The confinement MPRB-01 owns is not weakened by offering two paths:
        // both go through `resolve`, and a real file outside the root is
        // refused exactly as one path was before.
        let root = mprb10_temp_root("confined");
        let outside = mprb10_temp_root("confined-outside");
        std::fs::write(outside.join("escape.mkv"), b"x").expect("write the outside file");

        let core = probe_core(&root);
        let outcome = MediaCoreProber(&core)
            .probe(&[outside.join("escape.mkv")])
            .await;

        assert!(
            matches!(outcome, ProbeOutcome::Unresolved(_)),
            "a file outside MUSE_LIBRARY_ROOT must never be probed: {outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
