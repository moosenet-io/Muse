//! Repo functions for `taste_profile` / `taste_context_centroids` /
//! `taste_signals` (spec §3.4), plus the raw weighted-aggregate queries
//! MUSE-10's `taste_model` builds its genre/person/decade/keyword affinity
//! formulas from (mirrors the shape of `repo::taste_divergence`'s raw rows
//! for the radar formulas — a DB-facing row struct here, pure Rust
//! aggregation in `crate::taste_model`).

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

use crate::error::{MuseError, MuseResult};
use crate::models::taste::{
    NewTasteContextCentroid, NewTasteProfile, NewTasteSignal, TasteContextCentroid, TasteProfile,
    TasteSignal,
};

// --- taste_profile -------------------------------------------------------

/// Full-replace upsert — the taste-recompute worker always writes the
/// complete recomputed profile for an account.
pub async fn upsert_profile(pool: &PgPool, new: &NewTasteProfile) -> MuseResult<TasteProfile> {
    sqlx::query_as::<_, TasteProfile>(
        r#"
        INSERT INTO taste_profile (
            account_id, genre_affinity, person_affinity, keyword_affinity,
            runtime_pref, quality_sensitivity, overall_centroid, model_notes
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (account_id) DO UPDATE SET
            genre_affinity = EXCLUDED.genre_affinity,
            person_affinity = EXCLUDED.person_affinity,
            keyword_affinity = EXCLUDED.keyword_affinity,
            runtime_pref = EXCLUDED.runtime_pref,
            quality_sensitivity = EXCLUDED.quality_sensitivity,
            overall_centroid = EXCLUDED.overall_centroid,
            model_notes = EXCLUDED.model_notes,
            computed_at = now()
        RETURNING *
        "#,
    )
    .bind(new.account_id)
    .bind(&new.genre_affinity)
    .bind(&new.person_affinity)
    .bind(&new.keyword_affinity)
    .bind(&new.runtime_pref)
    .bind(&new.quality_sensitivity)
    .bind(&new.overall_centroid)
    .bind(&new.model_notes)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_profile(pool: &PgPool, account_id: i64) -> MuseResult<Option<TasteProfile>> {
    sqlx::query_as::<_, TasteProfile>("SELECT * FROM taste_profile WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)
}

// --- taste_context_centroids ---------------------------------------------

pub async fn upsert_context_centroid(pool: &PgPool, new: &NewTasteContextCentroid) -> MuseResult<TasteContextCentroid> {
    sqlx::query_as::<_, TasteContextCentroid>(
        r#"
        INSERT INTO taste_context_centroids (account_id, context_key, centroid, sample_size)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, context_key) DO UPDATE SET
            centroid = EXCLUDED.centroid,
            sample_size = EXCLUDED.sample_size
        RETURNING *
        "#,
    )
    .bind(new.account_id)
    .bind(&new.context_key)
    .bind(&new.centroid)
    .bind(new.sample_size)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_context_centroids(pool: &PgPool, account_id: i64) -> MuseResult<Vec<TasteContextCentroid>> {
    sqlx::query_as::<_, TasteContextCentroid>(
        "SELECT * FROM taste_context_centroids WHERE account_id = $1 ORDER BY context_key",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

// --- taste_signals ---------------------------------------------------------

pub async fn record_signal(pool: &PgPool, new: &NewTasteSignal) -> MuseResult<TasteSignal> {
    sqlx::query_as::<_, TasteSignal>(
        r#"
        INSERT INTO taste_signals (account_id, media_item_id, signal_type, weight, context_key, note)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING *
        "#,
    )
    .bind(new.account_id)
    .bind(new.media_item_id)
    .bind(&new.signal_type)
    .bind(new.weight)
    .bind(&new.context_key)
    .bind(&new.note)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_signals_for_account(pool: &PgPool, account_id: i64, limit: i64) -> MuseResult<Vec<TasteSignal>> {
    sqlx::query_as::<_, TasteSignal>(
        "SELECT * FROM taste_signals WHERE account_id = $1 ORDER BY observed_at DESC LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn list_signals_for_media_item(pool: &PgPool, account_id: i64, media_item_id: i64) -> MuseResult<Vec<TasteSignal>> {
    sqlx::query_as::<_, TasteSignal>(
        "SELECT * FROM taste_signals WHERE account_id = $1 AND media_item_id = $2 ORDER BY observed_at DESC",
    )
    .bind(account_id)
    .bind(media_item_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Delete every `taste_signals` row for `account_id` whose `signal_type` is
/// in `signal_types` — the MUSE-10 recompute's idempotency primitive.
/// `recompute_taste` always re-derives the *automatic* signal types
/// (finished/abandoned/rewatched/rated/watchlisted) fresh from
/// `watch_stats`/`ratings`/`watchlist` on every run, so it deletes-then-
/// reinserts them here rather than trying to diff/upsert individual rows.
/// A `signal_type` NOT passed (e.g. `'curation_note'`, a human-authored
/// signal) is left untouched. Returns the number of rows removed.
pub async fn delete_signals_by_types(
    pool: &PgPool,
    account_id: i64,
    signal_types: &[&str],
) -> MuseResult<u64> {
    let result = sqlx::query("DELETE FROM taste_signals WHERE account_id = $1 AND signal_type = ANY($2)")
        .bind(account_id)
        .bind(signal_types)
        .execute(pool)
        .await
        .map_err(MuseError::Database)?;
    Ok(result.rows_affected())
}

// --- raw weighted rows `taste_model`'s affinity formulas are built from ----
//
// Each row is one (dimension-value, taste_signals.weight, observed_at)
// contribution — recency-decay is applied client-side in
// `crate::taste_model::profile` (same split as
// `repo::taste_divergence`/`radar::divergence`: the DB does the join, Rust
// does the math). A signal whose `media_item_id` was cleared by a since-
// deleted library item (`taste_signals.media_item_id` is `ON DELETE SET
// NULL`) simply drops out of these joins — it stays in the raw signal log
// for audit, but can no longer contribute to a genre/person/decade/keyword
// it can no longer be traced to.

/// One `(genre, weight, observed_at)` contribution from a taste signal.
#[derive(Debug, Clone, FromRow)]
pub struct SignalGenreRow {
    pub genre: String,
    pub weight: f32,
    pub observed_at: DateTime<Utc>,
}

pub async fn signal_genre_rows(pool: &PgPool, account_id: i64) -> MuseResult<Vec<SignalGenreRow>> {
    sqlx::query_as::<_, SignalGenreRow>(
        r#"
        SELECT g.name AS genre, ts.weight AS weight, ts.observed_at AS observed_at
        FROM taste_signals ts
        JOIN media_items mi ON mi.id = ts.media_item_id
        JOIN media_metadata_genres mmg ON mmg.media_metadata_id = mi.media_metadata_id
        JOIN genres g ON g.id = mmg.genre_id
        WHERE ts.account_id = $1
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// One `(person_id, weight, observed_at)` contribution from a taste signal.
#[derive(Debug, Clone, FromRow)]
pub struct SignalPersonRow {
    pub person_id: i64,
    pub weight: f32,
    pub observed_at: DateTime<Utc>,
}

pub async fn signal_person_rows(pool: &PgPool, account_id: i64) -> MuseResult<Vec<SignalPersonRow>> {
    sqlx::query_as::<_, SignalPersonRow>(
        r#"
        SELECT mc.person_id AS person_id, ts.weight AS weight, ts.observed_at AS observed_at
        FROM taste_signals ts
        JOIN media_items mi ON mi.id = ts.media_item_id
        JOIN media_metadata_credits mc ON mc.media_metadata_id = mi.media_metadata_id
        WHERE ts.account_id = $1
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// One `(decade, weight, observed_at)` contribution from a taste signal.
/// Titles with no `year` are excluded (can't place them on the decade axis)
/// — same posture as `repo::taste_divergence::account_decade_weights`.
#[derive(Debug, Clone, FromRow)]
pub struct SignalDecadeRow {
    pub decade: i32,
    pub weight: f32,
    pub observed_at: DateTime<Utc>,
}

pub async fn signal_decade_rows(pool: &PgPool, account_id: i64) -> MuseResult<Vec<SignalDecadeRow>> {
    sqlx::query_as::<_, SignalDecadeRow>(
        r#"
        SELECT ((mm.year / 10) * 10)::int4 AS decade, ts.weight AS weight, ts.observed_at AS observed_at
        FROM taste_signals ts
        JOIN media_items mi ON mi.id = ts.media_item_id
        JOIN media_metadata mm ON mm.id = mi.media_metadata_id
        WHERE ts.account_id = $1 AND mm.year IS NOT NULL
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// One `(keyword, weight, observed_at)` contribution from a taste signal.
/// `media_metadata.keywords` is a jsonb array whose elements are either a
/// bare string or a TMDb-shaped `{"id":.., "name":..}` object; both forms
/// are unwrapped to a plain keyword string, and anything that resolves to
/// neither (a differently-shaped element) is filtered out rather than
/// risking a mangled key in the aggregate.
#[derive(Debug, Clone, FromRow)]
pub struct SignalKeywordRow {
    pub keyword: String,
    pub weight: f32,
    pub observed_at: DateTime<Utc>,
}

pub async fn signal_keyword_rows(pool: &PgPool, account_id: i64) -> MuseResult<Vec<SignalKeywordRow>> {
    sqlx::query_as::<_, SignalKeywordRow>(
        r#"
        SELECT keyword, weight, observed_at FROM (
            SELECT
                CASE
                    WHEN jsonb_typeof(elem) = 'string' THEN trim(both '"' from elem::text)
                    WHEN jsonb_typeof(elem) = 'object' THEN elem->>'name'
                    ELSE NULL
                END AS keyword,
                ts.weight AS weight,
                ts.observed_at AS observed_at
            FROM taste_signals ts
            JOIN media_items mi ON mi.id = ts.media_item_id
            JOIN media_metadata mm ON mm.id = mi.media_metadata_id
            CROSS JOIN LATERAL jsonb_array_elements(mm.keywords) AS elem
            WHERE ts.account_id = $1
        ) unwrapped
        WHERE keyword IS NOT NULL AND keyword <> ''
        "#,
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
