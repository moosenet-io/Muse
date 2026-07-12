//! Read-only Tautulli API v2 HTTP client (MUSE-06).
//!
//! Every Tautulli API v2 call is a single endpoint, `GET /api/v2`, with the
//! operation selected by a `cmd` query param and authenticated by an
//! `apikey` query param (Tautulli has no header-based auth) — see
//! `TautulliClient::call`. This module makes no writes to Tautulli; it only
//! feeds the one-time backfill importer in `tautulli::backfill`.
//!
//! Construction is via [`TautulliClient::from_config`], which returns `None`
//! when `TAUTULLI_URL`/`TAUTULLI_API_KEY` aren't configured — same
//! graceful-degrade posture as `PlexClient::from_config` /
//! `ProwlarrClient::from_config`.

use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

use super::models::{Envelope, HistoryData, HistoryRow, MetadataInfo, StreamData};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Default page size for `get_history` paging (`length` query param).
pub const DEFAULT_PAGE_SIZE: i64 = 250;

/// A typed, read-only Tautulli API v2 client.
#[derive(Debug, Clone)]
pub struct TautulliClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

/// One page of `get_history`, plus the paging cursor a caller advances by.
#[derive(Debug, Clone, Default)]
pub struct HistoryPage {
    pub rows: Vec<HistoryRow>,
    pub records_filtered: i64,
    pub records_total: i64,
}

impl TautulliClient {
    /// Build a client against a specific Tautulli base URL (e.g.
    /// `http://192.168.0.x:8181`) and API key.
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

    /// Build a client from `Config` (`TAUTULLI_URL`/`TAUTULLI_API_KEY`).
    /// Returns `None` when either is unset/empty or when the client fails to
    /// construct — Tautulli-backfill features degrade rather than blocking
    /// startup. Never panics.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config.tautulli_url.clone()?;
        let api_key = config.tautulli_api_key.clone()?;

