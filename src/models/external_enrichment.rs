//! `external_enrichment` — Terminus-tool-suite enrichment cache (spec §3.5):
//! forum sentiment, renewal news, trailers, deals, critic scores.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ExternalEnrichment {
    pub id: i64,
    pub media_item_id: i64,
    pub kind: String,
    pub source: String,
    pub payload: Json,
    pub confidence: Option<f32>,
    pub fetched_at: DateTime<Utc>,
    pub ttl_seconds: i32,
}

#[derive(Debug, Clone)]
pub struct NewExternalEnrichment {
    pub media_item_id: i64,
    pub kind: String,
    pub source: String,
    pub payload: Json,
    pub confidence: Option<f32>,
    pub ttl_seconds: i32,
}
