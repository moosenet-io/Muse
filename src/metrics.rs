//! PROMEX-03: application-level Prometheus metrics exporter for Muse.
//!
//! A process-global [`prometheus::Registry`] plus a small, fixed set of
//! application metrics — recommendation-engine request volume and latency —
//! exposed as `GET /metrics` in the standard Prometheus text exposition
//! format (see `crate::http::router`, mounted alongside `/health`).
//!
//! This mirrors the REFERENCE PATTERN merged in the `Terminus` repo
//! (`terminus-rs`'s `src/metrics/mod.rs`, PROMEX-01) and adapted for the
//! `Chord` repo (`src/metrics.rs`, PROMEX-02) — same process-global-
//! `OnceLock` idiom, same shape of metrics, same cardinality discipline —
//! adapted to Muse's domain: Muse is a media curation/recommendation
//! service, so its meaningful application metric is RECOMMEND requests
//! (`POST /recommend`, `GET /recommend/on_deck`, `GET /recommend/gaps` —
//! see `crate::curation::recommend`), not MCP tool calls or LLM inference.
//!
//! ## Design
//! - **One registry, lazily built once per process** (`OnceLock`), matching
//!   the Terminus/Chord pattern rather than pulling in a separate
//!   `lazy_static`/`once_cell` dependency.
//! - **Two metrics, deliberately minimal**:
//!   - `muse_recommend_requests_total{endpoint, result}` — a `CounterVec`,
//!     `result` is always `"ok"` or `"error"` (never a raw error message),
//!     so cardinality stays bounded by `endpoint count * 2`.
//!   - `muse_recommend_duration_seconds{endpoint}` — a `HistogramVec` of
//!     end-to-end handler latency (candidate gathering + ranking +
//!     rationale/because generation), default bucket boundaries.
//! - **No caller-controlled label values — and no `bounded_*_label` guard is
//!   needed here, unlike Terminus's `bounded_tool_label` or Chord's
//!   `bounded_model_label`.** The `endpoint` label is a closed, THREE-value
//!   set (`"recommend"`, `"on_deck"`, `"gaps"`) written as a Rust string
//!   literal at each of the three call sites in
//!   `crate::curation::recommend` — never parsed or derived from the
//!   request body, query string, or any other caller-supplied input (the
//!   handlers' actual request fields are `account_id: i64`, `limit: Option<i64>`,
//!   and boolean flags — none of which are ever used as a label value here).
//!   `result` is likewise a closed `"ok"`/`"error"` set computed from
//!   `MuseResult<_>::is_ok()`, never a raw error message. So both labels are
//!   bounded by construction and a mapping helper would add indirection
//!   without adding any safety.
//! - **Read-only, unauthenticated, always-on.** This crate's existing
//!   `/health` route is likewise unauthenticated (see `crate::http::router`'s
//!   doc comment, which enumerates the open vs. protected surface) — metrics
//!   are equally non-sensitive (counts and timings only, no account data, no
//!   titles/candidates), so `/metrics` is mounted the same way, no separate
//!   env gate and outside the `protected` sub-router (unlike `/recommend*`
//!   itself, which IS behind `auth::require_api_token` per MUSEX-CAP-SEC-03
//!   — the metrics endpoint never echoes any per-account recommendation
//!   content, only aggregate counts/timings).
//!
//! ## Usage
//! Call [`record_recommend`] once per completed request from each of the
//! three handlers in `crate::curation::recommend`
//! (`recommend_handler`/`on_deck_handler`/`gaps_handler`), timing from
//! handler entry to the point the response is built. Call [`gather_text`]
//! from the `/metrics` HTTP handler in `crate::http`.

use std::sync::OnceLock;
use std::time::Duration;

use prometheus::{CounterVec, HistogramVec, IntGauge, Registry, TextEncoder};

/// The result label recorded on `muse_recommend_requests_total`.
/// Deliberately a closed two-value set (never the raw error message) so the
/// metric's cardinality is bounded by `endpoint count * 2`, not by
/// arbitrary error text.
const RESULT_OK: &str = "ok";
const RESULT_ERROR: &str = "error";

/// The closed set of `endpoint` label values — one per handler in
/// `crate::curation::recommend`. Every call site passes one of these
/// literals directly; see this module's doc for why no `bounded_*_label`
/// guard is needed.
pub const ENDPOINT_RECOMMEND: &str = "recommend";
pub const ENDPOINT_ON_DECK: &str = "on_deck";
pub const ENDPOINT_GAPS: &str = "gaps";

/// MPRB-07: the closed `outcome` label set on
/// `muse_probe_backfill_files_total`. One literal per counter on
/// [`crate::media::backfill::BackfillReport`], written at the single call site
/// in [`record_probe_backfill`] — never derived from a path, an error message,
/// or anything else caller-supplied, so cardinality is fixed at six.
const BACKFILL_OUTCOMES: [&str; 6] = [
    "probed",
    "suspicious",
    "failed_retryable",
    "failed_terminal",
    "persist_failed",
    "skipped_unresolved",
];

