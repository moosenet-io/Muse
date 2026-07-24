//! S125: HTTP client for Chord's standardized OpenAI-compatible
//! `/v1/embeddings` endpoint — the fleet's single embedding door.
//!
//! Replaces the former direct-Ollama `nomic-embed-text` (768-dim) path: all
//! Muse embeddings now go through Chord's `qwen3-embedding` (1024-dim) serve,
//! so every module in the fleet shares one embedding space/model rather than
//! each service picking its own local Ollama model.
//!
//! Construction is via [`ChordEmbedClient::from_config`], which returns
//! `None` when neither `CHORD_EMBEDDINGS_URL` nor `CHORD_URL` is configured —
//! callers treat embeddings as an optional, gracefully-degrading dependency
//! exactly like the former `OllamaEmbedClient` did (and like
//! `PlexClient`/`ProwlarrClient`/`TmdbClient`).
//!
//! ## Auth (S125 deviation note)
//! The sibling [`crate::taste_model::chord_client::ChordClient`] (chat/vision)
//! currently sends NO auth header, so there is no pre-existing "Chord JWT
//! plumbing" to literally reuse. This client therefore introduces an
//! *optional* bearer credential — `CHORD_API_TOKEN`
//! ([`crate::config::Config::chord_api_token`]) — attached only when set,
//! materialized from <secret-manager> at runtime, never a literal (S1/S7). When the
//! Chord proxy is unauthenticated (the current deploy), leave it unset and
//! the client posts without an `Authorization` header, unchanged from the
//! Ollama-era behavior.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{MuseError, MuseResult};

/// Generous but bounded — <host>'s GPU is shared with a permanent
/// `lemonade-coder` production serve, so a request can queue behind other
/// work rather than failing outright. Chord itself may also route/queue.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// OpenAI-compatible embeddings request body (`{"input","model"}`). `input`
/// is a single string here (Muse embeds one composed `source_text` / query
/// at a time); Chord also accepts an array, which we don't need.
#[derive(Debug, Serialize)]
struct EmbeddingsRequest<'a> {
    input: &'a str,
    model: &'a str,
}

/// OpenAI-compatible embeddings response (`{"data":[{"embedding":[...]}]}`).
#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    #[serde(default)]
    data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    #[serde(default)]
    embedding: Vec<f32>,
}

/// A typed client for Chord's `POST /v1/embeddings` endpoint.
#[derive(Debug, Clone)]
pub struct ChordEmbedClient {
    http: reqwest::Client,
    base_url: String,
    /// Optional bearer credential (`CHORD_API_TOKEN`). `None` posts without
    /// an `Authorization` header (current unauthenticated Chord deploy).
    api_token: Option<String>,
}

