//! MUSEM-02: the download-client seam — Muse's first *write* to an
//! acquisition substrate.
//!
//! Everything up to this item (`arr::client::ArrClient`, `prowlarr::client`)
//! is deliberately read-only per the S96 founding spec §1. This module is
//! scoped narrowly: it only executes an add/list/info/delete against a
//! download client. It does NOT decide *what* to grab — that's a later item
//! (MUSEM-04) that will call [`DownloadClient::add`] with a
//! [`GrabRequest`] it has already picked.
//!
//! [`DownloadClient`] is a trait (mirrors
//! `crate::arr::request::MediaRequestSink` /
//! `crate::discord::client::DiscordClient`'s trait-plus-mock shape) so
//! SABnzbd or another client can slot in later without touching callers, and
//! so the grab path is mockable in tests via [`MockDownloadClient`].
//!
//! [`qbit::QbitClient`] is the only live implementation shipped in this item,
//! talking to the qBittorrent WebUI v2 API.

pub mod config;
pub mod qbit;

use std::sync::Mutex;

use crate::error::MuseResult;

/// A request to add one item (magnet URI or `.torrent` URL) to a download
/// client. `category`/`save_path` are left `None` when the caller has no
/// opinion — implementations omit those fields entirely rather than sending
/// an empty string, letting the download client apply its own defaults (see
/// the MUSEM-02 spec's EDGE CASES).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrabRequest {
    /// A magnet URI or a direct `.torrent` URL. qBittorrent's `torrents/add`
    /// `urls=` field accepts either.
    pub url: String,
    pub category: Option<String>,
    pub save_path: Option<String>,
    /// Add in a paused state (no auto-start).
    pub paused: bool,
}

impl GrabRequest {
    /// Convenience constructor for the common case: just a url, everything
    /// else defaulted (no category/save-path opinion, not paused).
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            category: None,
            save_path: None,
            paused: false,
        }
    }
}

/// The result of a successful [`DownloadClient::add`]. `hash` carries the
/// resolved infohash where it's known: some download clients don't return it
/// in the add response body at all (qBittorrent's `torrents/add` returns a
/// bare `"Ok."`), so callers that already know the infohash (a magnet URI
/// carries it in `xt=urn:btih:`) resolve it client-side instead of assuming
/// the response body carries one — see [`qbit::extract_infohash_from_magnet`].
/// `None` only when neither the response nor the submitted url yielded one
/// (e.g. a `.torrent`-URL add on an older qBittorrent build) — callers must
/// treat that as "submitted, hash not yet known" rather than a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrabReceipt {
    pub hash: Option<String>,
    /// The raw response body, kept for diagnostics/audit — never parsed for
    /// anything beyond the `"Ok."` success check.
    pub raw_response: String,
}

/// One torrent's current state, as reported by `torrents/info`. Field names
/// match the qBittorrent WebUI v2 JSON keys 1:1 (`hash`, `name`, `state`,
/// `progress`, `save_path`, `category`), so [`qbit::QbitClient`] derives
/// `Deserialize` straight onto this type rather than an intermediate wire
/// struct.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct TorrentStatus {
    pub hash: String,
    pub name: String,
    /// qBittorrent's own state string (e.g. `"downloading"`,
    /// `"pausedUP"`, `"error"`) — passed through verbatim rather than
    /// re-modeled into an enum, since the exact vocabulary is
    /// implementation-specific and callers that need semantics can match on
    /// the substrings they care about.
    pub state: String,
    /// 0.0..=1.0
    pub progress: f64,
    pub save_path: String,
    pub category: Option<String>,
}

/// The download-client seam. One instance talks to exactly one download
/// client (mirrors `ArrClient`: one instance = one target). Never decides
/// *what* to grab — only executes what a caller already picked.
#[async_trait::async_trait]
pub trait DownloadClient: Send + Sync {
    async fn add(&self, req: GrabRequest) -> MuseResult<GrabReceipt>;
    async fn list(&self) -> MuseResult<Vec<TorrentStatus>>;
    async fn info(&self, hash: &str) -> MuseResult<Option<TorrentStatus>>;
    async fn delete(&self, hash: &str, delete_files: bool) -> MuseResult<()>;
}

/// A deterministic, network-free [`DownloadClient`] for tests. Records every
/// [`GrabRequest`] it receives (for MUSEM-05's tests to inspect what was
/// submitted) and returns a synthetic receipt with no real download-client
/// round trip.
#[derive(Debug, Default)]
pub struct MockDownloadClient {
    pub added: Mutex<Vec<GrabRequest>>,
}

impl MockDownloadClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn added_count(&self) -> usize {
        self.added.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl DownloadClient for MockDownloadClient {
    async fn add(&self, req: GrabRequest) -> MuseResult<GrabReceipt> {
        let hash = qbit::extract_infohash_from_magnet(&req.url);
        self.added.lock().unwrap().push(req);
        Ok(GrabReceipt {
            hash,
            raw_response: "Ok.".to_string(),
        })
    }

    async fn list(&self) -> MuseResult<Vec<TorrentStatus>> {
        Ok(Vec::new())
    }

    async fn info(&self, _hash: &str) -> MuseResult<Option<TorrentStatus>> {
        Ok(None)
    }

    async fn delete(&self, _hash: &str, _delete_files: bool) -> MuseResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_records_adds_and_resolves_magnet_hash() {
        let mock = MockDownloadClient::new();
        let req = GrabRequest::new(
            "magnet:?xt=urn:btih:AABBCCDDEEFF00112233445566778899AABBCCDD&dn=Test",
        );

        let receipt = mock.add(req.clone()).await.expect("add should succeed");

        assert_eq!(mock.added_count(), 1);
        assert_eq!(
            receipt.hash.as_deref(),
            Some("aabbccddeeff00112233445566778899aabbccdd")
        );
        assert_eq!(mock.added.lock().unwrap()[0], req);
    }

    #[tokio::test]
    async fn mock_add_without_infohash_leaves_hash_none() {
        let mock = MockDownloadClient::new();
        let req = GrabRequest::new("https://example.invalid/some.torrent");

        let receipt = mock.add(req).await.expect("add should succeed");

        assert!(receipt.hash.is_none());
    }
}
