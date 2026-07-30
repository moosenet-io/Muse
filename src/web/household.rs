//! MUSE #85: the `GET` half of `/api/graph/*` — household viewing analytics
//! for the Constellation web GUI's Muse "taste" panel.
//!
//! ## Two unrelated things live at these paths, on purpose
//! `Terminus/constellation-web/src/hooks/useMuse.ts` fetches
//! `/api/graph/watch-history`, `/api/graph/group-dynamics` and
//! `/api/graph/taste-clusters` with a plain parameterless `GET`. The MUSEX-17
//! handlers already mounted there are **`POST`-only, client-fed KG
//! visualizations** over *Discord friend identities* (the caller sends
//! `friends`/`watches`/`co_views`/`personas`; the server assembles an
//! opt-in-filtered graph). They return `TasteMapViz`/`GroupDynamicsViz`.
//!
//! Those are NOT what the GUI asks for. It declares:
//!
//! ```text
//! MuseWatchHistory  { series: [{ date, [participant]: number }] }
//! MuseGroupDynamics { rows: [{ participant, watched_together_pct,
//!                              favorite_genre, sessions }] }
//! MuseTasteClusters { clusters: [{ cluster_id, label,
//!                                  points: [{ x, y, model }] }] }
//! ```
//!
//! — i.e. *household account* analytics. So this module adds `GET` handlers
//! at those paths reading `play_sessions` × `accounts`, and leaves the `POST`
//! handlers completely untouched. Same path, two verbs, two shapes: the GUI
//! chose the paths, and one route serving a read projection on `GET` and a
//! computed viz on `POST` is the smaller evil versus either breaking the
//! existing MUSEX-17 callers or shipping a GUI that cannot reach its own data.
//!
//! **What was deliberately NOT done:** adding `GET` verbs that reuse the
//! MUSEX-17 handlers with a defaulted `GraphSourceInput`. That assembles an
//! EMPTY graph and returns an empty visualization on every call — and
//! `useMuseSection` renders any 2xx body AS DATA, so it would render a
//! confident "nobody watches anything together". See
//! `crate::repo::household`'s module doc.
//!
//! Everything here is PROTECTED: it is per-account viewing data
//! (MUSEX-CAP-SEC-03), the same class `/api/taste` and `/api/curation` gate.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::http::AppState;
use crate::repo;

/// Trailing window for `/api/graph/watch-history` when the caller doesn't
/// pin one. 90 days keeps the series readable in a dashboard-sized chart
/// while comfortably covering a household's recent activity.
const DEFAULT_HISTORY_DAYS: i32 = 90;
/// Hard ceiling on the requested window — a dashboard card, not an export.
const MAX_HISTORY_DAYS: i32 = 730;

#[derive(Debug, Clone, Deserialize)]
pub struct HistoryQuery {
    pub days: Option<i32>,
}

/// `GET /api/graph/watch-history` — sessions per day per household member.
///
/// The response is the GUI's wide/pivoted form: one object per day, keyed
/// `date` plus one key per participant. `MuseWatchHistoryPoint` is declared as
/// `{ date: string; [seriesKey: string]: number | string }`, so a day where
/// someone watched nothing simply omits that key rather than carrying a zero —
/// the chart's own gap handling is then free to distinguish "no activity" from
/// "zero recorded".
pub async fn watch_history_get_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<HistoryQuery>,
) -> Json<Value> {
    let days = q
        .days
        .unwrap_or(DEFAULT_HISTORY_DAYS)
        .clamp(1, MAX_HISTORY_DAYS);

    let buckets = repo::household::watch_history(&state.pool, days)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "watch_history_get: query failed; serving empty series");
            Vec::new()
        });

    // BTreeMap keeps the days in chronological order without a second sort;
    // the SQL already returns them ordered, but the pivot has to group anyway.
    let mut by_day: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for b in buckets {
        let row = by_day.entry(b.day.to_string()).or_default();
        row.insert(b.participant, json!(b.sessions));
    }

    let series: Vec<Value> = by_day
        .into_iter()
        .map(|(date, mut row)| {
            row.insert("date".to_string(), json!(date));
            Value::Object(row)
        })
        .collect();

    Json(json!({ "series": series }))
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupDynamicsRow {
    pub participant: String,
    /// Percentage of this participant's sessions that overlap in time with a
    /// session on a DIFFERENT household account. This is a time-overlap
    /// proxy, not a couch-presence fact — see `repo::household::group_dynamics`
    /// for exactly what it catches and what it miscounts.
    pub watched_together_pct: f64,
    /// Empty string when this participant's watched titles carry no genre
    /// rows at all. `MuseGroupDynamicsRow.favorite_genre` is a non-optional
    /// `string`, so the key must be present; an empty string reads as "no
    /// genre known" in the table, whereas a fabricated placeholder like
    /// "Drama" would read as a finding.
    pub favorite_genre: String,
    pub sessions: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupDynamicsResponse {
    pub rows: Vec<GroupDynamicsRow>,
}

/// `GET /api/graph/group-dynamics` — per-household-member session counts,
/// co-viewing overlap, and most-watched genre.
pub async fn group_dynamics_get_handler(
    State(state): State<Arc<AppState>>,
) -> Json<GroupDynamicsResponse> {
    let rows = repo::household::group_dynamics(&state.pool)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "group_dynamics_get: query failed; serving empty");
            Vec::new()
        });

    Json(GroupDynamicsResponse {
        rows: rows
            .into_iter()
            .map(|r| GroupDynamicsRow {
                participant: r.participant,
                watched_together_pct: together_pct(r.together_sessions, r.sessions),
                favorite_genre: r.favorite_genre.unwrap_or_default(),
                sessions: r.sessions,
            })
            .collect(),
    })
}

