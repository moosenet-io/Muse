//! Repo functions for the trending/population feed (MUSE-19, spec §3.7).
//!
//! All queries use **runtime** sqlx per the MUSE-02 build constraint (never
//! `query!`/`query_as!`). `population_profile` queries list explicit
//! columns rather than `SELECT *` so they never touch the `mainstream_centroid`
//! `vector(768)` column — this crate has no `pgvector` decode support yet
//! (see `models::trending::PopulationProfile`).

use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};
use crate::models::media_metadata::MediaKind;
use crate::models::trending::{
    NewPopulationProfile, NewStreamingAvailability, NewTrendingSnapshot, PopulationProfile,
    StreamingAvailability, TrendingSnapshot,
};

const POPULATION_PROFILE_COLUMNS: &str =
    "id, \"window\", region, genre_distribution, decade_distribution, runtime_distribution, sample_size, computed_at";

/// Append one trending/popular entry. `trending_snapshots` is an append-only
/// rolling log (like `play_events`) — each ingest run writes a fresh
/// `captured_at`, so this is a plain insert, not an upsert. The table's own
/// `UNIQUE (source, scope, platform, region, window, rank, captured_at)`
/// constraint only guards against a genuinely simultaneous duplicate write;
/// on that vanishingly unlikely conflict we return the existing row rather
/// than erroring the whole ingest run.
pub async fn insert_snapshot(
    pool: &PgPool,
    new: &NewTrendingSnapshot,
) -> MuseResult<TrendingSnapshot> {
    let inserted = sqlx::query_as::<_, TrendingSnapshot>(
        r#"
        INSERT INTO trending_snapshots (
            source, scope, platform, region, "window", rank, media_metadata_id,
            external_ref, popularity
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (source, scope, platform, region, "window", rank, captured_at) DO NOTHING
        RETURNING *
        "#,
    )
    .bind(&new.source)
    .bind(&new.scope)
    .bind(&new.platform)
    .bind(&new.region)
    .bind(&new.window)
    .bind(new.rank)
    .bind(new.media_metadata_id)
    .bind(&new.external_ref)
    .bind(new.popularity)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?;

    if let Some(row) = inserted {
        return Ok(row);
    }

    // Conflict path: another write landed on the exact same
    // (source, scope, platform, region, window, rank, captured_at) tuple in
    // the same instant. Return the row that won rather than erroring.
    sqlx::query_as::<_, TrendingSnapshot>(
        r#"
        SELECT * FROM trending_snapshots
        WHERE source = $1 AND scope = $2
          AND platform IS NOT DISTINCT FROM $3
          AND region = $4 AND "window" = $5
          AND rank IS NOT DISTINCT FROM $6
        ORDER BY captured_at DESC
        LIMIT 1
        "#,
    )
    .bind(&new.source)
    .bind(&new.scope)
    .bind(&new.platform)
    .bind(&new.region)
    .bind(&new.window)
    .bind(new.rank)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)?
    .ok_or_else(|| MuseError::Conflict("trending_snapshots insert raced but no row found on retry lookup".to_string()))
}

pub async fn list_recent(
    pool: &PgPool,
    scope: &str,
    region: &str,
    limit: i64,
) -> MuseResult<Vec<TrendingSnapshot>> {
    sqlx::query_as::<_, TrendingSnapshot>(
        r#"
        SELECT * FROM trending_snapshots
        WHERE scope = $1 AND region = $2
        ORDER BY captured_at DESC, rank ASC NULLS LAST
        LIMIT $3
        "#,
    )
    .bind(scope)
    .bind(region)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Rows currently on record for a region — the placeholder `sample_size`
/// MUSE-19 writes into `population_profile` until MUSE-20's real
/// distribution math lands (see `trending::compute_population_profile`).
pub async fn count_recent_snapshots(pool: &PgPool, region: &str) -> MuseResult<i64> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM trending_snapshots WHERE region = $1")
        .bind(region)
        .fetch_one(pool)
        .await
        .map_err(MuseError::Database)
}

/// One trending title that resolved to a known `media_metadata` row (i.e.
/// its catalog entry exists — a title Muse has metadata for) but has no
/// `media_items` row at all (i.e. it isn't owned in any library). This is
/// MUSE-11's not-in-library candidate source: "you'd love X" picks the
/// curation engine can reason about, joined against MUSE-16 `availability`
/// by the caller to decide whether the pick is grabbable right now.
#[derive(Debug, Clone, FromRow)]
pub struct TrendingNotInLibraryRow {
    pub media_metadata_id: i64,
    pub title: String,
    pub year: Option<i32>,
    pub kind: MediaKind,
    pub popularity: Option<f32>,
}

