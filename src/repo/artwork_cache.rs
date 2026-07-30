//! Repo functions for `artwork_cache` (MUSE-27). Runtime sqlx only, per the
//! MUSE-02 build constraint (the crate must build without a live database).

use chrono::{DateTime, Utc};
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

    // MUSE #100: the master upsert and the rendition purge are ONE TRANSACTION.
    // As two separate pool operations there was a committed window in which the
    // new master coexisted with old renditions, and a rendition hit (which does
    // not read the master) would serve one — with a byte-accurate ETag, so the
    // client's 304 was "correct" about the wrong image. Atomicity closes that
    // window; the `master_generation` stamp closes the remaining lost-update race
    // (see `store_rendition`).
    let mut tx = pool.begin().await.map_err(MuseError::Database)?;

    let row = sqlx::query_as::<_, ArtworkCache>(
        r#"
        INSERT INTO artwork_cache (
            entity_kind, entity_id, variant, source_url, content_type, bytes, etag, fetched_at,
            width, format
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0, 'original')
        -- The inference target MUST be the new five-column unique index; a stale
        -- ON CONFLICT list fails at RUNTIME with "no unique or exclusion
        -- constraint matching the ON CONFLICT spec".
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
    .fetch_one(&mut *tx)
    .await
    .map_err(MuseError::Database)?;

    // Every derivative of this master is now stale. In-transaction, so a failure
    // ROLLS BACK the master write rather than being logged and swallowed — the
    // previous version's "log and continue" guaranteed a stale-serving path.
    sqlx::query(
        "DELETE FROM artwork_cache \
         WHERE entity_kind = $1 AND entity_id = $2 AND variant = $3 AND width <> $4",
    )
    .bind(entity_kind)
    .bind(entity_id)
    .bind(variant)
    .bind(ORIGINAL_WIDTH)
    .execute(&mut *tx)
    .await
    .map_err(MuseError::Database)?;

    tx.commit().await.map_err(MuseError::Database)?;
    Ok(row)
}

/// MUSE #100: the master's current generation — its `updated_at`. Selects ONLY
/// that column on purpose: the whole point of the rendition fast path is to avoid
/// moving a ~2 MB `bytea`, so the freshness check must stay a tiny indexed read.
///
/// `None` means there is no master row with bytes, in which case no rendition can
/// be valid.
pub async fn master_generation(
    pool: &PgPool,
    entity_kind: &str,
    entity_id: i64,
    variant: &str,
) -> MuseResult<Option<DateTime<Utc>>> {
    let row: Option<(DateTime<Utc>,)> = sqlx::query_as(
        "SELECT updated_at FROM artwork_cache \
         WHERE entity_kind = $1 AND entity_id = $2 AND variant = $3 \
           AND width = $4 AND format = $5 AND bytes IS NOT NULL",
    )
    .bind(entity_kind)
    .bind(entity_id)
    .bind(variant)
    .bind(ORIGINAL_WIDTH)
    .bind(ORIGINAL_FORMAT)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(row.map(|r| r.0))
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
    master_generation: DateTime<Utc>,
) -> MuseResult<Option<ArtworkCache>> {
    sqlx::query_as::<_, ArtworkCache>(
        // The `master_generation` predicate IS the freshness gate: a rendition
        // derived from a superseded master does not match, so it can never be
        // served — even if a purge was missed or lost a race with a slow encode.
        "SELECT * FROM artwork_cache \
         WHERE entity_kind = $1 AND entity_id = $2 AND variant = $3 \
           AND width = $4 AND format = $5 AND master_generation = $6",
    )
    .bind(entity_kind)
    .bind(entity_id)
    .bind(variant)
    .bind(width)
    .bind(format)
    .bind(master_generation)
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
    master_generation: DateTime<Utc>,
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
            entity_kind, entity_id, variant, width, format, content_type, bytes, etag, fetched_at,
            master_generation
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (entity_kind, entity_id, variant, width, format)
        DO UPDATE SET
            content_type = EXCLUDED.content_type,
            bytes = EXCLUDED.bytes,
            etag = EXCLUDED.etag,
            fetched_at = EXCLUDED.fetched_at,
            master_generation = EXCLUDED.master_generation,
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
    .bind(master_generation)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}
