//! Normalization of raw SearXNG/news results into `external_enrichment`
//! rows (spec §3.5), plus the TTL-aware read/upsert path.
//!
//! This module owns the mapping from "a pile of search results" to the
//! compact, normalized payload shape a later curation/proactive step reads.
//! It deliberately does no HTTP itself (see `super::client`) and no
//! source-selection policy (see `super::EnrichmentService`) — just
//! normalize-and-cache.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::external_enrichment::{ExternalEnrichment, NewExternalEnrichment};
use crate::repo;

use super::client::{NewsArticle, SearxngResult};

/// `external_enrichment.kind` values MUSE-14 produces. Other kinds listed
/// in the spec (`'deal'`, `'critic_score'`) are deliberately left as seams
/// for a later item — see the module doc on `super`.
pub mod kind {
    pub const FORUM_SENTIMENT: &str = "forum_sentiment";
    pub const DOES_IT_GET_GOOD: &str = "does_it_get_good";
    pub const RENEWAL_NEWS: &str = "renewal_news";
    pub const TRAILER: &str = "trailer";
}

/// `external_enrichment.source` values MUSE-14 produces.
pub mod source {
    pub const SEARXNG: &str = "searxng";
    pub const NEWS: &str = "news";
}

/// Sentiment/"does it get good" signals age slowly (forum consensus on a
/// show doesn't shift week to week) — refresh weekly, matching the
/// `external_enrichment.ttl_seconds` column default.
pub const SENTIMENT_TTL_SECONDS: i32 = 7 * 24 * 3600;
/// "Does it get good at episode N" is close to a static fact about a show
/// once enough of the fanbase has weighed in — refresh monthly.
pub const GETS_GOOD_TTL_SECONDS: i32 = 30 * 24 * 3600;
/// Renewal/trailer news is time-sensitive — refresh daily.
pub const NEWS_TTL_SECONDS: i32 = 24 * 3600;

/// Normalized forum/critic sentiment payload — `external_enrichment.payload`
/// for `kind = 'forum_sentiment'`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentimentPayload {
    /// Crude normalized sentiment in `[-1.0, 1.0]`; `None` when no
    /// sentiment-bearing language was found in any result.
    pub score: Option<f32>,
    pub summary: String,
    pub url: Option<String>,
    pub source_count: usize,
}

/// Normalized "does it get good at episode N" payload — the headline
/// abandonment de-risking signal — `external_enrichment.payload` for
/// `kind = 'does_it_get_good'`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetsGoodPayload {
    /// The episode number community consensus says a slow-starting show
    /// "gets good" at, when a number could be extracted. This is the field
    /// a later curation/proactive step reads to say e.g. "it picks up at
    /// ep 4, want to give it another shot?"
    pub gets_good_at_episode: Option<i32>,
    /// A compact `[0.0, 1.0]` "patience payoff" hint — how strongly the
    /// found results agree it's worth pushing through a slow start.
    /// Distinct from `gets_good_at_episode` so a caller can use the hint
    /// even when no specific episode number was extractable.
    pub patience_payoff: Option<f32>,
    pub summary: String,
    pub url: Option<String>,
    pub source_count: usize,
}

/// Normalized renewal/trailer news payload — `external_enrichment.payload`
/// for `kind = 'renewal_news'` or `kind = 'trailer'`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsSignalPayload {
    pub headline: String,
    pub url: Option<String>,
    pub published_at: Option<String>,
    /// Present (and non-`"unknown"`) only on `kind = 'renewal_news'` rows.
    pub renewal_status: Option<String>,
    /// Present only on `kind = 'trailer'` rows.
    pub trailer_url: Option<String>,
}

/// Positive/negative keyword lexicon for the forum-sentiment heuristic.
/// Deliberately small and conservative for a first cut — a false "neutral"
/// (score `None`) is a safe failure mode; a confidently wrong strong score
/// is not. Case-insensitive substring match against each result's title +
/// content.
const POSITIVE_WORDS: &[&str] = &[
    "amazing", "great", "excellent", "loved", "love it", "masterpiece", "brilliant",
    "fantastic", "underrated", "best show", "highly recommend", "worth it", "so good",
];
const NEGATIVE_WORDS: &[&str] = &[
    "boring", "terrible", "awful", "waste of time", "overrated", "disappointing",
    "hated", "worst", "unwatchable", "dropped it", "gave up",
];

