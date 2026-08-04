//! MPRB-09 — the two read-only doors onto [`crate::media::coverage`].
//!
//! - `GET  /probe/coverage`            → the report as JSON
//! - `POST /ops/probe/coverage-report` → the same report as Markdown, for the
//!   operator to commit as `docs/reports/probe-coverage.md`
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

use crate::error::MuseResult;
use crate::http::AppState;
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
