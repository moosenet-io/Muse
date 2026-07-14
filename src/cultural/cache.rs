//! MUSEX-07 (Plane TERM #383): [`TrendCache`] — the rate-limit-respecting
//! TTL cache in front of a [`TrendSource`]. A repeated `trending`/`talk`
//! pull for the same query, within the configured TTL window, is served
//! from the in-memory cache rather than re-hitting TMDb/Trakt — the AC's
//! "trend cache respects API rate limits" requirement.
//!
//! Deliberately NOT itself a [`TrendSource`] impl (a caching decorator
//! around a trait object, à la a decorator pattern, rather than a second
//! trait implementer) — this keeps the cache key logic in one obvious
//! place (`TrendCache::trending`/`talk` take the inner source explicitly)
//! rather than hiding it behind another layer of trait dispatch.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::MuseResult;

use super::source::{TalkEntry, TalkQuery, TrendEntry, TrendQuery, TrendSource};

/// Cache key for a `trending` pull — the full set of [`TrendQuery`] fields
/// that affect the result.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TrendCacheKey {
    region: String,
    window: &'static str,
    kind: Option<&'static str>,
}

impl From<&TrendQuery> for TrendCacheKey {
    fn from(q: &TrendQuery) -> Self {
        Self {
            region: q.region.clone(),
            window: q.window.as_str(),
            kind: q.kind.map(|k| match k {
                crate::models::media_metadata::MediaKind::Movie => "movie",
                crate::models::media_metadata::MediaKind::Show => "show",
            }),
        }
    }
}

/// Cache key for a `talk` pull — the sorted+deduped set of external ids
/// requested (order-independent: the same id set in a different `Vec`
/// order is the same cache entry).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TalkCacheKey(Vec<String>);

impl From<&TalkQuery> for TalkCacheKey {
    fn from(q: &TalkQuery) -> Self {
        let mut ids = q.external_ids.clone();
        ids.sort();
        ids.dedup();
        Self(ids)
    }
}

struct CacheEntry<T> {
    fetched_at: Instant,
    value: T,
}

/// A TTL cache in front of any [`TrendSource`]. `ttl` is normally
/// `Config::trend_cache_ttl_secs` (`MUSE_TREND_CACHE_TTL_SECS`, default 1h)
/// — see that field's doc for why an hour is the default cadence.
pub struct TrendCache {
    ttl: Duration,
    trending: Mutex<HashMap<TrendCacheKey, CacheEntry<Vec<TrendEntry>>>>,
    talk: Mutex<HashMap<TalkCacheKey, CacheEntry<Vec<TalkEntry>>>>,
}

