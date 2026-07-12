//! `embeddings` — pgvector recall over library items/people/collections and
//! taste centroids (spec §3.4). Dim is pinned to 768 (nomic-embed-text, S96
//! §0.7); see `migrations/0018_embeddings.sql`.

use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

pub const EMBEDDING_DIM: i32 = 768;
pub const DEFAULT_EMBEDDING_MODEL: &str = "nomic-embed-text";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingEntityKind {
    MediaItem,
    Person,
    Collection,
    TasteCentroid,
}

impl EmbeddingEntityKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EmbeddingEntityKind::MediaItem => "media_item",
            EmbeddingEntityKind::Person => "person",
            EmbeddingEntityKind::Collection => "collection",
            EmbeddingEntityKind::TasteCentroid => "taste_centroid",
        }
    }
}

// Only `Serialize` (not `Deserialize`): `pgvector::Vector` doesn't implement
// `Default`, and serde's `#[serde(skip)]` on a field requires one to derive
// `Deserialize` (it needs a value to fill the skipped field in). This type
// is only ever produced by reading rows back via `FromRow`, never built
// from deserialized JSON, so `Deserialize` isn't needed.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Embedding {
    pub id: i64,
    pub entity_kind: String,
    pub entity_id: i64,
    pub model: String,
    pub dim: i32,
    #[serde(skip)]
    pub embedding: Vector,
    pub source_text: Option<String>,
    pub embedded_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEmbedding {
    pub entity_kind: EmbeddingEntityKind,
    pub entity_id: i64,
    pub model: String,
    pub dim: i32,
    pub embedding: Vector,
    pub source_text: Option<String>,
}

impl NewEmbedding {
    /// Build a new embedding row using the pinned default model/dim
    /// (nomic-embed-text, 768) — the common case; pass `model`/`dim`
    /// explicitly via the struct literal for a non-default model.
    pub fn nomic(entity_kind: EmbeddingEntityKind, entity_id: i64, embedding: Vec<f32>, source_text: Option<String>) -> Self {
        Self {
            entity_kind,
            entity_id,
            model: DEFAULT_EMBEDDING_MODEL.to_string(),
            dim: EMBEDDING_DIM,
            embedding: Vector::from(embedding),
            source_text,
        }
    }
}

/// A nearest-neighbor search hit (entity + cosine distance).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct EmbeddingMatch {
    pub entity_kind: String,
    pub entity_id: i64,
    pub distance: f64,
}
