//! Repo functions for `media_files` + the `episode_files` many-to-many join
//! (blueprint §3/§7.3: 1:1 for movies via `media_item_id`, many-to-many for
//! TV season-pack files via `attach_to_episode`).

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::media_file::{MediaFile, NewMediaFile, ReleaseTypeKind};

/// MUSEL-B1: idempotent upsert for one file the library scanner found —
/// `(media_item_id, relative_path)` is the scanner's own de-dup key (there
/// is no DB-level `UNIQUE` on that pair; `media_files` predates the scanner
/// and its real uniqueness key is the `(id, media_item_id)` superkey used by
/// `episode_files`' composite FK — see `migrations/0009_media_files.sql`).
/// Looks the row up first, then either leaves it untouched (unchanged size
/// — the mtime/size guard the spec asks for; this schema has no
/// `media_files` mtime column, so size is the change signal actually
/// available), updates `size_bytes`/`media_info` when the file changed size
/// since the last scan, or inserts a fresh row. Returns `(row, changed)`
/// so a caller can distinguish "already up to date" from "recorded/updated
/// this pass" for scan-report counters.
pub async fn upsert_scanned(
    pool: &PgPool,
    media_item_id: i64,
    relative_path: &str,
    size_bytes: Option<i64>,
    media_info: Option<serde_json::Value>,
) -> MuseResult<(MediaFile, bool)> {
    let existing = sqlx::query_as::<_, MediaFile>(
        "SELECT * FROM media_files WHERE media_item_id = $1 AND relative_path = $2",
    )
    .bind(media_item_id)
    .bind(relative_path)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?;

    if let Some(row) = existing {
        if row.size_bytes == size_bytes {
            // Unchanged since the last scan -- clean no-op, no write.
            return Ok((row, false));
        }

        let updated = sqlx::query_as::<_, MediaFile>(
            r#"
            UPDATE media_files SET
                size_bytes = $2,
                media_info = COALESCE($3, media_info),
                date_added = COALESCE(date_added, now())
            WHERE id = $1
            RETURNING *
            "#,
        )
        .bind(row.id)
        .bind(size_bytes)
        .bind(&media_info)
        .fetch_one(pool)
        .await
        .map_err(MuseError::Database)?;
        return Ok((updated, true));
    }

    let created = sqlx::query_as::<_, MediaFile>(
        r#"
        INSERT INTO media_files (
            media_item_id, relative_path, size_bytes, media_info, date_added,
            languages, release_type
        )
        VALUES ($1, $2, $3, $4, now(), '{}', $5)
        RETURNING *
        "#,
    )
    .bind(media_item_id)
    .bind(relative_path)
    .bind(size_bytes)
    .bind(&media_info)
    .bind(ReleaseTypeKind::Single)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)?;

    Ok((created, true))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// MUSEL-B1: `upsert_scanned`'s idempotency/size-guard behavior,
    /// isolated from the full scanner. Gated on `MUSE_TEST_DATABASE_URL`,
    /// same skip-cleanly-when-unset posture as every other live-DB test in
    /// this crate.
    #[tokio::test]
    async fn upsert_scanned_is_idempotent_and_updates_only_on_a_size_change() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set -- skipping \
                 upsert_scanned_is_idempotent_and_updates_only_on_a_size_change \
                 (expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use uuid::Uuid;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();

        let library = crate::repo::library::create(
            &pool,
            &crate::models::library::NewLibrary {
                name: format!("media-file-upsert-test-{suffix}"),
                kind: crate::models::library::LibraryKind::Movie,
                root_folder: format!("/tmp/muse-media-file-upsert-test-{suffix}"),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("seed library");

        let metadata = crate::repo::media_metadata::upsert_by_tmdb(
            &pool,
            &crate::models::media_metadata::NewMediaMetadata {
                kind: crate::models::media_metadata::MediaKind::Movie,
                tmdb_id: Some(format!("mediafile-upsert-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Media File Upsert Test {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: None,
                year: None,
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("seed media_metadata");

        let media_item = crate::repo::media_item::upsert(
            &pool,
            &crate::models::media_item::NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: "Movie.mkv".to_string(),
                monitored: false,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("seed media_item");

        let (first, first_changed) = upsert_scanned(&pool, media_item.id, "Movie.mkv", Some(1000), None)
            .await
            .expect("first upsert_scanned");
        assert!(first_changed, "the first scan of a file must always be recorded as a change");
        assert_eq!(first.size_bytes, Some(1000));

        let (second, second_changed) = upsert_scanned(&pool, media_item.id, "Movie.mkv", Some(1000), None)
            .await
            .expect("second upsert_scanned with an unchanged size");
        assert!(!second_changed, "an unchanged size must be a clean no-op, not a write");
        assert_eq!(second.id, first.id);

        let rows = list_by_media_item(&pool, media_item.id).await.expect("list_by_media_item");
        assert_eq!(rows.len(), 1, "no duplicate row from the two upserts above");

        let (third, third_changed) = upsert_scanned(
            &pool,
            media_item.id,
            "Movie.mkv",
            Some(2000),
            Some(serde_json::json!({"container": "mkv"})),
        )
        .await
        .expect("third upsert_scanned with a changed size");
        assert!(third_changed, "a changed size must be recorded as an update");
        assert_eq!(third.id, first.id, "a size change updates the existing row, never inserts a duplicate");
        assert_eq!(third.size_bytes, Some(2000));

        let rows_again = list_by_media_item(&pool, media_item.id).await.expect("list_by_media_item again");
        assert_eq!(rows_again.len(), 1, "a size-changed update must still be exactly one row");
    }
}
