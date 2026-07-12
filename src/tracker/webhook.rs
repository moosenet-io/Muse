//! (A) Plex webhook receiver — spec §4-A.
//!
//! `POST /ingest/plex-webhook`. Plex Pass posts `multipart/form-data` with
//! the event JSON in a `payload` field (an optional `thumb` image field is
//! ignored). This handler must **never** answer non-2xx for a bad delivery
//! — Plex retries/backs off aggressively on failure, and one malformed or
//! unrecognized event must never take ingestion down for every subsequent
//! well-formed one. Every failure path here is logged and still answers
//! `200 OK`.

use std::sync::Arc;

use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use serde_json::Value as Json;

use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::account::NewAccount;
use crate::models::play_event::NewPlayEvent;
use crate::repo;

use super::reconstruct;

pub async fn plex_webhook(State(state): State<Arc<AppState>>, mut multipart: Multipart) -> StatusCode {
    let payload = match extract_payload(&mut multipart).await {
        Ok(Some(payload)) => payload,
        Ok(None) => {
            tracing::warn!("plex webhook: no 'payload' field in multipart body; ignoring");
            return StatusCode::OK;
        }
        Err(e) => {
            tracing::warn!(error = %e, "plex webhook: failed to read multipart body; ignoring");
            return StatusCode::OK;
        }
    };

    let parsed: Json = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "plex webhook: 'payload' field is not valid JSON; ignoring");
            return StatusCode::OK;
        }
    };

    if let Err(e) = handle_payload(&state, parsed).await {
        tracing::warn!(error = %e, "plex webhook: failed to persist/reconstruct event; ignoring");
    }

    StatusCode::OK
}

