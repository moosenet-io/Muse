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

/// MPRB-07: the library `root_folder` that a set of `media_items` belong to.
///
/// `media_files.relative_path` is relative to its library's `root_folder` — that
/// is how `library::scan::walk_media_files` formed it (`strip_prefix(root)`), so
/// it is how the backfill has to rebuild an absolute path. The backfill holds
/// `media_files` rows, which carry `media_item_id` and nothing about libraries,
/// hence this lookup.
///
/// **A lookup, not logic.** One `IN` query, no aggregation, no filtering, no
/// decision — everything the backfill decides is decided above the pool, where
/// it can execute without `MUSE_TEST_DATABASE_URL` (MUSE #130). It returns the
/// pairs it found; a `media_item_id` with no row is simply absent from the map,
/// and the caller decides what that means.
pub async fn root_folders_for_media_items(
    pool: &PgPool,
    media_item_ids: &[i64],
) -> MuseResult<std::collections::HashMap<i64, String>> {
    if media_item_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let rows: Vec<(i64, String)> = sqlx::query_as(
        r#"
        SELECT mi.id, l.root_folder
        FROM media_items mi
        JOIN libraries l ON l.id = mi.library_id
        WHERE mi.id = ANY($1)
        "#,
    )
    .bind(media_item_ids)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)?;

    Ok(rows.into_iter().collect())
}
