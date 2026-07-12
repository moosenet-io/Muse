//! Repo functions for `artwork_cache` (MUSE-27). Runtime sqlx only, per the
//! MUSE-02 build constraint (the crate must build without a live database).

use chrono::Utc;
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::ArtworkCache;

/// Fetch the cache row for `(entity_kind, entity_id, variant)`, if any.
pub async fn get(
    pool: &PgPool,
    entity_kind: &str,
    entity_id: i64,
    variant: &str,
) -> MuseResult<Option<ArtworkCache>> {
    sqlx::query_as::<_, ArtworkCache>(
        "SELECT * FROM artwork_cache WHERE entity_kind = $1 AND entity_id = $2 AND variant = $3",
    )
    .bind(entity_kind)
    .bind(entity_id)
    .bind(variant)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

/// Record (or refresh) the upstream `source_url` for an entity/variant,
/// without touching any already-cached bytes. Called by the guide handler
/// as it renders a lineup, so a later `/art/{kind}/{id}` request always has
/// something to fetch from on a cache miss.
pub async fn upsert_source(
    pool: &PgPool,
    entity_kind: &str,
    entity_id: i64,
    variant: &str,
    source_url: &str,
) -> MuseResult<ArtworkCache> {
    sqlx::query_as::<_, ArtworkCache>(
        r#"
        INSERT INTO artwork_cache (entity_kind, entity_id, variant, source_url)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (entity_kind, entity_id, variant)
        DO UPDATE SET source_url = EXCLUDED.source_url, updated_at = now()
        RETURNING *
        "#,
    )
    .bind(entity_kind)
    .bind(entity_id)
    .bind(variant)
    .bind(source_url)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// Store freshly-fetched bytes for `(entity_kind, entity_id, variant)`,
/// creating the row if it doesn't already exist (a direct `/art/...` request
/// can be the first thing to ever touch this entity/variant).
pub async fn store_bytes(
    pool: &PgPool,
    entity_kind: &str,
    entity_id: i64,
    variant: &str,
    source_url: Option<&str>,
    content_type: &str,
    bytes: &[u8],
    etag: Option<&str>,
) -> MuseResult<ArtworkCache> {
    let fetched_at = Utc::now();
    sqlx::query_as::<_, ArtworkCache>(
        r#"
        INSERT INTO artwork_cache (
            entity_kind, entity_id, variant, source_url, content_type, bytes, etag, fetched_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (entity_kind, entity_id, variant)
        DO UPDATE SET
            source_url = COALESCE(EXCLUDED.source_url, artwork_cache.source_url),
            content_type = EXCLUDED.content_type,
            bytes = EXCLUDED.bytes,
            etag = EXCLUDED.etag,
            fetched_at = EXCLUDED.fetched_at,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(entity_kind)
    .bind(entity_id)
    .bind(variant)
    .bind(source_url)
    .bind(content_type)
    .bind(bytes)
    .bind(etag)
    .bind(fetched_at)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}