async fn extract_payload(
    multipart: &mut Multipart,
) -> Result<Option<String>, axum::extract::multipart::MultipartError> {
    while let Some(field) = multipart.next_field().await? {
        if field.name() == Some("payload") {
            let bytes = field.bytes().await?;
            return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
    Ok(None)
}

/// Best-effort field extraction. Every accessor here is defensive
/// (`Option`-returning, never panics) because Plex's webhook payload shape
/// is not guaranteed stable across server versions or event types.
fn str_field<'a>(v: &'a Json, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str()
}

fn account_ref(payload: &Json) -> Option<String> {
    let account = payload.get("Account")?;
    account
        .get("id")
        .and_then(|v| v.as_i64().map(|n| n.to_string()).or_else(|| v.as_str().map(str::to_string)))
}

fn player_uuid(payload: &Json) -> Option<&str> {
    str_field(payload, &["Player", "uuid"])
}

/// Plex webhook payloads don't reliably carry `Metadata.sessionKey` across
/// server versions (unlike `/status/sessions`, which always does — see
/// `tracker::poller`); when it's absent, synthesize a stable key from
/// (account, player, rating_key) so events from the same playback still
/// stitch into one session for reconstruction.
fn session_key(payload: &Json, account: Option<&str>, player: Option<&str>, rating_key: Option<&str>) -> String {
    if let Some(key) = str_field(payload, &["Metadata", "sessionKey"]) {
        return key.to_string();
    }
    format!(
        "webhook:{}:{}:{}",
        account.unwrap_or("_"),
        player.unwrap_or("_"),
        rating_key.unwrap_or("_")
    )
}

async fn handle_payload(state: &AppState, payload: Json) -> MuseResult<()> {
    let event_type = payload
        .get("event")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let account_ref = account_ref(&payload);
    let rating_key = str_field(&payload, &["Metadata", "ratingKey"]).map(str::to_string);
    let player_uuid = player_uuid(&payload).map(str::to_string);
    let session_key = session_key(&payload, account_ref.as_deref(), player_uuid.as_deref(), rating_key.as_deref());
    let view_offset_ms = payload
        .get("Metadata")
        .and_then(|m| m.get("viewOffset"))
        .and_then(|v| v.as_i64());
    let player = str_field(&payload, &["Player", "title"]).map(str::to_string);
    let ip_address = str_field(&payload, &["Player", "publicAddress"]).and_then(|s| s.parse().ok());

    let new_event = NewPlayEvent {
        source: "plex_webhook".to_string(),
        event_type: event_type.clone(),
        account_ref: account_ref.clone(),
        session_key: Some(session_key.clone()),
        rating_key: rating_key.clone(),
        view_offset_ms,
        player,
        // Plex's webhook `Player` block doesn't carry platform/product/
        // device the way `/status/sessions` does — those are populated by
        // the poller instead; a webhook-only deployment simply won't have
        // them until the next poll tick fills them in via context
        // last-write-wins in `reconstruct::fold_events`.
        platform: None,
        product: None,
        device: None,
        ip_address,
        raw: payload.clone(),
    };

    let inserted = repo::play_event::insert(&state.pool, &new_event).await?;

    // A deduped delivery (webhook retry) was already folded on its first
    // arrival — skip redundant work, not correctness (re-folding would give
    // an identical result either way).
    if inserted.is_none() {
        return Ok(());
    }

    if event_type == "media.rate" {
        if let (Some(account_ref), Some(rating_key)) = (&account_ref, &rating_key) {
            if let Err(e) = handle_rating(&state.pool, account_ref, rating_key, &payload).await {
                tracing::warn!(
                    error = %e,
                    "plex webhook: failed to record rating; play_event was still persisted"
                );
            }
        }
        return Ok(());
    }

    if let Err(e) = reconstruct::reconstruct_and_persist(&state.pool, &session_key).await {
        tracing::warn!(
            error = %e,
            session_key,
            "plex webhook: session reconstruction failed; play_event was still persisted"
        );
    }

    Ok(())
}

/// `media.rate` → upsert `ratings` (spec §4-A). Best-effort: only persists
/// when the rating value, account, and media item are all resolvable;
/// otherwise logs and returns `Ok(())` (the raw event is already durable in
/// `play_events` regardless).
async fn handle_rating(
    pool: &sqlx::PgPool,
    account_ref: &str,
    rating_key: &str,
    payload: &Json,
) -> MuseResult<()> {
    let rating = payload
        .get("rating")
        .or_else(|| payload.get("Metadata").and_then(|m| m.get("userRating")))
        .and_then(|v| v.as_f64());
    let Some(rating) = rating else {
        return Ok(());
    };

    let account = repo::account::upsert_by_plex_account_id(
        pool,
        &NewAccount {
            plex_account_id: Some(account_ref.to_string()),
            is_home_user: true,
            ..Default::default()
        },
    )
    .await?;

    let (media_item_id, _episode_id) = reconstruct::resolve_rating_key(pool, rating_key).await?;
    let Some(media_item_id) = media_item_id else {
        tracing::debug!(rating_key, "plex webhook: media.rate for an unresolved item; rating not persisted yet");
        return Ok(());
    };

    repo::watch_stats::upsert_rating(pool, account.id, media_item_id, rating as f32, chrono::Utc::now()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_key_prefers_explicit_metadata_key() {
        let payload = json!({"Metadata": {"sessionKey": "42"}});
        assert_eq!(session_key(&payload, Some("1"), Some("uuid"), Some("rk")), "42");
    }

    #[test]
    fn session_key_synthesizes_when_absent() {
        let payload = json!({});
        assert_eq!(session_key(&payload, Some("1"), Some("uuid-1"), Some("rk-1")), "webhook:1:uuid-1:rk-1");
    }

    #[test]
    fn account_ref_reads_numeric_id() {
        let payload = json!({"Account": {"id": 7, "title": "moose"}});
        assert_eq!(account_ref(&payload), Some("7".to_string()));
    }

    #[test]
    fn account_ref_missing_account_is_none() {
        assert_eq!(account_ref(&json!({})), None);
    }

    #[test]
    fn str_field_walks_nested_path_defensively() {
        let payload = json!({"Metadata": {"ratingKey": "100"}});
        assert_eq!(str_field(&payload, &["Metadata", "ratingKey"]), Some("100"));
        assert_eq!(str_field(&payload, &["Metadata", "missing"]), None);
        assert_eq!(str_field(&payload, &["NotThere", "ratingKey"]), None);
    }
}