impl TrendCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            trending: Mutex::new(HashMap::new()),
            talk: Mutex::new(HashMap::new()),
        }
    }

    /// `trending(source, query)`: return the cached result when a pull for
    /// this exact query landed within `ttl`, otherwise call
    /// `source.trending(query)`, cache the result, and return it. A source
    /// error is never cached (so a transient upstream failure doesn't wedge
    /// the cache into returning `Err` for the rest of the TTL window).
    pub async fn trending(
        &self,
        source: &dyn TrendSource,
        query: &TrendQuery,
    ) -> MuseResult<Vec<TrendEntry>> {
        let key = TrendCacheKey::from(query);

        if let Some(cached) = self.fresh_trending(&key) {
            return Ok(cached);
        }

        let value = source.trending(query).await?;
        self.trending.lock().unwrap().insert(
            key,
            CacheEntry {
                fetched_at: Instant::now(),
                value: value.clone(),
            },
        );
        Ok(value)
    }

    /// Same TTL-cache posture as [`Self::trending`], keyed on the
    /// (order-independent) set of requested external ids.
    pub async fn talk(
        &self,
        source: &dyn TrendSource,
        query: &TalkQuery,
    ) -> MuseResult<Vec<TalkEntry>> {
        let key = TalkCacheKey::from(query);

        if let Some(cached) = self.fresh_talk(&key) {
            return Ok(cached);
        }

        let value = source.talk(query).await?;
        self.talk.lock().unwrap().insert(
            key,
            CacheEntry {
                fetched_at: Instant::now(),
                value: value.clone(),
            },
        );
        Ok(value)
    }

    fn fresh_trending(&self, key: &TrendCacheKey) -> Option<Vec<TrendEntry>> {
        let guard = self.trending.lock().unwrap();
        let entry = guard.get(key)?;
        if entry.fetched_at.elapsed() < self.ttl {
            Some(entry.value.clone())
        } else {
            None
        }
    }

    fn fresh_talk(&self, key: &TalkCacheKey) -> Option<Vec<TalkEntry>> {
        let guard = self.talk.lock().unwrap();
        let entry = guard.get(key)?;
        if entry.fetched_at.elapsed() < self.ttl {
            Some(entry.value.clone())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cultural::source::MockTrendSource;
    use crate::models::media_metadata::MediaKind;
    use crate::trending::TrendingWindow;

    fn entry() -> TrendEntry {
        TrendEntry {
            external_id: "12345".to_string(),
            kind: MediaKind::Show,
            title: "Severance".to_string(),
            year: Some(2022),
            popularity: 99.0,
        }
    }

    fn query() -> TrendQuery {
        TrendQuery {
            region: "US".to_string(),
            window: TrendingWindow::Day,
            kind: None,
        }
    }

    /// The rate-limit-respecting core behavior: a second `trending` call
    /// for the SAME query, within the TTL, must NOT re-hit the source.
    #[tokio::test]
    async fn repeated_call_within_ttl_does_not_re_hit_the_source() {
        let mock = MockTrendSource::new(vec![entry()], vec![]);
        let cache = TrendCache::new(Duration::from_secs(60));

        let first = cache.trending(&mock, &query()).await.expect("first pull");
        let second = cache.trending(&mock, &query()).await.expect("second pull");

        assert_eq!(first, second);
        assert_eq!(
            mock.trending_call_count(),
            1,
            "a cache hit within the TTL must not call the underlying TrendSource again"
        );
    }

    #[tokio::test]
    async fn call_after_ttl_expiry_does_re_hit_the_source() {
        let mock = MockTrendSource::new(vec![entry()], vec![]);
        let cache = TrendCache::new(Duration::from_millis(5));

        cache.trending(&mock, &query()).await.expect("first pull");
        tokio::time::sleep(Duration::from_millis(20)).await;
        cache
            .trending(&mock, &query())
            .await
            .expect("second pull, past TTL");

        assert_eq!(
            mock.trending_call_count(),
            2,
            "a pull past the TTL window must re-hit the underlying TrendSource"
        );
    }

    #[tokio::test]
    async fn distinct_queries_are_cached_independently() {
        let mock = MockTrendSource::new(vec![entry()], vec![]);
        let cache = TrendCache::new(Duration::from_secs(60));

        cache.trending(&mock, &query()).await.expect("day pull");
        let mut weekly = query();
        weekly.window = TrendingWindow::Week;
        cache.trending(&mock, &weekly).await.expect("week pull");

        assert_eq!(
            mock.trending_call_count(),
            2,
            "a different window is a different cache key and must call the source"
        );
    }

    #[tokio::test]
    async fn talk_cache_respects_ttl_and_is_order_independent_on_ids() {
        let mock = MockTrendSource::new(
            vec![],
            vec![TalkEntry {
                external_id: "12345".to_string(),
                talk_score: 0.8,
                comment_count: Some(40),
                rating_count: None,
            }],
        );
        let cache = TrendCache::new(Duration::from_secs(60));

        let q1 = TalkQuery {
            external_ids: vec!["12345".to_string(), "603".to_string()],
        };
        let q2 = TalkQuery {
            external_ids: vec!["603".to_string(), "12345".to_string()],
        };

        cache.talk(&mock, &q1).await.expect("first talk pull");
        cache
            .talk(&mock, &q2)
            .await
            .expect("second talk pull, same id set reordered");

        assert_eq!(
            mock.talk_call_count(),
            1,
            "the same id set in a different Vec order must hit the cache, not the source"
        );
    }
}
