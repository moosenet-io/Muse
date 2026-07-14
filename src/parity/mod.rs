//! MUSET-09 (Plane TERM #374): parity diff + retirement-readiness evidence.
//!
//! ## What this module is for
//! The strangler-fig plan for replacing Tautulli (spec §4) is: run Muse's
//! own analytics in shadow (MUSET-08, `crate::shadow`) alongside Tautulli,
//! and only retire Tautulli for a function once Muse has demonstrated
//! sustained parity with it. This module is the "demonstrated" half: it
//! diffs Muse's shadow output against Tautulli's own numbers field-by-field,
//! itemizes every divergence, and assembles a [`RetirementReadinessReport`]
//! — the evidence an operator (<operator>) would read before deciding to flip
//! Tautulli off for that function.
//!
//! ## What is diffed, and where each side comes from
//! - **Muse side**: [`crate::shadow::ShadowResult`] / [`crate::shadow::ShadowWatchStat`]
//!   (MUSET-08) — Muse's own watch-data analytics, computed purely from
//!   folding `play_events` (`crate::tracker::reconstruct::fold_events`).
//!   This module does not recompute anything Muse-side; it consumes
//!   `ShadowResult` as-is.
//! - **Tautulli side**: the snapshot's Tautulli-origin `play_events` rows —
//!   `source = 'snapshot:tautulli'`, `event_type = 'snapshot.history'`
//!   (`repo::play_event::list_tautulli_snapshot_events`), written by
//!   `crate::snapshot::normalize::normalize_tautulli_play_record` from a
//!   Tautulli `session_history` snapshot export. Crucially, each of those
//!   rows carries **Tautulli's own computed numbers** — not just raw Plex
//!   telemetry — inside its `raw` JSON payload:
//!   `percent_complete` (0-100), `watched_status` (0 / 0.5 / 1 — Tautulli's
//!   own finished determination), and `duration` (session length, seconds).
//!   [`aggregate_tautulli_stats`] parses those fields back out and rolls
//!   them up per `(account_ref, rating_key)`, exactly like Tautulli's own
//!   `get_history`/watch-stats endpoints would. So this is a genuine
//!   Muse-computed-number vs Tautulli-computed-number diff, not raw data
//!   vs raw data.
//!
//! ## Non-authoritative by construction (the AC's negative test)
//! Nothing in this module can retire Tautulli. There is no "retired" /
//! "authoritative" flag anywhere in this crate for this module to flip, no
//! `PgPool` parameter on any function here (the diff is pure, in-memory
//! data transformation over two already-fetched inputs — see
//! [`build_report`]'s signature), and no `INSERT`/`UPDATE` of any kind.
//! [`RetirementReadinessReport`] is a plain `Debug + Clone + Serialize`
//! value; its only behaviour is [`RetirementReadinessReport::headline`],
//! which formats a human-readable string that *always* states retirement is
//! not authorized and is a human decision — see
//! `tests::retirement_is_never_auto_triggered_even_at_100_percent_parity`
//! for the load-bearing assertion.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value as Json;

use crate::models::play_event::PlayEvent;
use crate::shadow::{ShadowResult, ShadowWatchStat};

/// Tautulli's own "fully watched" determination
/// (`HistoryRow::watched_status`): `1.0` means watched/scrobbled, `0.5`
/// means partially watched, `0` means not watched. Only the `1.0` case
/// counts as "finished" for this diff — matching the binary
/// `is_finished`/`is_abandoned` semantics on the Muse side, rather than
/// treating a partial watch as ambiguous.
pub const TAUTULLI_FINISHED_WATCHED_STATUS: f64 = 1.0;

/// Relative tolerance applied to `total_watched_ms` before a mismatch is
/// itemized as a divergence. Muse integrates `watched_ms` from
/// play/pause/resume/stop offset deltas (`tracker::reconstruct::fold_events`),
/// while Tautulli's `duration` is a coarser session-length figure recorded
/// once at session end — the two are expected to differ by a few percent
/// from rounding and poll-interval granularity even when both sides agree
/// on "what happened." A 5% relative band absorbs that expected noise
/// without hiding a real divergence (a transcode/seek-handling bug would
/// show up as a much larger delta than this).
pub const WATCHED_MS_TOLERANCE_FRACTION: f64 = 0.05;

/// Absolute tolerance (percentage points, on a 0.0-1.0 scale) applied to
/// `avg_percent` before a mismatch is itemized. Tautulli reports
/// `percent_complete` per-row against its own `duration` figure; Muse
/// computes `percent_complete` per-fold against `duration_ms` extracted from
/// Plex metadata. Two points of rounding/source-duration drift is expected
/// noise, matching the same reasoning as [`WATCHED_MS_TOLERANCE_FRACTION`].
pub const AVG_PERCENT_TOLERANCE: f32 = 0.02;

/// `play_count`/`finished_count` are simple integer counts derived from the
/// same underlying session concept on both sides — no tolerance is applied
/// (a mismatch here is exactly the kind of thing this diff exists to catch,
/// not smooth over).
const EXACT_COUNT_TOLERANCE_DESCRIPTION: &str = "exact match required (integer count)";

/// One Tautulli-computed, `ShadowWatchStat`-shaped aggregate for a
/// `(account_ref, rating_key)` pair, rolled up from that pair's Tautulli
/// history-snapshot rows. The Tautulli-side counterpart to
/// [`crate::shadow::ShadowWatchStat`] for the fields both sides can
/// meaningfully express (see the module doc for why `abandoned_count` has
/// no counterpart here).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TautulliWatchStat {
    pub account_ref: Option<String>,
    pub rating_key: Option<String>,
    /// Number of Tautulli history rows folded into this aggregate — the
    /// Tautulli-side analog of `ShadowWatchStat::play_count`.
    pub play_count: i32,
    /// Rows whose `watched_status` reached [`TAUTULLI_FINISHED_WATCHED_STATUS`].
    pub finished_count: i32,
    /// Sum of each row's `duration` (seconds, from Tautulli's `raw` payload),
    /// converted to milliseconds — the Tautulli-side analog of
    /// `ShadowWatchStat::total_watched_ms`.
    pub total_watched_ms: i64,
    /// Mean of each row's `percent_complete`, normalized from Tautulli's
    /// 0-100 scale to Muse's 0.0-1.0 scale.
    pub avg_percent: Option<f32>,
}

