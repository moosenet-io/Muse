//! BSEED-4: roll resolved `play_sessions` up into `watch_stats`.
//!
//! The taste pipeline ([`crate::taste_model::signals::derive_signals_for_account`])
//! reads `watch_stats`/`ratings`/`watchlist` — but nothing in production ever
//! *wrote* `watch_stats` from the imported Tautulli sessions (every
//! `upsert_watch_stats` caller before this was a test or fixture loader). So
//! even a perfectly-resolved session produced zero taste signals. This module
//! is the missing writer: it aggregates an account's resolved sessions,
//! per `media_item_id`, into the `watch_stats` shape the taste recompute reads.
//!
//! Episode sessions roll up to their owning show's `media_item_id`
//! automatically: `play_sessions.media_item_id` already points at the show for
//! TV plays (the specific episode is in `episode_id`), so grouping by
//! `media_item_id` folds every episode watch into the show-level aggregate.
//!
//! Idempotent, full-replace posture (same as
//! [`crate::taste_model::signals::replace_derived_signals`]): each
//! `(account_id, media_item_id)` aggregate is recomputed from scratch and
//! full-replace-upserted (`repo::watch_stats::upsert_watch_stats`), so
//! re-running with no new sessions reproduces the same rows.

use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};
use crate::models::watch_stats::NewWatchStats;
use crate::repo;

/// One `(account_id, media_item_id)` aggregate straight out of Postgres.
/// Every SUM/COUNT is cast back to a concrete width in SQL (Postgres widens
/// `SUM(bigint)`→`numeric` and `COUNT`→`bigint`) so the `FromRow` decode is
/// unambiguous.
#[derive(Debug, Clone, FromRow)]
struct WatchStatsAgg {
    media_item_id: i64,
    play_count: i64,
    finished_count: i64,
    total_watched_ms: i64,
    avg_percent: Option<f32>,
    last_watched_at: Option<chrono::DateTime<chrono::Utc>>,
    first_watched_at: Option<chrono::DateTime<chrono::Utc>>,
    any_abandoned: bool,
}

/// Aggregate `account_id`'s resolved `play_sessions` into `watch_stats`,
/// returning how many `(account, item)` aggregates were written. Never blends
/// across accounts — scoped entirely to the one `account_id` (multi-user
/// strict, matching the rest of `taste_model`).
///
/// Field mapping (per `(account_id, media_item_id)` over resolved sessions):
/// - `play_count`      = number of sessions
/// - `finished_count`  = sessions with `is_finished`
/// - `rewatch_count`   = `max(0, finished_count - 1)` (finishes beyond the first)
/// - `total_watched_ms`= `SUM(watched_ms)`
/// - `avg_percent`     = `AVG(percent_complete)`
/// - `first/last_watched_at` = min/max `started_at`
/// - `abandoned`       = any session abandoned AND never finished
pub async fn rebuild_watch_stats_for_account(pool: &PgPool, account_id: i64) -> MuseResult<usize> {
    let aggregates = compute_aggregates(pool, account_id).await?;
    write_aggregates(pool, account_id, &aggregates).await
}