        match Self::new(url, api_key) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct Tautulli client; backfill will degrade");
                None
            }
        }
    }

    async fn call<T: DeserializeOwned>(&self, cmd: &str, extra: &[(&str, String)]) -> MuseResult<T> {
        let url = format!("{}/api/v2", self.base_url);

        let mut query: Vec<(&str, String)> =
            vec![("apikey", self.api_key.clone()), ("cmd", cmd.to_string())];
        query.extend(extra.iter().cloned());

        let resp = self.http.get(&url).query(&query).send().await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("tautulli {cmd} request failed: {body}"),
            });
        }

        let envelope: Envelope<T> = serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse tautulli {cmd} response: {e}"),
        })?;

        if envelope.response.result != "success" {
            return Err(MuseError::upstream(format!(
                "tautulli {cmd} returned result={}: {}",
                envelope.response.result,
                envelope.response.message.unwrap_or_default()
            )));
        }

        Ok(envelope.response.data)
    }

    /// `cmd=get_history` — one page of watch history, oldest-first paging
    /// via `start`/`length` (Tautulli's DataTables-style pagination). The
    /// backfill importer drives this in a loop, advancing `start` by
    /// `length` until a page comes back with fewer rows than requested (or
    /// `start >= records_filtered`).
    pub async fn get_history(&self, start: i64, length: i64) -> MuseResult<HistoryPage> {
        let data: HistoryData = self
            .call(
                "get_history",
                &[
                    ("start", start.to_string()),
                    ("length", length.to_string()),
                    // Oldest-first so a resumed/interrupted backfill (or a
                    // future resumable cursor) has a stable paging order.
                    ("order_column", "date".to_string()),
                    ("order_dir", "asc".to_string()),
                ],
            )
            .await?;

        Ok(HistoryPage {
            rows: data.data,
            records_filtered: data.records_filtered,
            records_total: data.records_total,
        })
    }

    /// `cmd=get_metadata` — full metadata for a `rating_key`, used to enrich
    /// a history row with the item's true media runtime. Returns `Ok(None)`
    /// when Tautulli has nothing for that key (e.g. since-deleted item)
    /// rather than erroring — enrichment is best-effort.
    pub async fn get_metadata(&self, rating_key: &str) -> MuseResult<Option<MetadataInfo>> {
        let data: MetadataInfo = self
            .call("get_metadata", &[("rating_key", rating_key.to_string())])
            .await?;

        if data.rating_key.is_none() && data.media_type.is_none() {
            return Ok(None);
        }
        Ok(Some(data))
    }

    /// `cmd=get_stream_data` — quality/transcode detail for one history row
    /// (`row_id`), mapped onto `play_session_media_info`. Returns `Ok(None)`
    /// when Tautulli has no stream data for that row — enrichment is
    /// best-effort and must never fail the whole backfill row.
    pub async fn get_stream_data(&self, row_id: i64) -> MuseResult<Option<StreamData>> {
        let data: StreamData = self
            .call("get_stream_data", &[("row_id", row_id.to_string())])
            .await?;

        if data.video_decision.is_none()
            && data.audio_decision.is_none()
            && data.container.is_none()
        {
            return Ok(None);
        }
        Ok(Some(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> TautulliClient {
        TautulliClient::new(server.base_url(), "test-key").expect("client should construct")
    }

    #[tokio::test]
    async fn get_history_parses_a_page_of_rows() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v2")
                .query_param("cmd", "get_history")
                .query_param("apikey", "test-key")
                .query_param("start", "0")
                .query_param("length", "2");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "response": {
                            "result": "success",
                            "message": null,
                            "data": {
                                "recordsFiltered": 2,
                                "recordsTotal": 2,
                                "data": [
                                    {
                                        "reference_id": 1001,
                                        "row_id": 1001,
                                        "started": 1700000000,
                                        "stopped": 1700003600,
                                        "duration": 3600,
                                        "paused_counter": 1,
                                        "user_id": 42,
                                        "user": "moose",
                                        "friendly_name": "Moose",
                                        "player": "Living Room",
                                        "platform": "Roku",
                                        "product": "Plex for Roku",
                                        "ip_address": "192.0.2.5",
                                        "rating_key": 555,
                                        "grandparent_rating_key": null,
                                        "full_title": "Arrival",
                                        "media_type": "movie",
                                        "percent_complete": 96,
                                        "watched_status": 1
                                    },
                                    {
                                        "reference_id": 1002,
                                        "row_id": 1002,
                                        "started": 1700010000,
                                        "stopped": 1700010600,
                                        "duration": 600,
                                        "user_id": 42,
                                        "user": "moose",
                                        "rating_key": 556,
                                        "grandparent_rating_key": 500,
                                        "media_type": "episode",
                                        "percent_complete": 8,
                                        "watched_status": 0
                                    }
                                ]
                            }
                        }
                    }"#,
                );
        });

        let client = client_for(&server);
        let page = client.get_history(0, 2).await.expect("history should parse");

        mock.assert();
        assert_eq!(page.records_filtered, 2);
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0].rating_key_str(), Some("555".to_string()));
        assert_eq!(page.rows[0].media_type.as_deref(), Some("movie"));
        assert_eq!(page.rows[0].watched_status, Some(1.0));
        assert_eq!(page.rows[1].grandparent_rating_key_str(), Some("500".to_string()));
        assert_eq!(page.rows[1].percent_complete, Some(8.0));
    }

    #[tokio::test]
    async fn get_history_empty_page_parses_cleanly() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_history");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"response": {"result": "success", "data": {"recordsFiltered": 0, "recordsTotal": 0, "data": []}}}"#,
                );
        });

        let client = client_for(&server);
        let page = client.get_history(0, 250).await.expect("empty history should parse");

        assert!(page.rows.is_empty());
        assert_eq!(page.records_filtered, 0);
    }

    #[tokio::test]
    async fn get_metadata_parses_runtime() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v2")
                .query_param("cmd", "get_metadata")
                .query_param("rating_key", "555");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"response": {"result": "success", "data": {"media_type": "movie", "rating_key": 555, "title": "Arrival", "duration": 6300000}}}"#,
                );
        });

        let client = client_for(&server);
        let meta = client
            .get_metadata("555")
            .await
            .expect("metadata request should succeed")
            .expect("metadata should be present");

        assert_eq!(meta.duration, Some(6_300_000));
        assert_eq!(meta.title.as_deref(), Some("Arrival"));
    }

    #[tokio::test]
    async fn get_metadata_returns_none_for_empty_item() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_metadata");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"response": {"result": "success", "data": {}}}"#);
        });

        let client = client_for(&server);
        let meta = client.get_metadata("999").await.expect("request should succeed");

        assert!(meta.is_none());
    }

    #[tokio::test]
    async fn get_stream_data_parses_quality_fields() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/api/v2")
                .query_param("cmd", "get_stream_data")
                .query_param("row_id", "1001");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{
                        "response": {
                            "result": "success",
                            "data": {
                                "video_decision": "transcode",
                                "audio_decision": "direct play",
                                "transcode_decision": "transcode",
                                "container": "mp4",
                                "video_codec": "h264",
                                "audio_codec": "aac",
                                "audio_channels": 2,
                                "video_resolution": "1080",
                                "bitrate": 4000,
                                "width": 1920,
                                "height": 1080,
                                "transcode_reason": "video codec not supported"
                            }
                        }
                    }"#,
                );
        });

        let client = client_for(&server);
        let stream = client
            .get_stream_data(1001)
            .await
            .expect("stream data request should succeed")
            .expect("stream data should be present");

        assert_eq!(stream.video_decision.as_deref(), Some("transcode"));
        assert_eq!(stream.container.as_deref(), Some("mp4"));
        assert_eq!(stream.width, Some(1920));
    }

    #[tokio::test]
    async fn error_result_is_surfaced_as_upstream_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v2").query_param("cmd", "get_history");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"response": {"result": "error", "message": "Invalid apikey", "data": []}}"#);
        });

        let client = client_for(&server);
        let result = client.get_history(0, 250).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn non_success_http_status_is_surfaced_not_panicked() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v2");
            then.status(401).body("unauthorized");
        });

        let client = client_for(&server);
        let result = client.get_history(0, 250).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[test]
    fn from_config_returns_none_when_unconfigured() {
        let config = test_config(None, None);
        assert!(TautulliClient::from_config(&config).is_none());
    }

    #[test]
    fn from_config_builds_client_when_configured() {
        let config = test_config(
            Some("http://192.0.2.10:8181".to_string()),
            Some("test-key".to_string()),
        );
        assert!(TautulliClient::from_config(&config).is_some());
    }

    fn test_config(tautulli_url: Option<String>, tautulli_api_key: Option<String>) -> Config {
        Config {
            database_url: None,
            bind_addr: "0.0.0.0:8090".to_string(),
            log_level: "info".to_string(),
            plex_url: None,
            plex_token: None,
            tautulli_url,
            tautulli_api_key,
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
        }
    }
}
