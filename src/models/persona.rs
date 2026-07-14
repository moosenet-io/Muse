//! `personas` / `persona_members` (MUSEX-02, Plane TERM #378) — latent
//! taste personas, each a static pgvector taste centroid addressable by id
//! or `(account, name)`, plus its defining-signal provenance for
//! explainability. See `crate::persona` for derivation + the `explain()`
//! reader, `crate::repo::persona` for the CRUD/addressability layer these
//! row types back. `migrations/0100_personas.sql` is the schema.

use chrono::{DateTime, Utc};
use pgvector::Vector;
use serde::Serialize;
use serde_json::Value as Json;
use sqlx::FromRow;

/// A context-cluster-derived persona (`crate::persona::derive::derive_context_cluster_personas`).
pub const PERSONA_KIND_DERIVED: &str = "derived";
/// An operator/user-declared persona over a caller-chosen set of media
/// items (`crate::persona::derive::derive_explicit`). Stored as free text
/// (not a Postgres enum) to match this crate's existing
/// `taste_signals.signal_type` convention.
pub const PERSONA_KIND_EXPLICIT: &str = "explicit";

// Only `Serialize` (not `Deserialize`) — same `pgvector::Vector` +
// `#[serde(skip)]` + no-`Default` constraint as `models::taste::TasteProfile`
// / `models::embedding::Embedding`: this type is only ever produced by
// reading a row back via `FromRow`, never built from deserialized JSON.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Persona {
    pub id: i64,
    /// `Some(account_id)` — a persona owned directly by one account.
    /// `None` — a SHARED persona; its member accounts live in
    /// `persona_members` (see `crate::repo::persona::list_members`).
    pub account_id: Option<i64>,
    pub name: String,
    pub kind: String,
    #[serde(skip)]
    pub centroid: Vector,
    pub defining_signals: Json,
    pub metadata: Json,
    pub sample_size: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Full replacement set for a persona write — both
/// `repo::persona::upsert_for_account` (single-account, keyed upsert) and
/// `repo::persona::insert_shared` (multi-account, plain insert) take one of
/// these.
#[derive(Debug, Clone)]
pub struct NewPersona {
    pub account_id: Option<i64>,
    pub name: String,
    pub kind: String,
    pub centroid: Vector,
    pub defining_signals: Json,
    pub metadata: Json,
    pub sample_size: i32,
}
