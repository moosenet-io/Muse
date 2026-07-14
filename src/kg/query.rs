//! Group-dynamics + taste-neighbor query surfaces over an already-assembled
//! [`KgGraph`] — see `crate::kg`'s module doc. These are pure, in-process
//! graph algorithms (BFS/connected-components) over
//! [`crate::kg::assemble::assemble_shared_graph`]'s output; there is no live
//! `kg_query`-style server involved (see the module doc's "external Atlas
//! KG" note) — this is what Muse itself would answer these questions with
//! today.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::kg::model::{EdgeKind, KgGraph};

/// "Who watches with whom": every co-view neighbor of every person node,
/// derived from [`EdgeKind::CoView`] edges treated as UNDIRECTED (a co-view
/// record's `person_a`/`person_b` ordering carries no meaning — see
/// `crate::kg::assemble::CoViewRecord`'s doc). Both directions of every
/// co-view edge are present in the returned map, so `adjacency[&a]`
/// contains `b` and `adjacency[&b]` contains `a`.
pub fn co_view_adjacency(graph: &KgGraph) -> HashMap<String, HashSet<String>> {
    let mut adjacency: HashMap<String, HashSet<String>> = HashMap::new();
    for edge in graph.edges_of_kind(EdgeKind::CoView) {
        adjacency
            .entry(edge.source.clone())
            .or_default()
            .insert(edge.target.clone());
        adjacency
            .entry(edge.target.clone())
            .or_default()
            .insert(edge.source.clone());
    }
    adjacency
}

/// The shortest connector path between two person node ids over
/// [`EdgeKind::CoView`] edges only (breadth-first search, undirected) —
/// "what bridges these two people." Returns `None` when `from`/`to` are the
/// same node, either is absent from the graph, or no co-view path connects
/// them. When a path exists, the returned `Vec` includes both endpoints
/// (`[from, ..connectors.., to]`), so a direct co-view edge yields a
/// 2-element path with no connector in between.
pub fn bridge_between(graph: &KgGraph, from: &str, to: &str) -> Option<Vec<String>> {
    if from == to || !graph.has_node(from) || !graph.has_node(to) {
        return None;
    }
    let adjacency = co_view_adjacency(graph);

    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut parent: HashMap<String, String> = HashMap::new();

    visited.insert(from.to_string());
    queue.push_back(from.to_string());

    while let Some(current) = queue.pop_front() {
        if current == to {
            // Reconstruct the path by walking parents back to `from`.
            let mut path = vec![current.clone()];
            let mut node = current;
            while let Some(p) = parent.get(&node) {
                path.push(p.clone());
                node = p.clone();
            }
            path.reverse();
            return Some(path);
        }
        let Some(neighbors) = adjacency.get(&current) else {
            continue;
        };
        // Sorted for deterministic traversal order (HashSet iteration order
        // is unspecified) -- doesn't change whether a path is found, only
        // makes ties between equally-short paths reproducible.
        let mut sorted_neighbors: Vec<&String> = neighbors.iter().collect();
        sorted_neighbors.sort();
        for neighbor in sorted_neighbors {
            if visited.insert(neighbor.clone()) {
                parent.insert(neighbor.clone(), current.clone());
                queue.push_back(neighbor.clone());
            }
        }
    }
    None
}