impl ChordEmbedClient {
    /// Build a client against a specific Chord base URL (e.g. the proxy root
    /// `http://192.0.2.20:8099`, or an httpmock server in tests). The
    /// `/v1/embeddings` path is appended per call.
    pub fn new(base_url: impl Into<String>) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_token: None,
        })
    }

    /// Attach an optional bearer token (builder-style). `None` is a no-op.
    pub fn with_token(mut self, api_token: Option<String>) -> Self {
        self.api_token = api_token;
        self
    }

    /// Build a client from `Config`. Prefers `CHORD_EMBEDDINGS_URL`
    /// ([`Config::chord_embeddings_url`]) and falls back to the shared
    /// `CHORD_URL` ([`Config::chord_url`]) — both point at the same Chord
    /// proxy, so a deployment that already sets `CHORD_URL` gets embeddings
    /// routing for free, while `CHORD_EMBEDDINGS_URL` exists as an explicit
    /// override seam. Returns `None` when neither is set (or the client
    /// fails to construct) — the embedding pipeline degrades to "nothing to
    /// do" rather than blocking startup or a caller. Never panics.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config
            .chord_embeddings_url
            .clone()
            .or_else(|| config.chord_url.clone())?;

        match Self::new(url) {
            Ok(client) => Some(client.with_token(config.chord_api_token.clone())),
            Err(e) => {
                tracing::warn!(error = %e, "failed to construct Chord embed client; embeddings will degrade");
                None
            }
        }
    }

    /// Embed a single piece of text with the given model via Chord's
    /// `/v1/embeddings`, returning the raw vector. Callers are responsible
    /// for validating dimensionality matches what they intend to store (the
    /// `embeddings` table pins `vector(1024)` for `qwen3-embedding` post
    /// S125). Signature is intentionally identical to the former
    /// `OllamaEmbedClient::embed` so every call site is a drop-in repoint.
    pub async fn embed(&self, model: &str, text: &str) -> MuseResult<Vec<f32>> {
        let url = format!("{}/v1/embeddings", self.base_url);

        let mut req = self
            .http
            .post(&url)
            .json(&EmbeddingsRequest { input: text, model });
        if let Some(token) = &self.api_token {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(MuseError::Upstream {
                status: status.as_u16(),
                message: format!("chord embeddings request to {url} failed: {body}"),
            });
        }

        let parsed: EmbeddingsResponse = serde_json::from_slice(&bytes).map_err(|e| MuseError::Upstream {
            status: status.as_u16(),
            message: format!("failed to parse chord embeddings response from {url}: {e}"),
        })?;

        let embedding = parsed.data.into_iter().next().map(|d| d.embedding).unwrap_or_default();

        if embedding.is_empty() {
            return Err(MuseError::upstream(format!(
                "chord returned an empty embedding vector for model {model} (is it loaded/routable?)"
            )));
        }

        Ok(embedding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn client_for(server: &MockServer) -> ChordEmbedClient {
        ChordEmbedClient::new(server.base_url()).expect("client should construct")
    }

    #[tokio::test]
    async fn embed_sends_openai_shape_and_parses_vector_from_data() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .json_body(serde_json::json!({"input": "Arrival (2016)", "model": "qwen3-embedding"}));
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"data": [{"embedding": [0.1, 0.2, 0.3]}]}"#);
        });

        let client = client_for(&server);
        let vector = client
            .embed("qwen3-embedding", "Arrival (2016)")
            .await
            .expect("embed should succeed");

        mock.assert();
        assert_eq!(vector, vec![0.1, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn embed_attaches_bearer_when_token_configured() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .header("authorization", "Bearer secret-chord-token");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"data": [{"embedding": [1.0, 2.0]}]}"#);
        });

        let client = client_for(&server).with_token(Some("secret-chord-token".to_string()));
        let vector = client.embed("qwen3-embedding", "anything").await.expect("embed should succeed");

        mock.assert();
        assert_eq!(vector, vec![1.0, 2.0]);
    }

    #[tokio::test]
    async fn embed_surfaces_upstream_error_status() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(500).body("model not loaded");
        });

        let client = client_for(&server);
        let result = client.embed("qwen3-embedding", "anything").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn embed_rejects_empty_data_without_panicking() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"data": []}"#);
        });

        let client = client_for(&server);
        assert!(client.embed("qwen3-embedding", "anything").await.is_err());
    }

    #[tokio::test]
    async fn embed_rejects_empty_embedding_vector_without_panicking() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"data": [{"embedding": []}]}"#);
        });

        let client = client_for(&server);
        assert!(client.embed("qwen3-embedding", "anything").await.is_err());
    }

    #[tokio::test]
    async fn embed_malformed_json_does_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .header("content-type", "application/json")
                .body("{not valid json");
        });

        let client = client_for(&server);
        assert!(client.embed("qwen3-embedding", "anything").await.is_err());
    }

    #[test]
    fn from_config_returns_none_when_unconfigured() {
        let config = Config::default();
        assert!(ChordEmbedClient::from_config(&config).is_none());
    }

    #[test]
    fn from_config_prefers_embeddings_url_then_falls_back_to_chord_url() {
        // RFC 5737 TEST-NET addresses — never real fleet hosts.
        let mut config = Config::default();
        config.chord_url = Some("http://192.0.2.20:8099".to_string());
        assert!(
            ChordEmbedClient::from_config(&config).is_some(),
            "CHORD_URL alone should be enough to construct the embed client"
        );

        config.chord_embeddings_url = Some("http://192.0.2.30:8099".to_string());
        let client = ChordEmbedClient::from_config(&config).expect("dedicated embeddings url should construct");
        assert_eq!(client.base_url, "http://192.0.2.30:8099");
    }
}
