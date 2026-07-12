//! MUSE-12: the proactive content generator — event-driven proactive
//! message generation → `proactive_items`, and the `/proactive` HTTP surface
//! Lumina's reminders/engagement scheduler (and the Terminus `muse_proactive`
//! surface, MUSE-13) polls.
//!
//! - [`generators`] — the five event-driven generators (new-season/gap,
//!   Friday-evening, abandonment insight, grab-window/freeleech,
//!   zeitgeist/were-early) + the cooldown/dedup orchestrator.
//! - [`scheduler`] — the background worker that runs the generators for
//!   every account on a cadence.
//!
//! This module replaces `http::proactive_routes()`'s previous 501 stub.

pub mod generators;
pub mod scheduler;

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::error::{MuseError, MuseResult};
use crate::http::AppState;
use crate::models::proactive_item::ProactiveItem;
use crate::repo::proactive_item::AckOutcome;

/// The `/proactive/pending` response — one entry per undelivered, currently
/// eligible `proactive_items` row (see
/// `repo::proactive_item::list_pending_for_account`'s cooldown/eligibility
/// filter: `earliest_at` passed, `expires_at` not yet passed, not yet
/// delivered).
#[derive(Debug, Serialize)]
pub struct PendingResponse {
    pub items: Vec<ProactiveItem>,
}

#[derive(Debug, Deserialize)]
pub struct PendingQuery {
    pub account_id: i64,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Default/max page size for `GET /proactive/pending` — mirrors
/// `curation::recommend`'s `DEFAULT_RECOMMEND_LIMIT`/`MAX_RECOMMEND_LIMIT`
/// posture (a sane default, a hard ceiling, never unbounded).
const DEFAULT_PENDING_LIMIT: i64 = 20;
const MAX_PENDING_LIMIT: i64 = 100;

fn clamp_limit(requested: Option<i64>) -> i64 {
    requested.unwrap_or(DEFAULT_PENDING_LIMIT).clamp(1, MAX_PENDING_LIMIT)
}

/// `GET /proactive/pending?account_id=&limit=` — the surface Lumina's
/// reminders/engagement scheduler (and the Terminus `muse_proactive` tool,
/// MUSE-13) polls. Strictly scoped to `account_id` — never blends another
/// account's nudges in.
pub async fn pending_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<PendingQuery>,
) -> MuseResult<Json<PendingResponse>> {
    let limit = clamp_limit(q.limit);
    let now = chrono::Utc::now();

    let items = crate::repo::proactive_item::list_pending_for_account(&state.pool, q.account_id, now)
        .await?
        .into_iter()
        .take(limit as usize)
        .collect();

    Ok(Json(PendingResponse { items }))
}

#[derive(Debug, Deserialize)]
pub struct AckRequest {
    /// `"sent"` (Lumina delivered it) or `"dismissed"` (the account waved it
    /// off). Any other value is a 400, not a silent no-op.
    pub outcome: String,
}

#[derive(Debug, Serialize)]
pub struct AckResponse {
    pub item: ProactiveItem,
}

/// `POST /proactive/{id}/ack` — mark a proactive item `sent` or `dismissed`.
/// `sent` sets `delivered_at` (so it also drops out of
/// `list_pending_for_account`'s `delivered_at IS NULL` filter, keeping the
/// MUSE-03 pending-query and the MUSE-12 `status` column consistent with
/// each other); `dismissed` sets `dismissed_at` instead.
pub async fn ack_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(req): Json<AckRequest>,
) -> MuseResult<Json<AckResponse>> {
    let outcome = match req.outcome.as_str() {
        "sent" => AckOutcome::Sent,
        "dismissed" => AckOutcome::Dismissed,
        other => {
            return Err(MuseError::BadRequest(format!(
                "invalid ack outcome {other:?}: expected \"sent\" or \"dismissed\""
            )))
        }
    };

    let item = crate::repo::proactive_item::ack(&state.pool, id, outcome, chrono::Utc::now()).await?;
    Ok(Json(AckResponse { item }))
}
