//! Read-only Plex Media Server (+ Plex Discover watchlist) HTTP client.
//!
//! MUSE-04: this module is a *pure* typed HTTP client — it makes no writes to
//! Plex and persists nothing itself (ingest/persistence lands in MUSE-05+).
//! It feeds §3.2 (library/metadata), §3.3 (sessions/history — the native
//! Tautulli-replacement tracker), and §3.2/§4-C (accounts, ratings,
//! watchlist) of `specs/S96-muse-foundation.md`.
//!
//! Construction is via [`PlexClient::from_config`], which returns `None` when
//! `PLEX_URL`/`PLEX_TOKEN` aren't configured — callers (and `AppState`) treat
//! Plex as an optional, gracefully-degrading dependency.

mod models;

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT};
use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

pub use models::{Account, Guid, Library, MediaItem, PersonTag, SessionPlayer, SessionUser, Tag};

use models::{AccountContainer, DirectoryContainer, Envelope, MetadataContainer};

/// Plex Discover (plex.tv cloud) base URL, used for the watchlist endpoint
/// only — the account's Plex token is honored the same way as against a
/// local PMS. Not configurable: it's a fixed Plex cloud endpoint, not
/// fleet infra, so it isn't a "hardcoded host" in the secrets-discipline
/// sense (no credential lives in it).
const DISCOVER_BASE_URL: &str = "https://discover.provider.plex.tv";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A typed, read-only Plex client.
#[derive(Debug, Clone)]
pub struct PlexClient {
    http: reqwest::Client,
    base_url: String,
}

impl PlexClient {
    /// Build a client against a specific Plex server base URL (e.g.
    /// `http://192.168.0.x:32400`) and token.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> MuseResult<Self> {
        let token = token.into();

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-Plex-Token",
            HeaderValue::from_str(&token)
                .map_err(|e| MuseError::Config(format!("invalid PLEX_TOKEN: {e}")))?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        let base_url = base_url.into().trim_end_matches('/').to_string();

