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

/// Where a `media_item`'s files live: its library root, and the item's own
/// folder as the source that created it recorded it.
///
/// **Both are needed, and MPRB-10 is why.** MPRB-07 looked up `root_folder`
/// alone, on the stated belief that `media_files.relative_path` is always
/// relative to it. Run against the live database that belief is false for 90%
/// of the table — see [`crate::media::backfill::candidate_paths`] for the
/// measurement and the two conventions that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaItemLocation {
    /// `libraries.root_folder` — an absolute path in **Muse's** namespace, i.e.
    /// underneath `MUSE_LIBRARY_ROOT`.
    pub root_folder: String,
    /// `media_items.path` — the item's own folder. Absolute, but recorded by
    /// whichever source created the item, so its prefix is that source's
    /// namespace (Radarr/Sonarr report `/media/…` where Muse mounts
    /// `/srv/media/…`) and it may be `NULL`.
    pub item_path: Option<String>,
}

/// MPRB-07 (corrected by MPRB-10): where a set of `media_items`' files live.
///
/// The backfill holds `media_files` rows, which carry `media_item_id` and
/// nothing about libraries or item folders, hence this lookup.
///
/// **A lookup, not logic.** One `IN` query, no aggregation, no filtering, no
/// decision — everything the backfill decides is decided above the pool, where
/// it can execute without `MUSE_TEST_DATABASE_URL` (MUSE #130). It returns the
/// pairs it found; a `media_item_id` with no row is simply absent from the map,
/// and the caller decides what that means.
pub async fn locations_for_media_items(
    pool: &PgPool,
    media_item_ids: &[i64],
) -> MuseResult<std::collections::HashMap<i64, MediaItemLocation>> {
    if media_item_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let rows: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT mi.id, l.root_folder, mi.path
        FROM media_items mi
        JOIN libraries l ON l.id = mi.library_id
        WHERE mi.id = ANY($1)
        "#,
    )
    .bind(media_item_ids)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)?;

    Ok(rows
        .into_iter()
        .map(|(id, root_folder, item_path)| {
            (
                id,
                MediaItemLocation {
                    root_folder,
                    item_path,
                },
            )
        })
        .collect())
}