/// Words that (loosely) mark a result as talking about "does it get good" /
/// picking up rather than general sentiment.
const PICKS_UP_WORDS: &[&str] = &[
    "gets good", "picks up", "get better", "gets better", "worth pushing through",
    "stick with it", "give it a chance", "improves",
];

fn haystack(result: &SearxngResult) -> String {
    format!(
        "{} {}",
        result.title,
        result.content.as_deref().unwrap_or_default()
    )
    .to_lowercase()
}

/// Normalize a set of SearXNG results into a forum/critic sentiment
/// payload. Pure function — no I/O, easy to unit test.
pub fn normalize_sentiment(results: &[SearxngResult]) -> SentimentPayload {
    let mut pos = 0i32;
    let mut neg = 0i32;

    for r in results {
        let text = haystack(r);
        pos += POSITIVE_WORDS.iter().filter(|w| text.contains(*w)).count() as i32;
        neg += NEGATIVE_WORDS.iter().filter(|w| text.contains(*w)).count() as i32;
    }

    let score = if pos == 0 && neg == 0 {
        None
    } else {
        Some((pos - neg) as f32 / (pos + neg) as f32)
    };

    let summary = results
        .first()
        .and_then(|r| r.content.clone())
        .unwrap_or_else(|| "no forum/critic discussion found".to_string());

    SentimentPayload {
        score,
        summary: truncate(&summary, 280),
        url: results.first().and_then(|r| r.url.clone()),
        source_count: results.len(),
    }
}

/// Normalize a set of SearXNG results into a "does it get good at episode
/// N" payload — the headline abandonment de-risking signal (spec §6/MUSE-14).
pub fn normalize_gets_good(results: &[SearxngResult]) -> GetsGoodPayload {
    let mut episode_votes: Vec<i32> = Vec::new();
    let mut picks_up_hits = 0i32;

    for r in results {
        let text = haystack(r);
        picks_up_hits += PICKS_UP_WORDS.iter().filter(|w| text.contains(*w)).count() as i32;
        episode_votes.extend(extract_episode_numbers(&text));
    }

    // Most-common episode number mentioned, if any — a simple mode over a
    // small sample is the right amount of sophistication for a first cut.
    let gets_good_at_episode = mode(&episode_votes);

    let patience_payoff = if results.is_empty() {
        None
    } else {
        Some((picks_up_hits.min(results.len() as i32) as f32 / results.len() as f32).min(1.0))
    };

    let summary = results
        .iter()
        .find(|r| {
            let text = haystack(r);
            PICKS_UP_WORDS.iter().any(|w| text.contains(*w)) || !extract_episode_numbers(&text).is_empty()
        })
        .or_else(|| results.first())
        .and_then(|r| r.content.clone())
        .unwrap_or_else(|| "no consensus found on when/whether it picks up".to_string());

    GetsGoodPayload {
        gets_good_at_episode,
        patience_payoff,
        summary: truncate(&summary, 280),
        url: results.first().and_then(|r| r.url.clone()),
        source_count: results.len(),
    }
}

/// Extract small ("episode 4", "ep. 4", "episode four" is NOT handled —
/// first cut sticks to digits) episode-number mentions from free text.
/// Bounded to plausible episode numbers (1-99) to avoid picking up years,
/// ratings, etc.
fn extract_episode_numbers(text: &str) -> Vec<i32> {
    let markers = ["episode ", "ep. ", "ep "];
    let mut found = Vec::new();

    for marker in markers {
        let mut rest = text;
        while let Some(idx) = rest.find(marker) {
            let after = &rest[idx + marker.len()..];
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                if let Ok(n) = digits.parse::<i32>() {
                    if (1..=99).contains(&n) {
                        found.push(n);
                    }
                }
            }
            rest = &after[digits.len().min(after.len())..];
        }
    }

    found
}