        Ok(Self { http, base_url })
    }

    /// Build a client from `Config` (`PLEX_URL`/`PLEX_TOKEN`). Returns `None`
    /// when either is unset/empty or when the client fails to construct
    /// (e.g. a malformed token) — Plex features degrade rather than blocking
    /// startup. Never panics.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config.plex_url.clone()?;
        let token = config.plex_token.clone()?;

        match Self::new(url, token) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct Plex client; Plex features will degrade");
                None
            }
        }
    }

    async fn get_absolute<T: DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, &str)],
    ) -> MuseResult<T> {
        let resp = self.http.get(url).query(query).send().await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("plex request to {url} failed: {body}"),
            });
        }

        serde_json::from_slice::<Envelope<T>>(&bytes)
            .map(|env| env.media_container)
            .map_err(|e| MuseError::Upstream {
                status: status.as_u16(),
                message: format!("failed to parse plex response from {url}: {e}"),
            })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> MuseResult<T> {
        let url = format!("{}{}", self.base_url, path);
        self.get_absolute(&url, query).await
    }

    /// `GET /library/sections` — all library sections on the server.
    pub async fn libraries(&self) -> MuseResult<Vec<Library>> {
        let container: DirectoryContainer = self.get("/library/sections", &[]).await?;
        Ok(container.directory)
    }

    /// `GET /library/sections/{section}/all` — every item in a library
    /// section (shallow metadata; call [`Self::metadata`] per-item for full
    /// detail).
    pub async fn library_items(&self, section_key: &str) -> MuseResult<Vec<MediaItem>> {
        let path = format!("/library/sections/{section_key}/all");
        let container: MetadataContainer = self.get(&path, &[]).await?;
        Ok(container.metadata)
    }

    /// `GET /library/metadata/{ratingKey}` — full metadata for a single item
    /// (genres, directors/actors, guids, collections, ratings). Returns
    /// `Ok(None)` if the server has no such item rather than erroring.
    pub async fn metadata(&self, rating_key: &str) -> MuseResult<Option<MediaItem>> {
        let path = format!("/library/metadata/{rating_key}");
        let container: MetadataContainer = self.get(&path, &[]).await?;
        Ok(container.metadata.into_iter().next())
    }

    /// `GET /status/sessions` — currently active playback sessions, for the
    /// session poller (§4-B).
    pub async fn sessions(&self) -> MuseResult<Vec<MediaItem>> {
        let container: MetadataContainer = self.get("/status/sessions", &[]).await?;
        Ok(container.metadata)
    }

    /// `GET /status/sessions/history/all` — thin native playback history,
    /// optionally filtered to one account (`accountID`) for per-account
    /// taste isolation. `None` returns history across all accounts.
    pub async fn history(&self, account_id: Option<&str>) -> MuseResult<Vec<MediaItem>> {
        let query: Vec<(&str, &str)> = match account_id {
            Some(id) => vec![("accountID", id)],
            None => vec![],
        };
        let container: MetadataContainer = self
            .get("/status/sessions/history/all", &query)
            .await?;
        Ok(container.metadata)
    }

    /// `GET /library/onDeck` — continue-watching items across the server.
    pub async fn on_deck(&self) -> MuseResult<Vec<MediaItem>> {
        let container: MetadataContainer = self.get("/library/onDeck", &[]).await?;
        Ok(container.metadata)
    }

    /// `GET /library/recentlyAdded` — recently added items across the
    /// server.
    pub async fn recently_added(&self) -> MuseResult<Vec<MediaItem>> {
        let container: MetadataContainer = self.get("/library/recentlyAdded", &[]).await?;
        Ok(container.metadata)
    }

    /// User-rated items within a library section. Plex has no single
    /// cross-library "my ratings" endpoint; this fetches the section and
    /// filters to items carrying a `userRating`. UNVERIFIED against a live
    /// server — the orchestrator should confirm `userRating` is populated on
    /// `/library/sections/{key}/all` on <host>'s Plex, or whether a per-item
    /// `metadata()` call is required to see it.
    pub async fn ratings(&self, section_key: &str) -> MuseResult<Vec<MediaItem>> {
        let items = self.library_items(section_key).await?;
        Ok(items
            .into_iter()
            .filter(|item| item.user_rating.is_some())
            .collect())
    }

    /// Plex Watchlist, via the Plex Discover cloud API
    /// (`discover.provider.plex.tv/library/sections/watchlist/all`) rather
    /// than the local PMS — Watchlist is account-cloud state, not
    /// server-local. UNVERIFIED: this endpoint/shape is inferred from public
    /// Plex Discover documentation, not exercised against a live token in
    /// this change — the orchestrator should confirm on <host> that the
    /// configured `PLEX_TOKEN` (a server token) is also accepted here, or
    /// whether Watchlist needs a plex.tv *account* token instead.
    pub async fn watchlist(&self) -> MuseResult<Vec<MediaItem>> {
        let url = format!("{DISCOVER_BASE_URL}/library/sections/watchlist/all");
        let container: MetadataContainer = self.get_absolute(&url, &[]).await?;
        Ok(container.metadata)
    }

    /// `GET /accounts` — local Plex Media Server accounts, used to build
    /// §3.2 `accounts` rows (per-account taste isolation; never blend). See
    /// the note on [`models::AccountContainer`] about local-PMS accounts vs.
    /// Plex Home/managed users living on `plex.tv`.
    pub async fn accounts(&self) -> MuseResult<Vec<Account>> {
        let container: AccountContainer = self.get("/accounts", &[]).await?;
        Ok(container.account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> PlexClient {
        PlexClient::new(server.base_url(), "test-token").expect("client should construct")
    }

    #[tokio::test]
    async fn libraries_parses_directory_entries() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/library/sections")
                .header("X-Plex-Token", "test-token");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "MediaContainer": {
                            "size": 2,
                            "Directory": [
                                {"key": "1", "title": "Movies", "type": "movie"},
                                {"key": "2", "title": "TV Shows", "type": "show"}
                            ]
                        }
                    }"#,
                );
        });

        let client = client_for(&server);
        let libraries = client.libraries().await.expect("libraries should parse");

        mock.assert();
        assert_eq!(libraries.len(), 2);
        assert_eq!(libraries[0].key, "1");
        assert_eq!(libraries[0].title, "Movies");
        assert_eq!(libraries[1].library_type.as_deref(), Some("show"));
    }

    #[tokio::test]
    async fn metadata_parses_genres_people_guids_and_collections() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/library/metadata/100");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "MediaContainer": {
                            "size": 1,
                            "Metadata": [{
                                "ratingKey": "100",
                                "type": "movie",
                                "title": "Arrival",
                                "year": 2016,
                                "audienceRating": 8.1,
                                "userRating": 9.0,
                                "Genre": [{"tag": "Drama"}, {"tag": "Sci-Fi"}],
                                "Director": [{"tag": "Denis Villeneuve"}],
                                "Role": [{"tag": "Amy Adams", "role": "Louise Banks"}],
                                "Collection": [{"tag": "Denis Villeneuve Collection"}],
                                "Guid": [
                                    {"id": "tmdb://329865"},
                                    {"id": "imdb://tt2543164"},
                                    {"id": "tvdb://12345"}
                                ]
                            }]
                        }
                    }"#,
                );
        });

        let client = client_for(&server);
        let item = client
            .metadata("100")
            .await
            .expect("metadata should parse")
            .expect("item should be present");

        mock.assert();
        assert_eq!(item.title.as_deref(), Some("Arrival"));
        assert_eq!(item.genres.len(), 2);
        assert_eq!(item.directors[0].tag, "Denis Villeneuve");
        assert_eq!(item.actors[0].role.as_deref(), Some("Louise Banks"));
        assert_eq!(item.collections[0].tag, "Denis Villeneuve Collection");
        assert_eq!(item.tmdb_id(), Some("329865"));
        assert_eq!(item.imdb_id(), Some("tt2543164"));
        assert_eq!(item.tvdb_id(), Some("12345"));
        assert_eq!(item.user_rating, Some(9.0));
    }

    #[tokio::test]
    async fn metadata_returns_none_for_missing_item() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/library/metadata/999");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"MediaContainer": {"size": 0}}"#);
        });

        let client = client_for(&server);
        let item = client.metadata("999").await.expect("request should succeed");

        assert!(item.is_none());
    }

    #[tokio::test]
    async fn sessions_parses_active_playback() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/status/sessions");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "MediaContainer": {
                            "size": 1,
                            "Metadata": [{
                                "ratingKey": "200",
                                "title": "Arrival",
                                "viewOffset": 120000,
                                "sessionKey": "5",
                                "User": {"id": "1", "title": "moose"},
                                "Player": {"title": "Living Room", "state": "playing", "machineIdentifier": "abc123"}
                            }]
                        }
                    }"#,
                );
        });

        let client = client_for(&server);
        let sessions = client.sessions().await.expect("sessions should parse");

        mock.assert();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].view_offset, Some(120000));
        assert_eq!(sessions[0].user.as_ref().unwrap().id.as_deref(), Some("1"));
        assert_eq!(
            sessions[0].player.as_ref().unwrap().state.as_deref(),
            Some("playing")
        );
        assert_eq!(sessions[0].resolved_account_id(), Some("1".to_string()));
    }

    #[tokio::test]
    async fn history_filters_by_account_and_separates_users() {
        let server = MockServer::start();

        let mock_acct1 = server.mock(|when, then| {
            when.method(GET)
                .path("/status/sessions/history/all")
                .query_param("accountID", "1");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "MediaContainer": {
                            "size": 1,
                            "Metadata": [{"ratingKey": "300", "title": "Arrival", "accountID": 1}]
                        }
                    }"#,
                );
        });

        let mock_acct2 = server.mock(|when, then| {
            when.method(GET)
                .path("/status/sessions/history/all")
                .query_param("accountID", "2");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "MediaContainer": {
                            "size": 1,
                            "Metadata": [{"ratingKey": "301", "title": "Contact", "accountID": 2}]
                        }
                    }"#,
                );
        });

        let client = client_for(&server);

        let history1 = client.history(Some("1")).await.expect("history should parse");
        let history2 = client.history(Some("2")).await.expect("history should parse");

        mock_acct1.assert();
        mock_acct2.assert();

        assert_eq!(history1.len(), 1);
        assert_eq!(history1[0].resolved_account_id(), Some("1".to_string()));
        assert_eq!(history2.len(), 1);
        assert_eq!(history2[0].resolved_account_id(), Some("2".to_string()));
        assert_ne!(history1[0].rating_key, history2[0].rating_key);
    }

    #[tokio::test]
    async fn accounts_parses_local_server_accounts() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/accounts");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "MediaContainer": {
                            "size": 2,
                            "Account": [
                                {"id": 1, "name": "moose"},
                                {"id": 2, "name": "kid-profile"}
                            ]
                        }
                    }"#,
                );
        });

        let client = client_for(&server);
        let accounts = client.accounts().await.expect("accounts should parse");

        mock.assert();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].name.as_deref(), Some("moose"));
        assert_eq!(accounts[1].id, 2);
    }

    #[tokio::test]
    async fn ratings_filters_to_user_rated_items() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/library/sections/1/all");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "MediaContainer": {
                            "size": 2,
                            "Metadata": [
                                {"ratingKey": "1", "title": "Rated", "userRating": 8.0},
                                {"ratingKey": "2", "title": "Unrated"}
                            ]
                        }
                    }"#,
                );
        });

        let client = client_for(&server);
        let rated = client.ratings("1").await.expect("ratings should parse");

        assert_eq!(rated.len(), 1);
        assert_eq!(rated[0].title.as_deref(), Some("Rated"));
    }

    #[tokio::test]
    async fn on_deck_and_recently_added_parse() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/library/onDeck");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"MediaContainer": {"size": 1, "Metadata": [{"ratingKey": "1", "title": "Continue"}]}}"#);
        });
        server.mock(|when, then| {
            when.method(GET).path("/library/recentlyAdded");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"MediaContainer": {"size": 1, "Metadata": [{"ratingKey": "2", "title": "New"}]}}"#);
        });

        let client = client_for(&server);
        let on_deck = client.on_deck().await.expect("on_deck should parse");
        let recent = client.recently_added().await.expect("recently_added should parse");

        assert_eq!(on_deck[0].title.as_deref(), Some("Continue"));
        assert_eq!(recent[0].title.as_deref(), Some("New"));
    }

    #[tokio::test]
    async fn upstream_error_status_is_surfaced_not_panicked() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/library/sections");
            then.status(401).body("unauthorized");
        });

        let client = client_for(&server);
        let result = client.libraries().await;

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
            when.method(GET).path("/library/sections");
            then.status(200)
                .header("content-type", "application/json")
                .body("{not valid json");
        });

        let client = client_for(&server);
        let result = client.libraries().await;

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
            tautulli_url: None,
            tautulli_api_key: None,
            radarr_url: None,
            radarr_api_key: None,
            sonarr_url: None,
            sonarr_api_key: None,
            prowlarr_url: None,
            prowlarr_api_key: None,
            tmdb_api_key: None,
            ollama_url: None,
            chord_url: None,
            arr_instances_json: None,
            searxng_url: None,
            news_url: None,
            news_api_key: None,
        };
        assert!(PlexClient::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_client_when_configured() {
        let config = Config {
            database_url: None,
            bind_addr: "0.0.0.0:8090".to_string(),
            log_level: "info".to_string(),
            plex_url: Some("http://127.0.0.1:32400".to_string()),
            plex_token: Some("abc123".to_string()),
            tautulli_url: None,
            tautulli_api_key: None,
            radarr_url: None,
            radarr_api_key: None,
            sonarr_url: None,
            sonarr_api_key: None,
            prowlarr_url: None,
            prowlarr_api_key: None,
            tmdb_api_key: None,
            ollama_url: None,
            chord_url: None,
            arr_instances_json: None,
            searxng_url: None,
            news_url: None,
            news_api_key: None,
        };
        assert!(PlexClient::from_config(&config).is_some());
    }
}
