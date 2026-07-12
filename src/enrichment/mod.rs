//! MUSE-14 — enrichment via the Terminus tool suite (first cut).
//!
//! Muse pulls two Terminus-tool-suite-shaped signals into the MUSE-03
//! `external_enrichment` cache (spec §3.5/§6):
//!   (a) forum/critic sentiment + "does it get good at ep N" (de-risks
//!       abandonment) via a SearXNG-shaped search endpoint, and
//!   (b) renewal/trailer news via a news-search-shaped endpoint.
//!
//! Muse is a **standalone service** — it does not call Terminus MCP tools
//! in-process. `EnrichmentService` calls the configured HTTP endpoints
//! directly (`super::client`), normalizes the results (`super::cache`),
//! and upserts them into `external_enrichment` respecting the cache's TTL.
//!
//! This is a deliberate first cut: two source kinds done reasonably well
//! (sentiment + "does it get good" from search, renewal/trailer from
//! news), everything else in the spec's §6 wishlist (deals, calendar,
//! weather, council/wizard, crucible, best-watch-order) is an explicit,
//! documented seam for a later item — see [`Seam`] below. Folding this
//! into curation (MUSE-11) and proactive content (MUSE-12) is likewise a
//! later item; this module only populates the cache.

pub mod cache;
pub mod client;

use sqlx::PgPool;

use crate::config::Config;
use crate::error::MuseResult;
use crate::models::external_enrichment::ExternalEnrichment;

use client::{NewsClient, SearxngClient};

/// Enrichment sources documented in spec §6 that are explicitly OUT of
/// scope for this first cut. Not wired to anything — this exists purely as
/// a discoverable, named seam (see the module doc) so a future item knows
/// exactly what's left and doesn't have to re-derive it from the spec.
#[allow(dead_code)] // intentionally unwired — a discoverable marker of future scope, not live code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seam {
    /// `odyssey_deals` / a future `muse_deals` commerce hook — box-sets,
    /// source novels, soundtracks, streaming-rental deals tied to what's
    /// being watched.
    ShoppingDeals,
    /// `google_calendar_*` — schedule-aware suggestions (free evening ->
    /// a movie-night lineup; avoid a 3-hour film on a work night).
    CalendarAwareness,
    /// `weather` — "rainy weekend, cozy-binge weather" flavor.
    WeatherFlavor,
    /// `council_convene` / `wizard_consult` — multi-model deliberation for
    /// hard curation calls.
    CouncilDeliberation,
    /// `crucible_track_create` — cross-domain binge-to-learning bridges.
    CrucibleBridge,
    /// Critic-score aggregation (`kind = 'critic_score'` in the spec's
    /// enum) as a distinct source from forum sentiment — first cut folds
    /// critic language into `forum_sentiment` rather than giving it its
    /// own kind/scraper.
    CriticScoreAggregation,
    /// Best watch/release order threads — a `searxng_search` query shape
    /// this module doesn't issue yet.
    BestWatchOrder,
}

/// Owns the two MUSE-14 enrichment source clients and orchestrates
/// enrich-then-cache. Both clients are independently optional — either or
/// both may be `None` when unconfigured, in which case that source is
/// simply skipped (never a hard failure; see the crate-wide graceful-
/// degrade posture in `PlexClient`/`TmdbClient`/`ProwlarrClient`).
#[derive(Debug, Clone)]
pub struct EnrichmentService {
    searxng: Option<SearxngClient>,
    news: Option<NewsClient>,
}

impl EnrichmentService {
    pub fn from_config(config: &Config) -> Self {
        let searxng = SearxngClient::from_config(config);
        let news = NewsClient::from_config(config);

        tracing::info!(
            searxng_configured = searxng.is_some(),
            news_configured = news.is_some(),
            "enrichment service initialized"
        );

        Self { searxng, news }
    }

    /// True when at least one enrichment source is configured. Mainly for
    /// callers (e.g. a future worker) deciding whether it's worth scheduling
    /// enrichment runs at all.
    pub fn any_source_configured(&self) -> bool {
        self.searxng.is_some() || self.news.is_some()
    }

