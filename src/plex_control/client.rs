//! HTTP client for the Plex client-control (Companion) API.
//!
//! Endpoint/parameter shapes here follow the published Plex Companion
//! protocol and the widely-used `python-plexapi` reference implementation,
//! but this crate has never been exercised against a live Plex Media Server
//! or a real registered client (Chromecast/AppleTV/TV app) — the dev box
//! cannot run that verification (no cargo builds here, and no live Plex
//! reachable). Treat the exact header/query-param behavior (in particular
//! `commandID` monotonicity and whether targets require the request to be
//! proxied through the PMS vs. hit directly at `address:port`) as
//! best-effort until it's live-verified in Stage 7.

use std::sync::atomic::{AtomicI64, Ordering};

use reqwest::Client;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

use super::models::{
    ClientsMediaContainer, IdentityMediaContainer, MediaContainerEnvelope, PlayQueue,
    PlayQueueMediaContainer, PlayQueueRequest, PlexPlayer, TimelinePoll, TransportCommand,
};

/// Plex client-control client. Talks to a single Plex Media Server (`PLEX_URL`)
/// and issues commands targeted at a `machineIdentifier` via the
/// `X-Plex-Target-Client-Identifier` header, per the Companion protocol.
pub struct PlexControlClient {
    http: Client,
    base_url: String,
    token: String,
    /// This controller's own identity, sent as `X-Plex-Client-Identifier` on
    /// every request that targets another client — Plex Companion requires
    /// a stable, opaque id per *controller*, not per command.
    controller_identifier: String,
    /// Lazily fetched + cached via `GET /identity`; needed to build the
    /// `server://{machineIdentifier}/...` play-queue URI.
    server_machine_id: RwLock<Option<String>>,
    /// Companion commands are expected to carry a monotonically increasing
    /// `commandID` per controller session.
    command_id: AtomicI64,
}

impl PlexControlClient {
    /// Build a client from `PLEX_URL`/`PLEX_TOKEN` in `Config`.
    pub fn from_config(config: &Config) -> MuseResult<Self> {
        let base_url = config
            .plex_url
            .clone()
            .ok_or_else(|| MuseError::Config("PLEX_URL is not set".to_string()))?;
        let token = config
            .plex_token
            .clone()
            .ok_or_else(|| MuseError::Config("PLEX_TOKEN is not set".to_string()))?;

        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| MuseError::Upstream(format!("failed to build HTTP client: {e}")))?;

