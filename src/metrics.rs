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

use prometheus::{CounterVec, HistogramVec, Registry, TextEncoder};

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

struct Metrics {
    registry: Registry,
    recommend_requests_total: CounterVec,
    recommend_duration_seconds: HistogramVec,
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
        registry
            .register(Box::new(recommend_duration_seconds.clone()))
            .expect("muse_recommend_duration_seconds: single registration at process startup");

        Metrics {
            registry,
            recommend_requests_total,
            recommend_duration_seconds,
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
