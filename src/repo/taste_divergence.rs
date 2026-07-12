//! Repo functions for `taste_divergence` (spec §3.7/§4c, MUSE-20) plus the
//! raw weighted-aggregate queries `crate::radar::divergence`'s formulas are
//! built from: account-side and population-side genre/decade weights, the
//! trending "population sample" (distinct resolved trending titles for a
//! region), and per-account watch rows used for the were-early/blind-spot/
//! guilty-pleasure detection.
//!
//! All queries use **runtime** sqlx (never `query!`/`query_as!`), per the
//! MUSE-02 build constraint that the crate must build without a live
//! database.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};
use crate::models::taste_divergence::{NewTasteDivergence, TasteDivergence};

const DIVERGENCE_COLUMNS: &str = "id, account_id, computed_at, genre_index, decade_index, \
    mainstream_score, adventurousness, contrarian_index, were_early, blind_spots, guilty_pleasures";

// --- taste_divergence ------------------------------------------------------

/// Append a new radar snapshot. Like `population_profile`, this is tracked
/// over time (one row per computation) rather than upserted — the radar is
/// supposed to move between runs, not just hold a latest value.
pub async fn insert_divergence(
    pool: &PgPool,
    new: &NewTasteDivergence,
) -> MuseResult<TasteDivergence> {
    let query = format!(
        r#"
        INSERT INTO taste_divergence (
            account_id, genre_index, decade_index, mainstream_score,
            adventurousness, contrarian_index, were_early, blind_spots, guilty_pleasures
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING {DIVERGENCE_COLUMNS}
        "#
    );

    sqlx::query_as::<_, TasteDivergence>(&query)
        .bind(new.account_id)
        .bind(&new.genre_index)
        .bind(&new.decade_index)
        .bind(new.mainstream_score)
        .bind(new.adventurousness)
        .bind(new.contrarian_index)
        .bind(&new.were_early)
        .bind(&new.blind_spots)
        .bind(&new.guilty_pleasures)
        .fetch_one(pool)
        .await
        .map_err(MuseError::Database)
}

/// Most recent radar snapshot for an account, if one has ever been
/// computed.
pub async fn latest_divergence(
    pool: &PgPool,
    account_id: i64,
) -> MuseResult<Option<TasteDivergence>> {
    let query = format!(
        r#"
        SELECT {DIVERGENCE_COLUMNS} FROM taste_divergence
        WHERE account_id = $1
        ORDER BY computed_at DESC
        LIMIT 1
        "#
    );

    sqlx::query_as::<_, TasteDivergence>(&query)
        .bind(account_id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

/// The radar's history for an account, most recent first — lets a
/// downstream consumer (Lumina/Soma) plot "are you drifting mainstream or
/// more niche" over time, per the spec's §4c framing.
pub async fn list_divergence_history(
    pool: &PgPool,
    account_id: i64,
    limit: i64,
) -> MuseResult<Vec<TasteDivergence>> {
    let query = format!(
        r#"
        SELECT {DIVERGENCE_COLUMNS} FROM taste_divergence
        WHERE account_id = $1
        ORDER BY computed_at DESC
        LIMIT $2
        "#
    );

    sqlx::query_as::<_, TasteDivergence>(&query)
        .bind(account_id)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(MuseError::Database)
}

// --- raw weighted rows the `radar::divergence` formulas consume -----------

/// One `(genre, weight)` contribution — shared shape for both the
/// account-side and population-side genre distributions before
/// normalization into shares.
#[derive(Debug, Clone, FromRow)]
pub struct GenreWeight {
    pub genre: String,
    pub weight: f64,
}

/// One `(decade, weight)` contribution (decade = `(year / 10) * 10`).
#[derive(Debug, Clone, FromRow)]
pub struct DecadeWeight {
    pub decade: i32,
    pub weight: f64,
}

/// Account-side genre affinity weights: for every genre the account has any
/// watched title in, `SUM(finished_count + 1.5*rewatch_count -
/// 1.5*abandoned)` floored at 0 (mirrors the spec's own `taste_signals`
/// weighting scale: +1.0 finish, +2.5 rewatch is modeled here as the
/// *marginal* 1.5 on top of the first finish, -1.5 abandon). A title with
/// multiple genres contributes its full weight to each genre it belongs to
/// — this is intentional (see `radar::divergence::normalize`'s doc comment)
/// and mirrors how `population_genre_weights` treats multi-genre titles.
pub async fn account_genre_weights(pool: &PgPool, account_id: i64) -> MuseResult<Vec<GenreWeight>> {
    sqlx::query_as::<_, GenreWeight>(
        r#"
        SELECT g.name AS genre,
               SUM(GREATEST(
                   ws.finished_count + ws.rewatch_count * 1.5
                       - CASE WHEN ws.abandoned THEN 1.5 ELSE 0 END,
                   0
               ))::float8 AS weight
        FROM watch_stats ws
        JOIN media_items mi ON mi.id = ws.media_item_id
        JOIN media_metadata_genres mmg ON mmg.media_metadata_id = mi.media_metadata_id
        JOIN genres g ON g.id = mmg.genre_id
        WHERE ws.account_id = $1
        GROUP BY g.name
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Account-side decade affinity weights — same weighting formula as
/// [`account_genre_weights`], grouped by `(media_metadata.year / 10) * 10`.
/// Titles with no `year` are excluded (can't place them on the decade
/// axis).
pub async fn account_decade_weights(pool: &PgPool, account_id: i64) -> MuseResult<Vec<DecadeWeight>> {
    sqlx::query_as::<_, DecadeWeight>(
        r#"
        SELECT ((mm.year / 10) * 10)::int4 AS decade,
               SUM(GREATEST(
                   ws.finished_count + ws.rewatch_count * 1.5
                       - CASE WHEN ws.abandoned THEN 1.5 ELSE 0 END,
                   0
               ))::float8 AS weight
        FROM watch_stats ws
        JOIN media_items mi ON mi.id = ws.media_item_id
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        WHERE ws.account_id = $1 AND mm.year IS NOT NULL
        GROUP BY decade
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// A distinct resolved trending title currently on record for a region —
/// the "population sample" every population-side computation (genre/decade
/// distribution, blind-spots, were-early, guilty-pleasures) is built from.
#[derive(Debug, Clone, FromRow)]
pub struct PopulationSampleRow {
    pub media_metadata_id: i64,
    pub title: String,
    /// Earliest `trending_snapshots.captured_at` this title was ever seen
    /// trending at — the "trended_at" side of the were-early comparison.
    pub trended_at: DateTime<Utc>,
    pub popularity: Option<f32>,
    /// Best (numerically lowest) rank ever seen for this title.
    pub best_rank: Option<i32>,
}

/// Distinct resolved trending titles for `region`, deduped across every
/// `trending_snapshots` row that resolved to the same `media_metadata_id`
/// (a title seen trending on multiple days/scopes counts once).
pub async fn population_sample(pool: &PgPool, region: &str) -> MuseResult<Vec<PopulationSampleRow>> {
    sqlx::query_as::<_, PopulationSampleRow>(
        r#"
        SELECT ts.media_metadata_id AS media_metadata_id,
               mm.title AS title,
               MIN(ts.captured_at) AS trended_at,
               MAX(ts.popularity) AS popularity,
               MIN(ts.rank) AS best_rank
        FROM trending_snapshots ts
        JOIN media_metadata mm ON mm.id = ts.media_metadata_id
        WHERE ts.region = $1 AND ts.media_metadata_id IS NOT NULL
        GROUP BY ts.media_metadata_id, mm.title
        "#,
    )
    .bind(region)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Population-side genre weights: one weight-1.0 contribution per (sample
/// title, genre) pair. Every distinct trending title (see
/// [`population_sample`], which already dedupes repeated snapshot rows)
/// counts equally regardless of how many days it trended or at what rank —
/// a title with 3 genres contributes 1.0 to each, matching the account-side
/// multi-genre treatment in [`account_genre_weights`].
pub async fn population_genre_weights(pool: &PgPool, region: &str) -> MuseResult<Vec<GenreWeight>> {
    sqlx::query_as::<_, GenreWeight>(
        r#"
        WITH sample AS (
            SELECT DISTINCT ts.media_metadata_id
            FROM trending_snapshots ts
            WHERE ts.region = $1 AND ts.media_metadata_id IS NOT NULL
        )
        SELECT g.name AS genre, COUNT(*)::float8 AS weight
        FROM sample s
        JOIN media_metadata_genres mmg ON mmg.media_metadata_id = s.media_metadata_id
        JOIN genres g ON g.id = mmg.genre_id
        GROUP BY g.name
        "#,
    )
    .bind(region)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Population-side decade weights — same one-per-distinct-title treatment
/// as [`population_genre_weights`], grouped by `(media_metadata.year / 10)
/// * 10`.
pub async fn population_decade_weights(pool: &PgPool, region: &str) -> MuseResult<Vec<DecadeWeight>> {
    sqlx::query_as::<_, DecadeWeight>(
        r#"
        WITH sample AS (
            SELECT DISTINCT ts.media_metadata_id
            FROM trending_snapshots ts
            WHERE ts.region = $1 AND ts.media_metadata_id IS NOT NULL
        )
        SELECT ((mm.year / 10) * 10)::int4 AS decade, COUNT(*)::float8 AS weight
        FROM sample s
        JOIN media_metadata mm ON mm.id = s.media_metadata_id
        WHERE mm.year IS NOT NULL
        GROUP BY decade
        "#,
    )
    .bind(region)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// One title the account has actually watched (`play_count > 0`) — one row
/// per distinct `media_metadata_id`, even if the account has instances of
/// it in multiple libraries. `first_watched_at`/`rewatch_count` take the
/// earliest/highest across those instances respectively. Feeds
/// were-early/blind-spot/guilty-pleasure detection in
/// `radar::divergence`.
#[derive(Debug, Clone, FromRow)]
pub struct AccountWatchRow {
    pub media_metadata_id: i64,
    pub title: String,
    pub first_watched_at: Option<DateTime<Utc>>,
    pub rewatch_count: i32,
}

pub async fn account_watch_rows(pool: &PgPool, account_id: i64) -> MuseResult<Vec<AccountWatchRow>> {
    sqlx::query_as::<_, AccountWatchRow>(
        r#"
        SELECT mi.media_metadata_id AS media_metadata_id,
               mm.title AS title,
               MIN(ws.first_watched_at) AS first_watched_at,
               MAX(ws.rewatch_count) AS rewatch_count
        FROM watch_stats ws
        JOIN media_items mi ON mi.id = ws.media_item_id
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        WHERE ws.account_id = $1 AND ws.play_count > 0
        GROUP BY mi.media_metadata_id, mm.title
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
