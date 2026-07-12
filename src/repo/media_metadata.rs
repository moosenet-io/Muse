//! Repo functions for `media_metadata` — shared provider-keyed metadata.
//!
//! Two upsert entry points reflect the blueprint's provider-precedence
//! finding (§7.7): movies key primarily on `(kind, tmdb_id)` (Radarr), shows
//! key primarily on `(kind, tvdb_id)` (Sonarr).

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::media_metadata::{MediaKind, MediaMetadata, NewMediaMetadata};

pub async fn upsert_by_tmdb(pool: &PgPool, new: &NewMediaMetadata) -> MuseResult<MediaMetadata> {
    let tmdb_id = new
        .tmdb_id
        .as_deref()
        .ok_or_else(|| MuseError::Conflict("upsert_by_tmdb requires tmdb_id".to_string()))?;

    sqlx::query_as::<_, MediaMetadata>(
        r#"
        INSERT INTO media_metadata (
            kind, tmdb_id, tvdb_id, imdb_id, provider_ids, title, sort_title,
            original_title, original_language, status, overview, studio,
            network, runtime_minutes, year, images
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
        ON CONFLICT (kind, tmdb_id) DO UPDATE SET
            tvdb_id = EXCLUDED.tvdb_id,
            imdb_id = EXCLUDED.imdb_id,
            provider_ids = EXCLUDED.provider_ids,
            title = EXCLUDED.title,
            sort_title = EXCLUDED.sort_title,
            original_title = EXCLUDED.original_title,
            original_language = EXCLUDED.original_language,
            status = EXCLUDED.status,
            overview = EXCLUDED.overview,
            studio = EXCLUDED.studio,
            network = EXCLUDED.network,
            runtime_minutes = EXCLUDED.runtime_minutes,
            year = EXCLUDED.year,
            images = EXCLUDED.images,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(new.kind)
    .bind(tmdb_id)
    .bind(&new.tvdb_id)
    .bind(&new.imdb_id)
    .bind(&new.provider_ids)
    .bind(&new.title)
    .bind(&new.sort_title)
    .bind(&new.original_title)
    .bind(&new.original_language)
    .bind(&new.status)
    .bind(&new.overview)
    .bind(&new.studio)
    .bind(&new.network)
    .bind(new.runtime_minutes)
    .bind(new.year)
    .bind(&new.images)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn upsert_by_tvdb(pool: &PgPool, new: &NewMediaMetadata) -> MuseResult<MediaMetadata> {
    let tvdb_id = new
        .tvdb_id
        .as_deref()
        .ok_or_else(|| MuseError::Conflict("upsert_by_tvdb requires tvdb_id".to_string()))?;

    sqlx::query_as::<_, MediaMetadata>(
        r#"
        INSERT INTO media_metadata (
            kind, tmdb_id, tvdb_id, imdb_id, provider_ids, title, sort_title,
            original_title, original_language, status, overview, studio,
            network, runtime_minutes, year, images
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
        ON CONFLICT (kind, tvdb_id) DO UPDATE SET
            tmdb_id = EXCLUDED.tmdb_id,
            imdb_id = EXCLUDED.imdb_id,
            provider_ids = EXCLUDED.provider_ids,
            title = EXCLUDED.title,
            sort_title = EXCLUDED.sort_title,
            original_title = EXCLUDED.original_title,
            original_language = EXCLUDED.original_language,
            status = EXCLUDED.status,
            overview = EXCLUDED.overview,
            studio = EXCLUDED.studio,
            network = EXCLUDED.network,
            runtime_minutes = EXCLUDED.runtime_minutes,
            year = EXCLUDED.year,
            images = EXCLUDED.images,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(new.kind)
    .bind(&new.tmdb_id)
    .bind(tvdb_id)
    .bind(&new.imdb_id)
    .bind(&new.provider_ids)
    .bind(&new.title)
    .bind(&new.sort_title)
    .bind(&new.original_title)
    .bind(&new.original_language)
    .bind(&new.status)
    .bind(&new.overview)
    .bind(&new.studio)
    .bind(&new.network)
    .bind(new.runtime_minutes)
    .bind(new.year)
    .bind(&new.images)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<MediaMetadata> {
    sqlx::query_as::<_, MediaMetadata>("SELECT * FROM media_metadata WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("media_metadata {id} not found")))
}

/// Resolve a provider `tmdb_id` (+ kind) to an existing `media_metadata.id`,
/// if the title is already known to the catalog. Used by the trending
/// ingest (MUSE-19) to link a trending entry to a library title — most
/// trending entries won't resolve and stay `None` (the caller falls back to
/// `external_ref`).
pub async fn find_by_tmdb_id(
    pool: &PgPool,
    kind: MediaKind,
    tmdb_id: &str,
) -> MuseResult<Option<i64>> {
    sqlx::query_scalar::<_, i64>("SELECT id FROM media_metadata WHERE kind = $1 AND tmdb_id = $2")
        .bind(kind)
        .bind(tmdb_id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

/// Best-effort resolve a parsed release (title + optional year) to an
/// existing `media_metadata` row via an exact, case-insensitive title match
/// (+ year equality when a year was parsed). Used by the Prowlarr
/// report-pull worker (MUSE-17) to link a release to a title without ever
/// guessing at a fuzzy match -- a release that doesn't resolve stays
/// unmatched (`media_metadata_id = NULL`), which is preserved on purpose
/// (negative-space discovery, spec S4b-B) rather than silently dropped.
///
/// Deliberately NOT a fuzzy/similarity match (unlike `search_by_title`
/// above): a curation/availability signal is only as trustworthy as its
/// resolution, and a wrong match silently feeding curation is worse than a
/// visibly-unresolved release.
pub async fn find_by_title_year(
    pool: &PgPool,
    kind: MediaKind,
    title: &str,
    year: Option<i32>,
) -> MuseResult<Option<i64>> {
    match year {
        Some(y) => sqlx::query_scalar::<_, i64>(
            "SELECT id FROM media_metadata WHERE kind = $1 AND lower(title) = lower($2) AND year = $3 LIMIT 1",
        )
        .bind(kind)
        .bind(title)
        .bind(y)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database),
        None => sqlx::query_scalar::<_, i64>(
            "SELECT id FROM media_metadata WHERE kind = $1 AND lower(title) = lower($2) LIMIT 1",
        )
        .bind(kind)
        .bind(title)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database),
    }
}

pub async fn search_by_title(pool: &PgPool, query: &str, limit: i64) -> MuseResult<Vec<MediaMetadata>> {
    sqlx::query_as::<_, MediaMetadata>(
        r#"
        SELECT * FROM media_metadata
        WHERE title ILIKE '%' || $1 || '%'
        ORDER BY similarity(title, $1) DESC
        LIMIT $2
        "#,
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
