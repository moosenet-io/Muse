//! Assemble a [`KgGraph`] from real watch-history/co-view/persona source
//! data, PRIVACY-SCOPED to opted-in friends only — see `crate::kg`'s module
//! doc for the overall design and the "by construction" argument.
//!
//! ## Pure, DB-free (S9) — same posture as `crate::persona::blend`
//! [`assemble_shared_graph`] takes already-fetched source records (the
//! caller resolves them from `crate::repo::watch_stats`,
//! `crate::repo::persona`, `crate::premiere::schedule` RSVP data, etc. —
//! this module doesn't prescribe the source, only the shape) and a
//! [`TrustedFriends`] allowlist. It never opens a DB connection itself,
//! matching `crate::persona::blend`'s and
//! `crate::watch_together::create_group_session`'s "pure function over
//! already-fetched data, DB resolution is a separate caller-side step"
//! idiom.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::{json, Map};

use crate::discord::identity::TrustedFriends;
use crate::kg::model::{
    person_node_id, persona_node_id, session_node_id, title_node_id, EdgeKind, KgEdge, KgGraph,
    KgNode, NodeKind,
};
use crate::persona::blend::cosine_similarity;

/// One person having watched one title — the source for `Person`/`Title`
/// nodes and `Watched` edges.
#[derive(Debug, Clone)]
pub struct WatchRecord {
    pub discord_user_id: String,
    pub media_item_id: i64,
    pub title: String,
    pub watched_at: DateTime<Utc>,
}

/// Two people watching the same title together in one session — the source
/// for `Session` nodes and `CoView` edges. One record per (ordered) pair per
/// session; a group of 3+ co-viewers is represented as one record per pair
/// (see [`assemble_shared_graph`]'s doc for how pairs become edges).
#[derive(Debug, Clone)]
pub struct CoViewRecord {
    pub person_a: String,
    pub person_b: String,
    pub session_key: String,
    pub media_item_id: i64,
    pub title: String,
    pub watched_at: DateTime<Utc>,
}

/// One person's persona — the source for `Persona` nodes, `PersonaOf`
/// edges, and (via pairwise cosine similarity against every other included
/// persona) `TasteEdge`s.
#[derive(Debug, Clone)]
pub struct PersonaRecord {
    pub discord_user_id: String,
    pub persona_id: i64,
    pub persona_name: String,
    pub centroid: Vec<f32>,
}

/// Everything [`assemble_shared_graph`] needs, already fetched by the
/// caller from the repo layer.
#[derive(Debug, Clone, Default)]
pub struct GraphSourceData {
    pub watches: Vec<WatchRecord>,
    pub co_views: Vec<CoViewRecord>,
    pub personas: Vec<PersonaRecord>,
}

