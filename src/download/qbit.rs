//! `QbitClient` — a [`super::DownloadClient`] implementation against the
//! qBittorrent WebUI v2 API.
//!
//! Mirrors `arr::client::ArrClient` / `plex::PlexClient`'s shape: a `reqwest`
//! client built once, typed request/response methods, non-success statuses
//! and transport failures mapped to [`MuseError`] rather than panicking.
//! This crate's `reqwest` dependency doesn't enable the `cookie_store`
//! feature (see `Cargo.toml`), so the qBittorrent `SID` session cookie is
//! managed by hand: captured from `Set-Cookie` on login, held behind a
//! shared `RwLock` (clones of a `QbitClient` share one session), and sent
//! back as a plain `Cookie` header on every authenticated request.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::COOKIE;
use reqwest::StatusCode;
use tokio::sync::RwLock;

use crate::error::{MuseError, MuseResult};

use super::config::{QbitConfig, QbitPassword};
use super::{DownloadClient, GrabReceipt, GrabRequest, TorrentStatus};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const LOGIN_PATH: &str = "/api/v2/auth/login";
const ADD_PATH: &str = "/api/v2/torrents/add";
const INFO_PATH: &str = "/api/v2/torrents/info";
const DELETE_PATH: &str = "/api/v2/torrents/delete";

/// A qBittorrent WebUI v2 client for a single instance. Cheap to `Clone`
/// (the session cookie is shared via `Arc<RwLock<_>>`, same posture as
/// `reqwest::Client`'s own internal `Arc`).
#[derive(Clone)]
pub struct QbitClient {
    http: reqwest::Client,
    base_url: String,
    user: String,
    pass: QbitPassword,
    sid: Arc<RwLock<Option<String>>>,
}

// Manual `Debug` (rather than `#[derive(Debug)]`) so a stray
// `tracing::debug!(client = ?qbit, ...)` can never print the password *or*
// the live session cookie (a session cookie is bearer-credential-shaped for
// as long as it's valid).
impl std::fmt::Debug for QbitClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QbitClient")
            .field("base_url", &self.base_url)
            .field("user", &self.user)
            .field("pass", &self.pass)
            .field("sid", &"<redacted>")
            .finish()
    }
}

