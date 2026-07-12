//! Typed, read-only HTTP clients for the two enrichment sources MUSE-14
//! implements as a first cut: a fleet SearXNG instance (forum/critic
//! sentiment + "does it get good") and a generic news-search endpoint
//! (renewal/trailer signals).
//!
//! Muse is a standalone service — it does NOT invoke Terminus MCP tools
//! in-process. These clients call the configured HTTP endpoints directly,
//! mirroring the shape of `crate::plex::PlexClient` /
//! `crate::trending::client::TmdbClient`: pure typed HTTP, no persistence,
//! constructed via `from_config` which returns `None` when unconfigured so
//! callers degrade gracefully instead of failing.

use std::time::Duration;

use reqwest::header::ACCEPT;
use serde::Deserialize;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------
// SearXNG (forum/critic sentiment + "does it get good")
// ---------------------------------------------------------------------

/// A single SearXNG result entry (the subset of fields SearXNG's
/// `format=json` response we actually use).
#[derive(Debug, Clone, Deserialize)]
pub struct SearxngResult {
    pub title: String,
    pub url: Option<String>,
    /// SearXNG calls this `content` — a short snippet of the page text.
    pub content: Option<String>,
    /// Which engine produced the hit (e.g. "reddit", "google"), when present.
    pub engine: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

/// A typed, read-only client against a fleet SearXNG instance.
#[derive(Debug, Clone)]
pub struct SearxngClient {
    http: reqwest::Client,
    base_url: String,
}

impl SearxngClient {
    /// Build a client against a specific SearXNG base URL (e.g.
    /// `http://192.168.0.x:8888`, or an httpmock server in tests).
    pub fn new(base_url: impl Into<String>) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        })
    }

    /// Build a client from `Config` (`MUSE_SEARXNG_URL`). Returns `None`
    /// when unset/empty — sentiment/"gets good" enrichment simply becomes
    /// unavailable rather than failing enrichment as a whole.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config.searxng_url.clone()?;

        match Self::new(url) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct SearXNG client; sentiment enrichment will degrade");
                None
            }
        }
    }

    /// `GET /search?q=..&format=json` — free-text query against the fleet
    /// SearXNG instance.
    pub async fn search(&self, query: &str) -> MuseResult<Vec<SearxngResult>> {
        let url = format!("{}/search", self.base_url);

        let resp = self
            .http
            .get(&url)
            .header(ACCEPT, "application/json")
            .query(&[("q", query), ("format", "json")])
            .send()
            .await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("searxng search {query:?} failed: {body}"),
            });
        }

        let parsed: SearxngResponse = serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse searxng response for {query:?}: {e}"),
        })?;

        Ok(parsed.results)
    }
}

// ---------------------------------------------------------------------
// News search (renewal/trailer signals)
// ---------------------------------------------------------------------

