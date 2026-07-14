//! MUSET-03: snapshot ingestion pipeline.
//!
//! Read-only, out-of-band snapshots of source systems (Plex library SQLite,
//! Tautulli history SQLite, an *arr SQLite, and a `pg_dump` of the muse
//! Postgres) are loaded into an ISOLATED test Postgres, so muse's own logic
//! (recall/curation/recommend, and any future behavioral test) can run
//! against real-shaped data without the test suite ever holding a live
//! connection to a production system.
//!
//! ## The four pieces, and the one hard safety rule
//! - [`acquisition`] -- AC1: operator-run, out-of-band CODE (a file-copy for
//!   SQLite sources, a parameterized `pg_dump` invocation for Postgres) that
//!   produces a snapshot artifact. Reachable only via the `muse
//!   snapshot-acquire` subcommand (`run_snapshot_acquire_cli` in
//!   `src/main.rs`), gated behind an explicit argv check that returns before
//!   any normal service bootstrap -- it is NEVER part of `muse`'s normal
//!   service startup (which invokes the binary with no arguments) and is
//!   NEVER called by the test suite.
//! - [`normalize`] -- AC2: maps snapshot-shaped records into muse's real
//!   Postgres schema (`repo::*`/`models::*`), so muse's own query/handler
//!   code runs against the loaded snapshot unmodified.
//! - [`load`] -- AC3: the ONE connection path to the isolated snapshot/test
//!   Postgres. Every pool this pipeline hands out is guard-checked first.
//! - [`provenance`] -- AC4: source identity + snapshot vintage timestamp +
//!   checksum, recorded per load, so a test run is reproducible against a
//!   known data vintage.
//! - [`guard`] -- **AC5, the load-bearing safety rule**: NO connection
//!   string used by this pipeline may ever point at a live media DB or the
//!   production muse DB. [`load::connect_snapshot_db`] enforces this
//!   unconditionally via [`guard::validate_snapshot_dsn`] before opening any
//!   pool -- there is no code path in this module that connects without
//!   going through it first.
//!
//! ## AC6: no secrets/PII committed
//! Nothing in this module embeds a real hostname, IP, credential, or piece
//! of real user data. Every DSN/path is env-sourced (S1/S7); every test
//! fixture below uses synthetic, clearly-fake data (`"Test Movie"`,
//! `uuid::Uuid::new_v4()`-suffixed names) -- the same idiom
//! `src/endpoint_tests.rs`'s `db_gated` module already uses throughout this
//! crate.
//!
//! ## Tests read the LOADED snapshot, never the source (AC1/AC3)
//! Nothing under `#[cfg(test)]` in this module (or anywhere else in this
//! crate) opens a connection to a live Plex/Tautulli/*arr instance or the
//! production muse database. The integration test below
//! (`db_gated::snapshot_load_round_trip_is_isolated_and_reproducible`) is
//! gated exactly like `endpoint_tests.rs`'s `db_gated` tests: it skips
//! cleanly when no snapshot/test DSN is configured, and when one IS
//! configured it is validated by the same guard every other entry point
//! uses -- so even a misconfigured `MUSE_SNAPSHOT_DATABASE_URL` can't turn
//! this test into a live-system access.

pub mod acquisition;
pub mod guard;
pub mod load;
pub mod normalize;
pub mod provenance;

#[cfg(test)]
mod db_gated {
    //! Mirrors `crate::endpoint_tests::db_gated`'s `test_pool_or_skip` idiom:
    //! gated on a snapshot/test DSN, skips cleanly (never fails) when unset.

    use uuid::Uuid;

    use crate::snapshot::{load, normalize, provenance};

    async fn snapshot_pool_or_skip(test_name: &str) -> Option<sqlx::PgPool> {
        let Some(database_url) = load::snapshot_database_url_from_env() else {
            eprintln!(
                "{} / {} not set -- skipping {test_name} (expected in the \
                 default test run; the snapshot pipeline does not require a \
                 live DB)",
                load::SNAPSHOT_DATABASE_URL_VAR,
                load::TEST_DATABASE_URL_VAR,
            );
            return None;
        };
        let pool = load::connect_snapshot_db(&database_url)
            .await
            .expect("connect to the configured snapshot/test DSN (guard-checked)");
        load::migrate_snapshot_db(&pool)
            .await
            .expect("migrations should apply cleanly to the isolated snapshot DB");
        Some(pool)
    }

