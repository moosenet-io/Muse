//! Repo functions for `taste_profile` / `taste_context_centroids` /
//! `taste_signals` (spec §3.4).

use sqlx::PgPool;

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