#[derive(Default)]
struct TautulliAccumulator {
    play_count: i32,
    finished_count: i32,
    total_watched_ms: i64,
    percent_sum: f64,
    percent_count: i32,
}

/// Pull Tautulli's own computed numbers back out of one history-snapshot
/// row's `raw` JSON payload. Every field is best-effort (`None` on a
/// missing/unparseable key) — a partially-populated snapshot export
/// degrades to "this row contributes nothing to that field," never a panic
/// or a fatal error, matching this crate's general snapshot-tolerance
/// posture (see `snapshot::normalize::parse_epoch_seconds`).
struct ParsedTautulliRow {
    duration_secs: Option<i64>,
    percent_complete_0_100: Option<f64>,
    watched_status: Option<f64>,
}

fn parse_tautulli_row(raw: &Json) -> ParsedTautulliRow {
    ParsedTautulliRow {
        duration_secs: raw.get("duration").and_then(Json::as_i64),
        percent_complete_0_100: raw.get("percent_complete").and_then(Json::as_f64),
        watched_status: raw.get("watched_status").and_then(Json::as_f64),
    }
}

/// Roll up Tautulli history-snapshot `play_events` (as returned by
/// `repo::play_event::list_tautulli_snapshot_events`) into per-`(account_ref,
/// rating_key)` [`TautulliWatchStat`]s. Pure, no I/O — the caller fetches
/// the events; this function only transforms already-in-memory data, same
/// posture as `shadow::group_by_session`.
pub fn aggregate_tautulli_stats(events: &[PlayEvent]) -> Vec<TautulliWatchStat> {
    let mut accumulators: BTreeMap<(Option<String>, Option<String>), TautulliAccumulator> =
        BTreeMap::new();

    for event in events {
        let key = (event.account_ref.clone(), event.rating_key.clone());
        let parsed = parse_tautulli_row(&event.raw);
        let acc = accumulators.entry(key).or_default();

        acc.play_count += 1;
        if parsed.watched_status == Some(TAUTULLI_FINISHED_WATCHED_STATUS) {
            acc.finished_count += 1;
        }
        if let Some(secs) = parsed.duration_secs {
            acc.total_watched_ms += secs.saturating_mul(1000);
        }
        if let Some(pct) = parsed.percent_complete_0_100 {
            acc.percent_sum += pct / 100.0;
            acc.percent_count += 1;
        }
    }

    accumulators
        .into_iter()
        .map(|((account_ref, rating_key), acc)| TautulliWatchStat {
            account_ref,
            rating_key,
            play_count: acc.play_count,
            finished_count: acc.finished_count,
            total_watched_ms: acc.total_watched_ms,
            avg_percent: if acc.percent_count > 0 {
                Some((acc.percent_sum / acc.percent_count as f64) as f32)
            } else {
                None
            },
        })
        .collect()
}

/// What KIND of divergence a [`Divergence`] records. Two categories, both
/// blocking for retirement:
///
/// - **`FieldValue`**: both sides have this entity, but a field's value
///   differs beyond tolerance — a *correctness* gap on shared data.
/// - **Coverage gaps**: the two sides don't even agree on *which entities
///   exist*. This is the false-ready blind spot a naive intersection-only
///   diff misses: Muse could compute a handful of Tautulli's entities
///   perfectly (100% field parity on the overlap) while silently missing
///   the vast majority. For a *retirement*-readiness report that is
///   dangerous, so an entity present on only one side is itself a
///   first-class divergence:
///   - **`CoverageMissingOnMuse`**: Tautulli has this entity, Muse computed
///     nothing for it — Muse would *lose* watch-data if Tautulli were
///     retired now.
///   - **`CoverageMissingOnTautulli`**: Muse computed an entity Tautulli
///     never had — a spurious/extra aggregate that also signals the two
///     sides disagree on the entity set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DivergenceKind {
    FieldValue,
    CoverageMissingOnMuse,
    CoverageMissingOnTautulli,
}

/// One itemized mismatch between the Muse side and the Tautulli side for a
/// single `(account_ref, rating_key)` entity — the AC's "divergences
/// ITEMIZED (so they're fixable before retirement)" requirement. Covers
/// both per-field value drift (`kind = FieldValue`, `field` naming the
/// diverging field) and entity-set coverage gaps (`kind =
/// CoverageMissingOn*`, `field = "coverage"`, with `muse_value` or
/// `tautulli_value` set to `"absent"` for the side that has no counterpart).
/// Every field carries both raw values plus a delta/description so a reader
/// doesn't have to recompute anything to see how far apart the two sides
/// landed.
#[derive(Debug, Clone, Serialize)]
pub struct Divergence {
    pub kind: DivergenceKind,
    pub account_ref: Option<String>,
    pub rating_key: Option<String>,
    pub field: &'static str,
    pub muse_value: String,
    pub tautulli_value: String,
    pub delta_description: String,
}

/// Per-field agreement summary across every entity present on both sides.
#[derive(Debug, Clone, Serialize)]
pub struct ParityMetrics {
    pub field: &'static str,
    pub tolerance_description: String,
    pub entities_compared: usize,
    pub entities_matching: usize,
    pub agreement_pct: f64,
}

impl ParityMetrics {
    fn new(field: &'static str, tolerance_description: impl Into<String>) -> Self {
        Self {
            field,
            tolerance_description: tolerance_description.into(),
            entities_compared: 0,
            entities_matching: 0,
            agreement_pct: 0.0,
        }
    }

    fn record(&mut self, matched: bool) {
        self.entities_compared += 1;
        if matched {
            self.entities_matching += 1;
        }
    }

    fn finalize(&mut self) {
        self.agreement_pct = if self.entities_compared > 0 {
            100.0 * self.entities_matching as f64 / self.entities_compared as f64
        } else {
            // No entities to compare is not "100% agreement" -- it's "no
            // evidence." Reported as 0.0 so an empty diff can never masquerade
            // as a clean parity result.
            0.0
        };
    }
}

