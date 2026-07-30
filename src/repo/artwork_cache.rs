//! Repo functions for `artwork_cache` (MUSE-27). Runtime sqlx only, per the
//! MUSE-02 build constraint (the crate must build without a live database).

use chrono::Utc;
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::ArtworkCache;

/// The `width` sentinel for the original master image, and the `format` the
/// original row is always keyed on. See `0109_artwork_renditions.sql`.
pub const ORIGINAL_WIDTH: i32 = 0;
pub const ORIGINAL_FORMAT: &str = "original";

/// Fetch the ORIGINAL cache row for `(entity_kind, entity_id, variant)`, if any.
///
/// MUSE #100: the `width`/`format` predicates are load-bearing, not decoration.
/// Once renditions share this table, an unfiltered `SELECT *` would happily
/// return a 160px derivative as "the artwork" — so every original lookup pins
/// the sentinel.
pub async fn get(
    pool: &PgPool,
    entity_kind: &str,
    entity_id: i64,
    variant: &str,
) -> MuseResult<Option<ArtworkCache>> {
    sqlx::query_as::<_, ArtworkCache>(
        "SELECT * FROM artwork_cache \
         WHERE entity_kind = $1 AND entity_id = $2 AND variant = $3 \
           AND width = $4 AND format = $5",
    )
    .bind(entity_kind)
    .bind(entity_id)
    .bind(variant)
    .bind(ORIGINAL_WIDTH)
    .bind(ORIGINAL_FORMAT)
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
        INSERT INTO artwork_cache (entity_kind, entity_id, variant, source_url, width, format)
        VALUES ($1, $2, $3, $4, 0, 'original')
        -- MUSE #100: same runtime hazard as `store_bytes` — the inference list
        -- must match the new five-column unique index. A source_url belongs to
        -- the ORIGINAL row only; renditions are derived, never fetched.
        ON CONFLICT (entity_kind, entity_id, variant, width, format)
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
            entity_kind, entity_id, variant, source_url, content_type, bytes, etag, fetched_at,
            width, format
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0, 'original')
        -- MUSE #100: the inference target MUST be the new five-column unique
        -- index; the old three-column constraint no longer exists, and a stale
        -- ON CONFLICT list fails at RUNTIME (not compile time) with
        -- "no unique or exclusion constraint matching the ON CONFLICT spec".
        ON CONFLICT (entity_kind, entity_id, variant, width, format)
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

/// MUSE #100: fetch a cached RENDITION (a derived, resized encoding), if one exists.
///
/// Distinct from [`get`], which is pinned to the original master. A rendition is
/// identified by `(entity_kind, entity_id, variant, width, format)` and always has
/// bytes — a rendition row is only ever written after a successful encode, so
/// unlike the original there is no "row exists but bytes are NULL" state to handle.
pub async fn get_rendition(
    pool: &PgPool,
    entity_kind: &str,
    entity_id: i64,
    variant: &str,
    width: i32,
    format: &str,
) -> MuseResult<Option<ArtworkCache>> {
    sqlx::query_as::<_, ArtworkCache>(
        "SELECT * FROM artwork_cache \
         WHERE entity_kind = $1 AND entity_id = $2 AND variant = $3 \
           AND width = $4 AND format = $5",
    )
    .bind(entity_kind)
    .bind(entity_id)
    .bind(variant)
    .bind(width)
    .bind(format)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

/// Store a freshly-encoded rendition. Idempotent: two requests that raced the
/// same encode both succeed and the last write wins, which is safe because the
/// bytes are a deterministic function of (master, width, format).
///
/// `source_url` is deliberately NOT set — a rendition has no upstream; it is
/// derived from the original row. Leaving it NULL keeps "has an upstream to
/// refetch from" meaningful.
#[allow(clippy::too_many_arguments)]
pub async fn store_rendition(
    pool: &PgPool,
    entity_kind: &str,
    entity_id: i64,
    variant: &str,
    width: i32,
    format: &str,
    content_type: &str,
    bytes: &[u8],
    etag: Option<&str>,
) -> MuseResult<ArtworkCache> {
    let fetched_at = Utc::now();
    sqlx::query_as::<_, ArtworkCache>(
        r#"
        INSERT INTO artwork_cache (
            entity_kind, entity_id, variant, width, format, content_type, bytes, etag, fetched_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (entity_kind, entity_id, variant, width, format)
        DO UPDATE SET
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
    .bind(width)
    .bind(format)
    .bind(content_type)
    .bind(bytes)
    .bind(etag)
    .bind(fetched_at)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}