/// The per-`media_item` aggregates for `account_id`'s resolved sessions. Split
/// out (read-only) so the write half can be a single atomic transaction.
async fn compute_aggregates(pool: &PgPool, account_id: i64) -> MuseResult<Vec<WatchStatsAgg>> {
    sqlx::query_as::<_, WatchStatsAgg>(
        r#"
        SELECT
            media_item_id,
            COUNT(*)::bigint                                   AS play_count,
            (COUNT(*) FILTER (WHERE is_finished))::bigint      AS finished_count,
            COALESCE(SUM(watched_ms), 0)::bigint               AS total_watched_ms,
            AVG(percent_complete)::real                        AS avg_percent,
            MAX(started_at)                                    AS last_watched_at,
            MIN(started_at)                                    AS first_watched_at,
            COALESCE(bool_or(is_abandoned), false)             AS any_abandoned
        FROM play_sessions
        WHERE account_id = $1 AND media_item_id IS NOT NULL
        GROUP BY media_item_id
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Write the account's aggregates into `watch_stats` in a SINGLE transaction
/// (FIX 2a): the whole rebuild commits all-or-nothing, so a transient failure
/// mid-way rolls back and leaves `watch_stats` at its prior consistent state
/// (never a half-old/half-new mix that a subsequent taste recompute would read
/// as if authoritative). Returns the number of aggregates written.
async fn write_aggregates(
    pool: &PgPool,
    account_id: i64,
    aggregates: &[WatchStatsAgg],
) -> MuseResult<usize> {
    let mut tx = pool.begin().await.map_err(MuseError::Database)?;

    let mut written = 0usize;
    for agg in aggregates {
        let finished_count = i32::try_from(agg.finished_count).unwrap_or(i32::MAX);
        let play_count = i32::try_from(agg.play_count).unwrap_or(i32::MAX);
        let rewatch_count = (finished_count - 1).max(0);
        // Abandoned only if the account never actually finished it (a later
        // finish overrides an earlier abandon) — mirrors the per-session
        // `is_abandoned = !is_finished && ...` rule in the backfill importer.
        let abandoned = agg.any_abandoned && finished_count == 0;

        // On error, `?` returns and `tx` is dropped → the whole rebuild rolls
        // back (no partial write survives).
        repo::watch_stats::upsert_watch_stats(
            &mut *tx,
            &NewWatchStats {
                account_id,
                media_item_id: agg.media_item_id,
                play_count,
                finished_count,
                rewatch_count,
                total_watched_ms: agg.total_watched_ms,
                avg_percent: agg.avg_percent,
                last_watched_at: agg.last_watched_at,
                abandoned,
                first_watched_at: agg.first_watched_at,
            },
        )
        .await?;
        written += 1;
    }

    tx.commit().await.map_err(MuseError::Database)?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure derivation checks for the non-SQL bits of the mapping (rewatch
    /// derivation + the abandoned-overridden-by-finish rule), independent of a
    /// live DB. The SQL aggregation itself is exercised by the live-DB-gated
    /// `rebuild_watch_stats_rolls_up_resolved_sessions` test below.
    #[test]
    fn rewatch_and_abandoned_derivation() {
        // 3 finishes -> 2 rewatches; never abandoned since it was finished.
        let finished_count = 3i32;
        let any_abandoned = true;
        assert_eq!((finished_count - 1).max(0), 2);
        assert!(!(any_abandoned && finished_count == 0));

        // 0 finishes, was abandoned -> abandoned, 0 rewatches.
        let finished_count = 0i32;
        let any_abandoned = true;
        assert_eq!((finished_count - 1).max(0), 0);
        assert!(any_abandoned && finished_count == 0);

        // 1 finish -> 0 rewatches.
        assert_eq!((1i32 - 1).max(0), 0);
    }

    /// Live-DB round trip (gated on `MUSE_TEST_DATABASE_URL`; skips cleanly
    /// when unset, same posture as every other live-DB test in this crate):
    /// seeds an account + a movie `media_item` + three resolved
    /// `play_sessions` (two finished, one abandoned), runs
    /// [`rebuild_watch_stats_for_account`], and asserts the rolled-up
    /// `watch_stats` row matches the expected aggregate — proving resolved
    /// sessions become taste input.
    #[tokio::test]
    async fn rebuild_watch_stats_rolls_up_resolved_sessions() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 rebuild_watch_stats_rolls_up_resolved_sessions \
                 (expected in the default test run; the crate does not require a live DB)"
            );
            return;
        };

        use chrono::{Duration, Utc};
        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        use crate::models::account::NewAccount;
        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::play_session::NewPlaySession;

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

        let account = repo::account::create(
            &pool,
            &NewAccount {
                plex_account_id: Some(format!("bseed4-acct-{suffix}")),
                username: Some(format!("bseed4_{suffix}")),
                friendly_name: None,
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account");

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("bseed4-lib-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/bseed4".to_string(),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("bseed4-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("BSEED-4 Movie {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(120),
                year: Some(2020),
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
                path: format!("/media/bseed4/movie-{suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        let base = Utc::now() - Duration::days(10);
        // Two finished watches (a rewatch) + one abandoned, all resolved to the
        // same item, at distinct started_at (the UNIQUE key).
        for (offset_days, finished, abandoned, watched_ms, pct) in [
            (0i64, true, false, 120 * 60 * 1000i64, 0.98f32),
            (3, true, false, 120 * 60 * 1000, 0.95),
            (6, false, true, 8 * 60 * 1000, 0.07),
        ] {
            repo::play_session::upsert(
                &pool,
                &NewPlaySession {
                    account_id: Some(account.id),
                    media_item_id: Some(item.id),
                    episode_id: None,
                    session_key: Some(format!("bseed4-{suffix}-{offset_days}")),
                    tautulli_ref_id: None,
                    started_at: base + Duration::days(offset_days),
                    stopped_at: Some(base + Duration::days(offset_days) + Duration::hours(2)),
                    duration_ms: Some(120 * 60 * 1000),
                    watched_ms: Some(watched_ms),
                    view_offset_ms: Some(watched_ms),
                    percent_complete: Some(pct),
                    paused_counter: 0,
                    paused_ms: 0,
                    is_finished: finished,
                    is_abandoned: abandoned,
                    player: None,
                    platform: None,
                    product: None,
                    device: None,
                    ip_address: None,
                    started_hour: Some(20),
                    started_dow: Some(5),
                    is_cinema_context: None,
                },
            )
            .await
            .expect("upsert play_session");
        }

        let written = rebuild_watch_stats_for_account(&pool, account.id)
            .await
            .expect("rebuild watch_stats");
        assert_eq!(written, 1, "exactly one (account, item) aggregate expected");

        let stats = repo::watch_stats::get_watch_stats(&pool, account.id, item.id)
            .await
            .expect("get watch_stats")
            .expect("watch_stats row should now exist");

        assert_eq!(stats.play_count, 3);
        assert_eq!(stats.finished_count, 2);
        assert_eq!(stats.rewatch_count, 1, "2 finishes -> 1 rewatch");
        assert_eq!(stats.total_watched_ms, 120 * 60 * 1000 * 2 + 8 * 60 * 1000);
        assert!(
            !stats.abandoned,
            "a title that was finished at least once is never marked abandoned"
        );
        assert!(stats.first_watched_at.unwrap() < stats.last_watched_at.unwrap());

        // Idempotent: a second rebuild reproduces the identical aggregate.
        let written_again = rebuild_watch_stats_for_account(&pool, account.id)
            .await
            .expect("second rebuild watch_stats");
        assert_eq!(written_again, 1);
        let stats_again = repo::watch_stats::get_watch_stats(&pool, account.id, item.id)
            .await
            .expect("get watch_stats")
            .expect("row still present");
        assert_eq!(stats_again.play_count, 3);
        assert_eq!(stats_again.finished_count, 2);
    }

    /// FIX 2a (live-DB, gated): the watch_stats rebuild is atomic. A failure
    /// mid-transaction (here: a second aggregate referencing a nonexistent
    /// `media_item_id` → FK violation) must roll back the ENTIRE write, leaving
    /// the prior `watch_stats` row untouched — never a half-old/half-new mix
    /// that a subsequent taste recompute would read as authoritative.
    #[tokio::test]
    async fn write_aggregates_rolls_back_on_mid_loop_failure() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!("MUSE_TEST_DATABASE_URL not set — skipping write_aggregates_rolls_back_on_mid_loop_failure");
            return;
        };
        use crate::models::account::NewAccount;
        use crate::models::library::{LibraryKind, NewLibrary};
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use sqlx::postgres::PgPoolOptions;
        use uuid::Uuid;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrate");

        let suffix = Uuid::new_v4().simple().to_string();
        let account = repo::account::create(
            &pool,
            &NewAccount {
                plex_account_id: Some(format!("fix2a-acct-{suffix}")),
                username: Some(format!("fix2a_{suffix}")),
                friendly_name: None,
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("account");
        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("fix2a-lib-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/fix2a".into(),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("lib");
        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("fix2a-tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("FIX2a {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(90),
                year: Some(2020),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("md");
        let item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/media/fix2a/{suffix}"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("item");

        // Prior consistent state: a sentinel watch_stats row.
        repo::watch_stats::upsert_watch_stats(
            &pool,
            &NewWatchStats {
                account_id: account.id,
                media_item_id: item.id,
                play_count: 999,
                finished_count: 7,
                rewatch_count: 6,
                total_watched_ms: 12345,
                avg_percent: Some(0.5),
                last_watched_at: None,
                abandoned: false,
                first_watched_at: None,
            },
        )
        .await
        .expect("seed prior");

        // Two aggregates: the first valid (would overwrite the sentinel), the
        // second referencing a nonexistent media_item_id → FK violation mid-tx.
        let aggregates = vec![
            WatchStatsAgg {
                media_item_id: item.id,
                play_count: 3,
                finished_count: 2,
                total_watched_ms: 500,
                avg_percent: Some(0.9),
                last_watched_at: None,
                first_watched_at: None,
                any_abandoned: false,
            },
            WatchStatsAgg {
                media_item_id: 9_999_999_999,
                play_count: 1,
                finished_count: 0,
                total_watched_ms: 1,
                avg_percent: None,
                last_watched_at: None,
                first_watched_at: None,
                any_abandoned: true,
            },
        ];

        let result = write_aggregates(&pool, account.id, &aggregates).await;
        assert!(result.is_err(), "a mid-loop FK violation must surface as an error");

        // Rolled back: the sentinel row is untouched (the valid first upsert did
        // NOT survive the failed transaction).
        let stats = repo::watch_stats::get_watch_stats(&pool, account.id, item.id)
            .await
            .expect("get")
            .expect("row still present");
        assert_eq!(stats.play_count, 999, "prior watch_stats must be intact after rollback");
        assert_eq!(stats.finished_count, 7);
        assert_eq!(stats.total_watched_ms, 12345);
    }
}
