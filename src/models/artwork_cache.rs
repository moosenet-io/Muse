//! `artwork_cache` — the local proxy/cache backing `/art/{kind}/{id}`
//! (MUSE-27, spec §3.8/§4d-F). See `migrations/0095_artwork_cache.sql` for
//! the schema and the rationale for caching bytes in Postgres rather than a
//! disk path. This module owns the storage shape only — the fetch/cache
//! orchestration lives in `crate::web::artwork`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ArtworkCache {
    pub id: i64,
    pub entity_kind: String,
    pub entity_id: i64,
    pub variant: String,
    pub source_url: Option<String>,
    pub content_type: Option<String>,
    /// Cached image bytes. `None` until the first successful upstream fetch
    /// — a row can exist purely to record a `source_url` before any bytes
    /// have been fetched.
    pub bytes: Option<Vec<u8>>,
    pub etag: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
