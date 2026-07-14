//! Node/edge types for the MUSEX-16 watch-history + group-dynamics graph —
//! see `crate::kg`'s module doc for the overall design. Deliberately
//! generic/serializable (not tied to any one repo table's row shape) so
//! [`KgGraph`] is the same structure whether it came from
//! [`crate::kg::assemble::assemble_shared_graph`] or, later, a different
//! assembly path — and so it round-trips through JSON cleanly for an
//! external Atlas KG build step to ingest.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// What a [`KgNode`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// One opted-in Discord friend (`crate::discord::identity::FriendIdentity`).
    Person,
    /// One taste persona (`crate::models::persona::Persona`).
    Persona,
    /// One title (movie/show), keyed by `media_item_id`.
    Title,
    /// One shared-viewing session (a co-view grouping) — see
    /// [`crate::kg::assemble::CoViewRecord`].
    Session,
}

/// What a [`KgEdge`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Person <-> Person: two people watched together (undirected in
    /// practice — [`crate::kg::assemble::assemble_shared_graph`] emits one
    /// edge per co-view record; [`crate::kg::query::co_view_adjacency`]
    /// treats it symmetrically).
    CoView,
    /// Person <-> Person: two people's personas are taste-neighbors (cosine
    /// similarity at/above the configured threshold).
    TasteEdge,
    /// Person -> Title: this person watched this title.
    Watched,
    /// Person -> Persona: this persona belongs to this person.
    PersonaOf,
}

/// One node in the graph. `id` is stable and namespaced — see the id-helper
/// functions below — so the same source record always assembles to the same
/// node id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KgNode {
    pub id: String,
    pub kind: NodeKind,
    pub label: String,
    /// Free-form, kind-specific attributes (e.g. a title's year, a
    /// session's `watched_at`). Empty by default; omitted from JSON output
    /// when empty so an export stays compact.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub attrs: Map<String, Value>,
}

/// One edge in the graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KgEdge {
    pub id: String,
    pub kind: EdgeKind,
    pub source: String,
    pub target: String,
    /// e.g. a taste-edge's cosine similarity, or a co-view edge's count.
    /// `None` for edge kinds with no natural scalar weight (`Watched`,
    /// `PersonaOf`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub attrs: Map<String, Value>,
}

/// The assembled graph: every node/edge that survived
/// [`crate::kg::assemble::assemble_shared_graph`]'s privacy filter. This is
/// exactly what an external Atlas KG build step would ingest, and exactly
/// what [`crate::kg::query`]'s functions operate over.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KgGraph {
    pub nodes: Vec<KgNode>,
    pub edges: Vec<KgEdge>,
}

impl KgGraph {
    pub fn node(&self, id: &str) -> Option<&KgNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn has_node(&self, id: &str) -> bool {
        self.nodes.iter().any(|n| n.id == id)
    }

    pub fn edges_of_kind(&self, kind: EdgeKind) -> impl Iterator<Item = &KgEdge> {
        self.edges.iter().filter(move |e| e.kind == kind)
    }

    /// Every edge (of any kind) touching `node_id` as either endpoint.
    pub fn edges_touching<'a>(&'a self, node_id: &'a str) -> impl Iterator<Item = &'a KgEdge> {
        self.edges
            .iter()
            .filter(move |e| e.source == node_id || e.target == node_id)
    }
}

// --- stable id helpers ------------------------------------------------------
//
// Namespaced so a `person:` id can never collide with a `title:` id even if
// the underlying integer/string keys overlap (e.g. account id 1 and media
// item id 1).

pub fn person_node_id(discord_user_id: &str) -> String {
    format!("person:{discord_user_id}")
}

pub fn persona_node_id(persona_id: i64) -> String {
    format!("persona:{persona_id}")
}

pub fn title_node_id(media_item_id: i64) -> String {
    format!("title:{media_item_id}")
}

pub fn session_node_id(session_key: &str) -> String {
    format!("session:{session_key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids_are_stable_and_namespaced() {
        assert_eq!(person_node_id("discord-alex"), "person:discord-alex");
        assert_eq!(persona_node_id(42), "persona:42");
        assert_eq!(title_node_id(7), "title:7");
        assert_eq!(session_node_id("sess-1"), "session:sess-1");
        // Same input twice -> identical id (assembling twice from the same
        // source data must be deterministic).
        assert_eq!(
            person_node_id("discord-alex"),
            person_node_id("discord-alex")
        );
    }

    #[test]
    fn graph_lookup_helpers_find_nodes_and_edges() {
        let graph = KgGraph {
            nodes: vec![KgNode {
                id: "person:a".to_string(),
                kind: NodeKind::Person,
                label: "Alex".to_string(),
                attrs: Map::new(),
            }],
            edges: vec![KgEdge {
                id: "e1".to_string(),
                kind: EdgeKind::CoView,
                source: "person:a".to_string(),
                target: "person:b".to_string(),
                weight: None,
                attrs: Map::new(),
            }],
        };
        assert!(graph.has_node("person:a"));
        assert!(!graph.has_node("person:zzz"));
        assert_eq!(graph.node("person:a").unwrap().label, "Alex");
        assert_eq!(graph.edges_of_kind(EdgeKind::CoView).count(), 1);
        assert_eq!(graph.edges_of_kind(EdgeKind::TasteEdge).count(), 0);
        assert_eq!(graph.edges_touching("person:a").count(), 1);
        assert_eq!(graph.edges_touching("person:b").count(), 1);
        assert_eq!(graph.edges_touching("person:zzz").count(), 0);
    }
}