impl QbitClient {
    /// Build a client against a specific qBittorrent WebUI base url (e.g.
    /// `http://192.0.2.60:8080`) and credentials.
    pub fn new(
        base_url: impl Into<String>,
        user: impl Into<String>,
        pass: QbitPassword,
    ) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            user: user.into(),
            pass,
            sid: Arc::new(RwLock::new(None)),
        })
    }

    /// Build a client from a loaded [`QbitConfig`] (itself assembled from
    /// the central `crate::config::Config` via `Config::qbit` — see that
    /// module's doc). Mirrors `PlexClient::from_config`'s naming, but takes
    /// the already-narrowed `QbitConfig` rather than the whole `Config`,
    /// since a future caller typically already has `Some(QbitConfig)` in
    /// hand from `Config::qbit()`'s `None`-means-unconfigured check.
    pub fn from_config(config: &QbitConfig) -> MuseResult<Self> {
        Self::new(config.url.clone(), config.user.clone(), config.pass.clone())
    }

    /// `POST /api/v2/auth/login` — authenticate and capture the `SID`
    /// session cookie. Called lazily by [`Self::ensure_authenticated`] and
    /// again, once, on a transparent re-auth after a 403 — never called
    /// speculatively per-request when a cached SID is already held.
    async fn login(&self) -> MuseResult<String> {
        let url = format!("{}{LOGIN_PATH}", self.base_url);

        let resp = self
            .http
            .post(&url)
            .form(&[
                ("username", self.user.as_str()),
                ("password", self.pass.expose()),
            ])
            .send()
            .await?;

        let status = resp.status();
        let sid = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(extract_sid_cookie);
        let bytes = resp.bytes().await?;
        let body = String::from_utf8_lossy(&bytes).to_string();

        if !status.is_success() {
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("qbit login to {url} failed: {body}"),
            });
        }
        if body.trim() != "Ok." {
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("qbit login rejected: {body}"),
            });
        }
        let sid = sid.ok_or_else(|| MuseError::Upstream {
            status: status.as_u16(),
            message: "qbit login succeeded but returned no SID cookie".to_string(),
        })?;

        *self.sid.write().await = Some(sid.clone());
        Ok(sid)
    }

    /// Returns the cached SID if one is held, logging in for the first time
    /// otherwise. Does NOT validate that a cached SID is still accepted by
    /// the server — that's what the 403-triggered re-auth in
    /// [`Self::send_with_reauth`] is for.
    async fn ensure_authenticated(&self) -> MuseResult<String> {
        if let Some(sid) = self.sid.read().await.clone() {
            return Ok(sid);
        }
        self.login().await
    }

    /// Sends a request built by `build` (given the current `Cookie` header
    /// value), retrying **exactly once** with a fresh SID if the first
    /// attempt comes back `403 Forbidden` (an expired/invalidated session —
    /// see the MUSEM-02 spec's EDGE CASES). A second `403` (or any other
    /// status) is returned to the caller as-is; callers map it to a typed
    /// error, never a panic.
    async fn send_with_reauth<F>(&self, build: F) -> MuseResult<(u16, Vec<u8>)>
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        let sid = self.ensure_authenticated().await?;
        let resp = build(&sid).send().await?;

        if resp.status() == StatusCode::FORBIDDEN {
            let fresh_sid = self.login().await?;
            let resp = build(&fresh_sid).send().await?;
            let status = resp.status().as_u16();
            let bytes = resp.bytes().await?.to_vec();
            return Ok((status, bytes));
        }

        let status = resp.status().as_u16();
        let bytes = resp.bytes().await?.to_vec();
        Ok((status, bytes))
    }
}

#[async_trait::async_trait]
impl DownloadClient for QbitClient {
    /// `POST /api/v2/torrents/add` (form: `urls=`, optional
    /// `category=`/`savepath=`, `paused=` — see the body-encoding note
    /// below).
    async fn add(&self, req: GrabRequest) -> MuseResult<GrabReceipt> {
        let url = format!("{}{ADD_PATH}", self.base_url);

        // The MUSEM-02 spec's APPROACH describes this as a multipart form,
        // matching qBittorrent's own docs (which cover the file-upload
        // case). This crate's `reqwest` dependency doesn't enable the
        // `multipart` feature (see `Cargo.toml` — only `rustls-tls`/`json`
        // are on), and we're not uploading a `.torrent` file here (only
        // `urls=`, a text field) — the qBittorrent WebUI accepts a plain
        // `application/x-www-form-urlencoded` body for the text-only add
        // path just as well, so this uses `.form()` (already available,
        // same as `login`/`delete` below) rather than pulling in a new
        // feature for a body reqwest already knows how to send.
        let mut form: Vec<(&str, String)> = vec![
            ("urls", req.url.clone()),
            ("paused", if req.paused { "true" } else { "false" }.to_string()),
        ];
        if let Some(category) = &req.category {
            form.push(("category", category.clone()));
        }
        if let Some(save_path) = &req.save_path {
            form.push(("savepath", save_path.clone()));
        }

        let (status, bytes) = self
            .send_with_reauth(|sid| self.http.post(&url).header(COOKIE, sid).form(&form))
            .await?;

        let body = String::from_utf8_lossy(&bytes).to_string();
        if status != StatusCode::OK.as_u16() {
            return Err(MuseError::Upstream {
                status,
                message: format!("qbit add to {url} failed: {body}"),
            });
        }

        // qBittorrent's add response is a bare "Ok." with no hash (older
        // builds always behave this way) — resolve the infohash from the
        // submitted magnet ourselves rather than assuming the body carries
        // one (MUSEM-02 EDGE CASES).
        let hash = extract_infohash_from_magnet(&req.url);

        Ok(GrabReceipt {
            hash,
            raw_response: body,
        })
    }

