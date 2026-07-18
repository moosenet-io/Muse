//! Read-only TheTVDB v4 HTTP client (MUSEL-A1) — a [`MetadataProvider`]
//! implementation.
//!
//! Mirrors `trending::client::TmdbClient`'s shape (`struct { http,
//! base_url, api_key }`, `new`, `from_config` -> `Option<Self>`), with one
//! addition TMDb doesn't need: TheTVDB v4 requires a `POST /login` exchange
//! (apikey [+ pin] -> a short-lived bearer token) before any other call.
//! The token/re-auth handling follows `download::qbit::QbitClient`'s
//! transparent-single-reauth shape (cache the token behind a shared
//! `RwLock`; on a `401`, re-login exactly once and retry).
//!
//! TheTVDB v4 API shape assumed here (adapted from the public v4 OpenAPI
//! spec, not independently verified against a live account in this
//! session — see the worktree report for exactly what was assumed). Since
//! that shape is unverified, the response DTOs below are deliberately
//! tolerant of the older v3-style field names in case a live TheTVDB
//! response (or an intermediary) doesn't match the v4 shape exactly: the
//! title field accepts `name` (v4) or `seriesName`/`movieName` (v3-style)
//! via `#[serde(alias = ...)]`, the image field accepts either a plain URL
//! string (v4) or an object carrying a `url`/`image_url` field (v3-style),
//! and the network field accepts either a flat string or an
//! `originalNetwork: {name}` object. A live-API field-shape verification
//! against a real TheTVDB v4 account remains a follow-up once
//! `MUSE_TVDB_API_KEY` is provisioned (operator ops) — until then this
//! tolerant parsing is the defensive middle ground.
//! - `POST /login` body `{"apikey": "...", "pin": "..."}` (pin omitted when
//!   unset) -> `{"status": "success", "data": {"token": "..."}}`.
//! - Authenticated calls send `Authorization: Bearer <token>`.
//! - [`MetadataProvider::resolve_by_id`] calls the EXTENDED record
//!   endpoints — `GET /series/{id}/extended` / `GET /movies/{id}/extended`
//!   — not the base `/series/{id}` / `/movies/{id}` ones. TheTVDB v4's base
//!   record omits `overview`, `genres`, `originalNetwork`, and (critically)
//!   `remoteIds` — the imdb/tmdb id bridge MUSEL-A2's resolver depends on;
//!   only the extended record carries them. `name`/`image`/`score`/
//!   `firstAired`/`year` are present on both, so one `/extended` call per
//!   resolve is sufficient — no separate base-record call is made.
//! - `GET /search?query=..&type=series|movie` -> `{"status": "...", "data":
//!   [..search hits..]}`. Search hits are deliberately thin (discovery
//!   only) — a caller that needs the full record calls `search` to find a
//!   candidate id, then `resolve_by_id` (which hits `/extended`) for the
//!   complete `ProviderMetadata`.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::AUTHORIZATION;
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

use super::{MediaKind, MetadataProvider, ProviderImages, ProviderMetadata};

pub(crate) const DEFAULT_BASE_URL: &str = "https://api4.thetvdb.com/v4";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const LOGIN_PATH: &str = "/login";

/// A typed, read-only TheTVDB v4 client. Cheap to `Clone` (the cached bearer
/// token lives behind an `Arc<RwLock<_>>` internally via `reqwest::Client`'s
/// own `Arc` plus our own `Arc<RwLock<Option<String>>>`).
#[derive(Clone)]
pub struct TvdbClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pin: Option<String>,
    token: std::sync::Arc<RwLock<Option<String>>>,
}

// Manual `Debug` (not `#[derive(Debug)]`) so a stray `tracing::debug!(client
// = ?tvdb, ...)` can never print the API key or the live bearer token —
// same posture as `download::qbit::QbitClient`'s manual `Debug`.
impl std::fmt::Debug for TvdbClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TvdbClient")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("pin", &self.pin.as_ref().map(|_| "<redacted>"))
            .field("token", &"<redacted>")
            .finish()
    }
}