        Ok(Self::new(base_url, token, http))
    }

    /// Build a client against an explicit base URL/token (used in tests
    /// against an `httpmock` server, and available for callers that source
    /// Plex config from elsewhere).
    pub fn new(base_url: impl Into<String>, token: impl Into<String>, http: Client) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            controller_identifier: Uuid::new_v4().to_string(),
            server_machine_id: RwLock::new(None),
            command_id: AtomicI64::new(0),
        }
    }

    fn next_command_id(&self) -> i64 {
        self.command_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Discover registered players / cast targets (`GET /clients`).
    pub async fn list_clients(&self) -> MuseResult<Vec<PlexPlayer>> {
        let url = format!("{}/clients", self.base_url);

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .header("X-Plex-Token", &self.token)
            .send()
            .await
            .map_err(|e| MuseError::Upstream(format!("GET /clients failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(MuseError::Upstream(format!(
                "GET /clients returned {}",
                resp.status()
            )));
        }

        let body: MediaContainerEnvelope<ClientsMediaContainer> = resp
            .json()
            .await
            .map_err(|e| MuseError::Upstream(format!("GET /clients: invalid response body: {e}")))?;

        Ok(body
            .media_container
            .server
            .into_iter()
            .map(|raw| raw.into_player())
            .collect())
    }

    /// Fetch (and cache) this server's own `machineIdentifier` via
    /// `GET /identity`, needed to address play-queue item URIs.
    async fn server_machine_identifier(&self) -> MuseResult<String> {
        if let Some(id) = self.server_machine_id.read().await.clone() {
            return Ok(id);
        }

        let url = format!("{}/identity", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .header("X-Plex-Token", &self.token)
            .send()
            .await
            .map_err(|e| MuseError::Upstream(format!("GET /identity failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(MuseError::Upstream(format!(
                "GET /identity returned {}",
                resp.status()
            )));
        }

        let body: MediaContainerEnvelope<IdentityMediaContainer> = resp
            .json()
            .await
            .map_err(|e| MuseError::Upstream(format!("GET /identity: invalid response body: {e}")))?;

        let id = body.media_container.machine_identifier;
        *self.server_machine_id.write().await = Some(id.clone());
        Ok(id)
    }

    /// Build a Plex play queue (`POST /playQueues`) from an ordered list of
    /// `ratingKey`s — the native primitive for a sequenced "channel".
    pub async fn create_play_queue(&self, req: &PlayQueueRequest) -> MuseResult<PlayQueue> {
        if req.rating_keys.is_empty() {
            return Err(MuseError::Upstream(
                "cannot create a play queue from zero items".to_string(),
            ));
        }

        let server_id = self.server_machine_identifier().await?;
        let keys = req.rating_keys.join(",");
        let uri = format!(
            "server://{server_id}/com.plexapp.plugins.library/library/metadata/{keys}"
        );

        let url = format!("{}/playQueues", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Accept", "application/json")
            .header("X-Plex-Token", &self.token)
            .query(&[
                ("type", "video"),
                ("uri", uri.as_str()),
                ("shuffle", if req.shuffle { "1" } else { "0" }),
                ("continuous", if req.continuous { "1" } else { "0" }),
            ])
            .send()
            .await
            .map_err(|e| MuseError::Upstream(format!("POST /playQueues failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(MuseError::Upstream(format!(
                "POST /playQueues returned {}",
                resp.status()
            )));
        }

        let body: MediaContainerEnvelope<PlayQueueMediaContainer> = resp
            .json()
            .await
            .map_err(|e| MuseError::Upstream(format!("POST /playQueues: invalid response body: {e}")))?;

        Ok(PlayQueue {
            play_queue_id: body.media_container.play_queue_id,
            play_queue_selected_item_id: body.media_container.play_queue_selected_item_id,
            size: body.media_container.size,
        })
    }

    /// Start playback of `rating_key` on `target` (a `machineIdentifier`),
    /// optionally within a previously created play queue.
    pub async fn play_media(
        &self,
        target: &str,
        rating_key: &str,
        play_queue_id: Option<i64>,
        offset_ms: i64,
    ) -> MuseResult<()> {
        let key = format!("/library/metadata/{rating_key}");
        let mut query: Vec<(String, String)> = vec![
            ("key".to_string(), key),
            ("offset".to_string(), offset_ms.to_string()),
            ("machineIdentifier".to_string(), target.to_string()),
            ("commandID".to_string(), self.next_command_id().to_string()),
        ];
        if let Some(pq) = play_queue_id {
            query.push(("playQueueID".to_string(), pq.to_string()));
            query.push(("type".to_string(), "video".to_string()));
        }

        self.send_transport_request("playMedia", target, &query)
            .await
    }

    pub async fn play(&self, target: &str) -> MuseResult<()> {
        self.send_transport_command(TransportCommand::Play, target)
            .await
    }

    pub async fn pause(&self, target: &str) -> MuseResult<()> {
        self.send_transport_command(TransportCommand::Pause, target)
            .await
    }

    pub async fn stop(&self, target: &str) -> MuseResult<()> {
        self.send_transport_command(TransportCommand::Stop, target)
            .await
    }

    pub async fn skip_next(&self, target: &str) -> MuseResult<()> {
        self.send_transport_command(TransportCommand::SkipNext, target)
            .await
    }

    async fn send_transport_command(&self, cmd: TransportCommand, target: &str) -> MuseResult<()> {
        let query = [(
            "commandID".to_string(),
            self.next_command_id().to_string(),
        )];
        self.send_transport_request(cmd.endpoint(), target, &query)
            .await
    }

    async fn send_transport_request(
        &self,
        endpoint: &str,
        target: &str,
        query: &[(String, String)],
    ) -> MuseResult<()> {
        let url = format!("{}/player/playback/{endpoint}", self.base_url);

        let resp = self
            .http
            .get(&url)
            .header("X-Plex-Token", &self.token)
            .header("X-Plex-Target-Client-Identifier", target)
            .header("X-Plex-Client-Identifier", &self.controller_identifier)
            .query(query)
            .send()
            .await
            .map_err(|e| MuseError::Upstream(format!("GET /player/playback/{endpoint} failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(MuseError::Upstream(format!(
                "GET /player/playback/{endpoint} returned {}",
                resp.status()
            )));
        }

        Ok(())
    }

    /// Poll transport/state for `target` (`/player/timeline/poll`).
    pub async fn timeline_poll(&self, target: &str) -> MuseResult<TimelinePoll> {
        let url = format!("{}/player/timeline/poll", self.base_url);

        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .header("X-Plex-Token", &self.token)
            .header("X-Plex-Target-Client-Identifier", target)
            .header("X-Plex-Client-Identifier", &self.controller_identifier)
            .query(&[
                ("wait", "0"),
                ("commandID", &self.next_command_id().to_string()),
            ])
            .send()
            .await
            .map_err(|e| MuseError::Upstream(format!("GET /player/timeline/poll failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(MuseError::Upstream(format!(
                "GET /player/timeline/poll returned {}",
                resp.status()
            )));
        }

        let raw: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| MuseError::Upstream(format!("GET /player/timeline/poll: invalid response body: {e}")))?;

        Ok(parse_timeline_poll(raw))
    }
}