/// Taste-neighbor clusters: connected components of person nodes joined by
/// [`EdgeKind::TasteEdge`] edges (every edge that survived
/// [`crate::kg::assemble::assemble_shared_graph`]'s threshold filter, so no
/// additional threshold is applied here — see that function's
/// `taste_neighbor_threshold` parameter). A person with no taste edges at
/// all forms their own singleton cluster. Cluster order and within-cluster
/// order are both sorted for deterministic output.
pub fn taste_neighbor_clusters(graph: &KgGraph) -> Vec<Vec<String>> {
    // Union-find over every person node that participates in at least one
    // taste edge, plus (for completeness) every person node in the graph so
    // an isolated person still gets a singleton cluster.
    let mut parent: HashMap<String, String> = HashMap::new();
    for node in &graph.nodes {
        if node.kind == crate::kg::model::NodeKind::Person {
            parent.insert(node.id.clone(), node.id.clone());
        }
    }

    fn find(parent: &mut HashMap<String, String>, id: &str) -> String {
        let mut root = id.to_string();
        while parent.get(&root).map(|p| p != &root).unwrap_or(false) {
            root = parent[&root].clone();
        }
        // Path compression.
        let mut node = id.to_string();
        while node != root {
            let next = parent[&node].clone();
            parent.insert(node, root.clone());
            node = next;
        }
        root
    }

    fn union(parent: &mut HashMap<String, String>, a: &str, b: &str) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent.insert(ra, rb);
        }
    }

    for edge in graph.edges_of_kind(EdgeKind::TasteEdge) {
        if parent.contains_key(&edge.source) && parent.contains_key(&edge.target) {
            union(&mut parent, &edge.source, &edge.target);
        }
    }

    let mut clusters: HashMap<String, Vec<String>> = HashMap::new();
    let ids: Vec<String> = parent.keys().cloned().collect();
    for id in ids {
        let root = find(&mut parent, &id);
        clusters.entry(root).or_default().push(id);
    }

    let mut result: Vec<Vec<String>> = clusters
        .into_values()
        .map(|mut members| {
            members.sort();
            members
        })
        .collect();
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::identity::{FriendIdentity, TrustedFriends};
    use crate::kg::assemble::{
        assemble_shared_graph, CoViewRecord, GraphSourceData, PersonaRecord,
    };
    use crate::kg::model::person_node_id;
    use chrono::Utc;

    fn friends() -> TrustedFriends {
        TrustedFriends::from_friends([
            FriendIdentity::new("discord-alex", "Alex").opt_in(1),
            FriendIdentity::new("discord-sam", "Sam").opt_in(2),
            FriendIdentity::new("discord-taylor", "Taylor").opt_in(3),
            FriendIdentity::new("discord-morgan", "Morgan").opt_in(4),
        ])
    }

    fn full_vec(n: usize, value_at_0: f32, rest: f32) -> Vec<f32> {
        let mut v = vec![rest; n];
        v[0] = value_at_0;
        v
    }

    fn coview(a: &str, b: &str, session: &str) -> CoViewRecord {
        CoViewRecord {
            person_a: a.to_string(),
            person_b: b.to_string(),
            session_key: session.to_string(),
            media_item_id: 1,
            title: "Some Show".to_string(),
            watched_at: Utc::now(),
        }
    }

    // ------------------------------------------------------------------
    // Group-dynamics: who watches with whom + bridge-between
    // ------------------------------------------------------------------

    #[test]
    fn co_view_adjacency_is_symmetric() {
        let friends = friends();
        let data = GraphSourceData {
            watches: vec![],
            co_views: vec![coview("discord-alex", "discord-sam", "s1")],
            personas: vec![],
        };
        let graph = assemble_shared_graph(&friends, &data, 0.5);
        let adjacency = co_view_adjacency(&graph);

        let alex = person_node_id("discord-alex");
        let sam = person_node_id("discord-sam");
        assert!(adjacency.get(&alex).unwrap().contains(&sam));
        assert!(adjacency.get(&sam).unwrap().contains(&alex));
    }

    #[test]
    fn bridge_between_finds_the_connector_in_a_three_person_chain() {
        let friends = friends();
        // alex -- sam -- taylor, but NO direct alex<->taylor co-view.
        let data = GraphSourceData {
            watches: vec![],
            co_views: vec![
                coview("discord-alex", "discord-sam", "s1"),
                coview("discord-sam", "discord-taylor", "s2"),
            ],
            personas: vec![],
        };
        let graph = assemble_shared_graph(&friends, &data, 0.5);

        let alex = person_node_id("discord-alex");
        let sam = person_node_id("discord-sam");
        let taylor = person_node_id("discord-taylor");

        let path =
            bridge_between(&graph, &alex, &taylor).expect("alex and taylor are connected via sam");
        assert_eq!(path, vec![alex.clone(), sam.clone(), taylor.clone()]);

        // Direct co-view collapses to a 2-element path.
        let direct = bridge_between(&graph, &alex, &sam).expect("direct co-view");
        assert_eq!(direct, vec![alex, sam]);
    }

    #[test]
    fn bridge_between_returns_none_for_disconnected_people() {
        let friends = friends();
        let data = GraphSourceData {
            watches: vec![],
            co_views: vec![coview("discord-alex", "discord-sam", "s1")],
            personas: vec![],
        };
        let graph = assemble_shared_graph(&friends, &data, 0.5);
        let alex = person_node_id("discord-alex");
        let morgan = person_node_id("discord-morgan");
        // Morgan has no co-view edges at all -- not even a node.
        assert!(bridge_between(&graph, &alex, &morgan).is_none());
        assert!(bridge_between(&graph, &alex, &alex).is_none());
    }

    // ------------------------------------------------------------------
    // Taste-neighbor clustering
    // ------------------------------------------------------------------

    #[test]
    fn taste_neighbor_clusters_group_similar_and_separate_divergent_taste() {
        let friends = friends();
        let data = GraphSourceData {
            watches: vec![],
            co_views: vec![],
            personas: vec![
                PersonaRecord {
                    discord_user_id: "discord-alex".to_string(),
                    persona_id: 1,
                    persona_name: "alex".to_string(),
                    centroid: full_vec(8, 1.0, 0.0),
                },
                PersonaRecord {
                    discord_user_id: "discord-sam".to_string(),
                    persona_id: 2,
                    persona_name: "sam".to_string(),
                    // Near-identical to alex.
                    centroid: full_vec(8, 0.98, 0.02),
                },
                PersonaRecord {
                    discord_user_id: "discord-taylor".to_string(),
                    persona_id: 3,
                    persona_name: "taylor".to_string(),
                    // Opposite direction -- genuinely divergent taste.
                    centroid: full_vec(8, -1.0, 0.0),
                },
            ],
        };

        // Sanity: the seeded taste actually differs before we assert on
        // clustering (non-vacuous fixture).
        let alex_vec = full_vec(8, 1.0, 0.0);
        let taylor_vec = full_vec(8, -1.0, 0.0);
        assert!(
            crate::persona::blend::cosine_similarity(&alex_vec, &taylor_vec) < 0.0,
            "fixture must seed genuinely divergent taste for alex vs taylor"
        );

        let graph = assemble_shared_graph(&friends, &data, 0.5);
        let clusters = taste_neighbor_clusters(&graph);

        let alex = person_node_id("discord-alex");
        let sam = person_node_id("discord-sam");
        let taylor = person_node_id("discord-taylor");

        let alex_cluster = clusters
            .iter()
            .find(|c| c.contains(&alex))
            .expect("alex is in some cluster");
        assert!(
            alex_cluster.contains(&sam),
            "alex and sam have near-identical taste and must cluster together: {clusters:?}"
        );
        assert!(
            !alex_cluster.contains(&taylor),
            "taylor's divergent taste must NOT be in alex's cluster: {clusters:?}"
        );

        let taylor_cluster = clusters
            .iter()
            .find(|c| c.contains(&taylor))
            .expect("taylor is in some cluster (own singleton, since no persona in data is present for morgan)");
        assert_ne!(alex_cluster, taylor_cluster);
    }
}