impl TvdbClient {
    /// Build a client against a specific TheTVDB-v4-compatible base URL
    /// (the real `https://api4.thetvdb.com/v4`, or an httpmock server in
    /// tests) and credentials.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        pin: Option<String>,
    ) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            pin,
            token: std::sync::Arc::new(RwLock::new(None)),
        })
    }

    /// Build a client from `Config` (`MUSE_TVDB_API_KEY`[/`MUSE_TVDB_PIN`]).
    /// Returns `None` when unset/empty — same graceful-degrade posture as
    /// `TmdbClient::from_config`: TheTVDB is an optional metadata provider,
    /// never a startup-blocking dependency.
    pub fn from_config(config: &Config) -> Option<Self> {
        let tvdb = config.tvdb()?;

        match Self::new(
            tvdb.base_url,
            tvdb.api_key.expose().to_string(),
            tvdb.pin.map(|p| p.expose().to_string()),
        ) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct TheTVDB client; TVDB metadata will degrade");
                None
            }
        }
    }

    /// `POST /login` — exchange the api key (+ optional pin) for a bearer
    /// token. Called lazily by [`Self::ensure_token`] and again, once, on a
    /// transparent re-auth after a `401` — never speculatively per-request
    /// when a cached token is already held.
    async fn login(&self) -> MuseResult<String> {
        let url = format!("{}{LOGIN_PATH}", self.base_url);

        let resp = self
            .http
            .post(&url)
            .json(&LoginRequest {
                apikey: self.api_key.clone(),
                pin: self.pin.clone(),
            })
            .send()
            .await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("tvdb login failed: {body}"),
            });
        }

        let envelope: LoginEnvelope = serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse tvdb login response: {e}"),
        })?;

        let token = envelope.data.token;
        *self.token.write().await = Some(token.clone());
        Ok(token)
    }

    /// Returns the cached bearer token if one is held, logging in for the
    /// first time otherwise. Does NOT validate that a cached token is still
    /// accepted server-side — that's what the `401`-triggered re-auth in
    /// [`Self::get_with_reauth`] handles.
    async fn ensure_token(&self) -> MuseResult<String> {
        if let Some(token) = self.token.read().await.clone() {
            return Ok(token);
        }
        self.login().await
    }

    /// `GET {path}?{query}` with a bearer token, retrying **exactly once**
    /// with a fresh token if the first attempt comes back `401
    /// Unauthorized` (an expired/invalidated token — mirrors
    /// `QbitClient::send_with_reauth`'s `403` handling). A second `401` (or
    /// any other status) is surfaced to the caller as a typed error, never
    /// a panic.
    async fn get_with_reauth(&self, path: &str, query: &[(&str, &str)]) -> MuseResult<(u16, Vec<u8>)> {
        let url = format!("{}{path}", self.base_url);

        let token = self.ensure_token().await?;
        let resp = self
            .http
            .get(&url)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .query(query)
            .send()
            .await?;

        if resp.status() == StatusCode::UNAUTHORIZED {
            let fresh_token = self.login().await?;
            let resp = self
                .http
                .get(&url)
                .header(AUTHORIZATION, format!("Bearer {fresh_token}"))
                .query(query)
                .send()
                .await?;
            let status = resp.status().as_u16();
            let bytes = resp.bytes().await?.to_vec();
            return Ok((status, bytes));
        }

        let status = resp.status().as_u16();
        let bytes = resp.bytes().await?.to_vec();
        Ok((status, bytes))
    }

    async fn get_data<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> MuseResult<Option<T>> {
        let (status, bytes) = self.get_with_reauth(path, query).await?;

        if status == StatusCode::NOT_FOUND.as_u16() {
            return Ok(None);
        }
        if status < 200 || status >= 300 {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status,
                message: format!("tvdb request to {path} failed: {body}"),
            });
        }

        let envelope: DataEnvelope<T> = serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
            status,
            message: format!("failed to parse tvdb response from {path}: {e}"),
        })?;
        Ok(Some(envelope.data))
    }

    /// `GET /series/{id}/extended` — the EXTENDED record, not the base
    /// `/series/{id}` one: the base record omits `overview`, `genres`,
    /// `originalNetwork`, and (critically) `remoteIds` — the imdb/tmdb id
    /// bridge `resolve_by_id` callers (MUSEL-A2's resolver) depend on.
    /// Resolving via the base endpoint would silently return those fields
    /// NULL. See the module doc.
    async fn get_series(&self, id: &str) -> MuseResult<Option<TvdbRecord>> {
        self.get_data(&format!("/series/{id}/extended"), &[]).await
    }

    /// `GET /movies/{id}/extended` — see [`Self::get_series`]'s doc; the
    /// same base-vs-extended distinction applies to movies.
    async fn get_movie(&self, id: &str) -> MuseResult<Option<TvdbRecord>> {
        self.get_data(&format!("/movies/{id}/extended"), &[]).await
    }
}

