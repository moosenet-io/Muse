//! Trending/population feed ingest (MUSE-19, spec §3.7/§4c).
//!
//! Day-one source is TMDb only ([`TmdbClient`]): `/trending`, `/*/popular`,
//! `/*/watch/providers`. Richer streaming sources the spec calls out as
//! optional (Trakt most-watched/most-played, FlixPatrol/JustWatch per-
//! platform Top-10s) are **not built here** — [`OptionalSource`] documents
//! the seam so a later item can add them without reshaping
//! `trending_snapshots`/`streaming_availability`.
//!
//! This module also does **not** compute `taste_divergence` (over/under-
//! index, mainstream_score, adventurousness, were_early, blind_spots) —
//! that's MUSE-20, a separate later item that consumes the corpus this
//! module writes. [`compute_population_profile`] only ships the
//! ingest/storage half (`sample_size` + window/region); the distribution
//! and centroid math are a documented MUSE-20 seam (see that function's doc
//! comment and `migrations/0043_population_profile.sql`).

pub mod client;
pub mod models;

pub use client::{TmdbClient, TmdbMediaType, TrendingWindow};

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::media_metadata::MediaKind;
use crate::models::trending::{NewPopulationProfile, NewStreamingAvailability, NewTrendingSnapshot};
use crate::repo;

/// Default region ingest runs against when the caller doesn't override it
/// (day-one is US-centric; region is otherwise fully configurable per call).
pub const DEFAULT_REGION: &str = "US";

/// How many top-ranked entries per snapshot get a `/watch/providers`
/// lookup. TMDb's rate limits are generous but not infinite; day-one keeps
/// this bounded rather than fanning a providers call out across a full
/// trending page (20 movies + 20 tv, worst case, per ingest run).
const PROVIDERS_LOOKUP_LIMIT: usize = 20;

/// Streaming/consumption sources the spec (§4c) calls out as optional
/// richer enrichment beyond day-one TMDb. **Not implemented** — a future
/// item wires these in behind the same `trending_snapshots`/
/// `streaming_availability` shape (they only add `source` values and, for
/// FlixPatrol/JustWatch, a `platform`). Calling [`Self::fetch`] today is a
/// deliberate, explicit `NotImplemented` rather than a silent no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionalSource {
    /// Trakt most-watched/most-played — the best "actually being watched"
    /// signal from community scrobbles (spec §4c).
    Trakt,
    /// FlixPatrol per-platform streaming Top-10s.
    FlixPatrol,
    /// JustWatch per-platform streaming Top-10s.
    JustWatch,
}

impl OptionalSource {
    /// Always returns `Err(MuseError::NotImplemented)` — see the enum doc
    /// comment. Exists so MUSE-20+ (or an operator wiring in credentials)
    /// has a named, typed extension point rather than needing to invent one.
    pub fn fetch(&self) -> MuseResult<()> {
        Err(MuseError::NotImplemented)
    }
}

/// Outcome of one [`snapshot_trending`] run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrendingIngestSummary {
    pub snapshots_written: usize,
    pub providers_written: usize,
    /// Set when any individual TMDb call failed — the run still completes
    /// (graceful degradation) using whatever slices succeeded.
    pub degraded: bool,
}

