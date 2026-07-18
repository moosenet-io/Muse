//! Repo functions for the acquisition domain (MUSEM-01, Plane MUSE S119
//! Sprint 1): `monitored_items`, `media_requests`, `download_queue`,
//! `history_events`, `blocklist`. Runtime sqlx only (`query`/`query_as`,
//! never `query!`/`query_as!`), per `repo::mod`'s crate-wide rule.
//! Parameterized queries only — no string-interpolated SQL anywhere below.
//!
//! See `migrations/0104_acquisition_domain.sql` for why the pre-existing
//! quality tables (`quality_definitions`/`quality_profiles`/
//! `custom_formats`/`quality_profile_formats`, `src/repo/quality.rs`) are
//! reused here by FK/join rather than redefined.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::acquisition::{
    BlocklistEntry, DownloadQueueEntry, DownloadSource, HistoryEvent, MediaRequest,
    MonitoredItem, NewBlocklistEntry, NewDownloadQueueEntry, NewHistoryEvent, NewMediaRequest,
    NewMonitoredItem, WantedItem,
};

// ---------------------------------------------------------------------
// monitored_items
// ---------------------------------------------------------------------

pub async fn create_monitored_item(
    pool: &PgPool,
    new: &NewMonitoredItem,
) -> MuseResult<MonitoredItem> {
    sqlx::query_as::<_, MonitoredItem>(
        r#"
        INSERT INTO monitored_items (
            media_metadata_id, media_item_id, library_id, monitored,
            quality_profile_id, min_availability
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (media_metadata_id, library_id) DO UPDATE SET
            media_item_id = EXCLUDED.media_item_id,
            monitored = EXCLUDED.monitored,
            quality_profile_id = EXCLUDED.quality_profile_id,
            min_availability = EXCLUDED.min_availability,
            updated_at = now()
        RETURNING *
        "#,
    )
    .bind(new.media_metadata_id)
    .bind(new.media_item_id)
    .bind(new.library_id)
    .bind(new.monitored)
    .bind(new.quality_profile_id)
    .bind(&new.min_availability)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_monitored_item(pool: &PgPool, id: i64) -> MuseResult<MonitoredItem> {
    sqlx::query_as::<_, MonitoredItem>("SELECT * FROM monitored_items WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("monitored_item {id} not found")))
}

pub async fn list_monitored_items(
    pool: &PgPool,
    library_id: i64,
) -> MuseResult<Vec<MonitoredItem>> {
    sqlx::query_as::<_, MonitoredItem>(
        "SELECT * FROM monitored_items WHERE library_id = $1 ORDER BY id",
    )
    .bind(library_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn set_monitored(pool: &PgPool, id: i64, monitored: bool) -> MuseResult<MonitoredItem> {
    sqlx::query_as::<_, MonitoredItem>(
        "UPDATE monitored_items SET monitored = $2, updated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(monitored)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("monitored_item {id} not found")))
}

pub async fn touch_last_search(pool: &PgPool, id: i64) -> MuseResult<MonitoredItem> {
    sqlx::query_as::<_, MonitoredItem>(
        "UPDATE monitored_items SET last_search_at = now(), updated_at = now() \
         WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("monitored_item {id} not found")))
}

/// The hot "wanted" scan: everything monitored in `library_id` that either
/// has no file yet, or whose best on-disk file quality is strictly below
/// its quality profile's cutoff (blueprint: never assume quality-id
/// ordering — this compares `quality_definitions.sort_order`, the explicit
/// ordering column MUSE-02 added, never raw ids). A monitored row whose
/// profile has no cutoff configured is treated as always-wanted (no
/// upgrade stopping point), matching *arr's own "no cutoff = keep
/// upgrading" posture.
pub async fn list_wanted(pool: &PgPool, library_id: i64) -> MuseResult<Vec<WantedItem>> {
    sqlx::query_as::<_, WantedItem>(
        r#"
        SELECT
            mi.id AS monitored_item_id,
            mi.media_metadata_id,
            mi.library_id,
            mm.title,
            mi.quality_profile_id,
            (file_agg.file_count > 0) AS has_file,
            file_agg.best_sort_order AS best_quality_sort_order,
            cutoff_qd.sort_order AS cutoff_sort_order
        FROM monitored_items mi
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        LEFT JOIN LATERAL (
            SELECT
                count(*) AS file_count,
                max(qd.sort_order) AS best_sort_order
            FROM media_files mf
            LEFT JOIN quality_definitions qd ON qd.id = mf.quality_tier_id
            WHERE mf.media_item_id = mi.media_item_id
        ) file_agg ON true
        LEFT JOIN quality_profiles qp ON qp.id = mi.quality_profile_id
        LEFT JOIN quality_definitions cutoff_qd ON cutoff_qd.id = qp.cutoff_quality_id
        WHERE mi.library_id = $1
          AND mi.monitored = true
          AND (
            file_agg.file_count = 0
            OR file_agg.best_sort_order IS NULL
            OR cutoff_qd.sort_order IS NULL
            OR file_agg.best_sort_order < cutoff_qd.sort_order
          )
        ORDER BY mi.id
        "#,
    )
    .bind(library_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

// ---------------------------------------------------------------------
// media_requests
// ---------------------------------------------------------------------

pub async fn create_request(pool: &PgPool, new: &NewMediaRequest) -> MuseResult<MediaRequest> {
    sqlx::query_as::<_, MediaRequest>(
        r#"
        INSERT INTO media_requests (
            provider_ids, media_kind, title, requested_by, tier, quality_profile_id, note,
            monitored_item_id
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING *
        "#,
    )
    .bind(&new.provider_ids)
    .bind(&new.media_kind)
    .bind(&new.title)
    .bind(&new.requested_by)
    .bind(&new.tier)
    .bind(new.quality_profile_id)
    .bind(&new.note)
    .bind(new.monitored_item_id)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_request(pool: &PgPool, id: i64) -> MuseResult<MediaRequest> {
    sqlx::query_as::<_, MediaRequest>("SELECT * FROM media_requests WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("media_request {id} not found")))
}

pub async fn list_requests_by_status(
    pool: &PgPool,
    status: &str,
) -> MuseResult<Vec<MediaRequest>> {
    sqlx::query_as::<_, MediaRequest>(
        "SELECT * FROM media_requests WHERE status = $1 ORDER BY created_at",
    )
    .bind(status)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn update_request_status(
    pool: &PgPool,
    id: i64,
    status: &str,
) -> MuseResult<MediaRequest> {
    sqlx::query_as::<_, MediaRequest>(
        "UPDATE media_requests SET status = $2, updated_at = now() WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .bind(status)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("media_request {id} not found")))
}

// ---------------------------------------------------------------------
// download_queue
// ---------------------------------------------------------------------

fn download_source_ids(source: &DownloadSource) -> (Option<i64>, Option<i64>) {
    match *source {
        DownloadSource::Request(id) => (Some(id), None),
        DownloadSource::MonitoredItem(id) => (None, Some(id)),
        DownloadSource::Both {
            request_id,
            monitored_item_id,
        } => (Some(request_id), Some(monitored_item_id)),
    }
}

/// Insert a download-queue row. `new.source` guarantees at least one of
/// `request_id`/`monitored_item_id` is set at the Rust type level, matching
/// the DB's `download_queue_has_source` CHECK — this call can never violate
/// it. See the negative test in this module's `#[cfg(test)]` for what
/// happens when that CHECK is bypassed (a raw insert, exercised directly
/// against the constraint, not through this function).
pub async fn enqueue_download(
    pool: &PgPool,
    new: &NewDownloadQueueEntry,
) -> MuseResult<DownloadQueueEntry> {
    let (request_id, monitored_item_id) = download_source_ids(&new.source);
    sqlx::query_as::<_, DownloadQueueEntry>(
        r#"
        INSERT INTO download_queue (
            request_id, monitored_item_id, release_guid, release_title,
            indexer, download_client, client_hash, protocol, size_bytes
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING *
        "#,
    )
    .bind(request_id)
    .bind(monitored_item_id)
    .bind(&new.release_guid)
    .bind(&new.release_title)
    .bind(&new.indexer)
    .bind(&new.download_client)
    .bind(&new.client_hash)
    .bind(&new.protocol)
    .bind(new.size_bytes)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_download_queue_entry(pool: &PgPool, id: i64) -> MuseResult<DownloadQueueEntry> {
    sqlx::query_as::<_, DownloadQueueEntry>("SELECT * FROM download_queue WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("download_queue entry {id} not found")))
}

pub async fn list_download_queue_by_status(
    pool: &PgPool,
    status: &str,
) -> MuseResult<Vec<DownloadQueueEntry>> {
    sqlx::query_as::<_, DownloadQueueEntry>(
        "SELECT * FROM download_queue WHERE status = $1 ORDER BY added_at",
    )
    .bind(status)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// MUSEM-06: is `monitored_item_id` already active (`queued`/`downloading`,
/// i.e. not yet `completed`/`imported`/`failed`/`removed`) in
/// `download_queue`? The wanted worker's idempotency check — an item
/// already mid-flight must be skipped, never re-grabbed, by two passes (or
/// two ticks of the same pass) racing each other.
pub async fn is_monitored_item_active_in_queue(
    pool: &PgPool,
    monitored_item_id: i64,
) -> MuseResult<bool> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM download_queue \
         WHERE monitored_item_id = $1 AND status IN ('queued', 'downloading') \
         LIMIT 1",
    )
    .bind(monitored_item_id)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(row.is_some())
}

/// MUSEM-06 follow-up (review: codex): the real "has the wanted worker
/// already surfaced a pending request for this monitored item" check —
/// replaces the original `monitored_items.last_search_at IS NULL` proxy,
/// which was wrong (a FAILED search also sets `last_search_at` without
/// ever creating a request, so that proxy could permanently suppress a
/// request the item genuinely needed once a later pass's search finally
/// succeeded). "Open" means a `media_requests` row exists for
/// `monitored_item_id` whose `status` is not one of the terminal values
/// (`denied`/`failed`/`available`) — `requested`/`approved`/`searching`/
/// `grabbed` all still count as "already accounted for," so the worker
/// never creates a second request while an earlier one is still live in
/// any non-terminal state.
pub async fn has_open_worker_request_for_monitored_item(
    pool: &PgPool,
    monitored_item_id: i64,
) -> MuseResult<bool> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM media_requests \
         WHERE monitored_item_id = $1 \
           AND status NOT IN ('denied', 'failed', 'available') \
         LIMIT 1",
    )
    .bind(monitored_item_id)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?;
    Ok(row.is_some())
}

pub async fn update_download_status(
    pool: &PgPool,
    id: i64,
    status: &str,
    client_hash: Option<&str>,
) -> MuseResult<DownloadQueueEntry> {
    sqlx::query_as::<_, DownloadQueueEntry>(
        r#"
        UPDATE download_queue
        SET status = $2, client_hash = COALESCE($3, client_hash), updated_at = now()
        WHERE id = $1
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(status)
    .bind(client_hash)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::NotFound(format!("download_queue entry {id} not found")))
}

// ---------------------------------------------------------------------
// history_events
// ---------------------------------------------------------------------

pub async fn record_history_event(
    pool: &PgPool,
    new: &NewHistoryEvent,
) -> MuseResult<HistoryEvent> {
    sqlx::query_as::<_, HistoryEvent>(
        r#"
        INSERT INTO history_events (
            event_type, media_metadata_id, monitored_item_id, download_id,
            source_title, quality, data, languages
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING *
        "#,
    )
    .bind(&new.event_type)
    .bind(new.media_metadata_id)
    .bind(new.monitored_item_id)
    .bind(&new.download_id)
    .bind(&new.source_title)
    .bind(&new.quality)
    .bind(&new.data)
    .bind(&new.languages)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_history_for_metadata(
    pool: &PgPool,
    media_metadata_id: i64,
) -> MuseResult<Vec<HistoryEvent>> {
    sqlx::query_as::<_, HistoryEvent>(
        "SELECT * FROM history_events WHERE media_metadata_id = $1 ORDER BY created_at DESC",
    )
    .bind(media_metadata_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_history_for_download(
    pool: &PgPool,
    download_id: &str,
) -> MuseResult<Vec<HistoryEvent>> {
    sqlx::query_as::<_, HistoryEvent>(
        "SELECT * FROM history_events WHERE download_id = $1 ORDER BY created_at",
    )
    .bind(download_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

// ---------------------------------------------------------------------
// blocklist
// ---------------------------------------------------------------------

pub async fn add_to_blocklist(
    pool: &PgPool,
    new: &NewBlocklistEntry,
) -> MuseResult<BlocklistEntry> {
    sqlx::query_as::<_, BlocklistEntry>(
        r#"
        INSERT INTO blocklist (
            source_title, torrent_hash, media_metadata_id, indexer, message, size_bytes
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING *
        "#,
    )
    .bind(&new.source_title)
    .bind(&new.torrent_hash)
    .bind(new.media_metadata_id)
    .bind(&new.indexer)
    .bind(&new.message)
    .bind(new.size_bytes)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn is_blocklisted(pool: &PgPool, torrent_hash: &str) -> MuseResult<bool> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM blocklist WHERE torrent_hash = $1 LIMIT 1")
            .bind(torrent_hash)
            .fetch_optional(pool)
            .await
            .map_err(MuseError::Database)?;
    Ok(row.is_some())
}

pub async fn list_blocklist(pool: &PgPool) -> MuseResult<Vec<BlocklistEntry>> {
    sqlx::query_as::<_, BlocklistEntry>("SELECT * FROM blocklist ORDER BY created_at DESC")
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::models::acquisition::{
        HistoryEventType, NewDownloadQueueEntry, NewHistoryEvent, NewMediaRequest,
        NewMonitoredItem, QueueStatus, RequestStatus,
    };
    use crate::models::library::{LibraryKind, NewLibrary};
    use crate::models::media_file::{NewMediaFile, ReleaseTypeKind, Revision};
    use crate::models::media_item::NewMediaItem;
    use crate::models::media_metadata::MediaKind;
    use crate::models::quality::{NewQualityDefinition, NewQualityProfile};

    async fn test_pool_or_skip(test_name: &str) -> Option<PgPool> {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping {test_name} \
                 (expected in the default test run; this harness does not \
                 require a live DB)"
            );
            return None;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");
        Some(pool)
    }

    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    }

    async fn seed_library(pool: &PgPool) -> i64 {
        let new = NewLibrary {
            name: format!("musem01-lib-{}", unique_suffix()),
            kind: LibraryKind::Movie,
            root_folder: "/data/movies".to_string(),
            source_arr_name: None,
            source_arr_url: None,
        };
        crate::repo::library::create(pool, &new)
            .await
            .expect("seed library")
            .id
    }

    async fn seed_media_metadata(pool: &PgPool, title: &str) -> i64 {
        let row: (i64,) = sqlx::query_as(
            r#"
            INSERT INTO media_metadata (kind, title, provider_ids)
            VALUES ($1, $2, '{}'::jsonb)
            RETURNING id
            "#,
        )
        .bind(MediaKind::Movie)
        .bind(title)
        .fetch_one(pool)
        .await
        .expect("seed media_metadata");
        row.0
    }

    async fn seed_quality_definition(pool: &PgPool, key: &str, sort_order: i32) -> i64 {
        let new = NewQualityDefinition {
            quality_key: key.to_string(),
            title: key.to_string(),
            source: "webdl".to_string(),
            resolution: Some("1080p".to_string()),
            modifier: "none".to_string(),
            sort_order,
        };
        crate::repo::quality::create_definition(pool, &new)
            .await
            .expect("seed quality_definition")
            .id
    }

    async fn seed_quality_profile(pool: &PgPool, cutoff_quality_id: i64) -> i64 {
        let new = NewQualityProfile {
            name: format!("musem01-profile-{}", unique_suffix()),
            cutoff_quality_id: Some(cutoff_quality_id),
            items: serde_json::json!([]),
            upgrade_allowed: true,
            natural_language_intent: None,
        };
        crate::repo::quality::create_profile(pool, &new)
            .await
            .expect("seed quality_profile")
            .id
    }

    async fn seed_media_item(pool: &PgPool, library_id: i64, media_metadata_id: i64) -> i64 {
        let new = NewMediaItem {
            library_id,
            media_metadata_id,
            path: format!("/data/movies/musem01-{}", unique_suffix()),
            monitored: true,
            quality_profile_id: None,
            minimum_availability: None,
            plex_rating_key: None,
            added_at: None,
        };
        crate::repo::media_item::upsert(pool, &new)
            .await
            .expect("seed media_item")
            .id
    }

    async fn seed_media_file(pool: &PgPool, media_item_id: i64, quality_tier_id: i64) {
        let new = NewMediaFile {
            media_item_id,
            relative_path: format!("musem01-{}.mkv", unique_suffix()),
            size_bytes: Some(1_000_000_000),
            release_group: None,
            languages: vec!["eng".to_string()],
            release_type: ReleaseTypeKind::Single,
            quality_tier_id: Some(quality_tier_id),
            revision: Revision {
                version: 1,
                real: 0,
                is_repack: false,
            },
        };
        crate::repo::media_file::create(pool, &new)
            .await
            .expect("seed media_file");
    }

    #[tokio::test]
    async fn monitored_item_upsert_then_get_round_trips() {
        let Some(pool) = test_pool_or_skip("monitored_item_upsert_then_get_round_trips").await
        else {
            return;
        };
        let library_id = seed_library(&pool).await;
        let media_metadata_id = seed_media_metadata(&pool, "Wanted Movie").await;

        let new = NewMonitoredItem {
            media_metadata_id,
            media_item_id: None,
            library_id,
            monitored: true,
            quality_profile_id: None,
            min_availability: Some("released".to_string()),
        };
        let created = create_monitored_item(&pool, &new)
            .await
            .expect("create_monitored_item");
        assert!(created.monitored);

        let fetched = get_monitored_item(&pool, created.id)
            .await
            .expect("get_monitored_item");
        assert_eq!(fetched.media_metadata_id, media_metadata_id);

        let listed = list_monitored_items(&pool, library_id)
            .await
            .expect("list_monitored_items");
        assert!(listed.iter().any(|m| m.id == created.id));
    }

    #[tokio::test]
    async fn list_wanted_excludes_monitored_item_at_or_above_cutoff() {
        let Some(pool) =
            test_pool_or_skip("list_wanted_excludes_monitored_item_at_or_above_cutoff").await
        else {
            return;
        };
        let library_id = seed_library(&pool).await;

        let low_tier = seed_quality_definition(&pool, &format!("low-{}", unique_suffix()), 10).await;
        let cutoff_tier =
            seed_quality_definition(&pool, &format!("cutoff-{}", unique_suffix()), 20).await;
        let profile_id = seed_quality_profile(&pool, cutoff_tier).await;

        // Below-cutoff row: has a file, but at the low tier -- must be wanted.
        let below_metadata_id = seed_media_metadata(&pool, "Below Cutoff Movie").await;
        let below_item_id = seed_media_item(&pool, library_id, below_metadata_id).await;
        seed_media_file(&pool, below_item_id, low_tier).await;
        let below_monitored = create_monitored_item(
            &pool,
            &NewMonitoredItem {
                media_metadata_id: below_metadata_id,
                media_item_id: Some(below_item_id),
                library_id,
                monitored: true,
                quality_profile_id: Some(profile_id),
                min_availability: None,
            },
        )
        .await
        .expect("create below-cutoff monitored_item");

        // At-cutoff row: has a file exactly at the cutoff tier -- must be
        // excluded (this is the negative-test case the acceptance criteria
        // call for).
        let at_metadata_id = seed_media_metadata(&pool, "At Cutoff Movie").await;
        let at_item_id = seed_media_item(&pool, library_id, at_metadata_id).await;
        seed_media_file(&pool, at_item_id, cutoff_tier).await;
        let at_monitored = create_monitored_item(
            &pool,
            &NewMonitoredItem {
                media_metadata_id: at_metadata_id,
                media_item_id: Some(at_item_id),
                library_id,
                monitored: true,
                quality_profile_id: Some(profile_id),
                min_availability: None,
            },
        )
        .await
        .expect("create at-cutoff monitored_item");

        // No-file row: monitored but nothing on disk -- must be wanted.
        let missing_metadata_id = seed_media_metadata(&pool, "Missing Movie").await;
        let missing_monitored = create_monitored_item(
            &pool,
            &NewMonitoredItem {
                media_metadata_id: missing_metadata_id,
                media_item_id: None,
                library_id,
                monitored: true,
                quality_profile_id: Some(profile_id),
                min_availability: None,
            },
        )
        .await
        .expect("create missing monitored_item");

        let wanted = list_wanted(&pool, library_id)
            .await
            .expect("list_wanted");
        let wanted_ids: Vec<i64> = wanted.iter().map(|w| w.monitored_item_id).collect();

        assert!(
            wanted_ids.contains(&below_monitored.id),
            "below-cutoff row must be wanted"
        );
        assert!(
            wanted_ids.contains(&missing_monitored.id),
            "no-file row must be wanted"
        );
        assert!(
            !wanted_ids.contains(&at_monitored.id),
            "at-cutoff row must be excluded from list_wanted"
        );
    }

    #[tokio::test]
    async fn media_request_create_list_by_status_and_update_status() {
        let Some(pool) =
            test_pool_or_skip("media_request_create_list_by_status_and_update_status").await
        else {
            return;
        };

        let created = create_request(
            &pool,
            &NewMediaRequest {
                provider_ids: serde_json::json!({"tmdb": "12345"}),
                media_kind: "movie".to_string(),
                title: "Requested Movie".to_string(),
                requested_by: Some("musem01-test".to_string()),
                tier: None,
                quality_profile_id: None,
                note: None,
                monitored_item_id: None,
            },
        )
        .await
        .expect("create_request");
        assert_eq!(created.status, RequestStatus::Requested.as_str());

        let listed = list_requests_by_status(&pool, RequestStatus::Requested.as_str())
            .await
            .expect("list_requests_by_status");
        assert!(listed.iter().any(|r| r.id == created.id));

        let updated = update_request_status(&pool, created.id, RequestStatus::Approved.as_str())
            .await
            .expect("update_request_status");
        assert_eq!(updated.status, RequestStatus::Approved.as_str());
    }

    #[tokio::test]
    async fn enqueue_download_and_record_history_round_trip() {
        let Some(pool) =
            test_pool_or_skip("enqueue_download_and_record_history_round_trip").await
        else {
            return;
        };

        let request = create_request(
            &pool,
            &NewMediaRequest {
                provider_ids: serde_json::json!({}),
                media_kind: "movie".to_string(),
                title: "Queue Test Movie".to_string(),
                requested_by: None,
                tier: None,
                quality_profile_id: None,
                note: None,
                monitored_item_id: None,
            },
        )
        .await
        .expect("create_request");

        let queued = enqueue_download(
            &pool,
            &NewDownloadQueueEntry {
                source: DownloadSource::Request(request.id),
                release_guid: format!("guid-{}", unique_suffix()),
                release_title: "Queue.Test.Movie.1080p.WEB-DL".to_string(),
                indexer: Some("test-indexer".to_string()),
                download_client: Some("qbittorrent".to_string()),
                client_hash: None,
                protocol: Some("torrent".to_string()),
                size_bytes: Some(2_000_000_000),
            },
        )
        .await
        .expect("enqueue_download");
        assert_eq!(queued.status, QueueStatus::Queued.as_str());
        assert_eq!(queued.request_id, Some(request.id));

        let updated = update_download_status(
            &pool,
            queued.id,
            QueueStatus::Downloading.as_str(),
            Some("abc123hash"),
        )
        .await
        .expect("update_download_status");
        assert_eq!(updated.status, QueueStatus::Downloading.as_str());
        assert_eq!(updated.client_hash.as_deref(), Some("abc123hash"));

        let event = record_history_event(
            &pool,
            &NewHistoryEvent {
                event_type: HistoryEventType::Grabbed.as_str().to_string(),
                media_metadata_id: None,
                monitored_item_id: None,
                download_id: updated.client_hash.clone(),
                source_title: Some(updated.release_title.clone()),
                quality: None,
                data: serde_json::json!({"indexer": "test-indexer"}),
                languages: serde_json::json!(["eng"]),
            },
        )
        .await
        .expect("record_history_event");

        let history = list_history_for_download(&pool, "abc123hash")
            .await
            .expect("list_history_for_download");
        assert!(history.iter().any(|h| h.id == event.id));
    }

    #[tokio::test]
    async fn download_queue_check_rejects_row_with_neither_ref() {
        let Some(pool) =
            test_pool_or_skip("download_queue_check_rejects_row_with_neither_ref").await
        else {
            return;
        };

        // Deliberately bypasses `enqueue_download` (whose `DownloadSource`
        // type can't construct this shape) to exercise the DB-level CHECK
        // itself, matching the acceptance criterion "download_queue CHECK
        // rejects a row with neither ref".
        let result: Result<(i64,), sqlx::Error> = sqlx::query_as(
            r#"
            INSERT INTO download_queue (
                request_id, monitored_item_id, release_guid, release_title
            )
            VALUES (NULL, NULL, $1, $2)
            RETURNING id
            "#,
        )
        .bind(format!("guid-check-{}", unique_suffix()))
        .bind("Neither Ref Movie")
        .fetch_one(&pool)
        .await;

        assert!(
            result.is_err(),
            "inserting a download_queue row with neither request_id nor \
             monitored_item_id must violate download_queue_has_source"
        );
    }

    #[tokio::test]
    async fn blocklist_add_and_is_blocklisted() {
        let Some(pool) = test_pool_or_skip("blocklist_add_and_is_blocklisted").await else {
            return;
        };
        let hash = format!("blocked-{}", unique_suffix());

        assert!(!is_blocklisted(&pool, &hash).await.expect("is_blocklisted before add"));

        add_to_blocklist(
            &pool,
            &NewBlocklistEntry {
                source_title: "Bad Release".to_string(),
                torrent_hash: Some(hash.clone()),
                media_metadata_id: None,
                indexer: Some("test-indexer".to_string()),
                message: Some("failed import".to_string()),
                size_bytes: None,
            },
        )
        .await
        .expect("add_to_blocklist");

        assert!(is_blocklisted(&pool, &hash).await.expect("is_blocklisted after add"));

        let listed = list_blocklist(&pool).await.expect("list_blocklist");
        assert!(listed.iter().any(|b| b.torrent_hash.as_deref() == Some(hash.as_str())));
    }
}
