//! MUSE-08 / S125: embedding pipeline.
//!
//! Composes a deterministic `source_text` per `media_metadata`-backed
//! `media_item`, embeds it via Chord's standardized `/v1/embeddings`
//! (`qwen3-embedding`, 1024-dim) — the fleet's single embedding door — and
//! writes/updates the MUSE-03 `embeddings` table (pgvector, HNSW, cosine).
//! Everything here is read-only against Plex/arr and additive against
//! Postgres — it never touches the live media library.
//!
//! - [`chord::ChordEmbedClient`] — a tiny HTTP client for Chord's
//!   OpenAI-compatible `/v1/embeddings` endpoint. Constructed via
//!   [`chord::ChordEmbedClient::from_config`], which returns `None` when
//!   neither `CHORD_EMBEDDINGS_URL` nor `CHORD_URL` is configured — the
//!   embedding pipeline degrades (skips, doesn't fail) rather than blocking
//!   startup, same posture as `PlexClient`/`ProwlarrClient`/`TmdbClient`.
//! - [`pipeline`] — `compose_source_text` (deterministic composition),
//!   `embed_stale` (the incremental, VRAM-polite batch driver), and
//!   `nearest` (the cosine nearest-neighbor primitive MUSE-09's recall
//!   layer builds on).
//! - [`reembed_1024`] — S125 one-shot orchestrator helpers to migrate the
//!   `embeddings` table + derived centroids from nomic(768) to qwen3(1024).
//!
//! S125 rationale: embeddings now route through Chord (not a direct-Ollama
//! `nomic-embed-text` call) so every fleet module shares ONE embedding
//! model/space rather than each service pinning its own local Ollama model.

pub mod chord;
pub mod pipeline;
pub mod reembed_1024;

pub use chord::ChordEmbedClient;
pub use pipeline::{compose_source_text, embed_stale, nearest, EmbedOutcome};
