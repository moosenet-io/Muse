//! `taste_profile` / `taste_context_centroids` / `taste_signals` — the
//! per-account taste model + its raw, auditable inputs (spec §3.4).

use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

// Only `Serialize` (not `Deserialize`) — see the identical note on
// `models::embedding::Embedding`: `pgvector::Vector` has no `Default` impl,
// which `#[serde(skip)]` needs for `Deserialize` to synthesize the field.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct TasteProfile {
    pub account_id: i64,
    pub genre_affinity: Json,
    pub person_affinity: Json,
    pub keyword_affinity: Json,
    pub runtime_pref: Option<Json>,
    pub quality_sensitivity: Option<Json>,
    #[serde(skip)]
    pub overall_centroid: Option<Vector>,
    pub computed_at: DateTime<Utc>,
    pub model_notes: Option<String>,
}

/// Full replacement set for an upsert — the taste-recompute worker always
/// writes the complete recomputed profile.
#[derive(Debug, Clone)]
pub struct NewTasteProfile {
    pub account_id: i64,
    pub genre_affinity: Json,
    pub person_affinity: Json,
    pub keyword_affinity: Json,
    pub runtime_pref: Option<Json>,
    pub quality_sensitivity: Option<Json>,
    pub overall_centroid: Option<Vector>,
    pub model_notes: Option<String>,
}

// Only `Serialize` — same `pgvector::Vector` + `#[serde(skip)]` + `Default`
// constraint as `TasteProfile` above.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct TasteContextCentroid {
    pub account_id: i64,
    pub context_key: String,
    #[serde(skip)]
    pub centroid: Vector,
    pub sample_size: i32,
}

#[derive(Debug, Clone)]
pub struct NewTasteContextCentroid {
    pub account_id: i64,
    pub context_key: String,
    pub centroid: Vector,
    pub sample_size: i32,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct TasteSignal {
    pub id: i64,
    pub account_id: i64,
    pub media_item_id: Option<i64>,
    pub signal_type: String,
    pub weight: f32,
    pub context_key: Option<String>,
    pub note: Option<String>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewTasteSignal {
    pub account_id: i64,
    pub media_item_id: Option<i64>,
    pub signal_type: String,
    pub weight: f32,
    pub context_key: Option<String>,
    pub note: Option<String>,
}