    /// `GET /api/v2/torrents/info`.
    async fn list(&self) -> MuseResult<Vec<TorrentStatus>> {
        let url = format!("{}{INFO_PATH}", self.base_url);

        let (status, bytes) = self
            .send_with_reauth(|sid| self.http.get(&url).header(COOKIE, sid))
            .await?;

        if status != StatusCode::OK.as_u16() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status,
                message: format!("qbit list from {url} failed: {body}"),
            });
        }

        serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
            status,
            message: format!("failed to parse qbit torrents/info response: {e}"),
        })
    }

    /// `GET /api/v2/torrents/info?hashes=<hash>`.
    async fn info(&self, hash: &str) -> MuseResult<Option<TorrentStatus>> {
        let url = format!("{}{INFO_PATH}", self.base_url);
        let hash = hash.to_string();

        let (status, bytes) = self
            .send_with_reauth(|sid| {
                self.http
                    .get(&url)
                    .header(COOKIE, sid)
                    .query(&[("hashes", hash.as_str())])
            })
            .await?;

        if status != StatusCode::OK.as_u16() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status,
                message: format!("qbit info from {url} failed: {body}"),
            });
        }

        let mut torrents: Vec<TorrentStatus> =
            serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
                status,
                message: format!("failed to parse qbit torrents/info response: {e}"),
            })?;

        Ok(if torrents.is_empty() {
            None
        } else {
            Some(torrents.remove(0))
        })
    }

    /// `POST /api/v2/torrents/delete` (form: `hashes=`, `deleteFiles=`).
    async fn delete(&self, hash: &str, delete_files: bool) -> MuseResult<()> {
        let url = format!("{}{DELETE_PATH}", self.base_url);
        let hash = hash.to_string();
        let delete_files_str = if delete_files { "true" } else { "false" };

        let (status, bytes) = self
            .send_with_reauth(|sid| {
                self.http.post(&url).header(COOKIE, sid).form(&[
                    ("hashes", hash.as_str()),
                    ("deleteFiles", delete_files_str),
                ])
            })
            .await?;

        if status != StatusCode::OK.as_u16() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status,
                message: format!("qbit delete to {url} failed: {body}"),
            });
        }

        Ok(())
    }
}

/// Parses the `SID=...` pair out of a raw `Set-Cookie` header value (which
/// also carries `Path=/`, `HttpOnly`, etc., separated by `;`). Returns the
/// `name=value` pair as-is (ready to send straight back as a `Cookie`
/// header), or `None` if this `Set-Cookie` isn't the `SID` cookie.
fn extract_sid_cookie(raw: &str) -> Option<String> {
    let first = raw.split(';').next()?.trim();
    if first.starts_with("SID=") {
        Some(first.to_string())
    } else {
        None
    }
}