#[async_trait]
impl MetadataProvider for TvdbClient {
    async fn resolve_by_id(
        &self,
        kind: MediaKind,
        provider_id: &str,
    ) -> MuseResult<Option<ProviderMetadata>> {
        let record = match kind {
            MediaKind::Series => self.get_series(provider_id).await?,
            MediaKind::Movie => self.get_movie(provider_id).await?,
        };
        Ok(record.map(|r| r.into_metadata(provider_id)))
    }

    /// `GET /search?query=..&type=series|movie`.
    async fn search(&self, query: &str, kind: MediaKind) -> MuseResult<Vec<ProviderMetadata>> {
        let type_param = match kind {
            MediaKind::Series => "series",
            MediaKind::Movie => "movie",
        };

        let (status, bytes) = self
            .get_with_reauth("/search", &[("query", query), ("type", type_param)])
            .await?;

        if status < 200 || status >= 300 {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status,
                message: format!("tvdb search failed: {body}"),
            });
        }

        let envelope: DataEnvelope<Vec<TvdbSearchHit>> =
            serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
                status,
                message: format!("failed to parse tvdb search response: {e}"),
            })?;

        Ok(envelope.data.into_iter().map(TvdbSearchHit::into_metadata).collect())
    }
}

#[derive(Debug, serde::Serialize)]
struct LoginRequest {
    apikey: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pin: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LoginEnvelope {
    data: LoginData,
}

#[derive(Debug, Deserialize)]
struct LoginData {
    token: String,
}

#[derive(Debug, Deserialize)]
struct DataEnvelope<T> {
    data: T,
}

/// Tolerant image field: TheTVDB v4 documents a plain URL string, but a v3-
/// style response (or an intermediary/proxy) may instead send an object
/// carrying a `url`/`image_url` field. Untagged so serde tries each variant
/// in order and normalizes either shape down to the URL string via
/// [`Self::into_url`].
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TvdbImage {
    Url(String),
    Object {
        #[serde(alias = "image_url", default)]
        url: Option<String>,
    },
}

impl TvdbImage {
    fn into_url(self) -> Option<String> {
        match self {
            TvdbImage::Url(s) => Some(s),
            TvdbImage::Object { url } => url,
        }
    }
}

/// Tolerant network field: v4's base record nests it as `originalNetwork:
/// {name}`, but a v3-style response may send a flat network name string
/// instead. Untagged for the same reason as [`TvdbImage`].
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TvdbNetworkField {
    Name(String),
    Object {
        #[serde(default)]
        name: Option<String>,
    },
}

impl TvdbNetworkField {
    fn into_name(self) -> Option<String> {
        match self {
            TvdbNetworkField::Name(s) => Some(s),
            TvdbNetworkField::Object { name } => name,
        }
    }
}

/// A TheTVDB v4 `/series/{id}` or `/movies/{id}` base record — permissive
/// (`Option`/`#[serde(default)]`) like `trending::models::TmdbTitle`, since
/// not every field is populated for every title. `name`/`image`/
/// `originalNetwork` tolerate the v3-style field-name/shape variants too
/// (see the module doc).
#[derive(Debug, Deserialize)]
struct TvdbRecord {
    #[serde(alias = "seriesName", alias = "movieName", default)]
    name: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    image: Option<TvdbImage>,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    #[serde(rename = "firstAired")]
    first_aired: Option<String>,
    #[serde(default)]
    year: Option<String>,
    #[serde(default)]
    genres: Vec<TvdbGenre>,
    #[serde(alias = "network", rename = "originalNetwork", default)]
    original_network: Option<TvdbNetworkField>,
    #[serde(default)]
    #[serde(rename = "remoteIds")]
    remote_ids: Vec<TvdbRemoteId>,
}

#[derive(Debug, Deserialize)]
struct TvdbGenre {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TvdbRemoteId {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "sourceName", default)]
    source_name: Option<String>,
}

