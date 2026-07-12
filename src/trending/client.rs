//! Read-only TMDb (The Movie Database) HTTP client (MUSE-19).
//!
//! Mirrors `crate::plex::PlexClient`'s shape: a pure typed HTTP client that
//! persists nothing itself (the ingest routine in `trending::mod` owns
//! persistence), and constructs via [`TmdbClient::from_config`], which
//! returns `None` when `TMDB_API_KEY` isn't configured — trending features
//! degrade gracefully rather than blocking startup.

use std::time::Duration;

use reqwest::header::ACCEPT;
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

use super::models::{ResultsEnvelope, TmdbTitle, WatchProvidersEnvelope};
pub use super::models::RegionProviders;

const DEFAULT_BASE_URL: &str = "https://api.themoviedb.org/3";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// TMDb's `movie` vs `tv` media-type path segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmdbMediaType {
    Movie,
    Tv,
}

impl TmdbMediaType {
    fn as_path(&self) -> &'static str {
        match self {
            TmdbMediaType::Movie => "movie",
            TmdbMediaType::Tv => "tv",
        }
    }
}

/// TMDb's `/trending/{media_type}/{window}` day-vs-week axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendingWindow {
    Day,
    Week,
}

impl TrendingWindow {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrendingWindow::Day => "day",
            TrendingWindow::Week => "week",
        }
    }
}

