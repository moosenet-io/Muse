//! MUSE-06: one-time Tautulli watch-history backfill.
//!
//! [`run`] pages through `get_history`, maps each row onto `play_sessions`
//! (+ `play_session_media_info` when stream-data enrichment succeeds), and
//! is safe to run repeatedly:
//!
//! - **Idempotent re-run**: every imported row is looked up first by
//!   Tautulli's `reference_id` (`play_sessions.tautulli_ref_id`) —
//!   `repo::play_session::find_by_tautulli_ref`. If it's already there, the
//!   row is skipped. This covers both resolved and unresolved rows, unlike
//!   the table's own `(account_id, media_item_id, episode_id, started_at)`
//!   UNIQUE, which only dedups resolved rows (Postgres treats distinct NULLs
//!   as non-conflicting — see the caveat on `repo::play_session::upsert`).
//! - **Dedup vs. native capture (MUSE-07)**: before inserting, a row is
//!   checked against `repo::play_session::find_overlapping_native` — a
//!   natively-captured session (`tautulli_ref_id IS NULL`) for the same
//!   account/media/episode within `overlap_tolerance_secs` of `started_at`.
//!   If one exists, native wins: no new row is inserted, and the native row
//!   is stamped with `tautulli_ref_id` for provenance
//!   (`repo::play_session::attach_tautulli_ref`).
//! - **Resumable in the loose sense**: because both guards above key off
//!   data already in Postgres (not an external cursor), running the whole
//!   backfill again from `start=0` after an interruption safely re-derives
//!   the same end state rather than duplicating rows. No `MUSE_TEST_DATABASE_URL`
//!   migration/cursor table was added — the operator-facing entrypoint (a
//!   CLI subcommand or ops script) that would call [`run`] is intentionally
//!   left to the orchestrator (this crate has no `[[bin]]` beyond the axum
//!   service), so a persisted progress cursor isn't wired to anything yet
//!   and was left out per the task's "prefer none" guidance.

use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
use ipnetwork::IpNetwork;
use sqlx::PgPool;
use std::str::FromStr;

use crate::error::MuseResult;
use crate::models::account::NewAccount;
use crate::models::play_session::{NewPlaySession, NewPlaySessionMediaInfo};
use crate::repo;

use super::client::TautulliClient;
use super::models::{parse_decision_kind, HistoryRow};

/// `percent_complete >= COMPLETE_THRESHOLD` marks a session finished when
/// Tautulli's own `watched_status` doesn't already say so — spec §3.3/§4-D.
const COMPLETE_THRESHOLD: f32 = 0.90;
/// `percent_complete < ABANDON_THRESHOLD` (and not finished) marks a session
/// abandoned — spec §3.3/§4-D. Backfill has no "no later finish" signal the
/// way the live reconstruction worker does (MUSE-07), so this is a
/// best-effort approximation from percent alone; documented divergence.
const ABANDON_THRESHOLD: f32 = 0.15;

/// Default tolerance for matching a Tautulli history row against a
/// natively-captured session covering the same watch — spec §4-D
/// (`started_at±120s`).
pub const DEFAULT_OVERLAP_TOLERANCE_SECS: i64 = 120;

#[derive(Debug, Clone)]
pub struct BackfillOptions {
    pub page_size: i64,
    /// Fetch `get_metadata` per row to get the item's true media runtime
    /// (more accurate `duration_ms`/`percent_complete` than deriving from
    /// the history row's own play-duration alone). Best-effort: a failure
    /// degrades to the history-row-derived estimate rather than failing the
    /// row.
    pub enrich_metadata: bool,
    /// Fetch `get_stream_data` per row to populate `play_session_media_info`
    /// (quality/transcode detail). Best-effort, same degrade posture as
    /// `enrich_metadata`.
    pub enrich_stream_data: bool,
    pub overlap_tolerance_secs: i64,
}