/// A human-readable recommendation, computed from the metrics/divergences
/// above -- **never** an authorization. See [`RetirementReadinessReport`]'s
/// doc and `headline` for the load-bearing "this never flips a switch"
/// guarantee.
#[derive(Debug, Clone, Serialize)]
pub enum ReadinessAssessment {
    /// Parity data suggests Tautulli could reasonably be retired for this
    /// function. Still just a recommendation.
    RecommendReady {
        overall_parity_pct: f64,
        coverage_pct: f64,
    },
    /// Parity data shows divergences (field-value and/or coverage) that
    /// should be fixed first.
    RecommendNotReady {
        overall_parity_pct: f64,
        coverage_pct: f64,
        blocking_divergence_count: usize,
    },
}

/// Overall (field-value) parity threshold used only to color the
/// *recommendation* text — it has no effect on any system state and
/// authorizes nothing. Chosen high (99%) because "sustained parity ... over
/// a window" (per the AC) is meant to be a strong signal, not a bare
/// majority. NOTE: this only governs the *overlap* — see
/// [`COVERAGE_COMPLETE_THRESHOLD_PCT`] for the entity-set gate that closes
/// the false-ready blind spot.
const RECOMMEND_READY_PARITY_THRESHOLD_PCT: f64 = 99.0;

/// Entity-set coverage threshold. A retirement-readiness recommendation
/// requires FULL coverage (100%): every entity Tautulli has must also exist
/// on the Muse side, and vice-versa. This is deliberately stricter than the
/// field-parity threshold and carries NO tolerance band — matching 3 of
/// Tautulli's 1000 watch-stats perfectly is not "ready to retire," it is a
/// 0.3%-coverage report, and retiring Tautulli against it would silently
/// drop 997 entities' worth of watch data. If a justified coverage tolerance
/// is ever wanted (e.g. to ignore a known-unmappable long tail), it must be
/// introduced here explicitly and defended in review — never assumed.
const COVERAGE_COMPLETE_THRESHOLD_PCT: f64 = 100.0;

/// The retirement-readiness evidence report: parity %, itemized
/// divergences, and the window/vintage the comparison ran over. This type
/// is pure data — see the module doc and
/// `tests::retirement_is_never_auto_triggered_even_at_100_percent_parity`
/// for the proof that nothing here (or anywhere else in the crate) consumes
/// it to flip a "retire Tautulli" switch. <operator> decides; this report only
/// informs that decision.
#[derive(Debug, Clone, Serialize)]
pub struct RetirementReadinessReport {
    pub generated_at: DateTime<Utc>,
    /// Operator-supplied description of the comparison window / data
    /// vintage (e.g. "snapshot pulled 2026-07-01, covers 2026-06-01..2026-07-01"
    /// or "MUSE_SNAPSHOT_DATABASE_URL scratch load, ad hoc run") — this
    /// module has no notion of "the last N days" on its own; it diffs
    /// exactly the data it was handed, and callers must describe that
    /// data's provenance so the report isn't misread as covering more than
    /// it does.
    pub window_description: String,
    pub muse_side_entities: usize,
    pub tautulli_side_entities: usize,
    pub entities_present_on_both_sides: usize,
    /// Entities Muse computed that Tautulli never had (spurious/extra) —
    /// each also itemized as a `CoverageMissingOnTautulli` divergence.
    pub muse_only_entities: usize,
    /// Entities Tautulli has that Muse computed nothing for (a Tautulli
    /// function Muse does not yet replicate) — each also itemized as a
    /// `CoverageMissingOnMuse` divergence. This is the count that makes a
    /// "100% parity on the overlap" report honest.
    pub tautulli_only_entities: usize,
    /// Fraction of the entity UNION present on both sides, as a percentage:
    /// `100 * both / (both + muse_only + tautulli_only)`. 100.0 means the
    /// two sides agree on exactly which entities exist; anything less means
    /// there are coverage gaps regardless of how well the overlap matches.
    pub coverage_pct: f64,
    pub metrics: Vec<ParityMetrics>,
    pub divergences: Vec<Divergence>,
    pub overall_parity_pct: f64,
    pub assessment: ReadinessAssessment,
}

impl RetirementReadinessReport {
    /// A one-paragraph human-readable summary. Always states the
    /// human-decision disclaimer verbatim, regardless of how high parity
    /// is — this is the one piece of "behaviour" this module has, and it
    /// is deliberately incapable of ever recommending anything stronger
    /// than a recommendation.
    pub fn headline(&self) -> String {
        let recommendation = match &self.assessment {
            ReadinessAssessment::RecommendReady {
                overall_parity_pct,
                coverage_pct,
            } => format!(
                "overlap parity {overall_parity_pct:.2}% at {coverage_pct:.2}% entity coverage \
                 over {} with {} itemized divergence(s) \
                 -- recommend ready to retire Tautulli for this function",
                self.window_description,
                self.divergences.len()
            ),
            ReadinessAssessment::RecommendNotReady {
                overall_parity_pct,
                coverage_pct,
                blocking_divergence_count,
            } => format!(
                "overlap parity {overall_parity_pct:.2}% at {coverage_pct:.2}% entity coverage \
                 ({} muse-only, {} tautulli-only) over {} with {} itemized divergence(s) \
                 ({blocking_divergence_count} blocking) -- NOT ready to retire Tautulli for this function",
                self.muse_only_entities,
                self.tautulli_only_entities,
                self.window_description,
                self.divergences.len()
            ),
        };
        format!(
            "{recommendation}. This is evidence only -- retirement is NOT AUTHORIZED by this \
             report and is never auto-triggered; it is <operator>'s decision to make."
        )
    }
}

fn approx_eq_i32(muse: i32, tautulli: i32) -> bool {
    muse == tautulli
}

fn approx_eq_watched_ms(muse: i64, tautulli: i64) -> bool {
    if muse == tautulli {
        return true;
    }
    let denom = muse.max(tautulli).max(1) as f64;
    let delta = (muse - tautulli).unsigned_abs() as f64;
    (delta / denom) <= WATCHED_MS_TOLERANCE_FRACTION
}