/// Snapshot TMDb trending (day+week, movie+tv) and popular (movie+tv) into
/// `trending_snapshots`, resolve each entry to `media_metadata` where
/// possible, pull `/watch/providers` for the top-ranked resolved entries
/// into `streaming_availability`, and append a `population_profile` rollup
/// row.
///
/// Gracefully degrades: an unreachable/erroring TMDb call for one
/// media-type/window slice is logged and skipped (`degraded: true` in the
/// summary) rather than failing the whole run — see the module doc comment.
/// This never errors out on upstream failure; it only returns `Err` for a
/// local database failure (which the caller should treat as a genuine ingest
/// failure, e.g. retry the whole run).
pub async fn snapshot_trending(
    pool: &PgPool,
    client: &TmdbClient,
    region: &str,
) -> MuseResult<TrendingIngestSummary> {
    let mut summary = TrendingIngestSummary::default();

    for media_type in [TmdbMediaType::Movie, TmdbMediaType::Tv] {
        for window in [TrendingWindow::Day, TrendingWindow::Week] {
            match client.trending(media_type, window).await {
                Ok(titles) => {
                    write_snapshot(
                        pool, client, &mut summary, "tmdb", "trending", region, window.as_str(),
                        media_type, &titles,
                    )
                    .await?;
                }
                Err(e) => {
                    tracing::warn!(error = %e, ?media_type, window = window.as_str(), "tmdb trending fetch failed; skipping this slice");
                    summary.degraded = true;
                }
            }
        }

        match client.popular(media_type, Some(region)).await {
            Ok(titles) => {
                // TMDb `/movie|tv/popular` has no day/week axis of its own;
                // the spec's `window` column still requires a value, so
                // this is recorded as 'week' (the coarser, more stable
                // cadence) — a divergence from a real TMDb field, not from
                // the spec's schema.
                write_snapshot(
                    pool, client, &mut summary, "tmdb", "popular", region, "week", media_type, &titles,
                )
                .await?;
            }
            Err(e) => {
                tracing::warn!(error = %e, ?media_type, "tmdb popular fetch failed; skipping this slice");
                summary.degraded = true;
            }
        }
    }

    if let Err(e) = compute_population_profile(pool, region).await {
        tracing::warn!(error = %e, "population_profile rollup failed; trending_snapshots were still written");
        summary.degraded = true;
    }

    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
async fn write_snapshot(
    pool: &PgPool,
    client: &TmdbClient,
    summary: &mut TrendingIngestSummary,
    source: &str,
    scope: &str,
    region: &str,
    window: &str,
    media_type: TmdbMediaType,
    titles: &[models::TmdbTitle],
) -> MuseResult<()> {
    let kind = match media_type {
        TmdbMediaType::Movie => MediaKind::Movie,
        TmdbMediaType::Tv => MediaKind::Show,
    };

    for (idx, title) in titles.iter().enumerate() {
        let rank = (idx + 1) as i32;
        let tmdb_id = title.id.to_string();

        let media_metadata_id = repo::media_metadata::find_by_tmdb_id(pool, kind, &tmdb_id).await?;

        let external_ref = serde_json::json!({
            "tmdb_id": tmdb_id,
            "title": title.display_title(),
            "year": title.year(),
        });

        repo::trending::insert_snapshot(
            pool,
            &NewTrendingSnapshot {
                source: source.to_string(),
                scope: scope.to_string(),
                platform: None,
                region: region.to_string(),
                window: window.to_string(),
                rank: Some(rank),
                media_metadata_id,
                external_ref: Some(external_ref),
                popularity: title.popularity.map(|p| p as f32),
            },
        )
        .await?;
        summary.snapshots_written += 1;

        let Some(metadata_id) = media_metadata_id else {
            continue;
        };
        if idx >= PROVIDERS_LOOKUP_LIMIT {
            continue;
        }

        match client.watch_providers(media_type, &tmdb_id).await {
            Ok(regions) => {
                let Some(region_providers) = regions.get(region) else {
                    continue;
                };

                for (offer_type, entries) in [
                    ("flatrate", &region_providers.flatrate),
                    ("ads", &region_providers.ads),
                    ("rent", &region_providers.rent),
                    ("buy", &region_providers.buy),
                ] {
                    for entry in entries {
                        repo::trending::upsert_streaming_availability(
                            pool,
                            &NewStreamingAvailability {
                                media_metadata_id: metadata_id,
                                provider: entry.provider_name.clone(),
                                region: region.to_string(),
                                offer_type: offer_type.to_string(),
                                link: region_providers.link.clone(),
                            },
                        )
                        .await?;
                        summary.providers_written += 1;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, tmdb_id = %tmdb_id, "tmdb watch_providers fetch failed; skipping availability for this title");
                summary.degraded = true;
            }
        }
    }

    Ok(())
}

/// Records the MUSE-19 storage half of §3.7 `population_profile`:
/// `sample_size` (how many trending rows are currently on record for this
/// region) plus empty/NULL distributions. The genre/decade/runtime
/// distribution math and `mainstream_centroid` (needs resolved-item
/// embeddings) are MUSE-20 scope — see the module doc comment and
/// `migrations/0043_population_profile.sql`. This function is the seam
/// MUSE-20 replaces/extends, not a placeholder to delete.
pub async fn compute_population_profile(pool: &PgPool, region: &str) -> MuseResult<()> {
    let sample_size = repo::trending::count_recent_snapshots(pool, region).await?;

    repo::trending::insert_population_profile(
        pool,
        &NewPopulationProfile {
            window: "week".to_string(),
            region: region.to_string(),
            genre_distribution: serde_json::json!({}),
            decade_distribution: None,
            runtime_distribution: None,
            sample_size: Some(sample_size as i32),
        },
    )
    .await?;

    Ok(())
}