/// MPRB-07: the closed `result` label set on `muse_probe_backfill_runs_total`.
/// `halted` carries no reason label: [`crate::media::backfill::HaltReason`] has
/// seven variants today and would grow, and a metric is not the place an
/// operator diagnoses which one — the run report and the log are.
const BACKFILL_RESULT_COMPLETED: &str = "completed";
const BACKFILL_RESULT_HALTED: &str = "halted";

struct Metrics {
    registry: Registry,
    recommend_requests_total: CounterVec,
    recommend_duration_seconds: HistogramVec,
    probe_backfill_files_total: CounterVec,
    probe_backfill_runs_total: CounterVec,
    probe_backfill_remaining: IntGauge,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

fn metrics() -> &'static Metrics {
    METRICS.get_or_init(|| {
        let registry = Registry::new();

        let recommend_requests_total = CounterVec::new(
            prometheus::Opts::new(
                "muse_recommend_requests_total",
                "Total number of Muse recommendation-engine requests, by endpoint and outcome.",
            ),
            &["endpoint", "result"],
        )
        .expect("muse_recommend_requests_total: static metric definition is well-formed");

        let recommend_duration_seconds = HistogramVec::new(
            prometheus::HistogramOpts::new(
                "muse_recommend_duration_seconds",
                "Muse recommendation-engine request latency in seconds, by endpoint.",
            ),
            &["endpoint"],
        )
        .expect("muse_recommend_duration_seconds: static metric definition is well-formed");

        registry
            .register(Box::new(recommend_requests_total.clone()))
            .expect("muse_recommend_requests_total: single registration at process startup");
        // MPRB-07: the probe backfill worker's counters.
        let probe_backfill_files_total = CounterVec::new(
            prometheus::Opts::new(
                "muse_probe_backfill_files_total",
                "Files the probe backfill worker has processed, by outcome.",
            ),
            &["outcome"],
        )
        .expect("muse_probe_backfill_files_total: static metric definition is well-formed");

        let probe_backfill_runs_total = CounterVec::new(
            prometheus::Opts::new(
                "muse_probe_backfill_runs_total",
                "Probe backfill runs finished, by whether they drained the queue or halted early.",
            ),
            &["result"],
        )
        .expect("muse_probe_backfill_runs_total: static metric definition is well-formed");

        // A GAUGE, and a MEASURED one: it mirrors the `remaining` count the run
        // read back out of `media_files`, and it is left untouched when that
        // measurement could not be taken. There is deliberately no
        // `muse_probe_backfill_eta_seconds` — see `media::backfill`.
        let probe_backfill_remaining = IntGauge::new(
            "muse_probe_backfill_remaining",
            "Files still in the probe backfill queue, as measured after the last run.",
        )
        .expect("muse_probe_backfill_remaining: static metric definition is well-formed");

        registry
            .register(Box::new(recommend_duration_seconds.clone()))
            .expect("muse_recommend_duration_seconds: single registration at process startup");
        registry
            .register(Box::new(probe_backfill_files_total.clone()))
            .expect("muse_probe_backfill_files_total: single registration at process startup");
        registry
            .register(Box::new(probe_backfill_runs_total.clone()))
            .expect("muse_probe_backfill_runs_total: single registration at process startup");
        registry
            .register(Box::new(probe_backfill_remaining.clone()))
            .expect("muse_probe_backfill_remaining: single registration at process startup");

        Metrics {
            registry,
            recommend_requests_total,
            recommend_duration_seconds,
            probe_backfill_files_total,
            probe_backfill_runs_total,
            probe_backfill_remaining,
        }
    })
}

/// Record one completed recommendation-engine request: increments
/// `muse_recommend_requests_total{endpoint, result}` and observes `duration`
/// into `muse_recommend_duration_seconds{endpoint}`.
///
/// `endpoint` MUST be one of [`ENDPOINT_RECOMMEND`], [`ENDPOINT_ON_DECK`], or
/// [`ENDPOINT_GAPS`] — a fixed literal passed at the call site, never a
/// caller-supplied string. See this module's doc for why no bounding helper
/// is required.
pub fn record_recommend(endpoint: &str, is_ok: bool, duration: Duration) {
    let m = metrics();
    let result = if is_ok { RESULT_OK } else { RESULT_ERROR };
    m.recommend_requests_total
        .with_label_values(&[endpoint, result])
        .inc();
    m.recommend_duration_seconds
        .with_label_values(&[endpoint])
        .observe(duration.as_secs_f64());
}

