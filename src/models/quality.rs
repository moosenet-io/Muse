//! Quality tiers, profiles, and the custom-format scorer seam
//! (blueprint §2/§6/§7.4/§7.5).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct QualityDefinition {
    pub id: i64,
    pub quality_key: String,
    pub title: String,
    pub source: String,
    pub resolution: Option<String>,
    pub modifier: String,
    pub min_size_mb_per_min: Option<f32>,
    pub max_size_mb_per_min: Option<f32>,
    pub preferred_size_mb_per_min: Option<f32>,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewQualityDefinition {
    pub quality_key: String,
    pub title: String,
    pub source: String,
    pub resolution: Option<String>,
    pub modifier: String,
    pub sort_order: i32,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct QualityProfile {
    pub id: i64,
    pub name: String,
    pub cutoff_quality_id: Option<i64>,
    pub items: Json,
    pub language: Option<String>,
    pub upgrade_allowed: bool,
    pub min_format_score: i32,
    pub cutoff_format_score: i32,
    pub min_upgrade_format_score: i32,
    pub natural_language_intent: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewQualityProfile {
    pub name: String,
    pub cutoff_quality_id: Option<i64>,
    pub items: Json,
    pub upgrade_allowed: bool,
    pub natural_language_intent: Option<String>,
}

/// A named, scored matcher rule (blueprint §6/§7.5). `specifications` is a
/// documented seam — MUSE-02 stores and round-trips these, it does not
/// evaluate/score releases against them (no scorer exists yet).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CustomFormat {
    pub id: i64,
    pub name: String,
    pub specifications: Json,
    pub include_when_renaming: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewCustomFormat {
    pub name: String,
    pub specifications: Json,
    pub include_when_renaming: bool,
}

/// One row of a quality profile's `FormatItems` score table
/// (`quality_profile_formats` join).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct QualityProfileFormat {
    pub quality_profile_id: i64,
    pub custom_format_id: i64,
    pub score: i32,
}
