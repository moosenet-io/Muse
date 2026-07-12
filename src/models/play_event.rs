//! `play_events` — raw Plex webhook/poll event stream (spec §3.3), the
//! immutable, append-only source play_sessions is reconstructed from.

use chrono::{DateTime, Utc};
use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct PlayEvent {
    pub id: i64,
    pub received_at: DateTime<Utc>,
    pub source: String,
    pub event_type: String,
    pub account_ref: Option<String>,
    pub session_key: Option<String>,
    pub rating_key: Option<String>,
    pub view_offset_ms: Option<i64>,
    pub player: Option<String>,
    pub platform: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    pub ip_address: Option<IpNetwork>,
    pub raw: Json,
}

#[derive(Debug, Clone)]
pub struct NewPlayEvent {
    pub source: String,
    pub event_type: String,
    pub account_ref: Option<String>,
    pub session_key: Option<String>,
    pub rating_key: Option<String>,
    pub view_offset_ms: Option<i64>,
    pub player: Option<String>,
    pub platform: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    pub ip_address: Option<IpNetwork>,
    pub raw: Json,
}
