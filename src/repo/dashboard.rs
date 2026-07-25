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