/// Pull the first meaningful `Timeline` entry out of a
/// `/player/timeline/poll` body. The payload shape is
/// `{"MediaContainer":{"Timeline":[{...}, ...]}}`, one entry per media type
/// (video/music/photo); we prefer the first entry that reports a `state`.
fn parse_timeline_poll(raw: serde_json::Value) -> TimelinePoll {
    let timeline_entry = raw
        .get("MediaContainer")
        .and_then(|mc| mc.get("Timeline"))
        .and_then(|t| t.as_array())
        .and_then(|entries| {
            entries
                .iter()
                .find(|e| e.get("state").is_some())
                .or_else(|| entries.first())
        });

    let state = timeline_entry
        .and_then(|e| e.get("state"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let rating_key = timeline_entry
        .and_then(|e| e.get("ratingKey"))
        .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_i64().map(|n| n.to_string())));
    let time_ms = timeline_entry.and_then(|e| e.get("time")).and_then(|v| v.as_i64());
    let duration_ms = timeline_entry
        .and_then(|e| e.get("duration"))
        .and_then(|v| v.as_i64());

    TimelinePoll {
        state,
        rating_key,
        time_ms,
        duration_ms,
        raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    fn test_client(server: &MockServer) -> PlexControlClient {
        PlexControlClient::new(server.base_url(), "test-token", Client::new())
    }

    #[tokio::test]
    async fn list_clients_parses_players() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/clients");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "MediaContainer": {
                        "size": 2,
                        "Server": [
                            {
                                "name": "Living Room Chromecast",
                                "address": "192.0.2.10",
                                "port": 8009,
                                "machineIdentifier": "chromecast-1",
                                "product": "Chromecast",
                                "deviceClass": "stb",
                                "protocolCapabilities": "playback,navigation,timeline"
                            },
                            {
                                "name": "Bedroom AppleTV",
                                "address": "192.0.2.11",
                                "port": 3000,
                                "machineIdentifier": "appletv-1",
                                "product": "Plex for Apple TV",
                                "deviceClass": "stb",
                                "platform": "tvOS",
                                "protocolCapabilities": "playback,navigation,timeline,playqueues"
                            }
                        ]
                    }
                }));
        });

        let client = test_client(&server);
        let players = client.list_clients().await.expect("list_clients");

        mock.assert();
        assert_eq!(players.len(), 2);

        let cast = &players[0];
        assert_eq!(cast.machine_identifier, "chromecast-1");
        assert!(cast.is_cast_target);
        assert_eq!(
            cast.protocol_caps,
            vec!["playback", "navigation", "timeline"]
        );

        let appletv = &players[1];
        assert_eq!(appletv.machine_identifier, "appletv-1");
        assert!(!appletv.is_cast_target);
        assert_eq!(appletv.platform.as_deref(), Some("tvOS"));
    }

    #[tokio::test]
    async fn list_clients_maps_non_success_status_to_upstream_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/clients");
            then.status(500);
        });

        let client = test_client(&server);
        let err = client.list_clients().await.unwrap_err();
        assert!(matches!(err, MuseError::Upstream(_)));
    }

    #[tokio::test]
    async fn create_play_queue_fetches_identity_then_posts_uri() {
        let server = MockServer::start();
        let identity_mock = server.mock(|when, then| {
            when.method(GET).path("/identity");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "MediaContainer": { "machineIdentifier": "pms-server-1" }
                }));
        });
        let queue_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/playQueues")
                .query_param("uri", "server://pms-server-1/com.plexapp.plugins.library/library/metadata/1,2,3");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "MediaContainer": {
                        "playQueueID": 4242,
                        "playQueueSelectedItemID": 1,
                        "size": 3
                    }
                }));
        });

        let client = test_client(&server);
        let req = PlayQueueRequest::new(vec!["1".to_string(), "2".to_string(), "3".to_string()]);
        let queue = client.create_play_queue(&req).await.expect("create_play_queue");

        identity_mock.assert();
        queue_mock.assert();
        assert_eq!(queue.play_queue_id, 4242);
        assert_eq!(queue.size, 3);
    }

    #[tokio::test]
    async fn create_play_queue_rejects_empty_item_list() {
        let server = MockServer::start();
        let client = test_client(&server);
        let req = PlayQueueRequest::new(vec![]);
        let err = client.create_play_queue(&req).await.unwrap_err();
        assert!(matches!(err, MuseError::Upstream(_)));
    }

    #[tokio::test]
    async fn play_media_sends_target_header_and_query() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/player/playback/playMedia")
                .header("X-Plex-Target-Client-Identifier", "chromecast-1")
                .query_param("key", "/library/metadata/99")
                .query_param("machineIdentifier", "chromecast-1");
            then.status(200);
        });

        let client = test_client(&server);
        client
            .play_media("chromecast-1", "99", Some(4242), 0)
            .await
            .expect("play_media");

        mock.assert();
    }

    #[tokio::test]
    async fn transport_commands_hit_expected_endpoints() {
        let server = MockServer::start();
        let play_mock = server.mock(|when, then| {
            when.method(GET).path("/player/playback/play");
            then.status(200);
        });
        let pause_mock = server.mock(|when, then| {
            when.method(GET).path("/player/playback/pause");
            then.status(200);
        });
        let stop_mock = server.mock(|when, then| {
            when.method(GET).path("/player/playback/stop");
            then.status(200);
        });
        let skip_mock = server.mock(|when, then| {
            when.method(GET).path("/player/playback/skipNext");
            then.status(200);
        });

        let client = test_client(&server);
        client.play("chromecast-1").await.expect("play");
        client.pause("chromecast-1").await.expect("pause");
        client.stop("chromecast-1").await.expect("stop");
        client.skip_next("chromecast-1").await.expect("skip_next");

        play_mock.assert();
        pause_mock.assert();
        stop_mock.assert();
        skip_mock.assert();
    }

    #[tokio::test]
    async fn timeline_poll_parses_state_from_first_entry() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/player/timeline/poll");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "MediaContainer": {
                        "Timeline": [
                            { "type": "video", "state": "playing", "ratingKey": "99", "time": 15000, "duration": 1_500_000 }
                        ]
                    }
                }));
        });

        let client = test_client(&server);
        let poll = client.timeline_poll("chromecast-1").await.expect("timeline_poll");

        mock.assert();
        assert_eq!(poll.state.as_deref(), Some("playing"));
        assert_eq!(poll.rating_key.as_deref(), Some("99"));
        assert_eq!(poll.time_ms, Some(15000));
        assert_eq!(poll.duration_ms, Some(1_500_000));
    }

    #[tokio::test]
    async fn timeline_poll_handles_empty_timeline() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/player/timeline/poll");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({ "MediaContainer": { "Timeline": [] } }));
        });

        let client = test_client(&server);
        let poll = client.timeline_poll("chromecast-1").await.expect("timeline_poll");
        assert!(poll.state.is_none());
    }
}
