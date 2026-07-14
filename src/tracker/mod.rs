//! MUSE-07: the native Plex tracker — THE Tautulli replacement (spec §4).
//!
//! Three cooperating pieces, one append-only source of truth:
//!
//! - [`webhook`] — (A) `POST /ingest/plex-webhook`: Plex Pass event push.
//! - [`poller`] — (B) a background `/status/sessions` poll loop: fills the
//!   gaps the webhook misses (or is the *only* path when Plex Pass/webhooks
//!   aren't configured at all).
//! - [`reconstruct`] — (C) folds the raw `play_events` stream (written by
//!   both A and B) into a single, idempotent, late-event-tolerant
//!   `play_sessions` row per session_key.
//!
//! Both ingest paths funnel through the same `play_events` table and the
//! same [`reconstruct::reconstruct_and_persist`] — there is exactly one
//! reconstruction algorithm, not one per source.

pub mod poller;
// MUSET-08: widened from a bare `mod` to `pub(crate)` so the shadow runner
// (`crate::shadow`) can reuse the real fold/resolve analytics
// (`fold_events`, `resolve_rating_key`) instead of reimplementing them.
// Still crate-private -- no new public API surface outside this crate.
pub(crate) mod reconstruct;
pub mod webhook;

#[cfg(test)]
mod live_db_tests {
    //! MUSE-07 live-DB round-trip: persist a raw `play_events` stream (as
    //! the webhook/poller would) and confirm `reconstruct_and_persist`
    //! produces a correct, idempotent, late-event-tolerant `play_sessions`
    //! row. Gated on `MUSE_TEST_DATABASE_URL`, same skip-when-unset pattern
    //! as `src/integration_tests.rs` — this crate's suite must pass with no
    //! live database.

    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use crate::models::account::NewAccount;
    use crate::models::library::{LibraryKind, NewLibrary};
    use crate::models::media_item::NewMediaItem;
    use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
    use crate::models::play_event::NewPlayEvent;
    use crate::repo;

    use super::reconstruct;

    #[tokio::test]
    async fn persist_and_reconstruct_round_trip_is_idempotent_and_late_tolerant() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 persist_and_reconstruct_round_trip_is_idempotent_and_late_tolerant \
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
        let plex_account_id = format!("plex-{suffix}");
        let rating_key = format!("rk-{suffix}");
        let session_key = format!("sess-{suffix}");

        // --- fixtures: a resolvable account + media item, exactly the race
        // reconstruction is built to tolerate (events can arrive before or
        // after these exist). ---
        let account = repo::account::upsert_by_plex_account_id(
            &pool,
            &NewAccount {
                plex_account_id: Some(plex_account_id.clone()),
                is_home_user: true,
                ..Default::default()
            },
        )
        .await
        .expect("upsert account");

        let library = repo::library::create(
            &pool,
            &NewLibrary {
                name: format!("tracker_test_{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: "/media/Movies/".to_string(),
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
                tmdb_id: Some(format!("tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: "Tracker Test Movie".to_string(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(100),
                year: Some(2024),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        let media_item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: "/media/Movies/Tracker Test Movie (2024)".to_string(),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(rating_key.clone()),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        // --- simulate the raw event stream: play -> pause -> resume -> stop,
        // exactly what the webhook/poller would persist via
        // `repo::play_event::insert`. ---
        // Timestamps are DB-assigned (`received_at DEFAULT now()`) — each
        // `insert` below runs strictly after the previous one completes, so
        // `received_at` naturally increases in the same order, same as a
        // real webhook delivery sequence.
        let mk = |event_type: &str, offset_ms: i64| NewPlayEvent {
            source: "plex_webhook".to_string(),
            event_type: event_type.to_string(),
            account_ref: Some(plex_account_id.clone()),
            session_key: Some(session_key.clone()),
            rating_key: Some(rating_key.clone()),
            view_offset_ms: Some(offset_ms),
            player: Some("Living Room TV".to_string()),
            platform: Some("Plex for Android (TV)".to_string()),
            product: None,
            device: None,
            ip_address: None,
            raw: serde_json::json!({"event": event_type, "duration": 6_000_000}),
        };

        repo::play_event::insert(&pool, &mk("media.play", 0))
            .await
            .expect("insert play")
            .expect("first delivery must not dedup");
        repo::play_event::insert(&pool, &mk("media.pause", 1_000_000))
            .await
            .expect("insert pause")
            .expect("distinct offset must not dedup");

        // Reconstruct mid-stream: session should exist, still open (no
        // stopped_at), not finished.
        let mid = reconstruct::reconstruct_and_persist(&pool, &session_key)
            .await
            .expect("reconstruct mid-stream")
            .expect("account + media are resolvable; a session must be persisted");
        assert_eq!(mid.account_id, Some(account.id));
        assert_eq!(mid.media_item_id, Some(media_item.id));
        assert!(mid.stopped_at.is_none());
        assert!(!mid.is_finished);
        assert_eq!(mid.paused_counter, 1);

        // Re-running reconstruction with no new events must converge to the
        // SAME row (idempotent), not insert a duplicate.
        let mid_again = reconstruct::reconstruct_and_persist(&pool, &session_key)
            .await
            .expect("reconstruct again")
            .expect("still resolvable");
        assert_eq!(mid.id, mid_again.id, "idempotent re-run must update the same play_sessions row");

        // "Resume" arrives, then "stop" near the end -- and, simulating a
        // LATE delivery, a duplicate/older event shows up in the stream
        // after the stop already landed. Reconstruction always re-folds the
        // full current event set, so late arrival order must not matter.
        repo::play_event::insert(&pool, &mk("media.resume", 1_000_000))
            .await
            .expect("insert resume")
            .expect("distinct event_type at same offset must not dedup");
        repo::play_event::insert(&pool, &mk("media.stop", 5_700_000))
            .await
            .expect("insert stop")
            .expect("distinct offset must not dedup");

        let finished = reconstruct::reconstruct_and_persist(&pool, &session_key)
            .await
            .expect("reconstruct after stop")
            .expect("still resolvable");
        assert_eq!(finished.id, mid.id, "same session_key must keep resolving to the same row");
        assert!(finished.stopped_at.is_some());
        assert!(finished.is_finished, "95% watched must be finished");
        assert!(!finished.is_abandoned);

        // Re-running again (simulating a late-arriving already-seen event
        // being re-processed) must still converge to the identical result.
        let finished_again = reconstruct::reconstruct_and_persist(&pool, &session_key)
            .await
            .expect("reconstruct once more")
            .expect("still resolvable");
        assert_eq!(finished.watched_ms, finished_again.watched_ms);
        assert_eq!(finished.percent_complete, finished_again.percent_complete);
        assert_eq!(finished.paused_counter, finished_again.paused_counter);

        // The raw stream itself is fully preserved (append-only, never
        // mutated by reconstruction).
        let events = repo::play_event::list_for_session(&pool, &session_key)
            .await
            .expect("list events for session");
        assert_eq!(events.len(), 4);
    }
}
