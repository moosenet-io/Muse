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
    /// Rendition width in px. `0` is THE ORIGINAL master image (MUSE #100).
    pub width: i32,
    /// `"original"` for the master; the encoded container (`"jpeg"`, …) for a
    /// derived rendition. See the column comment in `0109_artwork_renditions`.
    pub format: String,
    /// Master rows: SHA-256 of this row's own bytes. `None` on a rendition and on
    /// a master row that has no bytes yet.
    pub content_hash: Option<String>,
    /// Rendition rows: the `content_hash` of the master this was derived FROM —
    /// its provenance. `None` on the master row. A rendition whose provenance does
    /// not equal the current master's `content_hash` is stale and is never served.
    pub master_content_hash: Option<String>,
    pub etag: Option<String>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
