//! Repo functions for `libraries`.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::library::{Library, NewLibrary};

pub async fn create(pool: &PgPool, new: &NewLibrary) -> MuseResult<Library> {
    sqlx::query_as::<_, Library>(
        r#"
        INSERT INTO libraries (name, kind, root_folder, source_arr_name, source_arr_url)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, name, kind, root_folder, source_arr_name, source_arr_url,
                  enabled, created_at, updated_at
        "#,
    )
    .bind(&new.name)
    .bind(new.kind)
    .bind(&new.root_folder)
    .bind(&new.source_arr_name)
    .bind(&new.source_arr_url)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get(pool: &PgPool, id: i64) -> MuseResult<Library> {
    sqlx::query_as::<_, Library>(
        r#"
        SELECT id, name, kind, root_folder, source_arr_name, source_arr_url,
               enabled, created_at, updated_at
        FROM libraries WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("library {id} not found")))
}

pub async fn get_by_name(pool: &PgPool, name: &str) -> MuseResult<Option<Library>> {
    sqlx::query_as::<_, Library>(
        r#"
        SELECT id, name, kind, root_folder, source_arr_name, source_arr_url,
               enabled, created_at, updated_at
        FROM libraries WHERE name = $1
        "#,
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list(pool: &PgPool) -> MuseResult<Vec<Library>> {
    sqlx::query_as::<_, Library>(
        r#"
        SELECT id, name, kind, root_folder, source_arr_name, source_arr_url,
               enabled, created_at, updated_at
        FROM libraries ORDER BY name
        "#,
    )
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
