//! S125: HTTP client for Chord's standardized OpenAI-compatible
//! `/v1/embeddings` endpoint — the fleet's single embedding door.
//!
//! Replaces the former direct-Ollama `nomic-embed-text` (768-dim) path: all
//! Muse embeddings now go through Chord's `qwen3-embedding` (1024-dim) serve,
//! so every module in the fleet shares one embedding space/model rather than
//! each service picking its own local Ollama model.
//!
//! Construction is via [`ChordEmbedClient::from_config`], which returns
//! `None` when the embeddings endpoint is not fully configured — callers
//! treat embeddings as an optional, gracefully-degrading dependency exactly
//! like the former `OllamaEmbedClient` did (and like
//! `PlexClient`/`ProwlarrClient`/`TmdbClient`).
//!
//! ## Auth — the bearer token is REQUIRED (S125 review finding)
//! Chord's `/v1/embeddings` is JWT-gated: an unauthenticated POST returns
//! `401 "Missing Authorization header"`, so a tokenless client would silently
//! 401 on every row. The token is therefore a REQUIRED field of this client
//! (`CHORD_API_TOKEN`, [`crate::config::Config::chord_api_token`]) — a
//! `ChordEmbedClient` cannot be constructed without one, which structurally
//! guarantees we never post unauthenticated. [`ChordEmbedClient::from_config`]
//! logs a loud error and returns `None` when a Chord URL is set but
//! `CHORD_API_TOKEN` is missing (a misconfiguration), rather than degrading
//! to a silent 401 storm. The token is materialized from <secret-manager> at
//! runtime, never a literal (S1/S7).
//!
//! NOTE: the sibling chat/vision [`crate::taste_model::chord_client::ChordClient`]
//! does not yet send this header (chat wasn't observed to be JWT-gated); if
//! that changes, wire `chord_api_token` into it too.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{MuseError, MuseResult};
use crate::models::embedding::EMBEDDING_DIM;

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
    /// REQUIRED bearer credential (`CHORD_API_TOKEN`). Non-optional so a
    /// client can never post unauthenticated (Chord embeddings are JWT-gated).
    api_token: String,
}