/// MPRB-07: record one finished probe backfill run.
///
/// **Every number here comes off the report; none is computed a second time.**
/// The report is the authority for what the run did, and a metric that
/// recomputed a count from the others is a second opinion that will eventually
/// disagree with the first.
pub fn record_probe_backfill(report: &crate::media::backfill::BackfillReport) {
    let m = metrics();
    // Positional, against the closed label array, so a counter added to the
    // report without a label added here fails to compile rather than being
    // silently unexported.
    let counts = [
        report.probed,
        report.suspicious,
        report.failed_retryable,
        report.failed_terminal,
        report.persist_failed,
        report.skipped_unresolved,
    ];
    for (outcome, count) in BACKFILL_OUTCOMES.iter().zip(counts) {
        m.probe_backfill_files_total
            .with_label_values(&[outcome])
            .inc_by(count as f64);
    }

    m.probe_backfill_runs_total
        .with_label_values(&[if report.halted.is_some() {
            BACKFILL_RESULT_HALTED
        } else {
            BACKFILL_RESULT_COMPLETED
        }])
        .inc();

    // Left at its previous value when the measurement was not taken: a gauge set
    // to 0 on an unavailable read reports an empty queue, which is the one
    // answer an operator must never be given wrongly.
    if let Some(remaining) = report.remaining {
        m.probe_backfill_remaining.set(remaining);
    }
}

/// Encode every registered metric in the Prometheus text exposition format
/// (the `GET /metrics` response body).
pub fn gather_text() -> String {
    let m = metrics();
    let families = m.registry.gather();
    let encoder = TextEncoder::new();
    encoder
        .encode_to_string(&families)
        .unwrap_or_else(|e| format!("# error encoding metrics: {e}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_recommend_appears_in_gathered_text() {
        record_recommend(ENDPOINT_RECOMMEND, true, Duration::from_millis(42));

        let text = gather_text();
        assert!(
            text.contains("muse_recommend_requests_total"),
            "expected counter family name in output:\n{text}"
        );
        assert!(
            text.contains("muse_recommend_duration_seconds"),
            "expected histogram family name in output:\n{text}"
        );
        assert!(
            text.contains("endpoint=\"recommend\""),
            "expected the recorded endpoint label in output:\n{text}"
        );
        assert!(
            text.contains("result=\"ok\""),
            "expected the ok result label in output:\n{text}"
        );
    }

    #[test]
    fn record_recommend_error_uses_error_result_label() {
        record_recommend(ENDPOINT_ON_DECK, false, Duration::from_millis(1));

        let text = gather_text();
        assert!(
            text.contains("endpoint=\"on_deck\",result=\"error\"")
                || text.contains("result=\"error\",endpoint=\"on_deck\""),
            "expected an error-result sample for the endpoint in output:\n{text}"
        );
    }

    /// One test, not two, and deliberately: `muse_probe_backfill_remaining` is a
    /// process-global GAUGE, so two tests asserting on its value would race in a
    /// multithreaded test binary and pass or fail by scheduling. The sequence is
    /// asserted in order inside one test instead. The COUNTERS are asserted by
    /// presence, not by value, for the same reason — this process may have run
    /// other backfills.
    #[test]
    fn a_backfill_run_is_exported_and_an_unmeasured_remaining_leaves_the_gauge_alone() {
        crate::metrics::record_probe_backfill(&crate::media::backfill::BackfillReport {
            considered: 3,
            probed: 2,
            suspicious: 1,
            failed_terminal: 1,
            remaining: Some(4_321),
            ..Default::default()
        });

        let text = gather_text();
        assert!(text.contains("muse_probe_backfill_files_total"), "{text}");
        assert!(text.contains("outcome=\"probed\""), "{text}");
        assert!(text.contains("outcome=\"failed_terminal\""), "{text}");
        assert!(text.contains("muse_probe_backfill_runs_total"), "{text}");
        assert!(
            text.contains("result=\"completed\""),
            "a run with no halt reason is a completed run:\n{text}"
        );
        assert!(
            text.contains("muse_probe_backfill_remaining 4321"),
            "the remaining gauge must carry the MEASURED count:\n{text}"
        );

        // A halted run that could not measure what is left.
        crate::metrics::record_probe_backfill(&crate::media::backfill::BackfillReport {
            halted: Some(crate::media::backfill::HaltReason::NoFfprobeOnThisHost),
            remaining: None,
            ..Default::default()
        });

        let text = gather_text();
        assert!(text.contains("result=\"halted\""), "{text}");
        assert!(
            text.contains("muse_probe_backfill_remaining 4321"),
            "an unavailable measurement must not be exported as an EMPTY queue:\n{text}"
        );
    }

    #[test]
    fn record_recommend_gaps_endpoint_label_is_distinct() {
        record_recommend(ENDPOINT_GAPS, true, Duration::from_millis(5));

        let text = gather_text();
        assert!(
            text.contains("endpoint=\"gaps\""),
            "expected the gaps endpoint label in output:\n{text}"
        );
    }
}
