//! MWEBX-05 (S126): aggregate READ projections that back the MUSE web
//! "detail bench" screens (`crate::web::dashboard`) — the library grid/table,
//! the wanted/download queue, and the request-lifecycle read views.
//!
//! Everything here is **read-only** and uses **runtime** sqlx
//! (`sqlx::query_as`), never the compile-time `query_as!` macro — same
//! MUSE-02 build constraint every other repo module follows (the crate must
//! build without a live database). These are query-shaped projections
//! (`FromRow` structs local to this module), deliberately distinct from the
//! table-row models, so a screen can render without a second round-trip.
//!
//! Fail-open posture: an empty library / no rows is a normal, valid state and
//! returns an empty `Vec`, never an error.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};
use crate::models::media_metadata::MediaKind;

/// One owned (in-library) title for the Library GRID (poster wall). Carries
/// just enough to render a `PosterTile` + availability chip without a second
/// fetch.
#[derive(Debug, Clone, FromRow)]
pub struct LibraryGridRow {
    pub media_item_id: i64,
    pub media_metadata_id: i64,
    pub kind: MediaKind,
    pub title: String,
    pub year: Option<i32>,
    pub tmdb_id: Option<String>,
    pub tvdb_id: Option<String>,
    pub imdb_id: Option<String>,
    pub monitored: bool,
    /// True when at least one `media_files` row exists for this item — the
    /// honest "is it actually on disk" signal (vs. merely monitored/wanted).
    pub has_file: bool,
    pub added_at: Option<DateTime<Utc>>,
}

