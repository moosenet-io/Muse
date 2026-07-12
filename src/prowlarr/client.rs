//! Read-only Prowlarr v1 API client (MUSE-16, blueprint §4b).
//!
//! Muse never talks to trackers/indexers directly — Prowlarr owns indexer
//! credentials and rate limits; this client only ever calls Prowlarr itself,
//! and only ever *reads* (indexer listing, RSS-mode/targeted search). A
//! search is not a grab: there is no download/execute path anywhere in this
//! module, matching the "report-pull, never a grab" framing of the founding
//! spec.
//!
//! Construction is via [`ProwlarrClient::from_config`], which returns `None`
//! when `PROWLARR_URL`/`PROWLARR_API_KEY` aren't configured — callers (and
//! `AppState`) treat Prowlarr as an optional, gracefully-degrading
//! dependency, same as the [`crate::plex::PlexClient`] pattern.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

use super::models::{ProwlarrIndexer, ProwlarrRelease};
use super::rate_limit::RateLimiter;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A typed, read-only Prowlarr client with built-in tracker-etiquette
/// rate-limiting (§4b: "through-Prowlarr only; RSS-pull-first; per-indexer
/// polite intervals; ... hard cap on searches/hour").
#[derive(Debug)]
pub struct ProwlarrClient {
    http: reqwest::Client,
    base_url: String,
    rate_limiter: RateLimiter,
}