impl ChordEmbedClient {
    /// Build a client against a specific Chord base URL (e.g. the proxy root
    /// `http://192.0.2.20:8099`, or an httpmock server in tests) with the
    /// required bearer token. The `/v1/embeddings` path is appended per call.
    pub fn new(base_url: impl Into<String>, api_token: impl Into<String>) -> MuseResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(MuseError::Http)?;

        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_token: api_token.into(),
        })
    }

    /// Build a client from `Config`. Prefers `CHORD_EMBEDDINGS_URL`
    /// ([`Config::chord_embeddings_url`]) and falls back to the shared
    /// `CHORD_URL` ([`Config::chord_url`]) — both point at the same Chord
    /// proxy. Returns `None` when:
    /// - neither URL is set (embeddings simply unconfigured — quiet degrade),
    ///   or
    /// - a URL IS set but `CHORD_API_TOKEN` is missing — a MISCONFIGURATION,
    ///   logged at ERROR (Chord embeddings need a JWT; we refuse to build a
    ///   client that would 401 on every call rather than post unauthenticated).
    ///
    /// Never panics.
    pub fn from_config(config: &Config) -> Option<Self> {
        let url = config
            .chord_embeddings_url
            .clone()
            .or_else(|| config.chord_url.clone())?;

        let Some(token) = config.chord_api_token.clone() else {
            tracing::error!(
                "CHORD_API_TOKEN required — Chord embeddings need a JWT, but a Chord URL is \
                 configured with no token; embeddings DISABLED (set CHORD_API_TOKEN)"
            );
            return None;
        };

        match Self::new(url, token) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::error!(error = %e, "failed to construct Chord embed client; embeddings DISABLED");
                None
            }
        }
    }

    /// Embed a single piece of text with the given model via Chord's
    /// `/v1/embeddings`, returning the raw vector. The returned vector's
    /// length is validated to equal [`EMBEDDING_DIM`] (1024) — a wrong-dim
    /// vector is an error, never stored (it would corrupt the pgvector space
    /// / fail the column width). Signature is intentionally identical to the
    /// former `OllamaEmbedClient::embed` so every call site is a drop-in
    /// repoint.
    pub async fn embed(&self, model: &str, text: &str) -> MuseResult<Vec<f32>> {
        let url = format!("{}/v1/embeddings", self.base_url);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_token)
            .json(&EmbeddingsRequest { input: text, model })
            .send()
            .await?;

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

        // Dimension guard: never hand back (and never let a caller store) a
        // vector that isn't the pinned 1024 width. A mismatch means the model
        // routed by Chord isn't the expected `qwen3-embedding` — surface it
        // loudly rather than corrupting the vector store.
        if embedding.len() != EMBEDDING_DIM as usize {
            return Err(MuseError::upstream(format!(
                "chord embedding for model {model} has wrong dimensionality: got {}, expected {} \
                 (EMBEDDING_DIM) — refusing to store a wrong-dim vector",
                embedding.len(),
                EMBEDDING_DIM
            )));
        }

        Ok(embedding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    const TEST_TOKEN: &str = "test-chord-jwt";

    fn client_for(server: &MockServer) -> ChordEmbedClient {
        ChordEmbedClient::new(server.base_url(), TEST_TOKEN).expect("client should construct")
    }

    /// A JSON body whose `data[0].embedding` is a full [`EMBEDDING_DIM`]-length
    /// vector — its first three entries are `[a, b, c]`, the rest `0.0`. Lets
    /// tests assert on recognizable leading values while still passing the
    /// dimensionality guard.
    fn body_with_full_vector(a: f32, b: f32, c: f32) -> String {
        let mut v = vec![0.0_f32; EMBEDDING_DIM as usize];
        v[0] = a;
        v[1] = b;
        v[2] = c;
        serde_json::json!({ "data": [{ "embedding": v }] }).to_string()
    }

    #[tokio::test]
    async fn embed_sends_openai_shape_and_parses_full_width_vector() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .header("authorization", format!("Bearer {TEST_TOKEN}"))
                .json_body(serde_json::json!({"input": "Arrival (2016)", "model": "qwen3-embedding"}));
            then.status(200)
                .header("content-type", "application/json")
                .body(body_with_full_vector(0.1, 0.2, 0.3));
        });

        let client = client_for(&server);
        let vector = client
            .embed("qwen3-embedding", "Arrival (2016)")
            .await
            .expect("embed should succeed");

        mock.assert();
        assert_eq!(vector.len(), EMBEDDING_DIM as usize);
        assert_eq!(&vector[0..3], &[0.1, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn embed_always_attaches_bearer() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .header("authorization", format!("Bearer {TEST_TOKEN}"));
            then.status(200)
                .header("content-type", "application/json")
                .body(body_with_full_vector(1.0, 2.0, 3.0));
        });

        let client = client_for(&server);
        let vector = client.embed("qwen3-embedding", "anything").await.expect("embed should succeed");

        mock.assert();
        assert_eq!(vector.len(), EMBEDDING_DIM as usize);
    }

    #[tokio::test]
    async fn embed_rejects_wrong_dimensionality_vector() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            // A 3-long vector — not EMBEDDING_DIM. Must be rejected, never stored.
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"data": [{"embedding": [0.1, 0.2, 0.3]}]}"#);
        });

        let client = client_for(&server);
        let result = client.embed("qwen3-embedding", "anything").await;
        assert!(result.is_err(), "a wrong-dim vector must be an error, never returned");
        match result.unwrap_err() {
            MuseError::Upstream { message, .. } => assert!(
                message.contains("wrong dimensionality"),
                "error should name the dimensionality mismatch, got: {message}"
            ),
            other => panic!("expected Upstream dimensionality error, got {other:?}"),
        }
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
    async fn embed_surfaces_401_when_chord_rejects_auth() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(401).body("Missing Authorization header");
        });

        let client = client_for(&server);
        match client.embed("qwen3-embedding", "anything").await.unwrap_err() {
            MuseError::Upstream { status, .. } => assert_eq!(status, 401),
            other => panic!("expected Upstream 401, got {other:?}"),
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
    fn from_config_returns_none_when_url_set_but_token_missing() {
        // RFC 5737 TEST-NET address — never a real fleet host.
        let mut config = Config::default();
        config.chord_url = Some("http://192.0.2.20:8099".to_string());
        // No CHORD_API_TOKEN -> refuse to build a client that would 401.
        assert!(
            ChordEmbedClient::from_config(&config).is_none(),
            "a URL without a token must NOT yield a client (would post unauthenticated / 401)"
        );
    }

    #[test]
    fn from_config_prefers_embeddings_url_then_falls_back_to_chord_url() {
        // RFC 5737 TEST-NET addresses — never real fleet hosts.
        let mut config = Config::default();
        config.chord_api_token = Some("a-token".to_string());
        config.chord_url = Some("http://192.0.2.20:8099".to_string());
        let client = ChordEmbedClient::from_config(&config).expect("CHORD_URL + token should construct");
        assert_eq!(client.base_url, "http://192.0.2.20:8099");

        config.chord_embeddings_url = Some("http://192.0.2.30:8099".to_string());
        let client = ChordEmbedClient::from_config(&config).expect("dedicated embeddings url should construct");
        assert_eq!(client.base_url, "http://192.0.2.30:8099");
    }
}