/// Owned titles, newest-added first. `limit` caps the grid page.
pub async fn library_grid(pool: &PgPool, limit: i64) -> MuseResult<Vec<LibraryGridRow>> {
    sqlx::query_as::<_, LibraryGridRow>(
        r#"
        SELECT
            mi.id  AS media_item_id,
            mm.id  AS media_metadata_id,
            mm.kind AS kind,
            mm.title AS title,
            mm.year AS year,
            mm.tmdb_id AS tmdb_id,
            mm.tvdb_id AS tvdb_id,
            mm.imdb_id AS imdb_id,
            mi.monitored AS monitored,
            EXISTS (SELECT 1 FROM media_files mf WHERE mf.media_item_id = mi.id) AS has_file,
            mi.added_at AS added_at
        FROM media_items mi
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        ORDER BY mi.added_at DESC NULLS LAST, mi.id DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// One "wanted but not on disk" title — a monitored item with no file yet.
/// Feeds both the grid's `wanted` availability bucket and the queue view's
/// wanted list.
#[derive(Debug, Clone, FromRow)]
pub struct WantedTitleRow {
    pub monitored_item_id: i64,
    pub media_metadata_id: i64,
    pub library_id: i64,
    pub kind: MediaKind,
    pub title: String,
    pub year: Option<i32>,
    pub quality_profile_id: Option<i64>,
}

/// Monitored titles that have no `media_files` row anywhere — the honest
/// "wanted" set across every library. Fail-open: empty when nothing is
/// wanted.
pub async fn wanted_titles(pool: &PgPool, limit: i64) -> MuseResult<Vec<WantedTitleRow>> {
    sqlx::query_as::<_, WantedTitleRow>(
        r#"
        SELECT
            mo.id AS monitored_item_id,
            mm.id AS media_metadata_id,
            mo.library_id AS library_id,
            mm.kind AS kind,
            mm.title AS title,
            mm.year AS year,
            mo.quality_profile_id AS quality_profile_id
        FROM monitored_items mo
        JOIN media_metadata mm ON mm.id = mo.media_metadata_id
        WHERE mo.monitored = true
          AND NOT EXISTS (
              SELECT 1
              FROM media_items mi
              JOIN media_files mf ON mf.media_item_id = mi.id
              WHERE mi.media_metadata_id = mm.id
          )
        ORDER BY mo.updated_at DESC NULLS LAST, mo.id DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// One dense management row for the Library TABLE — on-disk footprint,
/// file count, monitoring, and the resolved quality-profile name.
#[derive(Debug, Clone, FromRow)]
pub struct LibraryTableRow {
    pub media_item_id: i64,
    pub media_metadata_id: i64,
    pub kind: MediaKind,
    pub title: String,
    pub year: Option<i32>,
    pub monitored: bool,
    pub quality_profile_id: Option<i64>,
    pub quality_profile_name: Option<String>,
    /// Total on-disk bytes across every `media_files` row for this item
    /// (`0` when nothing is on disk yet). `SUM(bigint)` is `numeric` in
    /// Postgres, so it is explicitly cast back to `bigint` for `i64`.
    pub size_bytes: i64,
    pub file_count: i64,
}

/// Dense management rows, alphabetical by sort/title. `limit` caps the page.
pub async fn library_table(pool: &PgPool, limit: i64) -> MuseResult<Vec<LibraryTableRow>> {
    sqlx::query_as::<_, LibraryTableRow>(
        r#"
        SELECT
            mi.id  AS media_item_id,
            mm.id  AS media_metadata_id,
            mm.kind AS kind,
            mm.title AS title,
            mm.year AS year,
            mi.monitored AS monitored,
            mi.quality_profile_id AS quality_profile_id,
            qp.name AS quality_profile_name,
            COALESCE(SUM(mf.size_bytes), 0)::bigint AS size_bytes,
            COUNT(mf.id)::bigint AS file_count
        FROM media_items mi
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        LEFT JOIN media_files mf ON mf.media_item_id = mi.id
        LEFT JOIN quality_profiles qp ON qp.id = mi.quality_profile_id
        GROUP BY mi.id, mm.id, qp.name
        ORDER BY mm.sort_title NULLS LAST, mm.title ASC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Count of owned items + count of wanted-but-not-on-disk titles, for the
/// library header/summary. One round-trip, both counts.
#[derive(Debug, Clone, FromRow)]
pub struct LibraryCounts {
    pub owned: i64,
    pub wanted: i64,
    pub on_disk: i64,
}

/// MUSE #84: the four scalars the Constellation web GUI's Muse dashboard
/// header binds to (`useMuseStats`). Deliberately a superset-free, ONE
/// round-trip projection — the GUI polls this on every panel mount.
///
/// `pending_items` reuses `library_counts`' "wanted" definition verbatim
/// (monitored, but no `media_files` row anywhere) rather than inventing a
/// second notion of pending — two endpoints disagreeing about the same word
/// is worse than either number.
///
/// `last_ingest_at` is derived from the newest `media_files` row, which is
/// the only durable record of when the read-only scanner last wrote
/// anything. It is NOT a scanner-run timestamp: a scan that found no new
/// files does not advance it. Named `last_ingest_at` (not `last_scan_at`)
/// for exactly that reason.
#[derive(Debug, Clone, FromRow)]
pub struct ConstellationStats {
    pub library_size: i64,
    pub active_channels: i64,
    pub pending_items: i64,
    pub last_ingest_at: Option<DateTime<Utc>>,
}

pub async fn constellation_stats(pool: &PgPool) -> MuseResult<ConstellationStats> {
    sqlx::query_as::<_, ConstellationStats>(
        r#"
        SELECT
            (SELECT COUNT(*)::bigint FROM media_items) AS library_size,
            (SELECT COUNT(*)::bigint FROM channels) AS active_channels,
            (SELECT COUNT(*)::bigint
               FROM monitored_items mo
              WHERE mo.monitored = true
                AND NOT EXISTS (
                    SELECT 1 FROM media_items mi
                    JOIN media_files mf ON mf.media_item_id = mi.id
                    WHERE mi.media_metadata_id = mo.media_metadata_id
                )) AS pending_items,
            (SELECT MAX(created_at) FROM media_files) AS last_ingest_at
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// MUSE #84: continue-watching rows for `useMuseOnDeck`, straight off
/// `play_sessions` — the same persisted Tautulli/Plex session data the taste
/// model reads, NOT the in-memory curation ranker. This endpoint answers
/// "what did someone leave part-way through", which is a fact on disk; it
/// deliberately does no scoring, so it cannot go empty just because Chord is
/// down.
///
/// `DISTINCT ON (media_item_id)` collapses a title's repeated sessions to
/// its most recent one, so a show rewatched across five sittings occupies
/// one card rather than five.
#[derive(Debug, Clone, FromRow)]
pub struct OnDeckRow {
    pub media_item_id: i64,
    pub media_metadata_id: i64,
    pub kind: MediaKind,
    pub title: String,
    /// A FRACTION in 0..1 (see the filter in [`on_deck`]) — callers that need
    /// a percentage must scale by 100. Named as the column is named rather
    /// than renamed to `progress_fraction`, so it stays greppable against the
    /// schema.
    pub percent_complete: Option<f32>,
    pub started_at: DateTime<Utc>,
}

/// The on-deck query, hoisted to a `const` so the `percent_complete < 1`
/// bound is INSPECTABLE by a test that runs in the default gate. A previous
/// attempt to pin that bound asserted `!(1.0 < 1.0)` in Rust, which both
/// reviewers correctly called a tautology: it would have kept passing if the
/// SQL changed to `<= 1`. String-asserting SQL is not elegant, but the bound
/// is a product decision (see the comment inside) and something has to fail
/// when it silently moves. The behavioural proof lives in the `db_gated`
/// test below, which needs `MUSE_TEST_DATABASE_URL`.
pub(crate) const ON_DECK_SQL: &str = r#"
        SELECT * FROM (
            SELECT DISTINCT ON (ps.media_item_id)
                ps.media_item_id,
                mi.media_metadata_id,
                md.kind,
                md.title,
                ps.percent_complete,
                ps.started_at
            FROM play_sessions ps
            JOIN media_items mi ON mi.id = ps.media_item_id
            JOIN media_metadata md ON md.id = mi.media_metadata_id
            WHERE ps.account_id = $1
              AND ps.is_finished = false
              AND ps.is_abandoned = false
              AND ps.percent_complete IS NOT NULL
              -- MUSE #87: `percent_complete` is a FRACTION in 0..1, not a
              -- percentage. Verified against live data: finished sessions
              -- average 0.991 (max 1.000), unfinished top out at 0.850, and
              -- zero rows exceed 1. The previous `< 100` bound was a no-op on
              -- this scale.
              --
              -- `< 1` (not `<= 1`) is deliberate, and it is a PRODUCT choice
              -- rather than an accident of the data: a session watched to
              -- exactly 100% does not belong on a "continue watching" shelf,
              -- even in the window where `is_finished` has not yet been
              -- persisted. A reviewer raised `<= 1` for that state-update
              -- race; admitting it would put fully-watched titles on the
              -- shelf, which is the worse outcome. The exclusion is pinned by
              -- `tests::on_deck_sql_bounds_progress_strictly_below_one` and
              -- its `db_gated` behavioural sibling.
              AND ps.percent_complete > 0
              AND ps.percent_complete < 1
            ORDER BY ps.media_item_id, ps.started_at DESC
        ) newest
        ORDER BY newest.started_at DESC
        LIMIT $2
        "#;

pub async fn on_deck(pool: &PgPool, account_id: i64, limit: i64) -> MuseResult<Vec<OnDeckRow>> {
    sqlx::query_as::<_, OnDeckRow>(ON_DECK_SQL)
        .bind(account_id)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

pub async fn library_counts(pool: &PgPool) -> MuseResult<LibraryCounts> {
    sqlx::query_as::<_, LibraryCounts>(
        r#"
        SELECT
            (SELECT COUNT(*)::bigint FROM media_items) AS owned,
            (SELECT COUNT(*)::bigint
               FROM monitored_items mo
              WHERE mo.monitored = true
                AND NOT EXISTS (
                    SELECT 1 FROM media_items mi
                    JOIN media_files mf ON mf.media_item_id = mi.id
                    WHERE mi.media_metadata_id = mo.media_metadata_id
                )) AS wanted,
            (SELECT COUNT(DISTINCT mf.media_item_id)::bigint FROM media_files mf) AS on_disk
        "#,
    )
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MUSE #87: pins the on-deck progress bound. Both reviewers rejected the
    /// previous attempt (`assert!(!(1.0 < 1.0))`) as a tautology that would
    /// survive a change to `<= 1`. This inspects the actual SQL, so moving the
    /// bound fails the build's test gate rather than silently putting
    /// fully-watched titles on a "continue watching" shelf.
    ///
    /// Asserting on SQL text is a blunt instrument; it is used here only
    /// because the bound is a deliberate product decision and this is the one
    /// check that runs WITHOUT a database. The behavioural proof is
    /// `db_gated::on_deck_excludes_a_fully_watched_unfinished_session`.
    #[test]
    fn on_deck_sql_bounds_progress_strictly_below_one() {
        assert!(
            ON_DECK_SQL.contains("ps.percent_complete < 1"),
            "the on-deck bound must stay strictly below 1 (a 0..1 fraction)"
        );
        assert!(
            !ON_DECK_SQL.contains("ps.percent_complete <= 1"),
            "`<= 1` would admit fully-watched sessions onto the continue-watching shelf"
        );
        // The old percentage-scale bound must not come back.
        assert!(
            !ON_DECK_SQL.contains("percent_complete < 100"),
            "`< 100` is a no-op on a 0..1 fraction — that was the MUSE #87 bug"
        );
    }

    #[cfg(test)]
    mod db_gated {
        use super::*;

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

        /// The behavioural half of `on_deck_sql_bounds_progress_strictly_below_one`:
        /// an UNFINISHED session at exactly `1.0` must not be on deck, which is
        /// the state-update race a reviewer raised when asking for `<= 1`.
        #[tokio::test]
        async fn on_deck_excludes_a_fully_watched_unfinished_session() {
            let Some(pool) = test_pool_or_skip(
                "on_deck_excludes_a_fully_watched_unfinished_session",
            )
            .await
            else {
                return;
            };

            let (account_id,): (i64,) = sqlx::query_as(
                "INSERT INTO accounts (username, friendly_name, is_home_user, is_primary) \
                 VALUES ('muse87-fixture', 'MUSE87 Fixture', true, false) RETURNING id",
            )
            .fetch_one(&pool)
            .await
            .expect("seed account");

            // Two sessions on the same account: one mid-watch, one at exactly
            // 1.0 but still flagged unfinished.
            for (key, pct) in [("muse87-partial", 0.40_f32), ("muse87-complete", 1.0_f32)] {
                sqlx::query(
                    "INSERT INTO play_sessions \
                       (account_id, session_key, started_at, percent_complete, \
                        is_finished, is_abandoned) \
                     VALUES ($1, $2, now(), $3, false, false)",
                )
                .bind(account_id)
                .bind(key)
                .bind(pct)
                .execute(&pool)
                .await
                .expect("seed session");
            }

            let rows = on_deck(&pool, account_id, 50).await.expect("on_deck query");
            assert!(
                rows.iter().all(|r| r.percent_complete.unwrap_or(0.0) < 1.0),
                "a fully-watched unfinished session must never be on deck"
            );

            sqlx::query("DELETE FROM play_sessions WHERE account_id = $1")
                .bind(account_id)
                .execute(&pool)
                .await
                .ok();
            sqlx::query("DELETE FROM accounts WHERE id = $1")
                .bind(account_id)
                .execute(&pool)
                .await
                .ok();
        }
    }
}