/// The most frequently occurring value, ties broken by smallest value (a
/// slight bias toward "earlier is safer to promise the viewer").
fn mode(values: &[i32]) -> Option<i32> {
    if values.is_empty() {
        return None;
    }

    let mut counts: std::collections::BTreeMap<i32, i32> = std::collections::BTreeMap::new();
    for v in values {
        *counts.entry(*v).or_insert(0) += 1;
    }

    counts
        .into_iter()
        .max_by_key(|(value, count)| (*count, -*value))
        .map(|(value, _)| value)
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Classify a news article as a renewal signal, a trailer signal, or
/// neither — MUSE-14's first-cut news normalization. Returns `None` for
/// articles that match neither pattern (they are simply not cached; a
/// future news-kind seam can broaden this).
pub fn normalize_news_article(article: &NewsArticle) -> Option<(&'static str, NewsSignalPayload)> {
    let text = format!(
        "{} {}",
        article.title,
        article.description.as_deref().unwrap_or_default()
    )
    .to_lowercase();

    let is_trailer = text.contains("trailer") || text.contains("teaser");
    let renewal_status = if text.contains("renewed")
        || text.contains("renewal")
        || (text.contains("season") && text.contains("confirmed"))
    {
        Some("renewed")
    } else if text.contains("cancelled") || text.contains("canceled") || text.contains("axed") {
        Some("cancelled")
    } else {
        None
    };

    if is_trailer {
        Some((
            kind::TRAILER,
            NewsSignalPayload {
                headline: article.title.clone(),
                url: article.url.clone(),
                published_at: article.published_at.clone(),
                renewal_status: None,
                trailer_url: article.url.clone(),
            },
        ))
    } else if let Some(status) = renewal_status {
        Some((
            kind::RENEWAL_NEWS,
            NewsSignalPayload {
                headline: article.title.clone(),
                url: article.url.clone(),
                published_at: article.published_at.clone(),
                renewal_status: Some(status.to_string()),
                trailer_url: None,
            },
        ))
    } else {
        None
    }
}

/// Whether a cached row is still within its TTL as of `now` — the
/// "read-with-TTL" half of the cache contract.
pub fn is_fresh(entry: &ExternalEnrichment, now: DateTime<Utc>) -> bool {
    entry.fetched_at + chrono::Duration::seconds(entry.ttl_seconds as i64) > now
}

/// Look up an unexpired cached row for `(media_item_id, kind, source)`, if
/// one exists — callers use this to decide whether a fresh HTTP fetch is
/// needed at all (don't re-fetch fresh rows, per the MUSE-14 contract).
pub async fn fresh_entry(
    pool: &PgPool,
    media_item_id: i64,
    kind: &str,
    source: &str,
    now: DateTime<Utc>,
) -> MuseResult<Option<ExternalEnrichment>> {
    let rows = repo::external_enrichment::list_for_media_item(pool, media_item_id).await?;
    Ok(rows
        .into_iter()
        .find(|e| e.kind == kind && e.source == source && is_fresh(e, now)))
}

/// Upsert a normalized payload into the cache. Thin wrapper over
/// `repo::external_enrichment::upsert` that pins the `(kind, source)`
/// pairing this module owns.
pub async fn store(
    pool: &PgPool,
    media_item_id: i64,
    kind: &str,
    source: &str,
    payload: serde_json::Value,
    confidence: Option<f32>,
    ttl_seconds: i32,
) -> MuseResult<ExternalEnrichment> {
    repo::external_enrichment::upsert(
        pool,
        &NewExternalEnrichment {
            media_item_id,
            kind: kind.to_string(),
            source: source.to_string(),
            payload,
            confidence,
            ttl_seconds,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enrichment::client::{NewsArticle, SearxngResult};

    fn searxng_result(title: &str, content: &str) -> SearxngResult {
        SearxngResult {
            title: title.to_string(),
            url: Some("https://example.invalid/1".to_string()),
            content: Some(content.to_string()),
            engine: Some("reddit".to_string()),
        }
    }

    #[test]
    fn normalize_sentiment_scores_positive_language() {
        let results = vec![
            searxng_result("r/television", "this show is a masterpiece, highly recommend"),
            searxng_result("Letterboxd", "loved it, so good"),
        ];

        let payload = normalize_sentiment(&results);

        assert!(payload.score.unwrap() > 0.0);
        assert_eq!(payload.source_count, 2);
    }

    #[test]
    fn normalize_sentiment_scores_negative_language() {
        let results = vec![searxng_result("r/television", "boring and disappointing, dropped it")];

        let payload = normalize_sentiment(&results);

        assert!(payload.score.unwrap() < 0.0);
    }

    #[test]
    fn normalize_sentiment_none_when_no_signal_words() {
        let results = vec![searxng_result("r/television", "a show about a company")];

        let payload = normalize_sentiment(&results);

        assert!(payload.score.is_none());
    }

    #[test]
    fn normalize_sentiment_empty_results() {
        let payload = normalize_sentiment(&[]);
        assert!(payload.score.is_none());
        assert_eq!(payload.source_count, 0);
    }

    #[test]
    fn normalize_gets_good_extracts_consensus_episode() {
        let results = vec![
            searxng_result("r/television", "it really gets good starting at episode 4"),
            searxng_result("forum post", "stick with it, ep 4 is when it clicks"),
            searxng_result("review", "picks up around episode 5 honestly"),
        ];

        let payload = normalize_gets_good(&results);

        assert_eq!(payload.gets_good_at_episode, Some(4));
        assert!(payload.patience_payoff.unwrap() > 0.0);
    }

    #[test]
    fn normalize_gets_good_none_when_no_episode_mentioned() {
        let results = vec![searxng_result("review", "a fine show overall")];

        let payload = normalize_gets_good(&results);

        assert_eq!(payload.gets_good_at_episode, None);
    }

    #[test]
    fn normalize_news_article_detects_trailer() {
        let article = NewsArticle {
            title: "New trailer for Severance season 3 drops".to_string(),
            url: Some("https://example.invalid/news/1".to_string()),
            description: None,
            published_at: Some("2026-06-01T00:00:00Z".to_string()),
        };

        let (kind, payload) = normalize_news_article(&article).expect("should classify");

        assert_eq!(kind, self::kind::TRAILER);
        assert!(payload.trailer_url.is_some());
        assert!(payload.renewal_status.is_none());
    }

    #[test]
    fn normalize_news_article_detects_renewal() {
        let article = NewsArticle {
            title: "Severance renewed for season 3".to_string(),
            url: Some("https://example.invalid/news/2".to_string()),
            description: Some("Apple TV+ confirms renewal".to_string()),
            published_at: None,
        };

        let (kind, payload) = normalize_news_article(&article).expect("should classify");

        assert_eq!(kind, self::kind::RENEWAL_NEWS);
        assert_eq!(payload.renewal_status.as_deref(), Some("renewed"));
    }

    #[test]
    fn normalize_news_article_detects_cancellation() {
        let article = NewsArticle {
            title: "Show X cancelled after one season".to_string(),
            url: None,
            description: None,
            published_at: None,
        };

        let (kind, payload) = normalize_news_article(&article).expect("should classify");

        assert_eq!(kind, self::kind::RENEWAL_NEWS);
        assert_eq!(payload.renewal_status.as_deref(), Some("cancelled"));
    }

    #[test]
    fn normalize_news_article_none_when_irrelevant() {
        let article = NewsArticle {
            title: "Actor spotted at coffee shop".to_string(),
            url: None,
            description: None,
            published_at: None,
        };

        assert!(normalize_news_article(&article).is_none());
    }

    #[test]
    fn is_fresh_true_within_ttl_false_after() {
        let now = Utc::now();
        let fresh = ExternalEnrichment {
            id: 1,
            media_item_id: 1,
            kind: kind::FORUM_SENTIMENT.to_string(),
            source: source::SEARXNG.to_string(),
            payload: serde_json::json!({}),
            confidence: None,
            fetched_at: now - chrono::Duration::seconds(10),
            ttl_seconds: 3600,
        };
        let expired = ExternalEnrichment {
            fetched_at: now - chrono::Duration::seconds(7200),
            ..fresh.clone()
        };

        assert!(is_fresh(&fresh, now));
        assert!(!is_fresh(&expired, now));
    }

    /// Live-DB test: enrich -> cache upsert -> read-with-TTL round trip.
    /// Gated on `MUSE_TEST_DATABASE_URL` per the crate-wide convention (see
    /// `src/integration_tests.rs`) — skips cleanly (does not fail) when
    /// unset. HTTP is not exercised here; this proves the cache/TTL
    /// contract, not the client layer (covered by `super::client`'s
    /// httpmock tests).
    #[tokio::test]
    async fn enrich_upsert_and_ttl_round_trip() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping enrich_upsert_and_ttl_round_trip \
                 (this is expected in the default test run; the crate does not require a live DB)"
            );
            return;
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

        let suffix = uuid::Uuid::new_v4().simple().to_string();

        let library = repo::library::create(
            &pool,
            &crate::models::library::NewLibrary {
                name: format!("enrichment_test_lib_{suffix}"),
                kind: crate::models::library::LibraryKind::Tv,
                root_folder: "/media/TV/".to_string(),
                source_arr_name: Some("sonarr".to_string()),
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        let metadata = repo::media_metadata::upsert_by_tvdb(
            &pool,
            &crate::models::media_metadata::NewMediaMetadata {
                kind: crate::models::media_metadata::MediaKind::Show,
                tmdb_id: None,
                tvdb_id: Some(format!("tvdb-enrichment-{suffix}")),
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Slow Starter {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: Some("en".to_string()),
                status: Some("continuing".to_string()),
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(30),
                year: Some(2024),
                images: serde_json::json!([]),
            },
        )
        .await
        .expect("upsert media_metadata");

        let media_item = repo::media_item::upsert(
            &pool,
            &crate::models::media_item::NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: "/media/TV/Slow Starter".to_string(),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: None,
                added_at: None,
            },
        )
        .await
        .expect("upsert media item");

        let now = Utc::now();

        // No cached row yet.
        assert!(
            fresh_entry(&pool, media_item.id, kind::DOES_IT_GET_GOOD, source::SEARXNG, now)
                .await
                .expect("lookup should succeed")
                .is_none()
        );

        let results = vec![searxng_result(
            "r/television",
            "it really gets good starting at episode 4, stick with it",
        )];
        let payload = normalize_gets_good(&results);

        let stored = store(
            &pool,
            media_item.id,
            kind::DOES_IT_GET_GOOD,
            source::SEARXNG,
            serde_json::to_value(&payload).expect("payload should serialize"),
            payload.patience_payoff,
            GETS_GOOD_TTL_SECONDS,
        )
        .await
        .expect("upsert should succeed");

        assert_eq!(stored.kind, kind::DOES_IT_GET_GOOD);

        // Now the fresh-read path should find it.
        let found = fresh_entry(&pool, media_item.id, kind::DOES_IT_GET_GOOD, source::SEARXNG, now)
            .await
            .expect("lookup should succeed")
            .expect("row should be cached");
        let round_tripped: GetsGoodPayload =
            serde_json::from_value(found.payload).expect("payload should deserialize");
        assert_eq!(round_tripped.gets_good_at_episode, Some(4));

        // Simulate the TTL having elapsed: a lookup "as of" far in the
        // future should no longer see it as fresh (don't re-fetch fresh
        // rows, but DO refetch expired ones).
        let far_future = now + chrono::Duration::seconds(GETS_GOOD_TTL_SECONDS as i64 + 3600);
        assert!(
            fresh_entry(&pool, media_item.id, kind::DOES_IT_GET_GOOD, source::SEARXNG, far_future)
                .await
                .expect("lookup should succeed")
                .is_none()
        );

        // A second upsert (simulating a re-fetch) replaces the cached
        // payload rather than accumulating a second row.
        let results2 = vec![searxng_result("update", "actually gets good at episode 6 now")];
        let payload2 = normalize_gets_good(&results2);
        store(
            &pool,
            media_item.id,
            kind::DOES_IT_GET_GOOD,
            source::SEARXNG,
            serde_json::to_value(&payload2).expect("payload should serialize"),
            payload2.patience_payoff,
            GETS_GOOD_TTL_SECONDS,
        )
        .await
        .expect("re-upsert should succeed");

        let all_rows = repo::external_enrichment::list_for_media_item(&pool, media_item.id)
            .await
            .expect("list should succeed");
        assert_eq!(
            all_rows.len(),
            1,
            "upsert should replace, not accumulate, per (media_item_id, kind, source)"
        );
    }
}