/// Assemble the SHARED, exportable [`KgGraph`] from `data`, including ONLY
/// relations belonging to friends who are both allowlisted AND opted in per
/// `friends` — [`TrustedFriends::opted_in_friends`] is the single source of
/// truth for who's in scope, computed once up front into `opted_in` before
/// any node/edge is built. A discord user id absent from `opted_in` never
/// becomes a `person:` node id and never appears as an edge endpoint or in
/// any edge's attrs, no matter how many records in `data` mention them —
/// this is the enforcement point `crate::kg`'s module doc's "by
/// construction" claim rests on (mirrors
/// `crate::premiere::schedule::schedule_premiere`'s `invited` set and
/// `crate::promotion::targeting::promote_new_title`'s "never enters the
/// loop at all" argument, both built on the exact same accessor).
///
/// `taste_neighbor_threshold` is the minimum cosine similarity (in
/// `[-1.0, 1.0]`) between two opted-in people's persona centroids for a
/// `TasteEdge` to be emitted — callers should pass
/// `crate::config::Config::kg_taste_neighbor_threshold`, never a bare
/// literal (this module stays config-value-agnostic, same posture as
/// `crate::persona::blend` taking its thresholds as parameters).
pub fn assemble_shared_graph(
    friends: &TrustedFriends,
    data: &GraphSourceData,
    taste_neighbor_threshold: f32,
) -> KgGraph {
    // The ONE privacy gate: computed once, consulted before every node/edge
    // is built. Nothing below this line reads `data` without first checking
    // membership here.
    let opted_in: HashSet<String> = friends
        .opted_in_friends()
        .map(|f| f.discord_user_id.clone())
        .collect();

    let mut graph = KgGraph::default();
    let mut seen_person_nodes: HashSet<String> = HashSet::new();
    let mut seen_title_nodes: HashSet<String> = HashSet::new();
    let mut seen_session_nodes: HashSet<String> = HashSet::new();
    let mut seen_persona_nodes: HashSet<String> = HashSet::new();

    let mut ensure_person_node = |graph: &mut KgGraph, discord_user_id: &str| {
        let id = person_node_id(discord_user_id);
        if seen_person_nodes.insert(id.clone()) {
            let label = friends
                .get(discord_user_id)
                .map(|f| f.display_name.clone())
                .unwrap_or_else(|| discord_user_id.to_string());
            graph.nodes.push(KgNode {
                id,
                kind: NodeKind::Person,
                label,
                attrs: Map::new(),
            });
        }
    };

    // --- watched: Person -> Title, opted-in watcher only -------------------
    for w in &data.watches {
        if !opted_in.contains(&w.discord_user_id) {
            continue;
        }
        ensure_person_node(&mut graph, &w.discord_user_id);

        let title_id = title_node_id(w.media_item_id);
        if seen_title_nodes.insert(title_id.clone()) {
            let mut attrs = Map::new();
            attrs.insert("title".to_string(), json!(w.title));
            graph.nodes.push(KgNode {
                id: title_id.clone(),
                kind: NodeKind::Title,
                label: w.title.clone(),
                attrs,
            });
        }

        let person_id = person_node_id(&w.discord_user_id);
        graph.edges.push(KgEdge {
            id: format!("watched:{person_id}->{title_id}"),
            kind: EdgeKind::Watched,
            source: person_id,
            target: title_id,
            weight: None,
            attrs: {
                let mut a = Map::new();
                a.insert("watched_at".to_string(), json!(w.watched_at.to_rfc3339()));
                a
            },
        });
    }

    // --- co-view: Person <-> Person, BOTH ends opted-in ---------------------
    for cv in &data.co_views {
        if !opted_in.contains(&cv.person_a) || !opted_in.contains(&cv.person_b) {
            continue;
        }
        ensure_person_node(&mut graph, &cv.person_a);
        ensure_person_node(&mut graph, &cv.person_b);

        let session_id = session_node_id(&cv.session_key);
        if seen_session_nodes.insert(session_id.clone()) {
            let mut attrs = Map::new();
            attrs.insert("title".to_string(), json!(cv.title));
            attrs.insert("media_item_id".to_string(), json!(cv.media_item_id));
            attrs.insert("watched_at".to_string(), json!(cv.watched_at.to_rfc3339()));
            graph.nodes.push(KgNode {
                id: session_id.clone(),
                kind: NodeKind::Session,
                label: format!("{} ({})", cv.title, cv.session_key),
                attrs,
            });
        }

        let a_id = person_node_id(&cv.person_a);
        let b_id = person_node_id(&cv.person_b);
        graph.edges.push(KgEdge {
            id: format!("coview:{}:{a_id}<->{b_id}", cv.session_key),
            kind: EdgeKind::CoView,
            source: a_id,
            target: b_id,
            weight: None,
            attrs: {
                let mut a = Map::new();
                a.insert("session_id".to_string(), json!(session_id));
                a.insert("title".to_string(), json!(cv.title));
                a
            },
        });
    }

    // --- personas: Person -> Persona (PersonaOf), opted-in owner only ------
    // Only opted-in personas participate in taste-edge computation below,
    // by construction: `included_personas` is built from the same
    // `opted_in` filter.
    let mut included_personas: Vec<&PersonaRecord> = Vec::new();
    for p in &data.personas {
        if !opted_in.contains(&p.discord_user_id) {
            continue;
        }
        ensure_person_node(&mut graph, &p.discord_user_id);

        let persona_id = persona_node_id(p.persona_id);
        if seen_persona_nodes.insert(persona_id.clone()) {
            graph.nodes.push(KgNode {
                id: persona_id.clone(),
                kind: NodeKind::Persona,
                label: p.persona_name.clone(),
                attrs: Map::new(),
            });
        }

        let person_id = person_node_id(&p.discord_user_id);
        graph.edges.push(KgEdge {
            id: format!("personaof:{person_id}->{persona_id}"),
            kind: EdgeKind::PersonaOf,
            source: person_id,
            target: persona_id,
            weight: None,
            attrs: Map::new(),
        });

        included_personas.push(p);
    }

    // --- taste-edges: Person <-> Person, cosine similarity over opted-in
    //     personas only (each already filtered into `included_personas`) --
    for i in 0..included_personas.len() {
        for j in (i + 1)..included_personas.len() {
            let a = included_personas[i];
            let b = included_personas[j];
            if a.discord_user_id == b.discord_user_id {
                continue;
            }
            let similarity = cosine_similarity(&a.centroid, &b.centroid);
            if similarity < taste_neighbor_threshold {
                continue;
            }
            let a_id = person_node_id(&a.discord_user_id);
            let b_id = person_node_id(&b.discord_user_id);
            graph.edges.push(KgEdge {
                id: format!("taste:{a_id}<->{b_id}"),
                kind: EdgeKind::TasteEdge,
                source: a_id,
                target: b_id,
                weight: Some(similarity as f64),
                attrs: Map::new(),
            });
        }
    }

    graph
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::identity::FriendIdentity;
    use crate::kg::model::{person_node_id, title_node_id};

    fn friends_alex_sam_opted_in_jamie_not() -> TrustedFriends {
        TrustedFriends::from_friends([
            FriendIdentity::new("discord-alex", "Alex").opt_in(1),
            FriendIdentity::new("discord-sam", "Sam").opt_in(2),
            // Allowlisted, but never opted in -- the negative-test subject.
            FriendIdentity::new("discord-jamie", "Jamie"),
        ])
    }

    fn full_vec(n: usize, value_at_0: f32, rest: f32) -> Vec<f32> {
        let mut v = vec![rest; n];
        v[0] = value_at_0;
        v
    }

    // ------------------------------------------------------------------
    // Node/edge assembly from seeded data
    // ------------------------------------------------------------------

    #[test]
    fn assembles_watched_edges_and_title_nodes_for_opted_in_watchers() {
        let friends = friends_alex_sam_opted_in_jamie_not();
        let data = GraphSourceData {
            watches: vec![
                WatchRecord {
                    discord_user_id: "discord-alex".to_string(),
                    media_item_id: 100,
                    title: "Severance".to_string(),
                    watched_at: Utc::now(),
                },
                WatchRecord {
                    discord_user_id: "discord-sam".to_string(),
                    media_item_id: 100,
                    title: "Severance".to_string(),
                    watched_at: Utc::now(),
                },
            ],
            co_views: vec![],
            personas: vec![],
        };

        let graph = assemble_shared_graph(&friends, &data, 0.5);

        assert!(graph.has_node(&person_node_id("discord-alex")));
        assert!(graph.has_node(&person_node_id("discord-sam")));
        assert!(graph.has_node(&title_node_id(100)));
        assert_eq!(graph.edges_of_kind(EdgeKind::Watched).count(), 2);
        let alex_watched = graph
            .edges_of_kind(EdgeKind::Watched)
            .find(|e| e.source == person_node_id("discord-alex"))
            .expect("alex has a watched edge");
        assert_eq!(alex_watched.target, title_node_id(100));
    }

    #[test]
    fn assembles_co_view_edge_and_session_node_between_two_opted_in_people() {
        let friends = friends_alex_sam_opted_in_jamie_not();
        let data = GraphSourceData {
            watches: vec![],
            co_views: vec![CoViewRecord {
                person_a: "discord-alex".to_string(),
                person_b: "discord-sam".to_string(),
                session_key: "sess-1".to_string(),
                media_item_id: 100,
                title: "Severance".to_string(),
                watched_at: Utc::now(),
            }],
            personas: vec![],
        };

        let graph = assemble_shared_graph(&friends, &data, 0.5);

        assert_eq!(graph.edges_of_kind(EdgeKind::CoView).count(), 1);
        let edge = &graph.edges_of_kind(EdgeKind::CoView).next().unwrap();
        assert_eq!(edge.source, person_node_id("discord-alex"));
        assert_eq!(edge.target, person_node_id("discord-sam"));
        assert_eq!(
            graph
                .nodes
                .iter()
                .filter(|n| n.kind == NodeKind::Session)
                .count(),
            1
        );
    }

    #[test]
    fn assembles_persona_of_edge_and_taste_edge_between_similar_opted_in_personas() {
        let friends = friends_alex_sam_opted_in_jamie_not();
        let data = GraphSourceData {
            watches: vec![],
            co_views: vec![],
            personas: vec![
                PersonaRecord {
                    discord_user_id: "discord-alex".to_string(),
                    persona_id: 1,
                    persona_name: "alex-primary".to_string(),
                    centroid: full_vec(8, 1.0, 0.0),
                },
                PersonaRecord {
                    discord_user_id: "discord-sam".to_string(),
                    persona_id: 2,
                    persona_name: "sam-primary".to_string(),
                    centroid: full_vec(8, 1.0, 0.0),
                },
            ],
        };

        let graph = assemble_shared_graph(&friends, &data, 0.5);

        assert_eq!(graph.edges_of_kind(EdgeKind::PersonaOf).count(), 2);
        assert_eq!(graph.edges_of_kind(EdgeKind::TasteEdge).count(), 1);
        let taste_edge = graph.edges_of_kind(EdgeKind::TasteEdge).next().unwrap();
        assert!(
            taste_edge.weight.unwrap() > 0.99,
            "identical centroids must be near-perfect cosine similarity, got {:?}",
            taste_edge.weight
        );
    }

    #[test]
    fn taste_neighbor_threshold_excludes_dissimilar_personas() {
        let friends = friends_alex_sam_opted_in_jamie_not();
        let data = GraphSourceData {
            watches: vec![],
            co_views: vec![],
            personas: vec![
                PersonaRecord {
                    discord_user_id: "discord-alex".to_string(),
                    persona_id: 1,
                    persona_name: "alex-primary".to_string(),
                    centroid: full_vec(8, 1.0, 0.0),
                },
                PersonaRecord {
                    discord_user_id: "discord-sam".to_string(),
                    persona_id: 2,
                    persona_name: "sam-primary".to_string(),
                    // Opposite direction -> cosine similarity ~ -1.0.
                    centroid: full_vec(8, -1.0, 0.0),
                },
            ],
        };

        let graph = assemble_shared_graph(&friends, &data, 0.5);

        assert_eq!(
            graph.edges_of_kind(EdgeKind::TasteEdge).count(),
            0,
            "opposed taste centroids must not produce a taste-neighbor edge"
        );
    }

    // ------------------------------------------------------------------
    // LOAD-BEARING PRIVACY NEGATIVE TEST
    // ------------------------------------------------------------------

    /// Jamie (allowlisted but NOT opted in) genuinely has co-views and a
    /// taste-shaped persona in the SOURCE data, co-viewing with an
    /// opted-in friend (Alex) -- so this proves the FILTER, not merely
    /// empty/absent source data. After assembly, NONE of Jamie's relations
    /// (person node, watched edges, co-view edges, persona node/edges,
    /// taste edges) may appear in the shared graph, while Alex's/Sam's DO.
    #[test]
    fn opted_out_users_relations_never_enter_the_shared_graph() {
        let friends = friends_alex_sam_opted_in_jamie_not();

        let data = GraphSourceData {
            watches: vec![
                WatchRecord {
                    discord_user_id: "discord-alex".to_string(),
                    media_item_id: 100,
                    title: "Severance".to_string(),
                    watched_at: Utc::now(),
                },
                WatchRecord {
                    discord_user_id: "discord-jamie".to_string(),
                    media_item_id: 200,
                    title: "Jamie's Secret Show".to_string(),
                    watched_at: Utc::now(),
                },
            ],
            co_views: vec![
                CoViewRecord {
                    person_a: "discord-alex".to_string(),
                    person_b: "discord-sam".to_string(),
                    session_key: "sess-alex-sam".to_string(),
                    media_item_id: 100,
                    title: "Severance".to_string(),
                    watched_at: Utc::now(),
                },
                // Jamie co-viewed WITH an opted-in friend -- still must be
                // excluded because Jamie's own end isn't opted in.
                CoViewRecord {
                    person_a: "discord-alex".to_string(),
                    person_b: "discord-jamie".to_string(),
                    session_key: "sess-alex-jamie".to_string(),
                    media_item_id: 100,
                    title: "Severance".to_string(),
                    watched_at: Utc::now(),
                },
            ],
            personas: vec![
                PersonaRecord {
                    discord_user_id: "discord-alex".to_string(),
                    persona_id: 1,
                    persona_name: "alex-primary".to_string(),
                    centroid: full_vec(8, 1.0, 0.0),
                },
                PersonaRecord {
                    discord_user_id: "discord-jamie".to_string(),
                    persona_id: 3,
                    persona_name: "jamie-primary".to_string(),
                    // Deliberately near-identical to Alex's -- if the filter
                    // were broken this WOULD produce a taste edge, so this
                    // is a genuine test of the filter, not of the math.
                    centroid: full_vec(8, 1.0, 0.0),
                },
            ],
        };

        // Sanity: the source data genuinely contains Jamie's relations
        // before assembly -- proving the upcoming assertions test the
        // FILTER, not vacuously-empty input.
        assert!(data
            .watches
            .iter()
            .any(|w| w.discord_user_id == "discord-jamie"));
        assert!(data
            .co_views
            .iter()
            .any(|c| c.person_a == "discord-jamie" || c.person_b == "discord-jamie"));
        assert!(data
            .personas
            .iter()
            .any(|p| p.discord_user_id == "discord-jamie"));

        let graph = assemble_shared_graph(&friends, &data, 0.5);

        let jamie_person_id = person_node_id("discord-jamie");
        let jamie_persona_id = persona_node_id(3);
        let jamie_title_id = title_node_id(200);

        // No Jamie person/persona/title node.
        assert!(
            !graph.has_node(&jamie_person_id),
            "a non-opted-in friend must never get a person node in the shared graph"
        );
        assert!(!graph.has_node(&jamie_persona_id));
        assert!(
            !graph.has_node(&jamie_title_id),
            "a title only Jamie watched must not appear in the shared graph"
        );

        // No edge anywhere references Jamie's node id, as source OR target.
        for edge in &graph.edges {
            assert_ne!(
                edge.source, jamie_person_id,
                "edge {:?} sources from Jamie",
                edge
            );
            assert_ne!(
                edge.target, jamie_person_id,
                "edge {:?} targets Jamie",
                edge
            );
            assert_ne!(edge.source, jamie_persona_id);
            assert_ne!(edge.target, jamie_persona_id);
        }

        // The co-view between Alex and Jamie must be entirely absent (not
        // just relabeled) -- there must be exactly ONE co-view edge (the
        // Alex<->Sam one), not two.
        assert_eq!(
            graph.edges_of_kind(EdgeKind::CoView).count(),
            1,
            "the alex<->jamie co-view must be dropped, leaving only alex<->sam"
        );

        // No taste edge for Jamie, even though her centroid is near-
        // identical to Alex's (proves the filter runs BEFORE the
        // similarity computation, not as a post-hoc drop).
        assert_eq!(
            graph.edges_of_kind(EdgeKind::TasteEdge).count(),
            0,
            "jamie must be excluded from taste-edge computation entirely"
        );

        // Meanwhile Alex's (opted-in) relations DO appear -- proves this
        // is a real filter, not a bug that drops everyone.
        assert!(graph.has_node(&person_node_id("discord-alex")));
        assert!(graph.has_node(&person_node_id("discord-sam")));
        assert!(graph.has_node(&title_node_id(100)));
        assert_eq!(graph.edges_of_kind(EdgeKind::Watched).count(), 1);
        assert!(graph.has_node(&persona_node_id(1)));
    }
}