    /// Fetch (respecting cache freshness) and store both sentiment/"gets
    /// good" and renewal/trailer signals for one media item, identified by
    /// `media_item_id` and its display `title` (used to build search
    /// queries). Returns every row that ended up cached (freshly fetched
    /// or already-fresh), skipping sources that are unconfigured or whose
    /// upstream call failed — a single source's failure never aborts the
    /// other, and never propagates as an error to the caller (this is a
    /// best-effort enrichment pass, not a critical-path operation).
    pub async fn enrich_media_item(
        &self,
        pool: &PgPool,
        media_item_id: i64,
        title: &str,
    ) -> MuseResult<Vec<ExternalEnrichment>> {
        let now = chrono::Utc::now();
        let mut cached = Vec::new();

        if let Some(searxng) = &self.searxng {
            match self
                .refresh_searxng(pool, searxng, media_item_id, title, now)
                .await
            {
                Ok(rows) => cached.extend(rows),
                Err(e) => tracing::warn!(
                    error = %e,
                    media_item_id,
                    "searxng enrichment failed; skipping (graceful degrade)"
                ),
            }
        }

        if let Some(news) = &self.news {
            match self.refresh_news(pool, news, media_item_id, title, now).await {
                Ok(rows) => cached.extend(rows),
                Err(e) => tracing::warn!(
                    error = %e,
                    media_item_id,
                    "news enrichment failed; skipping (graceful degrade)"
                ),
            }
        }

        Ok(cached)
    }

    async fn refresh_searxng(
        &self,
        pool: &PgPool,
        searxng: &SearxngClient,
        media_item_id: i64,
        title: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> MuseResult<Vec<ExternalEnrichment>> {
        let mut rows = Vec::new();

        // --- forum/critic sentiment ---
        if let Some(existing) = cache::fresh_entry(
            pool,
            media_item_id,
            cache::kind::FORUM_SENTIMENT,
            cache::source::SEARXNG,
            now,
        )
        .await?
        {
            rows.push(existing);
        } else {
            let results = searxng.search(&format!("{title} reddit review")).await?;
            let payload = cache::normalize_sentiment(&results);
            let confidence = payload.score.map(|s| s.abs());
            let row = cache::store(
                pool,
                media_item_id,
                cache::kind::FORUM_SENTIMENT,
                cache::source::SEARXNG,
                serde_json::to_value(&payload).map_err(|e| {
                    crate::error::MuseError::upstream(format!("failed to serialize sentiment payload: {e}"))
                })?,
                confidence,
                cache::SENTIMENT_TTL_SECONDS,
            )
            .await?;
            rows.push(row);
        }

        // --- "does it get good at episode N" ---
        if let Some(existing) = cache::fresh_entry(
            pool,
            media_item_id,
            cache::kind::DOES_IT_GET_GOOD,
            cache::source::SEARXNG,
            now,
        )
        .await?
        {
            rows.push(existing);
        } else {
            let results = searxng
                .search(&format!("does {title} get good what episode"))
                .await?;
            let payload = cache::normalize_gets_good(&results);
            let confidence = payload.patience_payoff;
            let row = cache::store(
                pool,
                media_item_id,
                cache::kind::DOES_IT_GET_GOOD,
                cache::source::SEARXNG,
                serde_json::to_value(&payload).map_err(|e| {
                    crate::error::MuseError::upstream(format!("failed to serialize gets-good payload: {e}"))
                })?,
                confidence,
                cache::GETS_GOOD_TTL_SECONDS,
            )
            .await?;
            rows.push(row);
        }

        Ok(rows)
    }

    async fn refresh_news(
        &self,
        pool: &PgPool,
        news: &NewsClient,
        media_item_id: i64,
        title: &str,
        now: chrono::DateTime<chrono::Utc>,
    ) -> MuseResult<Vec<ExternalEnrichment>> {
        // Renewal and trailer signals share one query/TTL/freshness check —
        // a single news search is classified into whichever kind(s) its
        // results match (see `cache::normalize_news_article`), so a fresh
        // *either* row means we can skip the search this pass.
        let renewal_fresh = cache::fresh_entry(
            pool,
            media_item_id,
            cache::kind::RENEWAL_NEWS,
            cache::source::NEWS,
            now,
        )
        .await?;
        let trailer_fresh = cache::fresh_entry(
            pool,
            media_item_id,
            cache::kind::TRAILER,
            cache::source::NEWS,
            now,
        )
        .await?;

        let mut rows: Vec<ExternalEnrichment> = renewal_fresh.into_iter().chain(trailer_fresh).collect();
        if !rows.is_empty() {
            return Ok(rows);
        }

        let articles = news.search(&format!("{title} renewed trailer")).await?;

        for article in &articles {
            let Some((kind, payload)) = cache::normalize_news_article(article) else {
                continue;
            };
            let row = cache::store(
                pool,
                media_item_id,
                kind,
                cache::source::NEWS,
                serde_json::to_value(&payload).map_err(|e| {
                    crate::error::MuseError::upstream(format!("failed to serialize news payload: {e}"))
                })?,
                None,
                cache::NEWS_TTL_SECONDS,
            )
            .await?;
            rows.push(row);
        }

        Ok(rows)
    }
}
