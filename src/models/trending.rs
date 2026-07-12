//! Domain models for the trending/population feed (MUSE-19, spec §3.7).
//!
//! `media_metadata_id` diverges from the spec's `media_item_id` naming — see
//! `migrations/0041_trending_snapshots.sql` for why (tmdb_id lives on the
//! shared `media_metadata` table under the real MUSE-02 metadata/instance
//! split, not on per-library `media_items`; same divergence already made for
//! credits/genres/collections in `0011_people_genres_collections.sql`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

/// A single point-in-time trending/popular entry (one row per ranked title
/// per source/scope/window/region snapshot).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct TrendingSnapshot {
    pub id: i64,
    pub source: String,
    pub scope: String,
    pub platform: Option<String>,
    pub region: String,
    pub window: String,
    pub rank: Option<i32>,
    pub media_metadata_id: Option<i64>,
    pub external_ref: Option<Json>,
    pub popularity: Option<f32>,
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewTrendingSnapshot {
    pub source: String,
    pub scope: String,
    pub platform: Option<String>,
    pub region: String,
    pub window: String,
    pub rank: Option<i32>,
    pub media_metadata_id: Option<i64>,
    pub external_ref: Option<Json>,
    pub popularity: Option<f32>,
}

/// Where a resolved title streams (TMDb `/watch/providers`). Only exists for
/// entries that resolved to a `media_metadata` row — the spec's own
/// composite primary key requires a non-null title reference.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct StreamingAvailability {
    pub media_metadata_id: i64,
    pub provider: String,
    pub region: String,
    pub offer_type: String,
    pub link: Option<String>,
    pub seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewStreamingAvailability {
    pub media_metadata_id: i64,
    pub provider: String,
    pub region: String,
    pub offer_type: String,
    pub link: Option<String>,
}

/// Aggregate "mainstream" rollup of the trending set for a window/region.
///
/// Deliberately excludes `mainstream_centroid` (`vector(768)` in the
/// migration): this crate has no `pgvector` Rust dependency, and MUSE-19
/// never reads or writes that column — see
/// `migrations/0043_population_profile.sql` and
/// `trending::compute_population_profile`. MUSE-20 owns extending this
/// struct (or querying the column directly) when it adds the centroid math.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct PopulationProfile {
    pub id: i64,
    pub window: String,
    pub region: String,
    pub genre_distribution: Json,
    pub decade_distribution: Option<Json>,
    pub runtime_distribution: Option<Json>,
    pub sample_size: Option<i32>,
    pub computed_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewPopulationProfile {
    pub window: String,
    pub region: String,
    pub genre_distribution: Json,
    pub decade_distribution: Option<Json>,
    pub runtime_distribution: Option<Json>,
    pub sample_size: Option<i32>,
}