/// Pulls a BitTorrent infohash out of a magnet URI's `xt=urn:btih:<hash>`
/// parameter, lower-cased for stable comparison. Returns `None` for a
/// non-magnet url (a direct `.torrent` URL) — there is no client-side way
/// to know the infohash in that case, see the MUSEM-02 spec's EDGE CASES.
pub(crate) fn extract_infohash_from_magnet(url: &str) -> Option<String> {
    let query = url.strip_prefix("magnet:?")?;
    query
        .split('&')
        .find_map(|param| param.strip_prefix("xt=urn:btih:"))
        .map(|hash| hash.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn config_for(server: &MockServer) -> QbitConfig {
        QbitConfig {
            url: server.base_url(),
            user: "admin".to_string(),
            pass: QbitPassword::from("hunter2".to_string()),
        }
    }

    fn login_mock<'a>(server: &'a MockServer, sid: &str) -> httpmock::Mock<'a> {
        server.mock(|when, then| {
            when.method(POST)
                .path(LOGIN_PATH)
                .header("content-type", "application/x-www-form-urlencoded");
            then.status(200)
                .header("set-cookie", format!("SID={sid}; path=/; HttpOnly"))
                .body("Ok.");
        })
    }

    #[test]
    fn extract_sid_cookie_parses_the_sid_pair() {
        assert_eq!(
            extract_sid_cookie("SID=abc123; path=/; HttpOnly"),
            Some("SID=abc123".to_string())
        );
        assert_eq!(extract_sid_cookie("path=/; HttpOnly"), None);
    }

    #[test]
    fn extract_infohash_from_magnet_parses_and_lowercases() {
        let magnet = "magnet:?xt=urn:btih:AABBCCDDEEFF00112233445566778899AABBCCDD&dn=Test&tr=udp%3A%2F%2Ftracker";
        assert_eq!(
            extract_infohash_from_magnet(magnet).as_deref(),
            Some("aabbccddeeff00112233445566778899aabbccdd")
        );
    }

    #[test]
    fn extract_infohash_from_magnet_none_for_torrent_url() {
        assert_eq!(
            extract_infohash_from_magnet("https://example.invalid/some.torrent"),
            None
        );
    }

    #[tokio::test]
    async fn login_captures_sid_cookie() {
        let server = MockServer::start();
        let mock = login_mock(&server, "testsid123");

        let client =
            QbitClient::from_config(&config_for(&server)).expect("client should construct");
        let sid = client.login().await.expect("login should succeed");

        mock.assert();
        assert_eq!(sid, "SID=testsid123");
        assert_eq!(client.sid.read().await.as_deref(), Some("SID=testsid123"));
    }

    #[tokio::test]
    async fn login_401_is_a_typed_auth_error_not_a_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path(LOGIN_PATH);
            then.status(401).body("Unauthorized");
        });

        let client =
            QbitClient::from_config(&config_for(&server)).expect("client should construct");
        let result = client.login().await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn login_fails_body_is_a_typed_auth_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path(LOGIN_PATH);
            then.status(200).body("Fails.");
        });

        let client =
            QbitClient::from_config(&config_for(&server)).expect("client should construct");
        let result = client.login().await;

        assert!(matches!(result, Err(MuseError::Upstream { .. })));
    }

    #[tokio::test]
    async fn add_parses_receipt_and_resolves_magnet_hash() {
        let server = MockServer::start();
        login_mock(&server, "testsid123");
        let add_mock = server.mock(|when, then| {
            when.method(POST)
                .path(ADD_PATH)
                .header("cookie", "SID=testsid123");
            then.status(200).body("Ok.");
        });

        let client =
            QbitClient::from_config(&config_for(&server)).expect("client should construct");
        let req = GrabRequest {
            url: "magnet:?xt=urn:btih:AABBCCDDEEFF00112233445566778899AABBCCDD&dn=Test"
                .to_string(),
            category: Some("movies".to_string()),
            save_path: Some("/media/downloads".to_string()),
            paused: false,
        };

        let receipt = client.add(req).await.expect("add should succeed");

        add_mock.assert();
        assert_eq!(
            receipt.hash.as_deref(),
            Some("aabbccddeeff00112233445566778899aabbccdd")
        );
        assert_eq!(receipt.raw_response, "Ok.");
    }

    #[tokio::test]
    async fn add_against_5xx_returns_typed_error_not_panic() {
        let server = MockServer::start();
        login_mock(&server, "testsid123");
        server.mock(|when, then| {
            when.method(POST).path(ADD_PATH);
            then.status(500).body("internal server error");
        });

        let client =
            QbitClient::from_config(&config_for(&server)).expect("client should construct");
        let result = client.add(GrabRequest::new("magnet:?xt=urn:btih:AA")).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_parses_torrent_info_array() {
        let server = MockServer::start();
        login_mock(&server, "testsid123");
        let info_mock = server.mock(|when, then| {
            when.method(GET)
                .path(INFO_PATH)
                .header("cookie", "SID=testsid123");
            then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"[
                        {
                            "hash": "aabbccddeeff00112233445566778899aabbccdd",
                            "name": "Test.Movie.2020.1080p",
                            "state": "downloading",
                            "progress": 0.42,
                            "save_path": "/media/downloads",
                            "category": "movies"
                        }
                    ]"#,
                );
        });

        let client =
            QbitClient::from_config(&config_for(&server)).expect("client should construct");
        let torrents = client.list().await.expect("list should parse");

        info_mock.assert();
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].name, "Test.Movie.2020.1080p");
        assert_eq!(torrents[0].state, "downloading");
        assert!((torrents[0].progress - 0.42).abs() < f64::EPSILON);
        assert_eq!(torrents[0].category.as_deref(), Some("movies"));
    }

    #[tokio::test]
    async fn info_returns_none_for_unknown_hash() {
        let server = MockServer::start();
        login_mock(&server, "testsid123");
        server.mock(|when, then| {
            when.method(GET)
                .path(INFO_PATH)
                .query_param("hashes", "deadbeef");
            then.status(200)
                .header("content-type", "application/json")
                .body("[]");
        });

        let client =
            QbitClient::from_config(&config_for(&server)).expect("client should construct");
        let result = client.info("deadbeef").await.expect("info should parse");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn a_403_on_a_data_call_triggers_exactly_one_reauth_then_retry() {
        let server = MockServer::start();
        // The stale SID is rejected exactly once with 403.
        let forbidden_mock = server.mock(|when, then| {
            when.method(GET)
                .path(INFO_PATH)
                .header("cookie", "SID=stale-sid");
            then.status(403).body("Forbidden");
        });
        // Re-auth (the only mock on the login path in this test — a second
        // hit here, or none at all, would mean the retry logic is wrong).
        let relogin_mock = server.mock(|when, then| {
            when.method(POST).path(LOGIN_PATH);
            then.status(200)
                .header("set-cookie", "SID=fresh-sid; path=/; HttpOnly")
                .body("Ok.");
        });
        // Retry with the fresh SID succeeds.
        let retry_mock = server.mock(|when, then| {
            when.method(GET)
                .path(INFO_PATH)
                .header("cookie", "SID=fresh-sid");
            then.status(200)
                .header("content-type", "application/json")
                .body("[]");
        });

        let client =
            QbitClient::from_config(&config_for(&server)).expect("client should construct");
        // Prime the cache with an already-stale SID directly (rather than via
        // a real login round trip), mirroring a session that was valid
        // earlier in the process lifetime and has since expired server-side.
        *client.sid.write().await = Some("SID=stale-sid".to_string());

        let result = client.list().await;

        assert!(result.is_ok(), "expected retry-after-reauth to succeed");
        forbidden_mock.assert_hits(1);
        relogin_mock.assert_hits(1);
        retry_mock.assert_hits(1);
    }

    #[tokio::test]
    async fn delete_issues_the_right_form() {
        let server = MockServer::start();
        login_mock(&server, "testsid123");
        let delete_mock = server.mock(|when, then| {
            when.method(POST)
                .path(DELETE_PATH)
                .header("cookie", "SID=testsid123")
                .x_www_form_urlencoded_tuple("hashes", "aabbcc")
                .x_www_form_urlencoded_tuple("deleteFiles", "true");
            then.status(200).body("Ok.");
        });

        let client =
            QbitClient::from_config(&config_for(&server)).expect("client should construct");
        client
            .delete("aabbcc", true)
            .await
            .expect("delete should succeed");

        delete_mock.assert();
    }

    #[tokio::test]
    async fn connection_failure_is_surfaced_not_panicked() {
        let client = QbitClient::new(
            "http://127.0.0.1:1",
            "admin",
            QbitPassword::from("hunter2".to_string()),
        )
        .expect("client should construct even for an unreachable host");

        let result = client.login().await;
        assert!(result.is_err());
    }

    #[test]
    fn debug_never_prints_password_or_sid() {
        let client = QbitClient::new(
            "http://192.0.2.60:8080",
            "admin",
            QbitPassword::from("hunter2".to_string()),
        )
        .expect("client should construct");

        let debug = format!("{client:?}");
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("<redacted>"));
    }
}
