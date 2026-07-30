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

pub async fn on_deck(pool: &PgPool, account_id: i64, limit: i64) -> MuseResult<Vec<OnDeckRow>> {
    sqlx::query_as::<_, OnDeckRow>(
        r#"
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
              -- `progress_pct_excludes_a_fully_watched_fraction`.
              AND ps.percent_complete > 0
              AND ps.percent_complete < 1
            ORDER BY ps.media_item_id, ps.started_at DESC
        ) newest
        ORDER BY newest.started_at DESC
        LIMIT $2
        "#,
    )
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
