//! MUSEX-02 (Plane TERM #378): the persona model — latent taste PERSONAS,
//! per account and (for a household/couch-group) spanning several.
//!
//! ## What a persona is
//! A persona is a static-at-rest pgvector taste VECTOR plus metadata and
//! defining-signal provenance (`crate::models::persona::Persona`,
//! `migrations/0100_personas.sql`). It is deliberately a *view* over
//! already-computed taste data, never a second taste-storage path: a
//! persona's centroid is built by averaging the SAME `embeddings` rows
//! (`crate::repo::embedding`) `taste_model::profile::compute_overall_centroid`/
//! `compute_context_centroids` already average, via a shared helper
//! ([`crate::taste_model::profile::mean_embedding`]) so the two "average
//! these titles' embeddings" call sites can't drift apart. See
//! `docs/MUSEX-experience-layer.md` §1.2/§3 for how this module fits the
//! larger MUSEX build map — that document is the build map this module was
//! written against.
//!
//! ## Two ways a persona comes to exist ([`derive`])
//! - **Derived** ([`derive::derive_context_cluster_personas`]): clusters an
//!   account's finished watch signals by CONTEXT bucket (reusing
//!   `taste_model::profile::context_key_for`'s existing weekend/weekday x
//!   time-of-day buckets as the deterministic "cluster" — see `derive`'s
//!   module doc for why that's the chosen clustering algorithm over a
//!   general k-means).
//! - **Explicit** ([`derive::derive_explicit`]): an operator/user-declared
//!   persona over a caller-chosen set of media items — no context bucketing
//!   involved, just "these titles define this persona."
//!
//! Both paths produce a [`derive::DerivedPersona`] (name, centroid,
//! defining_signals, sample_size), which `crate::repo::persona::upsert_for_account`
//! (single-account) or `insert_shared`/`add_member` (household/couch-group,
//! spanning accounts — the "one persona spans people" half of the AC) then
//! persists.
//!
//! ## Addressability
//! `crate::repo::persona::{list_for_account, get_by_id, get_by_name_for_account}`
//! is the seam a later MUSEX-03 (persona blending) selects/blends personas
//! through — by id or by `(account, name)`, whether the persona is owned
//! directly or reached via a shared membership.
//!
//! ## Determinism
//! Every step from raw signal to stored vector is a pure function of its DB
//! inputs at a given instant: `context_key_for` is pure, bucket iteration
//! is a `BTreeMap` (never a `HashMap`), and [`crate::taste_model::profile::mean_embedding`]
//! sorts+dedups its id list before summing so floating-point addition order
//! never depends on database row order or `HashMap` iteration order. See
//! `derive`'s module doc and its determinism tests for the full argument,
//! and `crate::taste_model::profile`'s `mean_embedding_is_order_independent_and_bit_deterministic`
//! test for the negative-nondeterminism guard at the primitive level.
//!
//! ## Explainability ([`Persona::explain`])
//! A persona's `defining_signals` jsonb column is written exactly once, at
//! derivation time, by [`derive`] — [`Persona::explain`] below just reads it
//! back into a structured [`PersonaExplanation`] rather than recomputing
//! anything, so "why this persona" always matches the vector actually
//! stored even if the account's live taste has since moved on.
//!
//! ## Blending ([`blend`], MUSEX-03)
//! [`blend::blend_personas`] folds several of the above into one SESSION
//! taste vector for group watching — the addressability seam this module
//! doc already flagged ([`crate::repo::persona::list_for_account`]/
//! `get_by_id`) is exactly how a caller resolves the [`Persona`] rows it
//! hands to `blend_personas`. See `blend`'s module doc for the
//! intersection-not-average formula and the no-overlap rule.

pub mod blend;
pub mod derive;

use serde_json::Value as Json;

use crate::models::persona::Persona;

/// The "why this persona" explanation surfaced from a [`Persona`]'s stored
/// `defining_signals` — top genres that drove the persona's centroid, the
/// context bucket it was clustered from (derived personas only), and the
/// source media items whose embeddings were averaged to build it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PersonaExplanation {
    /// The context bucket (e.g. `"weekend_evening"`) a derived persona was
    /// clustered from. `None` for an explicit persona (no context bucket
    /// drove it) or if the field is missing/malformed.
    pub context_key: Option<String>,
    /// `(genre, count)`, ranked highest-count-first, alphabetical-name
    /// tiebreak (matches `repo::persona::genre_counts_for_media_items`'s
    /// `ORDER BY`) — the top genres among this persona's source titles.
    pub top_genres: Vec<(String, i64)>,
    /// The `media_items.id`s this persona's centroid was averaged over —
    /// the raw signal provenance behind `top_genres`.
    pub source_media_item_ids: Vec<i64>,
}

impl Persona {
    /// Parse this persona's `defining_signals` jsonb into a structured
    /// [`PersonaExplanation`]. Defensive against any malformed/missing
    /// field (degrades to `None`/empty, never panics) even though in
    /// practice `defining_signals` is written exclusively by
    /// [`derive::defining_signals_json`].
    pub fn explain(&self) -> PersonaExplanation {
        let Json::Object(map) = &self.defining_signals else {
            return PersonaExplanation::default();
        };

        let context_key = map
            .get("context_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let top_genres = map
            .get("top_genres")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| {
                        let genre = entry.get("genre")?.as_str()?.to_string();
                        let count = entry.get("count")?.as_i64()?;
                        Some((genre, count))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let source_media_item_ids = map
            .get("source_media_item_ids")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();

        PersonaExplanation {
            context_key,
            top_genres,
            source_media_item_ids,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::persona::{Persona, PERSONA_KIND_DERIVED};
    use chrono::Utc;
    use serde_json::json;

    fn persona_with_signals(defining_signals: Json) -> Persona {
        Persona {
            id: 1,
            account_id: Some(1),
            name: "test-persona".to_string(),
            kind: PERSONA_KIND_DERIVED.to_string(),
            centroid: pgvector::Vector::from(vec![0.0f32; 768]),
            defining_signals,
            metadata: json!({}),
            sample_size: 3,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn explain_parses_a_well_formed_defining_signals_value() {
        let persona = persona_with_signals(json!({
            "context_key": "weekend_evening",
            "top_genres": [
                {"genre": "comfort-drama", "count": 3},
                {"genre": "documentary", "count": 1},
            ],
            "source_media_item_ids": [10, 11, 12],
        }));
        let explanation = persona.explain();
        assert_eq!(explanation.context_key, Some("weekend_evening".to_string()));
        assert_eq!(
            explanation.top_genres,
            vec![
                ("comfort-drama".to_string(), 3),
                ("documentary".to_string(), 1)
            ]
        );
        assert_eq!(explanation.source_media_item_ids, vec![10, 11, 12]);
    }

    #[test]
    fn explain_degrades_cleanly_on_missing_or_malformed_fields() {
        assert_eq!(
            persona_with_signals(json!({})).explain(),
            PersonaExplanation::default()
        );
        assert_eq!(
            persona_with_signals(json!(null)).explain(),
            PersonaExplanation::default()
        );
        assert_eq!(
            persona_with_signals(json!({"top_genres": "not-an-array"})).explain(),
            PersonaExplanation::default()
        );
    }
}