impl ProwlarrClient {
    /// Build a client against a specific Prowlarr base URL (e.g.
    /// `http://192.0.2.10:9696` — a fleet-internal address, materialized
    /// from `PROWLARR_URL` at runtime, never hardcoded) and API key.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> MuseResult<Self> {
        let api_key = api_key.into();

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Api-Key",
            HeaderValue::from_str(&api_key)
                .map_err(|e| MuseError::Config(format!("invalid PROWLARR_API_KEY: {e}")))?,
        );

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        let base_url = base_url.into().trim_end_matches('/').to_string();

        Ok(Self {
            http,
            base_url,
            rate_limiter: RateLimiter::new(),
        })
    }

    /// Build a client from `Config` (`PROWLARR_URL`/`PROWLARR_API_KEY`).
    /// Returns `None` when either is unset/empty or when the client fails to
    /// construct — Prowlarr-backed availability features degrade rather than
    /// blocking startup or any other Muse functionality. Never panics.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config.prowlarr_url.clone()?;
        let key = config.prowlarr_api_key.clone()?;

        match Self::new(url, key) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct Prowlarr client; availability features will degrade");
                None
            }
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(String, String)]) -> MuseResult<T> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.http.get(&url).query(query).send().await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("prowlarr request to {url} failed: {body}"),
            });
        }

        serde_json::from_slice::<T>(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse prowlarr response from {url}: {e}"),
        })
    }

    /// `GET /api/v1/indexer` — the full indexer registry (§4b-A: "Refreshed
    /// daily"; no rate-limiting applied here since this is Prowlarr's own
    /// config listing, not a call that touches a tracker).
    pub async fn indexers(&self) -> MuseResult<Vec<ProwlarrIndexer>> {
        self.get("/api/v1/indexer", &[]).await
    }

    /// The *arr "RSS sync" analog (§4b-B): a no-query search against one
    /// indexer + category set, gated by that indexer's configured
    /// `polite_min_interval_secs`. Returns
    /// `Err(MuseError::Conflict)` (not a network error) if called again
    /// before the interval has elapsed — callers should treat that as "skip
    /// this tick," never retry-loop on it.
    pub async fn rss_pull(
        &self,
        indexer_id: i32,
        categories: &[i32],
        min_interval: Duration,
    ) -> MuseResult<Vec<ProwlarrRelease>> {
        self.rate_limiter
            .gate_min_interval(&format!("indexer:{indexer_id}"), min_interval)
            .await?;

        let mut query = vec![("indexerIds".to_string(), indexer_id.to_string())];
        for cat in categories {
            query.push(("categories".to_string(), cat.to_string()));
        }

        self.get("/api/v1/search", &query).await
    }

    /// A bounded, ID-preferred targeted search (§4b-C: "sparingly ... never
    /// fan a text search across all private indexers on a whim"). Gated by a
    /// rolling hourly cap shared across all targeted searches from this
    /// client instance. Prefer `tmdb_id` over `query` when both are
    /// available, per the spec's stated preference for ID-based lookups.
    pub async fn targeted_search(
        &self,
        query_text: Option<&str>,
        tmdb_id: Option<&str>,
        categories: &[i32],
        indexer_ids: &[i32],
        max_searches_per_hour: usize,
    ) -> MuseResult<Vec<ProwlarrRelease>> {
        if query_text.is_none() && tmdb_id.is_none() {
            return Err(MuseError::Config(
                "targeted_search requires at least one of query_text/tmdb_id".to_string(),
            ));
        }

        self.rate_limiter
            .gate_hourly_cap(max_searches_per_hour)
            .await?;

        let mut query = Vec::new();
        if let Some(tmdb) = tmdb_id {
            query.push(("tmdbid".to_string(), tmdb.to_string()));
        } else if let Some(q) = query_text {
            query.push(("query".to_string(), q.to_string()));
        }
        for cat in categories {
            query.push(("categories".to_string(), cat.to_string()));
        }
        for id in indexer_ids {
            query.push(("indexerIds".to_string(), id.to_string()));
        }

        self.get("/api/v1/search", &query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> ProwlarrClient {
        ProwlarrClient::new(server.base_url(), "test-key").expect("client should construct")
    }

    #[tokio::test]
    async fn indexers_parses_registry_entries() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/indexer")
                .header("X-Api-Key", "test-key");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"[
                        {"id": 1, "name": "PublicTracker", "protocol": "torrent", "privacy": "public", "enable": true,
                         "capabilities": {"categories": [{"id": 2000, "name": "Movies"}]}},
                        {"id": 2, "name": "PrivateTracker", "protocol": "torrent", "privacy": "private", "enable": false}
                    ]"#,
                );
        });

        let client = client_for(&server);
        let indexers = client.indexers().await.expect("indexers should parse");

        mock.assert();
        assert_eq!(indexers.len(), 2);
        assert_eq!(indexers[0].name, "PublicTracker");
        assert!(indexers[0].enable);
        assert_eq!(indexers[0].category_ids(), vec![2000]);
        assert!(!indexers[1].enable);
    }

    #[tokio::test]
    async fn rss_pull_sends_no_query_param_and_parses_releases() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/search")
                .query_param("indexerIds", "5")
                .query_param("categories", "2000");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"[
                        {"guid": "abc123", "title": "Movie.Name.2020.1080p.BluRay.x264-GRP",
                         "indexerId": 5, "size": 4300000000, "seeders": 42, "leechers": 3,
                         "downloadUrl": "http://example.invalid/download/abc123",
                         "infoUrl": "http://example.invalid/info/abc123"}
                    ]"#,
                );
        });

        let client = client_for(&server);
        let releases = client
            .rss_pull(5, &[2000], Duration::from_secs(900))
            .await
            .expect("rss_pull should parse");

        mock.assert();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].guid, "abc123");
        assert_eq!(releases[0].seeders, Some(42));
        assert!(!releases[0].is_freeleech());
    }

    #[tokio::test]
    async fn rss_pull_is_rate_limited_per_indexer() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body("[]");
        });

        let client = client_for(&server);
        let interval = Duration::from_secs(900);

        client
            .rss_pull(1, &[2000], interval)
            .await
            .expect("first pull should be allowed");

        let err = client
            .rss_pull(1, &[2000], interval)
            .await
            .expect_err("immediate re-pull of the same indexer should be rate limited");
        match err {
            MuseError::Conflict(_) => {}
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn targeted_search_prefers_tmdb_id_over_free_text() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/search")
                .query_param("tmdbid", "603")
                .query_param_exists("categories");
            then.status(200)
                .header("content-type", "application/json")
                .body("[]");
        });

        let client = client_for(&server);
        client
            .targeted_search(Some("The Matrix"), Some("603"), &[2000], &[1, 2], 30)
            .await
            .expect("targeted search should succeed");

        mock.assert();
    }

    #[tokio::test]
    async fn targeted_search_requires_a_query_or_tmdb_id() {
        let server = MockServer::start();
        let client = client_for(&server);

        let err = client
            .targeted_search(None, None, &[2000], &[1], 30)
            .await
            .expect_err("targeted search with no query/id should be rejected before any request");
        match err {
            MuseError::Config(_) => {}
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn targeted_search_respects_the_hourly_cap() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(200)
                .header("content-type", "application/json")
                .body("[]");
        });

        let client = client_for(&server);

        client
            .targeted_search(Some("query"), None, &[2000], &[1], 1)
            .await
            .expect("first search within budget should succeed");

        let err = client
            .targeted_search(Some("query"), None, &[2000], &[1], 1)
            .await
            .expect_err("second search should exceed the hourly cap of 1");
        match err {
            MuseError::Conflict(_) => {}
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upstream_error_status_is_surfaced_not_panicked() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/indexer");
            then.status(401).body("unauthorized");
        });

        let client = client_for(&server);
        let result = client.indexers().await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_json_does_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/indexer");
            then.status(200)
                .header("content-type", "application/json")
                .body("{not valid json");
        });

        let client = client_for(&server);
        let result = client.indexers().await;

        assert!(result.is_err());
    }

    #[test]
    fn from_config_returns_none_when_unconfigured() {
        let config = test_config(None, None);
        assert!(ProwlarrClient::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_client_when_configured() {
        let config = test_config(
            Some("http://127.0.0.1:9696".to_string()),
            Some("test-key".to_string()),
        );
        assert!(ProwlarrClient::from_config(&config).is_some());
    }

    fn test_config(prowlarr_url: Option<String>, prowlarr_api_key: Option<String>) -> Config {
        Config {
            prowlarr_url,
            prowlarr_api_key,
            ..Default::default()
        }
    }
}