impl TvdbRecord {
    fn into_metadata(self, tvdb_id: &str) -> ProviderMetadata {
        let mut provider_ids = std::collections::HashMap::new();
        provider_ids.insert("tvdb".to_string(), tvdb_id.to_string());
        for remote in &self.remote_ids {
            if let (Some(id), Some(source)) = (&remote.id, &remote.source_name) {
                // TheTVDB's remoteIds source names are things like "IMDB",
                // "TheMovieDB.com" — normalize to the lower-cased short keys
                // this crate's `ProviderMetadata::provider_ids` uses
                // elsewhere (`tvdb`/`tmdb`/`imdb`).
                let key = match source.to_ascii_lowercase().as_str() {
                    s if s.contains("imdb") => Some("imdb"),
                    s if s.contains("themoviedb") || s.contains("tmdb") => Some("tmdb"),
                    _ => None,
                };
                if let Some(key) = key {
                    provider_ids.insert(key.to_string(), id.clone());
                }
            }
        }

        let first_aired = self.first_aired.clone();
        ProviderMetadata {
            provider_ids,
            title: self.name,
            overview: self.overview,
            genres: self
                .genres
                .into_iter()
                .filter_map(|g| g.name)
                .collect(),
            images: ProviderImages {
                poster_url: self.image.and_then(TvdbImage::into_url),
                backdrop_url: None,
            },
            rating: self.score,
            first_aired: first_aired.clone(),
            year: self
                .year
                .as_deref()
                .and_then(|y| y.parse::<i32>().ok())
                .or_else(|| first_aired.as_deref().and_then(|d| d.get(0..4)).and_then(|y| y.parse().ok())),
            network: self.original_network.and_then(TvdbNetworkField::into_name),
            // MUSEL-A2: TheTVDB's extended record has no dedicated
            // free-text keywords field — left empty, same posture as the
            // MUSEL-A2 TMDb adapter (see `ProviderMetadata::keywords`'s
            // doc comment).
            keywords: Vec::new(),
        }
    }
}

/// A TheTVDB v4 `/search` hit — a different, flatter shape than the base
/// record (search results embed some fields TheTVDB otherwise nests, e.g.
/// `tvdb_id` alongside a compound `id` like `"series-121361"`).
#[derive(Debug, Deserialize)]
struct TvdbSearchHit {
    #[serde(default)]
    tvdb_id: Option<String>,
    #[serde(alias = "seriesName", alias = "movieName", default)]
    name: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(alias = "image", default)]
    image_url: Option<TvdbImage>,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    first_air_time: Option<String>,
    #[serde(default)]
    year: Option<String>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    remote_ids: Vec<TvdbRemoteId>,
}