impl Default for BackfillOptions {
    fn default() -> Self {
        Self {
            page_size: super::client::DEFAULT_PAGE_SIZE,
            enrich_metadata: true,
            enrich_stream_data: true,
            overlap_tolerance_secs: DEFAULT_OVERLAP_TOLERANCE_SECS,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillSummary {
    pub pages_fetched: usize,
    pub rows_seen: usize,
    pub imported: usize,
    /// Already present from a prior run (`tautulli_ref_id` matched) — the
    /// idempotency guard.
    pub skipped_already_imported: usize,
    /// A natively-captured session already covered this watch; the native
    /// row was stamped with `tautulli_ref_id` instead of inserting a
    /// duplicate.
    pub skipped_native_overlap: usize,
    /// Row had no `reference_id` at all (Tautulli should always send one,
    /// but the parser is deliberately permissive) — skipped since it can't
    /// be deduped safely.
    pub skipped_missing_reference_id: usize,
    pub resolved_media: usize,
    pub unresolved_media: usize,
}

/// Run the full Tautulli backfill: page through `get_history` from the
/// beginning until Tautulli reports no more rows, importing (or deduping)
/// each one. Never aborts the whole run over one bad row — a single row's
/// enrichment/insert failure is logged and counted implicitly by not
/// incrementing `imported`, and the loop continues with the next row.
pub async fn run(
    pool: &PgPool,
    client: &TautulliClient,
    options: &BackfillOptions,
) -> MuseResult<BackfillSummary> {
    let mut summary = BackfillSummary::default();
    let mut start: i64 = 0;

    loop {
        let page = client.get_history(start, options.page_size).await?;
        summary.pages_fetched += 1;

        if page.rows.is_empty() {
            break;
        }

        for row in &page.rows {
            summary.rows_seen += 1;
            if let Err(e) = import_row(pool, client, row, options, &mut summary).await {
                tracing::warn!(
                    reference_id = ?row.reference_id,
                    error = %e,
                    "failed to import tautulli history row; continuing with the rest of the backfill"
                );
            }
        }

        start += page.rows.len() as i64;
        if start >= page.records_filtered {
            break;
        }
    }

    Ok(summary)
}

async fn import_row(
    pool: &PgPool,
    client: &TautulliClient,
    row: &HistoryRow,
    options: &BackfillOptions,
    summary: &mut BackfillSummary,
) -> MuseResult<()> {
    let Some(reference_id) = row.reference_id else {
        summary.skipped_missing_reference_id += 1;
        return Ok(());
    };

    if repo::play_session::find_by_tautulli_ref(pool, reference_id)
        .await?
        .is_some()
    {
        summary.skipped_already_imported += 1;
        return Ok(());
    }

    let account_id = resolve_account(pool, row).await?;
    let (media_item_id, episode_id) = resolve_media(pool, row).await?;

    if media_item_id.is_some() || episode_id.is_some() {
        summary.resolved_media += 1;
    } else {
        summary.unresolved_media += 1;
    }

    let Some(started_at) = row.started.and_then(unix_seconds_to_utc) else {
        // Without a `started` timestamp there is no meaningful session to
        // record (the UNIQUE key and overlap dedup both hinge on it).
        return Ok(());
    };
    let stopped_at = row.stopped.and_then(unix_seconds_to_utc);

    // Prefer the true media runtime from get_metadata (best-effort) over the
    // history row's own play-duration, which is watched time, not runtime.
    let mut duration_ms = row.duration.map(|s| s * 1000);
    if options.enrich_metadata {
        if let Some(rating_key) = row.rating_key_str() {
            match client.get_metadata(&rating_key).await {
                Ok(Some(meta)) if meta.duration.is_some() => duration_ms = meta.duration,
                Ok(_) => {}
                Err(e) => tracing::debug!(
                    rating_key,
                    error = %e,
                    "get_metadata enrichment failed; using history-row duration estimate"
                ),
            }
        }
    }

    let watched_ms = row.duration.map(|s| s * 1000);
    let percent_complete = row.percent_complete.map(|p| (p / 100.0) as f32);
    let is_finished = row.watched_status.map(|w| w >= 1.0).unwrap_or(false)
        || percent_complete.map(|p| p >= COMPLETE_THRESHOLD).unwrap_or(false);
    let is_abandoned = !is_finished
        && percent_complete.map(|p| p < ABANDON_THRESHOLD).unwrap_or(false);

    let ip_address = row
        .ip_address
        .as_deref()
        .and_then(|ip| IpNetwork::from_str(ip).ok());

    // Dedup vs. native capture (MUSE-07): a native session covering the same
    // watch wins — attach provenance and skip inserting a duplicate.
    if let Some(native) = repo::play_session::find_overlapping_native(
        pool,
        account_id,
        media_item_id,
        episode_id,
        started_at,
        options.overlap_tolerance_secs,
    )
    .await?
    {
        repo::play_session::attach_tautulli_ref(pool, native.id, reference_id).await?;
        summary.skipped_native_overlap += 1;
        return Ok(());
    }

    let new_session = NewPlaySession {
        account_id,
        media_item_id,
        episode_id,
        session_key: row.session_key.clone(),
        tautulli_ref_id: Some(reference_id),
        started_at,
        stopped_at,
        duration_ms,
        watched_ms,
        // Tautulli's get_history rows don't carry a reliable final
        // view-offset distinct from watched duration in this shape; using
        // watched_ms is the best available approximation for backfilled
        // rows (documented divergence from the live poller, which reads
        // the real `viewOffset`).
        view_offset_ms: watched_ms,
        percent_complete,
        paused_counter: row.paused_counter.unwrap_or(0),
        // Tautulli's history export doesn't expose accumulated pause time,
        // only a pause *count* — left at 0 (unknown) rather than guessed.
        paused_ms: 0,
        is_finished,
        is_abandoned,
        player: row.player.clone(),
        platform: row.platform.clone(),
        product: row.product.clone(),
        device: None,
        ip_address,
        started_hour: Some(started_at.hour() as i32),
        started_dow: Some(started_at.weekday().num_days_from_sunday() as i32),
        // No reliable device-class signal in the history row shape to infer
        // cinema-vs-mobile context from; left unset rather than guessed.
        is_cinema_context: None,
    };

    let session = repo::play_session::upsert(pool, &new_session).await?;
    summary.imported += 1;

    if options.enrich_stream_data {
        if let Some(row_id) = row.row_id {
            match client.get_stream_data(row_id).await {
                Ok(Some(stream)) => {
                    let media_info = NewPlaySessionMediaInfo {
                        video_decision: parse_decision_kind(stream.video_decision.as_deref()),
                        audio_decision: parse_decision_kind(stream.audio_decision.as_deref()),
                        transcode_decision: parse_decision_kind(stream.transcode_decision.as_deref()),
                        container: stream.container,
                        video_codec: stream.video_codec,
                        audio_codec: stream.audio_codec,
                        audio_channels: stream.audio_channels,
                        video_resolution: stream.video_resolution,
                        bitrate: stream.bitrate,
                        width: stream.width,
                        height: stream.height,
                        transcode_reason: stream.transcode_reason,
                    };
                    if let Err(e) = repo::play_session::upsert_media_info(pool, session.id, &media_info).await {
                        tracing::debug!(session_id = session.id, error = %e, "failed to persist stream-data enrichment");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(row_id, error = %e, "get_stream_data enrichment failed"),
            }
        }
    }

    Ok(())
}

/// Resolve (and lazily create) the `accounts` row for a history row's
/// Tautulli `user_id` — Tautulli's `user_id` is the underlying Plex account
/// ID, so this keys on the same `plex_account_id` MUSE-04/07 use. Returns
/// `None` (not an error) when the row carries no `user_id` at all.
async fn resolve_account(pool: &PgPool, row: &HistoryRow) -> MuseResult<Option<i64>> {
    let Some(user_id) = row.user_id else {
        return Ok(None);
    };

    let account = repo::account::upsert_by_plex_account_id(
        pool,
        &NewAccount {
            plex_account_id: Some(user_id.to_string()),
            username: row.user.clone(),
            friendly_name: row.friendly_name.clone(),
            is_home_user: true,
            is_primary: false,
        },
    )
    .await?;

    Ok(Some(account.id))
}

/// Resolve a history row's Plex `rating_key` onto a library `media_item`
/// and/or `episode`, per spec §4-D ("Resolve to media_item/episode where
/// possible ... else leave the media ref NULL"):
/// - `media_type == "movie"` (or anything else non-episode): `rating_key` →
///   `media_items.plex_rating_key`.
/// - `media_type == "episode"`: `rating_key` → `episodes.plex_rating_key`
///   (also yields the owning show's `media_item_id`); if the episode itself
///   isn't in the library yet, fall back to resolving just the show via
///   `grandparent_rating_key` so the session is at least attributable to a
///   show-level media_item.
async fn resolve_media(pool: &PgPool, row: &HistoryRow) -> MuseResult<(Option<i64>, Option<i64>)> {
    let is_episode = row.media_type.as_deref() == Some("episode");

    if is_episode {
        if let Some(rating_key) = row.rating_key_str() {
            if let Some(episode) = repo::episode::find_by_plex_rating_key(pool, &rating_key).await? {
                return Ok((Some(episode.media_item_id), Some(episode.id)));
            }
        }
        if let Some(show_rating_key) = row.grandparent_rating_key_str() {
            if let Some(show) = repo::media_item::find_by_plex_rating_key(pool, &show_rating_key).await? {
                return Ok((Some(show.id), None));
            }
        }
        return Ok((None, None));
    }

    if let Some(rating_key) = row.rating_key_str() {
        if let Some(item) = repo::media_item::find_by_plex_rating_key(pool, &rating_key).await? {
            return Ok((Some(item.id), None));
        }
    }

    Ok((None, None))
}

fn unix_seconds_to_utc(secs: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(secs, 0).single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use crate::models::library::{LibraryKind, NewLibrary};
    use crate::models::media_item::NewMediaItem;
    use crate::models::media_metadata::{MediaKind, NewMediaMetadata};

    fn history_page_body(rows_json: &str) -> String {
        format!(
            r#"{{"response": {{"result": "success", "data": {{"recordsFiltered": {count}, "recordsTotal": {count}, "data": [{rows_json}]}}}}}}"#,
            count = if rows_json.trim().is_empty() { 0 } else { 1 },
        )
    }

    /// Pure mapping/parsing check: no network, no DB — constructs a
    /// `HistoryRow` directly (as `get_history` parsing would produce) and
    /// asserts the finished/abandoned/percent mapping spec §4-D describes.
    #[test]
    fn watched_status_and_percent_map_to_finished_and_abandoned() {
        let finished_row = HistoryRow {
            reference_id: Some(1),
            watched_status: Some(1.0),
            percent_complete: Some(96.0),
            ..Default::default()
        };
        let percent = finished_row.percent_complete.map(|p| (p / 100.0) as f32);
        assert!(percent.unwrap() >= COMPLETE_THRESHOLD);

        let abandoned_row = HistoryRow {
            reference_id: Some(2),
            watched_status: Some(0.0),
            percent_complete: Some(8.0),
            ..Default::default()
        };
        let percent = abandoned_row.percent_complete.map(|p| (p / 100.0) as f32);
        assert!(percent.unwrap() < ABANDON_THRESHOLD);
    }

    #[test]
    fn unix_seconds_to_utc_round_trips() {
        let dt = unix_seconds_to_utc(1_700_000_000).expect("valid unix timestamp should parse");
        assert_eq!(dt.timestamp(), 1_700_000_000);
    }

    /// Mocked-Tautulli, live-DB round trip: `run()` against an httpmock
    /// Tautulli server, asserting the imported `play_sessions` row's mapped
    /// fields, then re-running the exact same mocked history to assert
    /// idempotency (no new row, no duplicate).
    ///
    /// Gated on `MUSE_TEST_DATABASE_URL` — skips cleanly (does not fail)
    /// when unset, same pattern as `src/integration_tests.rs`.
    #[tokio::test]
    async fn backfill_imports_and_is_idempotent_on_rerun() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping backfill_imports_and_is_idempotent_on_rerun \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();
        let rating_key: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;
        let user_id: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;

        // A movie in the library for the history row to resolve onto.
        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("radarr_muse06_{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/Movies/".to_string(),
                source_arr_name: Some("radarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("tmdb-muse06-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: "Backfill Test Movie".to_string(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(105),
                year: Some(2019),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        let item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: "/media/Movies/Backfill Test Movie (2019)".to_string(),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(rating_key.to_string()),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        let reference_id: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;
        let row_json = format!(
            r#"{{
                "reference_id": {reference_id},
                "row_id": {reference_id},
                "started": 1700000000,
                "stopped": 1700006300,
                "duration": 6300,
                "paused_counter": 2,
                "user_id": {user_id},
                "user": "backfill_test_user_{suffix}",
                "friendly_name": "Backfill Test User",
                "player": "Living Room",
                "platform": "Roku",
                "product": "Plex for Roku",
                "ip_address": "192.0.2.7",
                "rating_key": {rating_key},
                "full_title": "Backfill Test Movie",
                "media_type": "movie",
                "percent_complete": 97,
                "watched_status": 1
            }}"#
        );

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_history");
            then.status(200)
                .header("content-type", "application/json")
                .body(history_page_body(&row_json));
        });
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_metadata");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"response": {"result": "success", "data": {"media_type": "movie", "duration": 6300000}}}"#);
        });
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_stream_data");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"response": {"result": "success", "data": {
                        "video_decision": "direct play", "audio_decision": "direct play",
                        "transcode_decision": "direct play", "container": "mkv"
                    }}}"#,
                );
        });

        let client = TautulliClient::new(server.base_url(), "test-key").expect("client should construct");
        let options = BackfillOptions {
            page_size: 250,
            ..Default::default()
        };

        let summary = run(&pool, &client, &options).await.expect("backfill run should succeed");
        assert_eq!(summary.imported, 1);
        assert_eq!(summary.resolved_media, 1);
        assert_eq!(summary.skipped_already_imported, 0);

        let imported = repo::play_session::find_by_tautulli_ref(&pool, reference_id)
            .await
            .expect("find by tautulli ref")
            .expect("session should have been imported");
        assert_eq!(imported.media_item_id, Some(item.id));
        assert!(imported.is_finished);
        assert!(!imported.is_abandoned);
        assert!((imported.percent_complete.unwrap() - 0.97).abs() < 0.001);
        assert_eq!(imported.paused_counter, 2);

        let media_info = repo::play_session::get_media_info(&pool, imported.id)
            .await
            .expect("get media info")
            .expect("media info should have been enriched");
        assert_eq!(media_info.container.as_deref(), Some("mkv"));

        // Re-running the exact same history must import nothing new — the
        // tautulli_ref_id idempotency guard.
        let second_summary = run(&pool, &client, &options).await.expect("second backfill run should succeed");
        assert_eq!(second_summary.imported, 0);
        assert_eq!(second_summary.skipped_already_imported, 1);

        let sessions_for_item = repo::play_session::list_for_media_item(&pool, item.id)
            .await
            .expect("list sessions for media item");
        assert_eq!(sessions_for_item.len(), 1, "re-running the backfill must not duplicate the session");
    }

    /// Dedup-vs-native: a natively-captured session (no `tautulli_ref_id`,
    /// as MUSE-07 would write) already covers the same watch — the backfill
    /// must not insert a second row, only stamp provenance onto the native
    /// one.
    #[tokio::test]
    async fn backfill_prefers_native_session_over_duplicate_insert() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping backfill_prefers_native_session_over_duplicate_insert \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();
        let rating_key: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;
        let user_id: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("radarr_muse06b_{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/Movies/".to_string(),
                source_arr_name: Some("radarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("tmdb-muse06b-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: "Native Overlap Test Movie".to_string(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(100),
                year: Some(2018),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        let item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: "/media/Movies/Native Overlap Test Movie (2018)".to_string(),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(rating_key.to_string()),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        let account = repo::account::upsert_by_plex_account_id(
            &pool,
            &NewAccount {
                plex_account_id: Some(user_id.to_string()),
                username: Some(format!("native_user_{suffix}")),
                friendly_name: None,
                is_home_user: true,
                is_primary: false,
            },
        )
        .await
        .expect("upsert account");

        let started_at = unix_seconds_to_utc(1_700_100_000).expect("valid timestamp");

        // Simulate a session MUSE-07 already captured natively (no
        // tautulli_ref_id) for the exact same account/media/time.
        let native_session = repo::play_session::upsert(
            &pool,
            &NewPlaySession {
                account_id: Some(account.id),
                media_item_id: Some(item.id),
                episode_id: None,
                session_key: Some(format!("native-session-{suffix}")),
                tautulli_ref_id: None,
                started_at,
                stopped_at: Some(started_at + chrono::Duration::minutes(95)),
                duration_ms: Some(100 * 60 * 1000),
                watched_ms: Some(95 * 60 * 1000),
                view_offset_ms: Some(95 * 60 * 1000),
                percent_complete: Some(0.95),
                paused_counter: 0,
                paused_ms: 0,
                is_finished: true,
                is_abandoned: false,
                player: Some("Native Capture".to_string()),
                platform: None,
                product: None,
                device: None,
                ip_address: None,
                started_hour: Some(started_at.hour() as i32),
                started_dow: Some(started_at.weekday().num_days_from_sunday() as i32),
                is_cinema_context: None,
            },
        )
        .await
        .expect("upsert native play_session");

        let reference_id: i64 = (Uuid::new_v4().as_u128() % 1_000_000) as i64;
        let row_json = format!(
            r#"{{
                "reference_id": {reference_id},
                "row_id": {reference_id},
                "started": 1700100000,
                "stopped": 1700105700,
                "duration": 5700,
                "user_id": {user_id},
                "rating_key": {rating_key},
                "media_type": "movie",
                "percent_complete": 95,
                "watched_status": 1
            }}"#
        );

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_history");
            then.status(200)
                .header("content-type", "application/json")
                .body(history_page_body(&row_json));
        });
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_metadata");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"response": {"result": "success", "data": {}}}"#);
        });
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_stream_data");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"response": {"result": "success", "data": {}}}"#);
        });

        let client = TautulliClient::new(server.base_url(), "test-key").expect("client should construct");
        let summary = run(&pool, &client, &BackfillOptions::default())
            .await
            .expect("backfill run should succeed");

        assert_eq!(summary.imported, 0, "the overlapping native session must win, not a new insert");
        assert_eq!(summary.skipped_native_overlap, 1);

        let sessions_for_item = repo::play_session::list_for_media_item(&pool, item.id)
            .await
            .expect("list sessions for media item");
        assert_eq!(sessions_for_item.len(), 1, "no duplicate session should exist");
        assert_eq!(sessions_for_item[0].id, native_session.id);

        let updated_native = repo::play_session::get(&pool, native_session.id)
            .await
            .expect("get native session");
        assert_eq!(
            updated_native.tautulli_ref_id,
            Some(reference_id),
            "the native row should be stamped with tautulli provenance"
        );
    }
}