fn approx_eq_percent(muse: Option<f32>, tautulli: Option<f32>) -> bool {
    match (muse, tautulli) {
        (None, None) => true,
        (Some(m), Some(t)) => (m - t).abs() <= AVG_PERCENT_TOLERANCE,
        // One side has no percent data at all -- not comparable, treated as
        // a divergence rather than silently skipped (a caller reading the
        // report should see that the data is incomplete, not assume parity).
        _ => false,
    }
}

/// Diff Muse's shadow output ([`ShadowResult`], MUSET-08) against
/// Tautulli's own snapshot-derived numbers ([`TautulliWatchStat`],
/// aggregated by [`aggregate_tautulli_stats`]) and assemble the retirement-
/// readiness evidence report.
///
/// Pure, in-memory, synchronous — takes no `PgPool` and performs no I/O of
/// any kind. Both inputs must already have been fetched by the caller
/// (typically: `shadow::run(pool)` for `muse`, and
/// `aggregate_tautulli_stats(&repo::play_event::list_tautulli_snapshot_events(pool).await?)`
/// for `tautulli`). Keeping this function pure is what makes the "no write
/// path to a retirement switch" guarantee checkable by inspection: there is
/// nowhere in its signature for a database handle to even enter.
pub fn build_report(
    muse: &ShadowResult,
    tautulli: &[TautulliWatchStat],
    window_description: impl Into<String>,
) -> RetirementReadinessReport {
    let muse_by_key: BTreeMap<(Option<String>, Option<String>), &ShadowWatchStat> = muse
        .stats
        .iter()
        .filter(|s| s.rating_key.is_some())
        .map(|s| ((s.account_ref.clone(), s.rating_key.clone()), s))
        .collect();
    let tautulli_by_key: BTreeMap<(Option<String>, Option<String>), &TautulliWatchStat> = tautulli
        .iter()
        .filter(|s| s.rating_key.is_some())
        .map(|s| ((s.account_ref.clone(), s.rating_key.clone()), s))
        .collect();

    let mut play_count_metric = ParityMetrics::new("play_count", EXACT_COUNT_TOLERANCE_DESCRIPTION);
    let mut finished_count_metric =
        ParityMetrics::new("finished_count", EXACT_COUNT_TOLERANCE_DESCRIPTION);
    let mut watched_ms_metric = ParityMetrics::new(
        "total_watched_ms",
        format!(
            "relative tolerance {:.0}%",
            WATCHED_MS_TOLERANCE_FRACTION * 100.0
        ),
    );
    let mut avg_percent_metric = ParityMetrics::new(
        "avg_percent",
        format!(
            "absolute tolerance {:.0} points",
            AVG_PERCENT_TOLERANCE * 100.0
        ),
    );

    let mut divergences = Vec::new();
    let mut both_sides_count = 0usize;
    let mut muse_only_entities = 0usize;
    let mut tautulli_only_entities = 0usize;

    // Iterate the UNION of keys, not just the intersection. An
    // intersection-only walk would silently ignore every entity present on
    // exactly one side -- the false-ready blind spot. `BTreeMap` keys are
    // sorted, so a merge over the two key sets is both complete and
    // deterministic.
    let union_keys: std::collections::BTreeSet<&(Option<String>, Option<String>)> =
        muse_by_key.keys().chain(tautulli_by_key.keys()).collect();

    for key in union_keys {
        match (muse_by_key.get(key), tautulli_by_key.get(key)) {
            (Some(muse_stat), Some(tautulli_stat)) => {
                both_sides_count += 1;

                let play_count_match =
                    approx_eq_i32(muse_stat.play_count, tautulli_stat.play_count);
                play_count_metric.record(play_count_match);
                if !play_count_match {
                    divergences.push(Divergence {
                        kind: DivergenceKind::FieldValue,
                        account_ref: key.0.clone(),
                        rating_key: key.1.clone(),
                        field: "play_count",
                        muse_value: muse_stat.play_count.to_string(),
                        tautulli_value: tautulli_stat.play_count.to_string(),
                        delta_description: (muse_stat.play_count - tautulli_stat.play_count)
                            .to_string(),
                    });
                }

                let finished_match =
                    approx_eq_i32(muse_stat.finished_count, tautulli_stat.finished_count);
                finished_count_metric.record(finished_match);
                if !finished_match {
                    divergences.push(Divergence {
                        kind: DivergenceKind::FieldValue,
                        account_ref: key.0.clone(),
                        rating_key: key.1.clone(),
                        field: "finished_count",
                        muse_value: muse_stat.finished_count.to_string(),
                        tautulli_value: tautulli_stat.finished_count.to_string(),
                        delta_description: (muse_stat.finished_count
                            - tautulli_stat.finished_count)
                            .to_string(),
                    });
                }

                let watched_ms_match = approx_eq_watched_ms(
                    muse_stat.total_watched_ms,
                    tautulli_stat.total_watched_ms,
                );
                watched_ms_metric.record(watched_ms_match);
                if !watched_ms_match {
                    divergences.push(Divergence {
                        kind: DivergenceKind::FieldValue,
                        account_ref: key.0.clone(),
                        rating_key: key.1.clone(),
                        field: "total_watched_ms",
                        muse_value: muse_stat.total_watched_ms.to_string(),
                        tautulli_value: tautulli_stat.total_watched_ms.to_string(),
                        delta_description: (muse_stat.total_watched_ms
                            - tautulli_stat.total_watched_ms)
                            .to_string(),
                    });
                }

                let percent_match =
                    approx_eq_percent(muse_stat.avg_percent, tautulli_stat.avg_percent);
                avg_percent_metric.record(percent_match);
                if !percent_match {
                    divergences.push(Divergence {
                        kind: DivergenceKind::FieldValue,
                        account_ref: key.0.clone(),
                        rating_key: key.1.clone(),
                        field: "avg_percent",
                        muse_value: format!("{:?}", muse_stat.avg_percent),
                        tautulli_value: format!("{:?}", tautulli_stat.avg_percent),
                        delta_description: match (muse_stat.avg_percent, tautulli_stat.avg_percent)
                        {
                            (Some(m), Some(t)) => format!("{:.4}", m - t),
                            _ => "not comparable (missing on one side)".to_string(),
                        },
                    });
                }
            }
            // Tautulli has this entity, Muse computed nothing for it -- a
            // coverage gap that would LOSE watch data if Tautulli were
            // retired now. First-class blocking divergence.
            (None, Some(tautulli_stat)) => {
                tautulli_only_entities += 1;
                divergences.push(Divergence {
                    kind: DivergenceKind::CoverageMissingOnMuse,
                    account_ref: key.0.clone(),
                    rating_key: key.1.clone(),
                    field: "coverage",
                    muse_value: "absent".to_string(),
                    tautulli_value: format!(
                        "play_count={}, finished_count={}, total_watched_ms={}",
                        tautulli_stat.play_count,
                        tautulli_stat.finished_count,
                        tautulli_stat.total_watched_ms
                    ),
                    delta_description:
                        "entity present in Tautulli but MISSING from Muse's shadow output"
                            .to_string(),
                });
            }
            // Muse computed an entity Tautulli never had -- spurious/extra.
            // Also a coverage disagreement, also blocking.
            (Some(muse_stat), None) => {
                muse_only_entities += 1;
                divergences.push(Divergence {
                    kind: DivergenceKind::CoverageMissingOnTautulli,
                    account_ref: key.0.clone(),
                    rating_key: key.1.clone(),
                    field: "coverage",
                    muse_value: format!(
                        "play_count={}, finished_count={}, total_watched_ms={}",
                        muse_stat.play_count, muse_stat.finished_count, muse_stat.total_watched_ms
                    ),
                    tautulli_value: "absent".to_string(),
                    delta_description:
                        "entity present in Muse's shadow output but MISSING from Tautulli"
                            .to_string(),
                });
            }
            // Impossible: a union key came from at least one of the two maps.
            (None, None) => unreachable!("union key must exist on at least one side"),
        }
    }

    for metric in [
        &mut play_count_metric,
        &mut finished_count_metric,
        &mut watched_ms_metric,
        &mut avg_percent_metric,
    ] {
        metric.finalize();
    }

    let metrics = vec![
        play_count_metric,
        finished_count_metric,
        watched_ms_metric,
        avg_percent_metric,
    ];

    let overall_parity_pct = if metrics.is_empty() {
        0.0
    } else {
        metrics.iter().map(|m| m.agreement_pct).sum::<f64>() / metrics.len() as f64
    };

    // Coverage = fraction of the entity UNION present on both sides. Full
    // coverage (100%) means the two sides agree on which entities exist;
    // anything less means the report is over an incomplete slice and cannot
    // support retiring Tautulli no matter how well the overlap matches.
    let union_size = both_sides_count + muse_only_entities + tautulli_only_entities;
    let coverage_pct = if union_size > 0 {
        100.0 * both_sides_count as f64 / union_size as f64
    } else {
        // No entities on either side is "no evidence," not "perfect
        // coverage" -- reported as 0.0 so an empty report can never present
        // as ready.
        0.0
    };

    // Readiness now GATES on entity-set coverage in addition to overlap
    // parity: RecommendReady requires (1) at least one comparable entity,
    // (2) NO divergences of ANY kind -- which, because coverage gaps are
    // themselves divergences, already implies full coverage, but we also
    // (3) assert coverage_pct explicitly so the gate is legible and robust
    // to future divergence-collection changes, and (4) high overlap parity.
    let assessment = if both_sides_count > 0
        && divergences.is_empty()
        && coverage_pct >= COVERAGE_COMPLETE_THRESHOLD_PCT
        && overall_parity_pct >= RECOMMEND_READY_PARITY_THRESHOLD_PCT
    {
        ReadinessAssessment::RecommendReady {
            overall_parity_pct,
            coverage_pct,
        }
    } else {
        ReadinessAssessment::RecommendNotReady {
            overall_parity_pct,
            coverage_pct,
            blocking_divergence_count: divergences.len(),
        }
    };

    RetirementReadinessReport {
        generated_at: Utc::now(),
        window_description: window_description.into(),
        muse_side_entities: muse_by_key.len(),
        tautulli_side_entities: tautulli_by_key.len(),
        entities_present_on_both_sides: both_sides_count,
        muse_only_entities,
        tautulli_only_entities,
        coverage_pct,
        metrics,
        divergences,
        overall_parity_pct,
        assessment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tautulli_event(
        account_ref: &str,
        rating_key: &str,
        duration_secs: i64,
        percent_complete: f64,
        watched_status: f64,
    ) -> PlayEvent {
        PlayEvent {
            id: 1,
            received_at: Utc::now(),
            source: "snapshot:tautulli".to_string(),
            event_type: "snapshot.history".to_string(),
            account_ref: Some(account_ref.to_string()),
            session_key: None,
            rating_key: Some(rating_key.to_string()),
            view_offset_ms: None,
            player: None,
            platform: None,
            product: None,
            device: None,
            ip_address: None,
            raw: serde_json::json!({
                "duration": duration_secs,
                "percent_complete": percent_complete,
                "watched_status": watched_status,
            }),
        }
    }

    fn shadow_stat(
        account_ref: &str,
        rating_key: &str,
        play_count: i32,
        finished_count: i32,
        total_watched_ms: i64,
        avg_percent: Option<f32>,
    ) -> ShadowWatchStat {
        ShadowWatchStat {
            account_id: None,
            account_ref: Some(account_ref.to_string()),
            media_item_id: None,
            episode_id: None,
            rating_key: Some(rating_key.to_string()),
            play_count,
            finished_count,
            abandoned_count: 0,
            total_watched_ms,
            avg_percent,
            first_started_at: None,
            last_started_at: None,
        }
    }

    #[test]
    fn aggregate_tautulli_stats_rolls_up_by_account_and_rating_key() {
        let events = vec![
            tautulli_event("user-1", "rk-1", 90, 95.0, 1.0),
            tautulli_event("user-1", "rk-1", 10, 8.0, 0.0),
            tautulli_event("user-2", "rk-1", 50, 50.0, 0.5),
        ];
        let stats = aggregate_tautulli_stats(&events);
        assert_eq!(
            stats.len(),
            2,
            "two distinct (account_ref, rating_key) pairs"
        );

        let user1 = stats
            .iter()
            .find(|s| s.account_ref.as_deref() == Some("user-1"))
            .unwrap();
        assert_eq!(user1.play_count, 2);
        assert_eq!(
            user1.finished_count, 1,
            "only the watched_status==1.0 row counts"
        );
        assert_eq!(user1.total_watched_ms, 100_000, "(90 + 10) seconds -> ms");
        assert!((user1.avg_percent.unwrap() - 0.515).abs() < 0.001);
    }

    #[test]
    fn aggregate_tautulli_stats_tolerates_missing_fields() {
        let mut event = tautulli_event("user-1", "rk-1", 90, 95.0, 1.0);
        event.raw = serde_json::json!({"unrelated": true});
        let stats = aggregate_tautulli_stats(&[event]);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].play_count, 1);
        assert_eq!(stats[0].finished_count, 0);
        assert_eq!(stats[0].total_watched_ms, 0);
        assert_eq!(stats[0].avg_percent, None);
    }

    #[test]
    fn build_report_flags_no_divergence_for_matching_data() {
        let muse = ShadowResult {
            computed_at: Utc::now(),
            session_keys_considered: 1,
            sessions_folded: 1,
            stats: vec![shadow_stat("user-1", "rk-1", 1, 1, 90_000, Some(0.95))],
        };
        let tautulli = vec![TautulliWatchStat {
            account_ref: Some("user-1".to_string()),
            rating_key: Some("rk-1".to_string()),
            play_count: 1,
            finished_count: 1,
            total_watched_ms: 90_500, // within the 5% band of 90_000
            avg_percent: Some(0.951), // within the 0.02 absolute band
        }];

        let report = build_report(&muse, &tautulli, "unit test fixture");
        assert!(
            report.divergences.is_empty(),
            "expected no divergences: {:?}",
            report.divergences
        );
        assert_eq!(report.entities_present_on_both_sides, 1);
        assert_eq!(report.muse_only_entities, 0);
        assert_eq!(report.tautulli_only_entities, 0);
        assert_eq!(report.coverage_pct, 100.0, "full entity coverage");
        assert_eq!(report.overall_parity_pct, 100.0);
        assert!(matches!(
            report.assessment,
            ReadinessAssessment::RecommendReady { .. }
        ));
    }

    #[test]
    fn build_report_itemizes_a_real_divergence() {
        let muse = ShadowResult {
            computed_at: Utc::now(),
            session_keys_considered: 1,
            sessions_folded: 1,
            stats: vec![shadow_stat("user-1", "rk-1", 2, 1, 90_000, Some(0.95))],
        };
        let tautulli = vec![TautulliWatchStat {
            account_ref: Some("user-1".to_string()),
            rating_key: Some("rk-1".to_string()),
            play_count: 1, // diverges from muse's 2
            finished_count: 1,
            total_watched_ms: 90_000,
            avg_percent: Some(0.95),
        }];

        let report = build_report(&muse, &tautulli, "unit test fixture");
        assert_eq!(report.divergences.len(), 1);
        let d = &report.divergences[0];
        assert_eq!(d.field, "play_count");
        assert_eq!(d.muse_value, "2");
        assert_eq!(d.tautulli_value, "1");
        assert!(matches!(
            report.assessment,
            ReadinessAssessment::RecommendNotReady { .. }
        ));
    }

    #[test]
    fn build_report_with_no_overlapping_entities_itemizes_coverage_gaps_and_is_not_ready() {
        // Two entities, one on each side, NO overlap. Previously this
        // produced zero divergences (the intersection-only blind spot);
        // now each one-sided entity is a first-class coverage divergence.
        let muse = ShadowResult {
            computed_at: Utc::now(),
            session_keys_considered: 0,
            sessions_folded: 0,
            stats: vec![shadow_stat(
                "user-1",
                "rk-only-on-muse",
                1,
                1,
                1000,
                Some(0.5),
            )],
        };
        let tautulli = vec![TautulliWatchStat {
            account_ref: Some("user-1".to_string()),
            rating_key: Some("rk-only-on-tautulli".to_string()),
            play_count: 1,
            finished_count: 1,
            total_watched_ms: 1000,
            avg_percent: Some(0.5),
        }];

        let report = build_report(&muse, &tautulli, "unit test fixture, no overlap");
        assert_eq!(report.entities_present_on_both_sides, 0);
        assert_eq!(report.muse_only_entities, 1);
        assert_eq!(report.tautulli_only_entities, 1);
        assert_eq!(report.coverage_pct, 0.0, "no entity is on both sides");
        assert_eq!(
            report.divergences.len(),
            2,
            "each one-sided entity must be itemized as a coverage divergence"
        );
        assert!(report
            .divergences
            .iter()
            .any(|d| d.kind == DivergenceKind::CoverageMissingOnMuse
                && d.rating_key.as_deref() == Some("rk-only-on-tautulli")
                && d.muse_value == "absent"));
        assert!(report
            .divergences
            .iter()
            .any(|d| d.kind == DivergenceKind::CoverageMissingOnTautulli
                && d.rating_key.as_deref() == Some("rk-only-on-muse")
                && d.tautulli_value == "absent"));
        assert!(
            matches!(
                report.assessment,
                ReadinessAssessment::RecommendNotReady { .. }
            ),
            "an empty/zero-coverage comparison must never present as ready"
        );
    }

    #[test]
    fn build_report_perfect_overlap_but_incomplete_coverage_is_not_ready() {
        // THE false-ready blind spot, directly: Muse computes ONE of
        // Tautulli's THREE entities, and matches that one perfectly. An
        // intersection-only report would say 100% parity, zero divergences,
        // "ready to retire" -- while silently missing 2 of 3 entities. The
        // coverage gate must catch this.
        let muse = ShadowResult {
            computed_at: Utc::now(),
            session_keys_considered: 1,
            sessions_folded: 1,
            stats: vec![shadow_stat("user-1", "rk-1", 3, 2, 300_000, Some(0.9))],
        };
        let tautulli = vec![
            TautulliWatchStat {
                account_ref: Some("user-1".to_string()),
                rating_key: Some("rk-1".to_string()),
                play_count: 3,
                finished_count: 2,
                total_watched_ms: 300_000,
                avg_percent: Some(0.9),
            },
            TautulliWatchStat {
                account_ref: Some("user-1".to_string()),
                rating_key: Some("rk-2".to_string()),
                play_count: 5,
                finished_count: 5,
                total_watched_ms: 500_000,
                avg_percent: Some(0.95),
            },
            TautulliWatchStat {
                account_ref: Some("user-2".to_string()),
                rating_key: Some("rk-3".to_string()),
                play_count: 1,
                finished_count: 0,
                total_watched_ms: 20_000,
                avg_percent: Some(0.1),
            },
        ];

        let report = build_report(&muse, &tautulli, "one-of-three coverage fixture");

        // The overlap is PERFECT -- proves the not-ready verdict is driven
        // by coverage, not by any field-value mismatch.
        assert_eq!(
            report.overall_parity_pct, 100.0,
            "the single overlapping entity matches on every field"
        );
        assert_eq!(report.entities_present_on_both_sides, 1);
        assert_eq!(
            report.tautulli_only_entities, 2,
            "rk-2 and rk-3 are missing on Muse"
        );
        assert_eq!(report.muse_only_entities, 0);
        assert!(
            (report.coverage_pct - (100.0 / 3.0)).abs() < 1e-9,
            "1 of 3 union entities on both sides == ~33.3% coverage, got {}",
            report.coverage_pct
        );

        // Both missing Tautulli entities are itemized so they're fixable.
        let missing: Vec<_> = report
            .divergences
            .iter()
            .filter(|d| d.kind == DivergenceKind::CoverageMissingOnMuse)
            .map(|d| d.rating_key.as_deref())
            .collect();
        assert!(missing.contains(&Some("rk-2")));
        assert!(missing.contains(&Some("rk-3")));

        // And despite perfect overlap parity, the verdict is NOT ready.
        assert!(
            matches!(
                report.assessment,
                ReadinessAssessment::RecommendNotReady { .. }
            ),
            "perfect overlap parity at 33% coverage must NOT be ready to retire Tautulli"
        );
    }

    // =======================================================================
    // Load-bearing negative test (AC): retirement is human-decided and is
    // NEVER auto-triggered by this report, at any parity level -- including
    // a perfect, zero-divergence 100% match. This is asserted by
    // observation of the report's own content/behaviour, not just by code
    // inspection:
    //   1. `build_report` takes no `PgPool`/handle of any kind -- there is
    //      structurally nowhere for a write path to enter (see its
    //      signature above); this test calls it exactly as any caller
    //      would and confirms the return value is inert data.
    //   2. Even the "recommend ready" branch's `headline()` text is
    //      required to say the report is not an authorization -- so a
    //      caller who only reads the headline (the most likely thing to
    //      get copy-pasted into a Plane comment/PR) still can't
    //      misinterpret a 100%-parity report as having retired anything.
    //   3. `RetirementReadinessReport` carries no boolean/enum field named
    //      or shaped like a "retired"/"authoritative" switch -- its only
    //      fields are the metrics/divergences/assessment evidence above.
    // =======================================================================
    #[test]
    fn retirement_is_never_auto_triggered_even_at_100_percent_parity() {
        let muse = ShadowResult {
            computed_at: Utc::now(),
            session_keys_considered: 3,
            sessions_folded: 3,
            stats: vec![
                shadow_stat("user-1", "rk-1", 4, 3, 400_000, Some(0.88)),
                shadow_stat("user-2", "rk-2", 1, 1, 90_000, Some(0.99)),
            ],
        };
        let tautulli = vec![
            TautulliWatchStat {
                account_ref: Some("user-1".to_string()),
                rating_key: Some("rk-1".to_string()),
                play_count: 4,
                finished_count: 3,
                total_watched_ms: 400_000,
                avg_percent: Some(0.88),
            },
            TautulliWatchStat {
                account_ref: Some("user-2".to_string()),
                rating_key: Some("rk-2".to_string()),
                play_count: 1,
                finished_count: 1,
                total_watched_ms: 90_000,
                avg_percent: Some(0.99),
            },
        ];

        let report = build_report(&muse, &tautulli, "perfect-parity fixture");

        // Sanity: this really is a perfect-parity, FULL-coverage report,
        // not an accidental pass -- otherwise the negative test below would
        // be vacuous.
        assert!(report.divergences.is_empty());
        assert_eq!(report.overall_parity_pct, 100.0);
        assert_eq!(report.coverage_pct, 100.0);
        assert!(matches!(
            report.assessment,
            ReadinessAssessment::RecommendReady { overall_parity_pct, coverage_pct }
                if overall_parity_pct == 100.0 && coverage_pct == 100.0
        ));

        // The actual negative assertions:
        let headline = report.headline();
        assert!(
            headline.contains("NOT AUTHORIZED"),
            "even a 100%-parity report's headline must say it is not an authorization; got: {headline}"
        );
        assert!(
            headline.to_ascii_lowercase().contains("<operator>'s decision"),
            "headline must attribute the actual decision to the human operator; got: {headline}"
        );

        // Structural: serialize the report and confirm no key/value pair
        // anywhere resembles a flipped "retired"/"authoritative" switch.
        // (If a future change added such a field, this would need to be
        // updated deliberately -- which is the point: it can't happen by
        // accident inside `build_report`, since the function is pure and
        // has no side-effect surface to begin with.)
        let json = serde_json::to_value(&report).expect("report must serialize");
        let json_str = json.to_string().to_ascii_lowercase();
        assert!(
            !json_str.contains("\"retired\":true") && !json_str.contains("\"authorized\":true"),
            "report JSON must never carry a flipped retirement/authorization switch: {json_str}"
        );

        // Calling `build_report` again with the same inputs must be a pure
        // function of those inputs -- no hidden state accrued anywhere
        // that a caller running this twice (e.g. re-generating the report)
        // could observe as a state change.
        let report2 = build_report(&muse, &tautulli, "perfect-parity fixture");
        assert_eq!(report.divergences.len(), report2.divergences.len());
        assert_eq!(report.overall_parity_pct, report2.overall_parity_pct);
    }

    // ===================================================================
    // DB-gated: an end-to-end run against the guarded snapshot/test
    // Postgres -- seeds BOTH a Muse-shadow-computable event stream (raw
    // play/scrobble events under a session_key) and a Tautulli
    // history-snapshot row for the SAME (account_ref, rating_key), runs
    // `shadow::run` + `repo::play_event::list_tautulli_snapshot_events`,
    // and builds a real `RetirementReadinessReport` from live-fetched
    // (but snapshot-only) data.
    //
    // Gated exactly like `shadow::tests::db_gated` / `snapshot::db_gated` --
    // skips cleanly when no MUSE_SNAPSHOT_DATABASE_URL/MUSE_TEST_DATABASE_URL
    // is configured; never touches anything live.
    // ===================================================================
    mod db_gated {
        use sqlx::PgPool;
        use uuid::Uuid;

        use super::*;
        use crate::models::play_event::NewPlayEvent;
        use crate::repo;
        use crate::snapshot::load;

        async fn snapshot_pool_or_skip(test_name: &str) -> Option<PgPool> {
            let Some(database_url) = load::snapshot_database_url_from_env() else {
                eprintln!(
                    "{} / {} not set -- skipping {test_name} (expected in the \
                     default test run; the parity report builder does not \
                     require a live DB)",
                    load::SNAPSHOT_DATABASE_URL_VAR,
                    load::TEST_DATABASE_URL_VAR,
                );
                return None;
            };
            let pool = load::connect_snapshot_db(&database_url)
                .await
                .expect("connect to the configured snapshot/test DSN (guard-checked)");
            load::migrate_snapshot_db(&pool)
                .await
                .expect("migrations should apply cleanly");
            Some(pool)
        }

        /// Seed a Muse-shadow-computable session (raw play + scrobble
        /// events, source `plex_webhook`, matching `shadow::tests::db_gated`'s
        /// convention) AND a Tautulli history-snapshot row (source
        /// `snapshot:tautulli`, event_type `snapshot.history`, matching
        /// `snapshot::normalize::normalize_tautulli_play_record`'s shape)
        /// for the same `(account_ref, rating_key)`, so the two sides have
        /// something to overlap on.
        async fn seed_overlapping_pair(pool: &PgPool, suffix: &str) -> (String, String) {
            let account_ref = format!("muset09-account-{suffix}");
            let rating_key = format!("muset09-rk-{suffix}");
            let session_key = format!("muset09-session-{suffix}");

            repo::play_event::insert(
                pool,
                &NewPlayEvent {
                    source: "plex_webhook".to_string(),
                    event_type: "media.play".to_string(),
                    account_ref: Some(account_ref.clone()),
                    session_key: Some(session_key.clone()),
                    rating_key: Some(rating_key.clone()),
                    view_offset_ms: Some(0),
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    raw: serde_json::json!({"duration": 100_000}),
                },
            )
            .await
            .expect("insert play event");
            repo::play_event::insert(
                pool,
                &NewPlayEvent {
                    source: "plex_webhook".to_string(),
                    event_type: "media.scrobble".to_string(),
                    account_ref: Some(account_ref.clone()),
                    session_key: Some(session_key.clone()),
                    rating_key: Some(rating_key.clone()),
                    view_offset_ms: Some(95_000),
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    raw: serde_json::json!({"duration": 100_000}),
                },
            )
            .await
            .expect("insert scrobble event");

            repo::play_event::insert(
                pool,
                &NewPlayEvent {
                    source: "snapshot:tautulli".to_string(),
                    event_type: "snapshot.history".to_string(),
                    account_ref: Some(account_ref.clone()),
                    session_key: None,
                    rating_key: Some(rating_key.clone()),
                    view_offset_ms: Some(95_000),
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    raw: serde_json::json!({
                        "duration": 95,
                        "percent_complete": 95.0,
                        "watched_status": 1.0,
                    }),
                },
            )
            .await
            .expect("insert tautulli history-snapshot event");

            (account_ref, rating_key)
        }

        #[tokio::test]
        async fn build_report_end_to_end_over_seeded_snapshot_data() {
            let Some(pool) =
                snapshot_pool_or_skip("build_report_end_to_end_over_seeded_snapshot_data").await
            else {
                return;
            };

            let suffix = Uuid::new_v4().simple().to_string();
            let (account_ref, rating_key) = seed_overlapping_pair(&pool, &suffix).await;

            let muse_result = crate::shadow::run(&pool)
                .await
                .expect("shadow run should succeed");
            let tautulli_events = repo::play_event::list_tautulli_snapshot_events(&pool)
                .await
                .expect("listing tautulli snapshot events should succeed");
            let tautulli_stats = aggregate_tautulli_stats(&tautulli_events);

            let report = build_report(
                &muse_result,
                &tautulli_stats,
                format!("db_gated fixture run, suffix={suffix}"),
            );

            // We only assert on the seeded pair's presence, not a global
            // zero-divergence claim -- other data may already be in a
            // shared scratch DB. Find our own entity's contribution:
            let our_muse_stat = muse_result
                .stats
                .iter()
                .find(|s| s.account_ref.as_deref() == Some(account_ref.as_str()));
            assert!(
                our_muse_stat.is_some(),
                "the seeded session should have produced a shadow stat"
            );
            let our_tautulli_stat = tautulli_stats
                .iter()
                .find(|s| s.account_ref.as_deref() == Some(account_ref.as_str()));
            assert!(
                our_tautulli_stat.is_some(),
                "the seeded history row should have produced a tautulli stat"
            );
            assert_eq!(
                our_tautulli_stat.unwrap().rating_key.as_deref(),
                Some(rating_key.as_str())
            );

            // The report is evidence, never an authorization, regardless of
            // what this run's parity happens to be.
            assert!(report.headline().contains("NOT AUTHORIZED"));

            sqlx::query("DELETE FROM play_events WHERE account_ref = $1")
                .bind(&account_ref)
                .execute(&pool)
                .await
                .ok();
        }
    }
}