impl TvdbSearchHit {
    fn into_metadata(self) -> ProviderMetadata {
        let mut provider_ids = std::collections::HashMap::new();
        if let Some(id) = &self.tvdb_id {
            provider_ids.insert("tvdb".to_string(), id.clone());
        }
        for remote in &self.remote_ids {
            if let (Some(id), Some(source)) = (&remote.id, &remote.source_name) {
                let key = match source.to_ascii_lowercase().as_str() {
                    s if s.contains("imdb") => Some("imdb"),
                    s if s.contains("themoviedb") || s.contains("tmdb") => Some("tmdb"),
                    _ => None,
                };
                if let Some(key) = key {
                    provider_ids.insert(key.to_string(), id.clone());
                }
            }
        }

        ProviderMetadata {
            provider_ids,
            title: self.name,
            overview: self.overview,
            genres: self.genres,
            images: ProviderImages {
                poster_url: self.image_url.and_then(TvdbImage::into_url),
                backdrop_url: None,
            },
            rating: self.score,
            first_aired: self.first_air_time.clone(),
            year: self
                .year
                .as_deref()
                .and_then(|y| y.parse::<i32>().ok())
                .or_else(|| self.first_air_time.as_deref().and_then(|d| d.get(0..4)).and_then(|y| y.parse().ok())),
            network: self.network,
            keywords: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> TvdbClient {
        TvdbClient::new(server.base_url(), "test-key", None).expect("client should construct")
    }

    fn login_mock<'a>(server: &'a MockServer, token: &str) -> httpmock::Mock<'a> {
        server.mock(|when, then| {
            when.method(POST)
                .path(LOGIN_PATH)
                .json_body(serde_json::json!({"apikey": "test-key"}));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "status": "success",
                    "data": {"token": token}
                }));
        })
    }

    #[tokio::test]
    async fn login_captures_bearer_token() {
        let server = MockServer::start();
        let mock = login_mock(&server, "test-token-abc");

        let client = client_for(&server);
        let token = client.login().await.expect("login should succeed");

        mock.assert();
        assert_eq!(token, "test-token-abc");
        assert_eq!(client.token.read().await.as_deref(), Some("test-token-abc"));
    }

    #[tokio::test]
    async fn login_sends_pin_when_configured() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path(LOGIN_PATH)
                .json_body(serde_json::json!({"apikey": "test-key", "pin": "1234"}));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({"status": "success", "data": {"token": "tok"}}));
        });

        let client = TvdbClient::new(server.base_url(), "test-key", Some("1234".to_string()))
            .expect("client should construct");
        client.login().await.expect("login should succeed");

        mock.assert();
    }

    #[tokio::test]
    async fn login_401_is_a_typed_error_not_a_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path(LOGIN_PATH);
            then.status(401).body("invalid api key");
        });

        let client = client_for(&server);
        let result = client.login().await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_by_id_parses_series_record() {
        let server = MockServer::start();
        login_mock(&server, "tok");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/series/121361/extended")
                .header("authorization", "Bearer tok");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "status": "success",
                    "data": {
                        "name": "Game of Thrones",
                        "overview": "Nine noble families fight for control.",
                        "image": "https://artworks.thetvdb.com/poster.jpg",
                        "score": 9.1,
                        "firstAired": "2011-04-17",
                        "genres": [{"name": "Drama"}, {"name": "Fantasy"}],
                        "originalNetwork": {"name": "HBO"},
                        "remoteIds": [
                            {"id": "tt0944947", "sourceName": "IMDB"}
                        ]
                    }
                }));
        });

        let client = client_for(&server);
        let metadata = client
            .resolve_by_id(MediaKind::Series, "121361")
            .await
            .expect("resolve should not error")
            .expect("series should be found");

        mock.assert();
        assert_eq!(metadata.title.as_deref(), Some("Game of Thrones"));
        assert_eq!(metadata.network.as_deref(), Some("HBO"));
        assert_eq!(metadata.provider_ids.get("tvdb").map(String::as_str), Some("121361"));
        assert_eq!(metadata.provider_ids.get("imdb").map(String::as_str), Some("tt0944947"));
        assert_eq!(metadata.genres, vec!["Drama".to_string(), "Fantasy".to_string()]);
        assert_eq!(metadata.year, Some(2011));
    }

    /// Closes a review finding: the v4 API shape assumed above
    /// (`name`/string `image`/`originalNetwork: {name}`) is unverified
    /// against a live TheTVDB account. This test feeds the client a
    /// v3-style response instead (`seriesName`, an image OBJECT with a
    /// `url` field, and a flat `network` string) and asserts it parses into
    /// the exact same `ProviderMetadata` as the v4-shaped fixture above —
    /// proving the tolerant `#[serde(alias = ...)]`/untagged-enum handling
    /// actually works for both shapes, not just the assumed one.
    #[tokio::test]
    async fn resolve_by_id_tolerates_v3_style_field_names() {
        let server = MockServer::start();
        login_mock(&server, "tok");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/series/121361/extended")
                .header("authorization", "Bearer tok");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "status": "success",
                    "data": {
                        "seriesName": "Game of Thrones",
                        "overview": "Nine noble families fight for control.",
                        "image": {"url": "https://artworks.thetvdb.com/poster.jpg"},
                        "score": 9.1,
                        "firstAired": "2011-04-17",
                        "genres": [{"name": "Drama"}, {"name": "Fantasy"}],
                        "network": "HBO",
                        "remoteIds": [
                            {"id": "tt0944947", "sourceName": "IMDB"}
                        ]
                    }
                }));
        });

        let client = client_for(&server);
        let metadata = client
            .resolve_by_id(MediaKind::Series, "121361")
            .await
            .expect("resolve should not error")
            .expect("series should be found");

        mock.assert();
        assert_eq!(metadata.title.as_deref(), Some("Game of Thrones"));
        assert_eq!(metadata.network.as_deref(), Some("HBO"));
        assert_eq!(
            metadata.images.poster_url.as_deref(),
            Some("https://artworks.thetvdb.com/poster.jpg")
        );
        assert_eq!(metadata.provider_ids.get("tvdb").map(String::as_str), Some("121361"));
        assert_eq!(metadata.provider_ids.get("imdb").map(String::as_str), Some("tt0944947"));
        assert_eq!(metadata.genres, vec!["Drama".to_string(), "Fantasy".to_string()]);
        assert_eq!(metadata.year, Some(2011));
    }

    /// A v3-style `image_url` object (`{"image_url": "..."}` rather than
    /// `{"url": "..."}`) is also tolerated — TheTVDB's own docs are
    /// inconsistent about which key name a nested image object uses.
    #[tokio::test]
    async fn resolve_by_id_tolerates_image_object_with_image_url_key() {
        let server = MockServer::start();
        login_mock(&server, "tok");
        server.mock(|when, then| {
            when.method(GET).path("/movies/999/extended");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "status": "success",
                    "data": {
                        "movieName": "Arrival",
                        "image": {"image_url": "https://artworks.thetvdb.com/arrival.jpg"}
                    }
                }));
        });

        let client = client_for(&server);
        let metadata = client
            .resolve_by_id(MediaKind::Movie, "999")
            .await
            .expect("resolve should not error")
            .expect("movie should be found");

        assert_eq!(metadata.title.as_deref(), Some("Arrival"));
        assert_eq!(
            metadata.images.poster_url.as_deref(),
            Some("https://artworks.thetvdb.com/arrival.jpg")
        );
    }

    #[tokio::test]
    async fn resolve_by_id_parses_movie_record() {
        let server = MockServer::start();
        login_mock(&server, "tok");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/movies/12345/extended")
                .header("authorization", "Bearer tok");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "status": "success",
                    "data": {
                        "name": "Arrival",
                        "overview": "A linguist deciphers an alien language.",
                        "firstAired": "2016-11-10",
                        "genres": [{"name": "Sci-Fi"}],
                        "remoteIds": [
                            {"id": "tt2543164", "sourceName": "IMDB"},
                            {"id": "329865", "sourceName": "TheMovieDB.com"}
                        ]
                    }
                }));
        });

        let client = client_for(&server);
        let metadata = client
            .resolve_by_id(MediaKind::Movie, "12345")
            .await
            .expect("resolve should not error")
            .expect("movie should be found");

        mock.assert();
        assert_eq!(metadata.title.as_deref(), Some("Arrival"));
        assert_eq!(metadata.overview.as_deref(), Some("A linguist deciphers an alien language."));
        assert_eq!(metadata.genres, vec!["Sci-Fi".to_string()]);
        assert_eq!(metadata.year, Some(2016));
        // The critical remoteIds -> provider_ids bridge (codex review
        // finding): resolving via /extended is what makes this data
        // available at all — the base /movies/{id} endpoint has no
        // remoteIds field.
        assert_eq!(
            metadata.provider_ids.get("imdb").map(String::as_str),
            Some("tt2543164")
        );
        assert_eq!(
            metadata.provider_ids.get("tmdb").map(String::as_str),
            Some("329865")
        );
    }

    #[tokio::test]
    async fn resolve_by_id_returns_none_for_404() {
        let server = MockServer::start();
        login_mock(&server, "tok");
        server.mock(|when, then| {
            when.method(GET).path("/series/999999/extended");
            then.status(404).body("not found");
        });

        let client = client_for(&server);
        let result = client
            .resolve_by_id(MediaKind::Series, "999999")
            .await
            .expect("404 should not be an error");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_by_id_5xx_is_a_typed_error_not_a_panic() {
        let server = MockServer::start();
        login_mock(&server, "tok");
        server.mock(|when, then| {
            when.method(GET).path("/series/121361/extended");
            then.status(500).body("internal server error");
        });

        let client = client_for(&server);
        let result = client.resolve_by_id(MediaKind::Series, "121361").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_parses_hits() {
        let server = MockServer::start();
        login_mock(&server, "tok");
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/search")
                .query_param("query", "arrival")
                .query_param("type", "movie");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "status": "success",
                    "data": [
                        {
                            "tvdb_id": "12345",
                            "name": "Arrival",
                            "year": "2016",
                            "genres": ["Sci-Fi"],
                            "remote_ids": [{"id": "329865", "sourceName": "TheMovieDB.com"}]
                        }
                    ]
                }));
        });

        let client = client_for(&server);
        let hits = client
            .search("arrival", MediaKind::Movie)
            .await
            .expect("search should parse");

        mock.assert();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title.as_deref(), Some("Arrival"));
        assert_eq!(hits[0].provider_ids.get("tmdb").map(String::as_str), Some("329865"));
    }

    /// v3-style search-hit shape: `movieName` instead of `name`, and a
    /// nested `image_url` object instead of the v4-style flat `image_url`
    /// string — same review finding as the resolve-by-id tests above,
    /// closed for the `/search` path too.
    #[tokio::test]
    async fn search_tolerates_v3_style_field_names() {
        let server = MockServer::start();
        login_mock(&server, "tok");
        server.mock(|when, then| {
            when.method(GET)
                .path("/search")
                .query_param("query", "arrival")
                .query_param("type", "movie");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "status": "success",
                    "data": [
                        {
                            "tvdb_id": "12345",
                            "movieName": "Arrival",
                            "year": "2016",
                            "genres": ["Sci-Fi"],
                            "image": {"image_url": "https://artworks.thetvdb.com/arrival.jpg"},
                            "remote_ids": [{"id": "329865", "sourceName": "TheMovieDB.com"}]
                        }
                    ]
                }));
        });

        let client = client_for(&server);
        let hits = client
            .search("arrival", MediaKind::Movie)
            .await
            .expect("search should parse");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title.as_deref(), Some("Arrival"));
        assert_eq!(
            hits[0].images.poster_url.as_deref(),
            Some("https://artworks.thetvdb.com/arrival.jpg")
        );
        assert_eq!(hits[0].provider_ids.get("tmdb").map(String::as_str), Some("329865"));
    }

    /// The critical re-auth path: a stale cached token is rejected once with
    /// `401`, the client transparently re-logs-in exactly once, and the
    /// retried request succeeds — mirrors
    /// `qbit::tests::a_403_on_a_data_call_triggers_exactly_one_reauth_then_retry`.
    #[tokio::test]
    async fn a_401_triggers_exactly_one_reauth_then_retry() {
        let server = MockServer::start();
        let unauthorized_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/series/121361/extended")
                .header("authorization", "Bearer stale-token");
            then.status(401).body("Unauthorized");
        });
        let relogin_mock = server.mock(|when, then| {
            when.method(POST).path(LOGIN_PATH);
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({"status": "success", "data": {"token": "fresh-token"}}));
        });
        let retry_mock = server.mock(|when, then| {
            when.method(GET)
                .path("/series/121361/extended")
                .header("authorization", "Bearer fresh-token");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "status": "success",
                    "data": {"name": "Game of Thrones"}
                }));
        });

        let client = client_for(&server);
        *client.token.write().await = Some("stale-token".to_string());

        let result = client.resolve_by_id(MediaKind::Series, "121361").await;

        assert!(result.is_ok(), "expected retry-after-reauth to succeed");
        unauthorized_mock.assert_hits(1);
        relogin_mock.assert_hits(1);
        retry_mock.assert_hits(1);
    }

    #[test]
    fn from_config_returns_none_when_unconfigured() {
        let config = Config {
            ..Default::default()
        };
        assert!(TvdbClient::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_client_when_configured() {
        let config = Config {
            tvdb_api_key: Some(crate::download::config::QbitPassword::from("abc123".to_string())),
            ..Default::default()
        };
        assert!(TvdbClient::from_config(&config).is_some());
    }

    #[test]
    fn debug_never_prints_api_key_pin_or_token() {
        let client = TvdbClient::new(
            "https://api4.thetvdb.com/v4",
            "super-secret-key",
            Some("9999".to_string()),
        )
        .expect("client should construct");

        let debug = format!("{client:?}");
        assert!(!debug.contains("super-secret-key"));
        assert!(!debug.contains("9999"));
        assert!(debug.contains("<redacted>"));
    }
}