/// A single news-article result.
#[derive(Debug, Clone, Deserialize)]
pub struct NewsArticle {
    pub title: String,
    pub url: Option<String>,
    /// Short description/snippet, when the source provides one.
    #[serde(default)]
    pub description: Option<String>,
    /// ISO-8601 publish timestamp, when the source provides one. Left as a
    /// raw string rather than `DateTime<Utc>` since news-endpoint shapes
    /// vary in the wild and a parse failure here shouldn't fail the whole
    /// response.
    #[serde(default)]
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct NewsResponse {
    #[serde(default)]
    articles: Vec<NewsArticle>,
}

/// A typed, read-only client against a configured news-search endpoint
/// (the fleet `news_search`-shaped HTTP surface, or any generically
/// compatible one). An optional API key is sent as a bearer token when
/// configured; many self-hosted news aggregators need none.
#[derive(Debug, Clone)]
pub struct NewsClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl NewsClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        })
    }

    /// Build a client from `Config` (`MUSE_NEWS_URL` + optional
    /// `MUSE_NEWS_API_KEY`). Returns `None` when the URL is unset/empty —
    /// renewal/trailer enrichment simply becomes unavailable.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config.news_url.clone()?;

        match Self::new(url, config.news_api_key.clone()) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct news client; renewal/trailer enrichment will degrade");
                None
            }
        }
    }

    /// `GET /search?q=..` — free-text query against the configured news
    /// endpoint.
    pub async fn search(&self, query: &str) -> MuseResult<Vec<NewsArticle>> {
        let url = format!("{}/search", self.base_url);

        let mut req = self
            .http
            .get(&url)
            .header(ACCEPT, "application/json")
            .query(&[("q", query)]);

        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("news search {query:?} failed: {body}"),
            });
        }

        let parsed: NewsResponse = serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse news response for {query:?}: {e}"),
        })?;

        Ok(parsed.articles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn searxng_for(server: &MockServer) -> SearxngClient {
        SearxngClient::new(server.base_url()).expect("client should construct")
    }

    fn news_for(server: &MockServer) -> NewsClient {
        NewsClient::new(server.base_url(), None).expect("client should construct")
    }

    #[tokio::test]
    async fn searxng_search_parses_results() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/search")
                .query_param("q", "Severance does it get good")
                .query_param("format", "json");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "results": [
                            {"title": "r/television: Severance", "url": "https://example.invalid/1", "content": "it gets really good starting at episode 4", "engine": "reddit"},
                            {"title": "Letterboxd review", "url": "https://example.invalid/2", "content": "slow start but worth it", "engine": "letterboxd"}
                        ]
                    }"#,
                );
        });

        let client = searxng_for(&server);
        let results = client
            .search("Severance does it get good")
            .await
            .expect("search should parse");

        mock.assert();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].engine.as_deref(), Some("reddit"));
    }

    #[tokio::test]
    async fn searxng_upstream_error_is_surfaced_not_panicked() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/search");
            then.status(503).body("unavailable");
        });

        let client = searxng_for(&server);
        let result = client.search("anything").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn searxng_malformed_json_does_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/search");
            then.status(200)
                .header("content-type", "application/json")
                .body("{not valid json");
        });

        let client = searxng_for(&server);
        assert!(client.search("anything").await.is_err());
    }

    #[tokio::test]
    async fn news_search_parses_articles() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/search").query_param("q", "Severance renewed");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "articles": [
                            {"title": "Severance renewed for season 3", "url": "https://example.invalid/news/1", "description": "Apple TV+ confirms renewal", "published_at": "2026-06-01T00:00:00Z"}
                        ]
                    }"#,
                );
        });

        let client = news_for(&server);
        let articles = client
            .search("Severance renewed")
            .await
            .expect("search should parse");

        mock.assert();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Severance renewed for season 3");
    }

    #[tokio::test]
    async fn news_search_sends_bearer_auth_when_configured() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/search")
                .header("authorization", "Bearer test-key");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"articles": []}"#);
        });

        let client = NewsClient::new(server.base_url(), Some("test-key".to_string()))
            .expect("client should construct");
        let articles = client.search("anything").await.expect("search should parse");

        mock.assert();
        assert!(articles.is_empty());
    }

    #[tokio::test]
    async fn news_upstream_error_is_surfaced_not_panicked() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/search");
            then.status(500).body("boom");
        });

        let client = news_for(&server);
        let result = client.search("anything").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    /// Mirrors the `test_config`/blank-`Config` helper pattern used by
    /// `crate::plex::PlexClient`/`crate::trending::client::TmdbClient`/
    /// `crate::prowlarr::ProwlarrClient` tests: every field is explicit
    /// (`Config` has no `Default` impl) so this is the one place that needs
    /// updating if a field is added.
    fn blank_config() -> Config {
        Config::default()
    }

    #[test]
    fn searxng_from_config_returns_none_when_unconfigured() {
        let config = blank_config();
        assert!(SearxngClient::from_config(&config).is_none());
    }

    #[test]
    fn searxng_from_config_builds_client_when_configured() {
        let mut config = blank_config();
        config.searxng_url = Some("http://127.0.0.1:8888".to_string());
        assert!(SearxngClient::from_config(&config).is_some());
    }

    #[test]
    fn news_from_config_returns_none_when_unconfigured() {
        let config = blank_config();
        assert!(NewsClient::from_config(&config).is_none());
    }

    #[test]
    fn news_from_config_builds_client_when_configured() {
        let mut config = blank_config();
        config.news_url = Some("http://127.0.0.1:8889".to_string());
        assert!(NewsClient::from_config(&config).is_some());
    }
}
