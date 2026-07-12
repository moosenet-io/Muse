//! Repo functions for `episodes`.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::episode::{Episode, NewEpisode};

pub async fn upsert(pool: &PgPool, new: &NewEpisode) -> MuseResult<Episode> {
    sqlx::query_as::<_, Episode>(
        r#"
        INSERT INTO episodes (
            season_id, media_item_id, episode_number, absolute_episode_number,
            title, overview, air_date, air_date_utc, runtime_minutes, monitored, tvdb_id
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (season_id, episode_number) DO UPDATE SET
            absolute_episode_number = EXCLUDED.absolute_episode_number,
            title = EXCLUDED.title,
            overview = EXCLUDED.overview,
            air_date = EXCLUDED.air_date,
            air_date_utc = EXCLUDED.air_date_utc,
            runtime_minutes = EXCLUDED.runtime_minutes,
            monitored = EXCLUDED.monitored,
            tvdb_id = EXCLUDED.tvdb_id,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(new.season_id)
    .bind(new.media_item_id)
    .bind(new.episode_number)
    .bind(new.absolute_episode_number)
    .bind(&new.title)
    .bind(&new.overview)
    .bind(new.air_date)
    .bind(new.air_date_utc)
    .bind(new.runtime_minutes)
    .bind(new.monitored)
    .bind(&new.tvdb_id)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<Episode> {
    sqlx::query_as::<_, Episode>("SELECT * FROM episodes WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("episode {id} not found")))
}

pub async fn list_by_season(pool: &PgPool, season_id: i64) -> MuseResult<Vec<Episode>> {
    sqlx::query_as::<_, Episode>(
        "SELECT * FROM episodes WHERE season_id = $1 ORDER BY episode_number",
    )
    .bind(season_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Mark `has_file` for an episode — flipped by `repo::media_file::attach_to_episode`
/// once a file join row exists, kept as a separate explicit call rather than
/// a trigger so the write path stays visible in application code.
pub async fn set_has_file(pool: &PgPool, episode_id: i64, has_file: bool) -> MuseResult<()> {
    sqlx::query("UPDATE episodes SET has_file = $2, updated_at = now() WHERE id = $1")
        .bind(episode_id)
        .bind(has_file)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    Ok(())
}