    /// End-to-end: normalize a synthetic "Plex snapshot" library + one media
    /// item, load it into the isolated DB, record provenance for the load,
    /// then read the LOADED data back (never re-touching any "source") and
    /// confirm the provenance record reproduces the load's vintage +
    /// checksum. This is the AC1+AC2+AC3+AC4 round trip in one test.
    #[tokio::test]
    async fn snapshot_load_round_trip_is_isolated_and_reproducible() {
        let Some(pool) =
            snapshot_pool_or_skip("snapshot_load_round_trip_is_isolated_and_reproducible").await
        else {
            return;
        };

        let suffix = Uuid::new_v4().simple().to_string();

        // A synthetic record shaped like what an operator's out-of-band
        // acquisition step would have extracted from a Plex library-SQLite
        // snapshot (see the `normalize` module doc) -- never real user data.
        let raw_library = normalize::RawPlexLibrary {
            name: format!("muset03-snapshot-movies-{suffix}"),
            section_type: 1, // movie
            root_folder: format!("/test/muset03-{suffix}"),
        };
        let raw_item = normalize::RawPlexMediaItem {
            title: format!("MUSET-03 Snapshot Fixture {suffix}"),
            is_show: false,
            year: Some(2022),
            tmdb_id: Some(format!("muset03-tmdb-{suffix}")),
            tvdb_id: None,
            file_path: format!("/test/muset03-{suffix}/fixture.mkv"),
            rating_key: format!("muset03-rk-{suffix}"),
        };

        let loaded = normalize::load_plex_library_snapshot(
            &pool,
            &raw_library,
            std::slice::from_ref(&raw_item),
        )
        .await
        .expect("loading the normalized snapshot fixture should succeed");

        assert_eq!(loaded.library.name, raw_library.name);
        assert_eq!(loaded.media_items.len(), 1);
        assert_eq!(loaded.media_items[0].0.title, raw_item.title);

        // Record provenance for this load: a synthetic vintage timestamp +
        // a checksum over some stand-in "artifact bytes" (in real operator
        // use this is `provenance::checksum_file` over the copied SQLite
        // file -- see `acquisition::copy_sqlite_snapshot`).
        let checksum = provenance::checksum_bytes(raw_library.name.as_bytes());
        let record = provenance::record(
            &pool,
            &provenance::NewSnapshotProvenance {
                source_identity: "plex-library-sqlite".to_string(),
                source_kind: provenance::SnapshotSourceKind::PlexSqlite,
                snapshot_taken_at: chrono::Utc::now(),
                checksum_sha256: checksum.clone(),
                notes: Some(format!("MUSET-03 round-trip test fixture {suffix}")),
            },
        )
        .await
        .expect("recording provenance should succeed");

        assert_eq!(record.checksum_sha256, checksum);
        assert_eq!(record.source_kind, "plex-library-sqlite");

        // Read the LOADED data back -- this is the whole point of AC1/AC3:
        // the assertions above and below never touched a live Plex/Tautulli/
        // *arr instance or the production muse DB, only the isolated
        // snapshot/test Postgres this test connected to via the guard.
        let fetched_library = crate::repo::library::get(&pool, loaded.library.id)
            .await
            .expect("re-fetching the loaded library should succeed");
        assert_eq!(fetched_library.id, loaded.library.id);

        let provenance_rows = provenance::list(&pool)
            .await
            .expect("listing provenance should succeed");
        assert!(
            provenance_rows.iter().any(|r| r.id == record.id),
            "the recorded provenance row must be retrievable via list()"
        );

        // Cleanup -- same posture as every other db_gated test in this
        // crate: leave the scratch DB as clean as we found it.
        sqlx::query("DELETE FROM media_items WHERE id = $1")
            .bind(loaded.media_items[0].1.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM media_metadata WHERE id = $1")
            .bind(loaded.media_items[0].0.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM libraries WHERE id = $1")
            .bind(loaded.library.id)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM snapshot_provenance WHERE id = $1")
            .bind(record.id)
            .execute(&pool)
            .await
            .ok();
    }

    /// A Tautulli history-snapshot round trip: normalize a synthetic record,
    /// load it as a `play_events` row, and confirm it is tagged
    /// distinguishably from a live-ingested event (`source =
    /// "snapshot:tautulli"`).
    #[tokio::test]
    async fn tautulli_history_snapshot_loads_and_is_tagged_as_snapshot_derived() {
        let Some(pool) = snapshot_pool_or_skip(
            "tautulli_history_snapshot_loads_and_is_tagged_as_snapshot_derived",
        )
        .await
        else {
            return;
        };

        let suffix = Uuid::new_v4().simple().to_string();
        let raw = normalize::RawTautulliPlayRecord {
            rating_key: Some(format!("muset03-tautulli-rk-{suffix}")),
            user: Some(format!("muset03-user-{suffix}")),
            view_offset_ms: Some(42_000),
            player: Some("muset03-test-player".to_string()),
            platform: Some("muset03-test-platform".to_string()),
            raw: serde_json::json!({"fixture": "muset03", "suffix": suffix}),
        };

        let inserted = normalize::load_tautulli_history_snapshot(&pool, std::slice::from_ref(&raw))
            .await
            .expect("loading the tautulli snapshot fixture should succeed");
        assert_eq!(inserted, 1);

        let recent = crate::repo::play_event::list_recent(&pool, 50)
            .await
            .expect("listing recent play_events should succeed");
        let found = recent
            .iter()
            .find(|e| e.rating_key.as_deref() == raw.rating_key.as_deref())
            .expect("the loaded snapshot play_event should be retrievable");
        assert_eq!(found.source, "snapshot:tautulli");
        assert_eq!(found.event_type, "snapshot.history");

        sqlx::query("DELETE FROM play_events WHERE id = $1")
            .bind(found.id)
            .execute(&pool)
            .await
            .ok();
    }
}
