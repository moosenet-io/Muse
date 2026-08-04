//! MPRB-09 — the two read-only doors onto [`crate::media::coverage`].
//!
//! - `GET  /probe/coverage`            → the report as JSON
//! - `POST /ops/probe/coverage-report` → the same report as Markdown, for the
//!   operator to commit as `docs/reports/probe-coverage.md`
//!
//! MPRB-07 added two more, on the same protected router:
//!
//! - `POST /ops/probe/backfill` → start one backfill run in the background
//! - `GET  /ops/probe/backfill` → whether one is running, and what the last one did
//!
//! Both are mounted on the **protected** router
//! (`crate::http::auth::require_api_token`), and both are read-only: the census
//! is a `SELECT` and nothing here writes a row, a file, or a metric.
//!
//! # Why the Markdown door is a POST
//!
//! It is generation, not retrieval: the operator asks for an artifact to be
//! produced at a point in time, and the response is meant to be redirected into
//! a file and committed. A `GET` returning a body that changes with every call
//! invites caching it, and a cached coverage report is a stale denominator
//! presented as a current one.
//!
//! # There is nothing to test here without a database
//!
//! Every handler is three lines: call the census, render, return. That is
//! deliberate — with no `MUSE_TEST_DATABASE_URL` (#130) anything expressed at
//! this layer cannot execute in a test, so the rules live in
//! [`crate::media::coverage`] where they run for real.

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;

use crate::error::{MuseError, MuseResult};
use crate::http::AppState;
use crate::media::backfill;
use crate::media::coverage::{self, render_markdown};

/// `GET /probe/coverage` — the coverage report as JSON.
pub async fn coverage_json(State(state): State<Arc<AppState>>) -> MuseResult<Json<serde_json::Value>> {
    let report = coverage::report_from_pool(&state.pool).await?;
    Ok(Json(serde_json::to_value(report).unwrap_or_else(|_| {
        // `CoverageReport` is counts and strings; there is no float in it that
        // could fail to serialise. An `unwrap` here would still be a panic in a
        // handler, so it degrades to an explicit object instead.
        serde_json::json!({ "error": "coverage report failed to serialise" })
    })))
}

/// `POST /ops/probe/coverage-report` — the same report as Markdown.
///
/// `text/markdown` rather than JSON-wrapped: the operator pipes this straight
/// into `docs/reports/probe-coverage.md`, and a JSON-escaped body would have to
/// be unescaped by hand before it was diffable.
pub async fn coverage_markdown(State(state): State<Arc<AppState>>) -> MuseResult<impl IntoResponse> {
    let report = coverage::report_from_pool(&state.pool).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        render_markdown(&report),
    ))
}

// --- MPRB-07: the probe backfill worker's two operator doors ---------------

/// `POST /ops/probe/backfill` — start one backfill run, in the background.
///
/// # Why it starts a run rather than being one
///
/// The queue is 16,221 files and the configured rate is 30/min: a full sweep is
/// hours. Holding an HTTP request open for that is not a design, it is a
/// timeout — the operator's proxy closes it, the client retries, and a second
/// sweep starts over the same queue. So the handler claims the run gate, spawns
/// the run, and returns `202 Accepted` immediately; `GET /ops/probe/backfill`
/// is where the answer arrives.
///
/// # One run at a time, enforced, not documented
///
/// A second call while a run is in flight is a `409` — two sweeps would double
/// the load on a shared NFS mount and interleave two cursors over one queue. The
/// gate is [`crate::media::backfill::RunGate`], and the permit releases on drop,
/// so a panicking run reopens the door rather than wedging it shut until
/// restart.
///
/// # Degrade
///
/// A host with no usable `ffprobe`, or with `MUSE_LIBRARY_ROOT` unset, does not
/// error: the run starts, finds itself inert, and reports a halt reason (Module
/// Contract §2). That is deliberately visible in the status payload rather than
/// hidden behind a `503` with no counters.
pub async fn backfill_start(State(state): State<Arc<AppState>>) -> MuseResult<impl IntoResponse> {
    // The gate is `&'static`, so the permit is `RunPermit<'static>` and moves
    // into the spawned task — the door stays shut for exactly as long as the run
    // lasts, with no second claim and no window between the two.
    let Some(permit) = backfill::global_gate().try_begin() else {
        return Err(MuseError::Conflict(
            "a probe backfill run is already in flight; poll GET /ops/probe/backfill".to_string(),
        ));
    };

    let config = backfill::BackfillConfig::resolve(&state.config);
    let task_state = Arc::clone(&state);
    tokio::spawn(async move {
        // Built inside the task, not on the request path: construction takes the
        // host capability snapshot, which costs three bounded subprocess spawns
        // (CAPDET-01), and the handler has already returned.
        let media = crate::media::MediaCore::from_config(&task_state.config);
        let report = backfill::run_from_pool(&task_state.pool, &media, config).await;
        tracing::info!(
            considered = report.considered,
            probed = report.probed,
            suspicious = report.suspicious,
            failed_retryable = report.failed_retryable,
            failed_terminal = report.failed_terminal,
            exhausted = report.exhausted,
            persist_failed = report.persist_failed,
            skipped_unresolved = report.skipped_unresolved,
            remaining = ?report.remaining,
            halted = ?report.halted,
            "probe backfill: run finished"
        );
        permit.complete(report);
    });

    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "started": true,
            "config": config,
        })),
    ))
}

/// `GET /ops/probe/backfill` — whether a run is in flight, and what the last one
/// did.
///
/// `last_run` is [`crate::media::backfill::BackfillReport`] verbatim: counts of
/// things that happened, plus a measured `remaining`. **No ETA** — see that
/// module for why an estimate computed from an average is a fabricated
/// measurement.
pub async fn backfill_status() -> Json<serde_json::Value> {
    let gate = backfill::global_gate();
    Json(serde_json::json!({
        "running": gate.is_running(),
        "last_run": gate.last_report(),
    }))
}
