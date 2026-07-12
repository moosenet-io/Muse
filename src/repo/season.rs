//! Repo functions for `seasons`.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::season::{NewSeason, Season};

pub async fn upsert(pool: &PgPool, new: &NewSeason) -> MuseResult<Season> {
    sqlx::query_as::<_, Season>(
        r#"
        INSERT INTO seasons (media_item_id, season_number, title, overview, monitored, air_date)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (media_item_id, season_number) DO UPDATE SET
            title = EXCLUDED.title,
            overview = EXCLUDED.overview,
            monitored = EXCLUDED.monitored,
            air_date = EXCLUDED.air_date,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(new.media_item_id)
    .bind(new.season_number)
    .bind(&new.title)
    .bind(&new.overview)
    .bind(new.monitored)
    .bind(new.air_date)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<Season> {
    sqlx::query_as::<_, Season>("SELECT * FROM seasons WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("season {id} not found")))
}

pub async fn list_by_media_item(pool: &PgPool, media_item_id: i64) -> MuseResult<Vec<Season>> {
    sqlx::query_as::<_, Season>(
        "SELECT * FROM seasons WHERE media_item_id = $1 ORDER BY season_number",
    )
    .bind(media_item_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