/// MUSE-11: the most recent trending snapshot per not-in-library resolved
/// title for `region`, highest popularity first. Deduplicated with an inner
/// `DISTINCT ON` (one row per `media_metadata.id`, keeping its
/// highest-popularity observed snapshot) before the outer popularity sort +
/// `LIMIT`, so a title that's appeared in multiple trending scopes/windows
/// doesn't crowd out distinct titles.
pub async fn list_trending_not_in_library(
    pool: &PgPool,
    region: &str,
    limit: i64,
) -> MuseResult<Vec<TrendingNotInLibraryRow>> {
    sqlx::query_as::<_, TrendingNotInLibraryRow>(
        r#"
        SELECT * FROM (
            SELECT DISTINCT ON (mm.id)
                mm.id AS media_metadata_id,
                mm.title AS title,
                mm.year AS year,
                mm.kind AS kind,
                ts.popularity AS popularity
            FROM trending_snapshots ts
            JOIN media_metadata mm ON mm.id = ts.media_metadata_id
            WHERE ts.region = $1
              AND NOT EXISTS (SELECT 1 FROM media_items mi WHERE mi.media_metadata_id = mm.id)
            ORDER BY mm.id, ts.popularity DESC NULLS LAST
        ) deduped
        ORDER BY popularity DESC NULLS LAST
        LIMIT $2
        "#,
    )
    .bind(region)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Upsert (per resolved title, provider, region, offer type) streaming
/// availability, matching the spec's composite primary key — a re-ingest
/// refreshes `link`/`seen_at` rather than duplicating rows.
pub async fn upsert_streaming_availability(
    pool: &PgPool,
    new: &NewStreamingAvailability,
) -> MuseResult<StreamingAvailability> {
    sqlx::query_as::<_, StreamingAvailability>(
        r#"
        INSERT INTO streaming_availability (media_metadata_id, provider, region, offer_type, link)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (media_metadata_id, provider, region, offer_type) DO UPDATE SET
            link = EXCLUDED.link,
            seen_at = now()
        RETURNING *
        "#,
    )
    .bind(new.media_metadata_id)
    .bind(&new.provider)
    .bind(&new.region)
    .bind(&new.offer_type)
    .bind(&new.link)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_streaming_availability(
    pool: &PgPool,
    media_metadata_id: i64,
) -> MuseResult<Vec<StreamingAvailability>> {
    sqlx::query_as::<_, StreamingAvailability>(
        "SELECT * FROM streaming_availability WHERE media_metadata_id = $1 ORDER BY provider, offer_type",
    )
    .bind(media_metadata_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Append a `population_profile` rollup row. Like `taste_divergence`, this
/// is tracked over time (one row per computation) rather than upserted, so
/// a later consumer (MUSE-20) can see the corpus move between runs.
pub async fn insert_population_profile(
    pool: &PgPool,
    new: &NewPopulationProfile,
) -> MuseResult<PopulationProfile> {
    let query = format!(
        r#"
        INSERT INTO population_profile (
            "window", region, genre_distribution, decade_distribution, runtime_distribution, sample_size
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING {POPULATION_PROFILE_COLUMNS}
        "#
    );

    sqlx::query_as::<_, PopulationProfile>(&query)
        .bind(&new.window)
        .bind(&new.region)
        .bind(&new.genre_distribution)
        .bind(&new.decade_distribution)
        .bind(&new.runtime_distribution)
        .bind(new.sample_size)
        .fetch_one(pool)
        .await
        .map_err(MuseError::Database)
}

pub async fn latest_population_profile(
    pool: &PgPool,
    window: &str,
    region: &str,
) -> MuseResult<Option<PopulationProfile>> {
    let query = format!(
        r#"
        SELECT {POPULATION_PROFILE_COLUMNS} FROM population_profile
        WHERE "window" = $1 AND region = $2
        ORDER BY computed_at DESC
        LIMIT 1
        "#
    );

    sqlx::query_as::<_, PopulationProfile>(&query)
        .bind(window)
        .bind(region)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}
