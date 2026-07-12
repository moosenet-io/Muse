//! Repo functions for `media_files` + the `episode_files` many-to-many join
//! (blueprint §3/§7.3: 1:1 for movies via `media_item_id`, many-to-many for
//! TV season-pack files via `attach_to_episode`).

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::media_file::{MediaFile, NewMediaFile};

pub async fn create(pool: &PgPool, new: &NewMediaFile) -> MuseResult<MediaFile> {
    sqlx::query_as::<_, MediaFile>(
        r#"
        INSERT INTO media_files (
            media_item_id, relative_path, size_bytes, release_group, languages,
            release_type, quality_tier_id, revision_version, revision_real, revision_is_repack
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        RETURNING *
        "#,
    )
    .bind(new.media_item_id)
    .bind(&new.relative_path)
    .bind(new.size_bytes)
    .bind(&new.release_group)
    .bind(&new.languages)
    .bind(new.release_type)
    .bind(new.quality_tier_id)
    .bind(new.revision.version)
    .bind(new.revision.real)
    .bind(new.revision.is_repack)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<MediaFile> {
    sqlx::query_as::<_, MediaFile>("SELECT * FROM media_files WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("media_file {id} not found")))
}

pub async fn list_by_media_item(pool: &PgPool, media_item_id: i64) -> MuseResult<Vec<MediaFile>> {
    sqlx::query_as::<_, MediaFile>(
        "SELECT * FROM media_files WHERE media_item_id = $1 ORDER BY date_added DESC NULLS LAST",
    )
    .bind(media_item_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Link a (possibly season-pack) file to an episode it satisfies. Idempotent.
///
/// The join's `media_item_id` is derived from the episode, so the composite FK
/// to `media_files (id, media_item_id)` rejects any attempt to attach a file
/// from a different show (surfaces as `MuseError::Database`). Attaching to a
/// non-existent episode inserts nothing (the SELECT yields no row).
pub async fn attach_to_episode(pool: &PgPool, episode_id: i64, media_file_id: i64) -> MuseResult<()> {
    sqlx::query(
        r#"
        INSERT INTO episode_files (episode_id, media_file_id, media_item_id)
        SELECT e.id, $2, e.media_item_id
        FROM episodes e
        WHERE e.id = $1
        ON CONFLICT (episode_id, media_file_id) DO NOTHING
        "#,
    )
    .bind(episode_id)
    .bind(media_file_id)
    .execute(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(())
}

/// All files that (fully or partially, via a season pack) satisfy an episode.
pub async fn list_for_episode(pool: &PgPool, episode_id: i64) -> MuseResult<Vec<MediaFile>> {
    sqlx::query_as::<_, MediaFile>(
        r#"
        SELECT mf.* FROM media_files mf
        JOIN episode_files ef ON ef.media_file_id = mf.id
        WHERE ef.episode_id = $1
        ORDER BY mf.date_added DESC NULLS LAST
        "#,
    )
    .bind(episode_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// All episodes a given (possibly season-pack) file satisfies.
pub async fn list_episode_ids_for_file(pool: &PgPool, media_file_id: i64) -> MuseResult<Vec<i64>> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT episode_id FROM episode_files WHERE media_file_id = $1 ORDER BY episode_id",
    )
    .bind(media_file_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}
