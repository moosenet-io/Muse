//! Repo functions for `media_items` — per-library instance state.

use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};
use crate::models::media_item::{MediaItem, NewMediaItem};

pub async fn upsert(pool: &PgPool, new: &NewMediaItem) -> MuseResult<MediaItem> {
    sqlx::query_as::<_, MediaItem>(
        r#"
        INSERT INTO media_items (
            library_id, media_metadata_id, path, monitored, quality_profile_id,
            minimum_availability, plex_rating_key, added_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (library_id, media_metadata_id) DO UPDATE SET
            path = EXCLUDED.path,
            monitored = EXCLUDED.monitored,
            quality_profile_id = EXCLUDED.quality_profile_id,
            minimum_availability = EXCLUDED.minimum_availability,
            plex_rating_key = EXCLUDED.plex_rating_key,
            added_at = COALESCE(media_items.added_at, EXCLUDED.added_at),
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(new.library_id)
    .bind(new.media_metadata_id)
    .bind(&new.path)
    .bind(new.monitored)
    .bind(new.quality_profile_id)
    .bind(&new.minimum_availability)
    .bind(&new.plex_rating_key)
    .bind(new.added_at)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<MediaItem> {
    sqlx::query_as::<_, MediaItem>("SELECT * FROM media_items WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("media_item {id} not found")))
}

pub async fn list_by_library(pool: &PgPool, library_id: i64) -> MuseResult<Vec<MediaItem>> {
    sqlx::query_as::<_, MediaItem>(
        "SELECT * FROM media_items WHERE library_id = $1 ORDER BY added_at DESC NULLS LAST",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Look up a media_item by its Plex `ratingKey` — MUSE-07's session
/// reconstruction resolves a raw `play_events.rating_key` to a local
/// `media_item_id` this way (movies are 1:1; TV rating keys resolve via
/// `repo::episode::get_by_plex_rating_key` instead). Returns `Ok(None)`
/// rather than erroring when unresolved — a session for an item not yet
/// seen by *arr ingest is a normal race, not a failure.
pub async fn get_by_plex_rating_key(pool: &PgPool, plex_rating_key: &str) -> MuseResult<Option<MediaItem>> {
    sqlx::query_as::<_, MediaItem>("SELECT * FROM media_items WHERE plex_rating_key = $1")
        .bind(plex_rating_key)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

pub async fn list_by_metadata(pool: &PgPool, media_metadata_id: i64) -> MuseResult<Vec<MediaItem>> {
    sqlx::query_as::<_, MediaItem>(
        "SELECT * FROM media_items WHERE media_metadata_id = $1 ORDER BY id",
    )
    .bind(media_metadata_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Resolve a Plex `ratingKey` to its `media_items` row (the table's
/// `UNIQUE (plex_rating_key)` — see `migrations/0006_media_items.sql`).
/// Used by MUSE-06's Tautulli backfill importer to resolve a history row's
/// movie/show `rating_key` onto a library item; returns `Ok(None)` (not an
/// error) when the item isn't in the library yet — the caller leaves the
/// media reference NULL rather than failing the whole import.
pub async fn find_by_plex_rating_key(
    pool: &PgPool,
    plex_rating_key: &str,
) -> MuseResult<Option<MediaItem>> {
    sqlx::query_as::<_, MediaItem>("SELECT * FROM media_items WHERE plex_rating_key = $1")
        .bind(plex_rating_key)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

/// `(media_item_id, runtime_minutes)` for a set of media_items whose shared
/// `media_metadata.runtime_minutes` is set — MUSE-10's `runtime_pref`
/// bucketing lookup. An item with no runtime on record simply doesn't
/// appear in the result (the `runtime_minutes IS NOT NULL` filter), so
/// callers should treat an absent id as "unknown runtime", not zero.
#[derive(Debug, Clone, FromRow)]
struct MediaItemRuntimeRow {
    media_item_id: i64,
    runtime_minutes: i32,
}

pub async fn runtimes_for_media_items(pool: &PgPool, media_item_ids: &[i64]) -> MuseResult<Vec<(i64, i32)>> {
    let rows = sqlx::query_as::<_, MediaItemRuntimeRow>(
        r#"
        SELECT mi.id AS media_item_id, mm.runtime_minutes AS runtime_minutes
        FROM media_items mi
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        WHERE mi.id = ANY($1) AND mm.runtime_minutes IS NOT NULL
        "#,
    )
    .bind(media_item_ids)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)?;

    Ok(rows.into_iter().map(|r| (r.media_item_id, r.runtime_minutes)).collect())
}
