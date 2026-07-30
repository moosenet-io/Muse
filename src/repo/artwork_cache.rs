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
    let row = sqlx::query_as::<_, ArtworkCache>(
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
    .map_err(MuseError::Database)?;

    // MUSE #100 — THE INVALIDATION. Every derivative of this master is now
    // stale, so it is deleted in the same call that replaced the master. Without
    // this, a replaced poster would keep serving its OLD rendition indefinitely
    // (the rendition lookup deliberately runs BEFORE the master read, and the
    // response carries a long max-age), which is exactly the bug the review
    // caught. Deleting rather than versioning keeps the cache key simple: a
    // rendition either matches the current master or does not exist.
    if let Err(e) = delete_renditions(pool, entity_kind, entity_id, variant).await {
        // A failed purge must not fail the master write — the master is the
        // valuable artifact. Log loudly: the consequence is a stale thumbnail,
        // not a lost image.
        tracing::warn!(
            error = %e, entity_kind, entity_id, variant,
            "failed to purge renditions after a master update; a stale rendition may be served"
        );
    }

    Ok(row)
}

/// Delete every derived rendition of `(entity_kind, entity_id, variant)`,
/// leaving the original untouched. Called on any master write.
pub async fn delete_renditions(
    pool: &PgPool,
    entity_kind: &str,
    entity_id: i64,
    variant: &str,
) -> MuseResult<u64> {
    let res = sqlx::query(
        "DELETE FROM artwork_cache \
         WHERE entity_kind = $1 AND entity_id = $2 AND variant = $3 AND width <> $4",
    )
    .bind(entity_kind)
    .bind(entity_id)
    .bind(variant)
    .bind(ORIGINAL_WIDTH)
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(res.rows_affected())
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
    // Mirror the DB CHECK in code so a bad call fails with a clear message rather
    // than a constraint violation from three layers down.
    if width <= ORIGINAL_WIDTH || format == ORIGINAL_FORMAT {
        return Err(MuseError::Internal(anyhow::anyhow!(
            "store_rendition refuses the original sentinel (width={width}, format={format}); \
             use store_bytes for the master"
        )));
    }
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
