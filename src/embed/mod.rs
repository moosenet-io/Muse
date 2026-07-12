//! MUSE-08: local embedding pipeline.
//!
//! Composes a deterministic `source_text` per `media_metadata`-backed
//! `media_item`, embeds it via a LOCAL `nomic-embed-text` model served by
//! Ollama on <host>, and writes/updates the MUSE-03 `embeddings` table
//! (pgvector, HNSW, cosine). Everything here is read-only against Plex/arr
//! and additive against Postgres — it never touches the live media library.
//!
//! - [`ollama::OllamaEmbedClient`] — a tiny, direct HTTP client for
//!   Ollama's `/api/embeddings` endpoint. Constructed via
//!   [`ollama::OllamaEmbedClient::from_config`], which returns `None` when
//!   `MUSE_OLLAMA_URL` isn't configured — the embedding pipeline degrades
//!   (skips, doesn't fail) rather than blocking startup, same posture as
//!   `PlexClient`/`ProwlarrClient`/`TmdbClient::from_config`.
//! - [`pipeline`] — `compose_source_text` (deterministic composition),
//!   `embed_stale` (the incremental, VRAM-polite batch driver), and
//!   `nearest` (the cosine nearest-neighbor primitive MUSE-09's recall
//!   layer builds on).
//!
//! No dependency on Chord: embeddings are cheap enough (a tiny model, one
//! short vector per title) to call Ollama directly rather than routing
//! through the proxy/orchestrator the way heavier reasoning calls do.

pub mod ollama;
pub mod pipeline;

pub use ollama::OllamaEmbedClient;
pub use pipeline::{compose_source_text, embed_stale, nearest, EmbedOutcome};
