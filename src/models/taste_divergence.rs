//! `taste_divergence` — the "you vs the masses" radar payload, tracked over
//! time (spec §3.7/§4c, MUSE-20). See
//! `migrations/0044_taste_divergence.sql` for the table and
//! `crate::radar::divergence` for the math that produces the JSON fields.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct TasteDivergence {
    pub id: i64,
    pub account_id: i64,
    pub computed_at: DateTime<Utc>,
    /// `{genre: your_share/pop_share}` — see `radar::divergence::index_map`.
    pub genre_index: Json,
    pub decade_index: Option<Json>,
    /// 0..1 distribution-overlap based mainstream-ness (see
    /// `radar::divergence::mainstream_score` for why this isn't the spec's
    /// literal "cosine of centroids" — no embeddings pipeline exists yet).
    pub mainstream_score: Option<f32>,
    pub adventurousness: Option<f32>,
    pub contrarian_index: Option<f32>,
    /// `[{media_metadata_id, title, watched_at, trended_at, lead_days}]`.
    pub were_early: Option<Json>,
    /// `[{media_metadata_id, title, best_rank, popularity}]`.
    pub blind_spots: Option<Json>,
    /// `[{media_metadata_id, title, rewatch_count}]`.
    pub guilty_pleasures: Option<Json>,
}

/// A fresh radar snapshot to append — `taste_divergence` is append-only
/// (tracked over time), never upserted, so this is always a plain insert.
#[derive(Debug, Clone)]
pub struct NewTasteDivergence {
    pub account_id: i64,
    pub genre_index: Json,
    pub decade_index: Option<Json>,
    pub mainstream_score: Option<f32>,
    pub adventurousness: Option<f32>,
    pub contrarian_index: Option<f32>,
    pub were_early: Json,
    pub blind_spots: Json,
    pub guilty_pleasures: Json,
}