/// Overlap share as a 0–100 percentage, rounded to one decimal.
///
/// Guards `sessions == 0` explicitly: the SQL only emits accounts that have
/// at least one session, so this is unreachable today, but a `0/0` here would
/// serialize as `NaN`, which is not valid JSON and would make `serde_json`
/// fail the whole response rather than just this row.
fn together_pct(together: i64, sessions: i64) -> f64 {
    if sessions <= 0 {
        return 0.0;
    }
    ((together as f64 / sessions as f64) * 1000.0).round() / 10.0
}

/// `GET /api/graph/taste-clusters` — returns `501`, and that is the honest
/// answer today.
///
/// `MuseTasteClusters` wants persona clusters as 2-D points with the
/// embedding model that produced them. Muse has the tables for this
/// (`personas.centroid`, `embeddings.embedding`, `taste_context_centroids`,
/// all `vector` columns) but **all three are empty** — the embedding pipeline
/// has never run on this deployment. There are therefore no centroids to
/// cluster and no model name to attribute.
///
/// Returning `{"clusters":[]}` would assert "this household has no taste
/// clusters", which `useMuseSection` renders as data. The truth is "no
/// embeddings have ever been computed", and `501` is the status the hook
/// already classifies as `not yet wired` — so the card degrades honestly.
///
/// Unblocking this is a pipeline/backfill job (compute embeddings → derive
/// personas → project to 2-D), not a handler change, which is why it is not
/// bolted on here. Tracked as MUSE #87.
pub async fn taste_clusters_get_handler() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "taste clusters are not computed yet",
            "detail": "personas, embeddings and taste_context_centroids are all empty — the \
                       embedding pipeline has never run on this deployment, so there are no \
                       centroids to cluster and no model to attribute. Returning an empty \
                       clusters list would falsely assert that this household has no taste \
                       clusters.",
            "tracked_as": "MUSE #87",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn together_pct_is_a_rounded_percentage() {
        assert_eq!(together_pct(1, 3), 33.3);
        assert_eq!(together_pct(0, 10), 0.0);
        assert_eq!(together_pct(10, 10), 100.0);
    }

    #[test]
    fn together_pct_never_divides_by_zero_into_a_nan() {
        // A NaN would serialize as invalid JSON and fail the WHOLE response,
        // not just this row — so this guard is a response-integrity guard,
        // not just arithmetic tidiness.
        let pct = together_pct(0, 0);
        assert!(pct.is_finite());
        assert_eq!(pct, 0.0);
        assert!(serde_json::to_string(&json!(pct)).is_ok());
    }

    #[test]
    fn group_dynamics_row_matches_the_use_muse_contract() {
        let json = serde_json::to_value(GroupDynamicsResponse {
            rows: vec![GroupDynamicsRow {
                participant: "Example Member".to_string(),
                watched_together_pct: 33.3,
                favorite_genre: "Science Fiction".to_string(),
                sessions: 42,
            }],
        })
        .unwrap();
        let row = &json["rows"][0];
        let mut keys: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["favorite_genre", "participant", "sessions", "watched_together_pct"]
        );
        assert!(row["sessions"].is_i64());
        assert!(row["watched_together_pct"].is_f64());
    }

    #[test]
    fn missing_genre_serializes_as_an_empty_string_not_null() {
        // `MuseGroupDynamicsRow.favorite_genre` is a non-optional `string`; a
        // `null` would render as "null" in the table cell.
        let json = serde_json::to_value(GroupDynamicsRow {
            participant: "Example Member".to_string(),
            watched_together_pct: 0.0,
            favorite_genre: String::new(),
            sessions: 1,
        })
        .unwrap();
        assert_eq!(json["favorite_genre"], "");
        assert!(!json["favorite_genre"].is_null());
    }

    #[tokio::test]
    async fn taste_clusters_returns_501_rather_than_a_falsely_empty_cluster_list() {
        let (status, Json(body)) = taste_clusters_get_handler().await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["tracked_as"], "MUSE #87");
        assert!(
            body.get("clusters").is_none(),
            "must not look like a successful empty payload"
        );
    }
}