/// A typed, read-only TMDb client.
#[derive(Debug, Clone)]
pub struct TmdbClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl TmdbClient {
    /// Build a client against a specific TMDb-compatible base URL (e.g. the
    /// real `https://api.themoviedb.org/3`, or an httpmock server in tests)
    /// and API key.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        })
    }

    /// Build a client from `Config` (`TMDB_API_KEY`) against the real TMDb
    /// API. Returns `None` when unset/empty — callers (and the trending
    /// ingest routine) treat TMDb as an optional, gracefully-degrading
    /// dependency, same as `PlexClient::from_config`.
    pub fn from_config(config: &Config) -> Option<Self> {
        let api_key = config.tmdb_api_key.clone()?;

        match Self::new(DEFAULT_BASE_URL, api_key) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct TMDb client; trending features will degrade");
                None
            }
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> MuseResult<T> {
        let url = format!("{}{}", self.base_url, path);

        let mut all_query: Vec<(&str, &str)> = vec![("api_key", self.api_key.as_str())];
        all_query.extend_from_slice(query);

        let resp = self
            .http
            .get(&url)
            .header(ACCEPT, "application/json")
            .query(&all_query)
            .send()
            .await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("tmdb request to {path} failed: {body}"),
            });
        }

        serde_json::from_slice::<T>(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse tmdb response from {path}: {e}"),
        })
    }

    /// `GET /trending/{media_type}/{window}` — the day-one trending source.
    pub async fn trending(
        &self,
        media_type: TmdbMediaType,
        window: TrendingWindow,
    ) -> MuseResult<Vec<TmdbTitle>> {
        let path = format!("/trending/{}/{}", media_type.as_path(), window.as_str());
        let envelope: ResultsEnvelope<TmdbTitle> = self.get(&path, &[]).await?;
        Ok(envelope.results)
    }

    /// `GET /movie|tv/popular` — region-configurable.
    pub async fn popular(
        &self,
        media_type: TmdbMediaType,
        region: Option<&str>,
    ) -> MuseResult<Vec<TmdbTitle>> {
        let path = format!("/{}/popular", media_type.as_path());
        let query: Vec<(&str, &str)> = match region {
            Some(r) => vec![("region", r)],
            None => vec![],
        };
        let envelope: ResultsEnvelope<TmdbTitle> = self.get(&path, &query).await?;
        Ok(envelope.results)
    }

    /// `GET /movie|tv/{id}/watch/providers` — where a title streams, keyed
    /// by ISO 3166-1 region code.
    pub async fn watch_providers(
        &self,
        media_type: TmdbMediaType,
        tmdb_id: &str,
    ) -> MuseResult<std::collections::HashMap<String, RegionProviders>> {
        let path = format!("/{}/{}/watch/providers", media_type.as_path(), tmdb_id);
        let envelope: WatchProvidersEnvelope = self.get(&path, &[]).await?;
        Ok(envelope.results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> TmdbClient {
        TmdbClient::new(server.base_url(), "test-key").expect("client should construct")
    }

    #[tokio::test]
    async fn trending_parses_results_and_sends_api_key() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/trending/movie/day")
                .query_param("api_key", "test-key");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "page": 1,
                        "results": [
                            {"id": 27205, "title": "Inception", "release_date": "2010-07-15", "popularity": 80.5},
                            {"id": 603, "title": "The Matrix", "release_date": "1999-03-30", "popularity": 60.1}
                        ],
                        "total_pages": 1,
                        "total_results": 2
                    }"#,
                );
        });

        let client = client_for(&server);
        let titles = client
            .trending(TmdbMediaType::Movie, TrendingWindow::Day)
            .await
            .expect("trending should parse");

        mock.assert();
        assert_eq!(titles.len(), 2);
        assert_eq!(titles[0].display_title(), Some("Inception"));
        assert_eq!(titles[0].year(), Some(2010));
    }

    #[tokio::test]
    async fn popular_sends_region_query_param() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/tv/popular")
                .query_param("region", "US");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"results": [{"id": 1399, "name": "Game of Thrones", "first_air_date": "2011-04-17"}]}"#);
        });

        let client = client_for(&server);
        let titles = client
            .popular(TmdbMediaType::Tv, Some("US"))
            .await
            .expect("popular should parse");

        mock.assert();
        assert_eq!(titles.len(), 1);
        assert_eq!(titles[0].display_title(), Some("Game of Thrones"));
    }

    #[tokio::test]
    async fn watch_providers_parses_region_map() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/movie/603/watch/providers");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "id": 603,
                        "results": {
                            "US": {
                                "link": "https://www.themoviedb.org/movie/603/watch",
                                "flatrate": [{"provider_id": 8, "provider_name": "Netflix"}]
                            }
                        }
                    }"#,
                );
        });

        let client = client_for(&server);
        let regions = client
            .watch_providers(TmdbMediaType::Movie, "603")
            .await
            .expect("watch_providers should parse");

        let us = regions.get("US").expect("US region present");
        assert_eq!(us.flatrate[0].provider_name, "Netflix");
    }

    #[tokio::test]
    async fn upstream_error_status_is_surfaced_not_panicked() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/trending/movie/day");
            then.status(401).body("invalid api key");
        });

        let client = client_for(&server);
        let result = client.trending(TmdbMediaType::Movie, TrendingWindow::Day).await;

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
            when.method(GET).path("/trending/movie/day");
            then.status(200)
                .header("content-type", "application/json")
                .body("{not valid json");
        });

        let client = client_for(&server);
        let result = client.trending(TmdbMediaType::Movie, TrendingWindow::Day).await;

        assert!(result.is_err());
    }

    #[test]
    fn from_config_returns_none_when_unconfigured() {
        let config = Config {
            database_url: None,
            bind_addr: "0.0.0.0:8090".to_string(),
            log_level: "info".to_string(),
            plex_url: None,
            plex_token: None,
            plex_poll_secs: None,
            tautulli_url: None,
            tautulli_api_key: None,
            radarr_url: None,
            radarr_api_key: None,
            sonarr_url: None,
            sonarr_api_key: None,
            prowlarr_url: None,
            prowlarr_api_key: None,
            tmdb_api_key: None,
            arr_instances_json: None,
            ollama_url: None,
            chord_url: None,
        };
        assert!(TmdbClient::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_client_when_configured() {
        let config = Config {
            database_url: None,
            bind_addr: "0.0.0.0:8090".to_string(),
            log_level: "info".to_string(),
            plex_url: None,
            plex_token: None,
            plex_poll_secs: None,
            tautulli_url: None,
            tautulli_api_key: None,
            radarr_url: None,
            radarr_api_key: None,
            sonarr_url: None,
            sonarr_api_key: None,
            prowlarr_url: None,
            prowlarr_api_key: None,
            tmdb_api_key: Some("abc123".to_string()),
            arr_instances_json: None,
            ollama_url: None,
            chord_url: None,
        };
        assert!(TmdbClient::from_config(&config).is_some());
    }
}
