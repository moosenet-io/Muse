//! MUSEX-17 (Plane TERM #393): visualization-ready DATA structures built
//! from an already-assembled, privacy-scoped [`KgGraph`] (`crate::kg`) —
//! see that module's doc for the "by construction" privacy argument this
//! module INHERITS rather than re-implements. A [`KgGraph`] reaching any
//! function here has already been through
//! [`crate::kg::assemble::assemble_shared_graph`]'s opt-in filter, so a
//! non-opted-in person's id never appears in `graph` at all, which means it
//! can never appear in any of these builders' output either — the same
//! "never enters the loop" posture `crate::kg`'s module doc documents for
//! itself. See this module's `tests::privacy` submodule for the load-
//! bearing negative test that exercises this end-to-end (source data with a
//! genuinely non-opted-in person's relations -> assembled graph -> every
//! viz builder -> none of that person's ids anywhere in any output).
//!
//! Every builder here is pure (no DB, no I/O, no panics/unwraps) and
//! produces the DATA a Constellation GUI would render (nodes+positions/
//! edges/labels, or a series) — never pixels, layout coordinates, or
//! colors; that's the GUI's job. Each degrades gracefully on a sparse or
//! empty [`KgGraph`] (see each `*_on_empty_graph` test): an absent person id
//! or a graph with no relevant edges yields an empty-but-valid structure,
//! never a panic.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::kg::model::{EdgeKind, KgGraph, NodeKind};
use crate::kg::query::{co_view_adjacency, taste_neighbor_clusters};

// ===========================================================================
// 1. Personal taste-map: one opted-in person's persona constellation
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TasteMapPersona {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TasteMapNeighbor {
    pub person_id: String,
    pub label: String,
    /// The taste-edge's cosine similarity. `None` only if the underlying
    /// [`crate::kg::model::KgEdge::weight`] was itself `None` — mirrored
    /// rather than unwrapped, since a `TasteEdge` is documented to always
    /// carry a weight but this module never assumes an invariant it doesn't
    /// enforce itself.
    pub similarity: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TasteMapViz {
    pub person_id: String,
    /// `None` when `person_id` has no node in `graph` at all — not opted
    /// in (so [`crate::kg::assemble::assemble_shared_graph`] never gave
    /// them a node), or simply has no data yet. The caller renders "no
    /// data for this person" rather than the endpoint erroring.
    pub label: Option<String>,
    pub personas: Vec<TasteMapPersona>,
    pub neighbors: Vec<TasteMapNeighbor>,
}

/// Build a persona-constellation for `person_id` (a `person:`-namespaced
/// node id — see [`crate::kg::model::person_node_id`]): every persona they
/// own (via [`EdgeKind::PersonaOf`]) and every taste-neighbor edge touching
/// them (via [`EdgeKind::TasteEdge`]), each resolved to the neighbor's
/// display label. `person_id` absent from `graph` degrades to an
/// empty-but-valid [`TasteMapViz`] (`label: None`, both lists empty) —
/// never a panic.
pub fn build_taste_map(graph: &KgGraph, person_id: &str) -> TasteMapViz {
    let label = graph.node(person_id).map(|n| n.label.clone());

    let personas: Vec<TasteMapPersona> = graph
        .edges_of_kind(EdgeKind::PersonaOf)
        .filter(|e| e.source == person_id)
        .filter_map(|e| graph.node(&e.target))
        .map(|n| TasteMapPersona {
            id: n.id.clone(),
            label: n.label.clone(),
        })
        .collect();

    let mut neighbors: Vec<TasteMapNeighbor> = graph
        .edges_of_kind(EdgeKind::TasteEdge)
        .filter_map(|e| {
            let neighbor_id = if e.source == person_id {
                &e.target
            } else if e.target == person_id {
                &e.source
            } else {
                return None;
            };
            let node = graph.node(neighbor_id)?;
            Some(TasteMapNeighbor {
                person_id: node.id.clone(),
                label: node.label.clone(),
                similarity: e.weight,
            })
        })
        .collect();
    neighbors.sort_by(|a, b| a.person_id.cmp(&b.person_id));

    TasteMapViz {
        person_id: person_id.to_string(),
        label,
        personas,
        neighbors,
    }
}

// ===========================================================================
// 2. Household group-dynamics: who-bridges-whom, bridge/centrality annotated
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupDynamicsNode {
    pub id: String,
    pub label: String,
    /// Degree centrality over [`EdgeKind::CoView`] edges only: how many
    /// distinct co-view partners this person has.
    pub co_view_degree: usize,
    /// True when this person is an articulation point (cut vertex) of the
    /// co-view adjacency graph — removing them would disconnect at least
    /// two of their remaining co-view neighbors from each other. This is
    /// the graph-theoretic "bridges the group" signal: the person whose
    /// presence is what connects otherwise-separate viewing sub-groups.
    /// See [`articulation_points`].
    pub is_bridge: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupDynamicsEdge {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GroupDynamicsViz {
    pub nodes: Vec<GroupDynamicsNode>,
    pub edges: Vec<GroupDynamicsEdge>,
}

/// Build the who-bridges-whom household group-dynamics graph, reusing
/// [`co_view_adjacency`] for the underlying relation. Every node gets a
/// centrality annotation (co-view degree) and a bridge annotation
/// (articulation-point status); edges are deduplicated (one entry per
/// undirected co-view pair, `{a,b} == {b,a}`) and both nodes/edges are
/// sorted for deterministic output. A graph with no co-view edges (or no
/// nodes at all) degrades to empty `nodes`/`edges` — never a panic.
pub fn build_group_dynamics(graph: &KgGraph) -> GroupDynamicsViz {
    let adjacency = co_view_adjacency(graph);
    let bridges = articulation_points(&adjacency);

    let mut nodes: Vec<GroupDynamicsNode> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Person && adjacency.contains_key(&n.id))
        .map(|n| GroupDynamicsNode {
            id: n.id.clone(),
            label: n.label.clone(),
            co_view_degree: adjacency.get(&n.id).map(HashSet::len).unwrap_or(0),
            is_bridge: bridges.contains(&n.id),
        })
        .collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));

    let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
    let mut edges: Vec<GroupDynamicsEdge> = Vec::new();
    for (a, neighbors) in &adjacency {
        for b in neighbors {
            let pair = if a < b {
                (a.clone(), b.clone())
            } else {
                (b.clone(), a.clone())
            };
            if seen_pairs.insert(pair.clone()) {
                edges.push(GroupDynamicsEdge {
                    source: pair.0,
                    target: pair.1,
                });
            }
        }
    }
    edges.sort_by(|a, b| (&a.source, &a.target).cmp(&(&b.source, &b.target)));

    GroupDynamicsViz { nodes, edges }
}

/// Classic Tarjan articulation-point (cut-vertex) detection over an
/// undirected adjacency map, recursive DFS (household/friend co-view graphs
/// are small — dozens of nodes at most — so recursion depth is not a
/// concern in practice, and the recursive form is far less error-prone to
/// get right than an iterative rewrite). Deterministic: neighbor visitation
/// order and root selection are both sorted first.
fn articulation_points(adjacency: &HashMap<String, HashSet<String>>) -> HashSet<String> {
    struct Dfs<'a> {
        adjacency: &'a HashMap<String, HashSet<String>>,
        disc: HashMap<String, usize>,
        low: HashMap<String, usize>,
        timer: usize,
        result: HashSet<String>,
    }

    fn visit(
        dfs: &mut Dfs<'_>,
        node: &str,
        parent: Option<&str>,
        is_root: bool,
        root_children: &mut usize,
    ) {
        dfs.disc.insert(node.to_string(), dfs.timer);
        dfs.low.insert(node.to_string(), dfs.timer);
        dfs.timer += 1;

        let mut neighbors: Vec<String> = dfs
            .adjacency
            .get(node)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        neighbors.sort();

        for neighbor in neighbors {
            if Some(neighbor.as_str()) == parent {
                // Skip the edge straight back to the parent we arrived
                // from -- co-view adjacency is a simple graph (no parallel
                // edges), so this can only ever be the true parent edge.
                continue;
            }
            if let Some(&neighbor_disc) = dfs.disc.get(&neighbor) {
                // Back edge to an already-visited node.
                let node_low = dfs.low[node];
                dfs.low
                    .insert(node.to_string(), node_low.min(neighbor_disc));
            } else {
                if is_root {
                    *root_children += 1;
                }
                visit(dfs, &neighbor, Some(node), false, root_children);
                let neighbor_low = dfs.low[&neighbor];
                let node_low = dfs.low[node];
                dfs.low.insert(node.to_string(), node_low.min(neighbor_low));

                let node_disc = dfs.disc[node];
                if !is_root && neighbor_low >= node_disc {
                    dfs.result.insert(node.to_string());
                }
            }
        }
    }

    let mut dfs = Dfs {
        adjacency,
        disc: HashMap::new(),
        low: HashMap::new(),
        timer: 0,
        result: HashSet::new(),
    };

    let mut node_ids: Vec<String> = adjacency.keys().cloned().collect();
    node_ids.sort();

    for start in node_ids {
        if dfs.disc.contains_key(&start) {
            continue;
        }
        let mut root_children = 0usize;
        visit(&mut dfs, &start, None, true, &mut root_children);
        if root_children > 1 {
            dfs.result.insert(start.clone());
        }
    }

    dfs.result
}

// ===========================================================================
// 3. Watch-history over time: a temporal series
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchHistoryEntry {
    pub person_id: String,
    pub title_id: String,
    pub title: String,
    pub watched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WatchHistoryViz {
    /// Ordered oldest -> newest — a real temporal series, not insertion
    /// order — so the GUI can render a left-to-right timeline without
    /// re-sorting.
    pub entries: Vec<WatchHistoryEntry>,
}

/// Build a temporal watch-history series from every [`EdgeKind::Watched`]
/// edge in `graph`, optionally scoped to one `person_id` (`None` = every
/// opted-in person's watch history — already privacy-scoped by
/// construction, see this module's doc). An edge with a missing or
/// unparseable `watched_at` attr, or whose target title node is absent, is
/// skipped rather than panicking — a malformed record degrades to "this
/// entry doesn't render," never a failed series. `limit` caps the series to
/// the `limit` MOST RECENT entries (oldest are dropped first when over
/// limit), still returned oldest-first; callers should pass
/// `crate::config::Config::kg_viz_watch_history_limit`, never a bare
/// literal (this module stays config-value-agnostic, same posture as
/// `crate::kg::assemble::assemble_shared_graph`'s `taste_neighbor_threshold`
/// parameter).
pub fn build_watch_history(
    graph: &KgGraph,
    person_id: Option<&str>,
    limit: usize,
) -> WatchHistoryViz {
    let mut entries: Vec<WatchHistoryEntry> = graph
        .edges_of_kind(EdgeKind::Watched)
        .filter(|e| person_id.map(|p| e.source == p).unwrap_or(true))
        .filter_map(|e| {
            let title_node = graph.node(&e.target)?;
            let watched_at = e
                .attrs
                .get("watched_at")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))?;
            Some(WatchHistoryEntry {
                person_id: e.source.clone(),
                title_id: title_node.id.clone(),
                title: title_node.label.clone(),
                watched_at,
            })
        })
        .collect();

    entries.sort_by_key(|e| e.watched_at);
    if entries.len() > limit {
        let drop_count = entries.len() - limit;
        entries.drain(0..drop_count);
    }

    WatchHistoryViz { entries }
}

// ===========================================================================
// 4. Taste-neighbor clusters: labeled cluster groupings
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TasteClusterMember {
    pub person_id: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TasteClusterViz {
    pub clusters: Vec<Vec<TasteClusterMember>>,
}

/// Wrap [`taste_neighbor_clusters`]'s connected-component output with
/// display labels, ready for the GUI to render as grouped clusters. Cluster
/// order and within-cluster order are inherited from
/// [`taste_neighbor_clusters`] (both already sorted, deterministic). A
/// graph with no person nodes degrades to an empty `clusters` list.
pub fn build_taste_clusters(graph: &KgGraph) -> TasteClusterViz {
    let clusters = taste_neighbor_clusters(graph)
        .into_iter()
        .map(|members| {
            members
                .into_iter()
                .map(|id| {
                    let label = graph
                        .node(&id)
                        .map(|n| n.label.clone())
                        .unwrap_or_else(|| id.clone());
                    TasteClusterMember {
                        person_id: id,
                        label,
                    }
                })
                .collect()
        })
        .collect();

    TasteClusterViz { clusters }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::identity::{FriendIdentity, TrustedFriends};
    use crate::kg::assemble::{
        assemble_shared_graph, CoViewRecord, GraphSourceData, PersonaRecord, WatchRecord,
    };
    use crate::kg::model::{person_node_id, KgEdge, KgNode};
    use chrono::{Duration as ChronoDuration, Utc};
    use serde_json::{json, Map};

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

    // -----------------------------------------------------------------
    // 1. Personal taste-map
    // -----------------------------------------------------------------

    mod taste_map {
        use super::*;

        fn friends() -> TrustedFriends {
            TrustedFriends::from_friends([
                FriendIdentity::new("discord-alex", "Alex").opt_in(1),
                FriendIdentity::new("discord-sam", "Sam").opt_in(2),
                FriendIdentity::new("discord-taylor", "Taylor").opt_in(3),
            ])
        }

        #[test]
        fn includes_own_personas_and_similar_neighbors_excludes_divergent() {
            let friends = friends();
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
                        centroid: full_vec(8, 0.98, 0.02),
                    },
                    PersonaRecord {
                        discord_user_id: "discord-taylor".to_string(),
                        persona_id: 3,
                        persona_name: "taylor-primary".to_string(),
                        centroid: full_vec(8, -1.0, 0.0),
                    },
                ],
            };
            let graph = assemble_shared_graph(&friends, &data, 0.5);

            let alex_id = person_node_id("discord-alex");
            let viz = build_taste_map(&graph, &alex_id);

            assert_eq!(viz.label.as_deref(), Some("Alex"));
            assert_eq!(viz.personas.len(), 1);
            assert_eq!(viz.personas[0].label, "alex-primary");

            let taylor_id = person_node_id("discord-taylor");
            assert!(
                viz.neighbors
                    .iter()
                    .any(|n| n.person_id == person_node_id("discord-sam")),
                "sam's near-identical taste must appear as a neighbor: {:?}",
                viz.neighbors
            );
            assert!(
                !viz.neighbors.iter().any(|n| n.person_id == taylor_id),
                "taylor's divergent taste must NOT appear as a neighbor: {:?}",
                viz.neighbors
            );
            let sam_neighbor = viz
                .neighbors
                .iter()
                .find(|n| n.person_id == person_node_id("discord-sam"))
                .expect("sam neighbor present");
            assert!(sam_neighbor.similarity.unwrap_or(0.0) > 0.9);
        }

        #[test]
        fn degrades_to_empty_for_a_person_absent_from_the_graph() {
            let graph = KgGraph::default();
            let viz = build_taste_map(&graph, "person:nobody");
            assert!(viz.label.is_none());
            assert!(viz.personas.is_empty());
            assert!(viz.neighbors.is_empty());
        }
    }

    // -----------------------------------------------------------------
    // 2. Group-dynamics
    // -----------------------------------------------------------------

    mod group_dynamics {
        use super::*;

        fn friends() -> TrustedFriends {
            TrustedFriends::from_friends([
                FriendIdentity::new("discord-alex", "Alex").opt_in(1),
                FriendIdentity::new("discord-sam", "Sam").opt_in(2),
                FriendIdentity::new("discord-taylor", "Taylor").opt_in(3),
                FriendIdentity::new("discord-morgan", "Morgan").opt_in(4),
            ])
        }

        #[test]
        fn marks_the_correct_bridge_in_a_four_person_chain() {
            // alex -- sam -- taylor -- morgan (a straight chain). sam and
            // taylor are cut vertices (bridges); alex and morgan are leaves.
            let friends = friends();
            let data = GraphSourceData {
                watches: vec![],
                co_views: vec![
                    coview("discord-alex", "discord-sam", "s1"),
                    coview("discord-sam", "discord-taylor", "s2"),
                    coview("discord-taylor", "discord-morgan", "s3"),
                ],
                personas: vec![],
            };
            let graph = assemble_shared_graph(&friends, &data, 0.5);

            let viz = build_group_dynamics(&graph);

            let alex = person_node_id("discord-alex");
            let sam = person_node_id("discord-sam");
            let taylor = person_node_id("discord-taylor");
            let morgan = person_node_id("discord-morgan");

            let node = |id: &str| viz.nodes.iter().find(|n| n.id == id).expect("node present");

            assert!(!node(&alex).is_bridge, "leaf must not be a bridge");
            assert!(node(&sam).is_bridge, "sam connects alex to taylor+morgan");
            assert!(
                node(&taylor).is_bridge,
                "taylor connects morgan to alex+sam"
            );
            assert!(!node(&morgan).is_bridge, "leaf must not be a bridge");

            assert_eq!(node(&alex).co_view_degree, 1);
            assert_eq!(node(&sam).co_view_degree, 2);
            assert_eq!(node(&taylor).co_view_degree, 2);
            assert_eq!(node(&morgan).co_view_degree, 1);

            // 3 co-view pairs, deduplicated to 3 undirected edges (not 6).
            assert_eq!(viz.edges.len(), 3);
        }

        #[test]
        fn degrades_to_empty_on_a_graph_with_no_co_view_edges() {
            let graph = KgGraph::default();
            let viz = build_group_dynamics(&graph);
            assert!(viz.nodes.is_empty());
            assert!(viz.edges.is_empty());
        }
    }

    // -----------------------------------------------------------------
    // 3. Watch-history over time
    // -----------------------------------------------------------------

    mod watch_history {
        use super::*;

        fn friends() -> TrustedFriends {
            TrustedFriends::from_friends([
                FriendIdentity::new("discord-alex", "Alex").opt_in(1),
                FriendIdentity::new("discord-sam", "Sam").opt_in(2),
            ])
        }

        #[test]
        fn is_ordered_ascending_and_scoped_to_one_person() {
            let friends = friends();
            let base = Utc::now();
            let data = GraphSourceData {
                watches: vec![
                    WatchRecord {
                        discord_user_id: "discord-alex".to_string(),
                        media_item_id: 100,
                        title: "Third".to_string(),
                        watched_at: base + ChronoDuration::hours(3),
                    },
                    WatchRecord {
                        discord_user_id: "discord-alex".to_string(),
                        media_item_id: 101,
                        title: "First".to_string(),
                        watched_at: base + ChronoDuration::hours(1),
                    },
                    WatchRecord {
                        discord_user_id: "discord-alex".to_string(),
                        media_item_id: 102,
                        title: "Second".to_string(),
                        watched_at: base + ChronoDuration::hours(2),
                    },
                    // Sam's watch must be excluded when scoped to Alex.
                    WatchRecord {
                        discord_user_id: "discord-sam".to_string(),
                        media_item_id: 200,
                        title: "Sam's Show".to_string(),
                        watched_at: base + ChronoDuration::hours(2),
                    },
                ],
                co_views: vec![],
                personas: vec![],
            };
            let graph = assemble_shared_graph(&friends, &data, 0.5);

            let alex_id = person_node_id("discord-alex");
            let viz = build_watch_history(&graph, Some(&alex_id), 100);

            assert_eq!(viz.entries.len(), 3);
            let titles: Vec<&str> = viz.entries.iter().map(|e| e.title.as_str()).collect();
            assert_eq!(
                titles,
                vec!["First", "Second", "Third"],
                "must be ascending by watched_at"
            );
            assert!(viz.entries.iter().all(|e| e.person_id == alex_id));
        }

        #[test]
        fn limit_keeps_the_most_recent_entries_still_ascending() {
            let friends = friends();
            let base = Utc::now();
            let data = GraphSourceData {
                watches: (0..5)
                    .map(|i| WatchRecord {
                        discord_user_id: "discord-alex".to_string(),
                        media_item_id: 100 + i,
                        title: format!("Title {i}"),
                        watched_at: base + ChronoDuration::hours(i),
                    })
                    .collect(),
                co_views: vec![],
                personas: vec![],
            };
            let graph = assemble_shared_graph(&friends, &data, 0.5);
            let alex_id = person_node_id("discord-alex");

            let viz = build_watch_history(&graph, Some(&alex_id), 2);

            assert_eq!(viz.entries.len(), 2);
            let titles: Vec<&str> = viz.entries.iter().map(|e| e.title.as_str()).collect();
            // Titles 3 and 4 are the most recent two, still ascending.
            assert_eq!(titles, vec!["Title 3", "Title 4"]);
        }

        #[test]
        fn skips_entries_with_a_missing_or_unparseable_timestamp_without_panicking() {
            // Hand-built graph (not via assemble_shared_graph) so a
            // malformed watched_at attr can be constructed directly.
            let mut graph = KgGraph::default();
            graph.nodes.push(KgNode {
                id: "person:x".to_string(),
                kind: NodeKind::Person,
                label: "X".to_string(),
                attrs: Map::new(),
            });
            graph.nodes.push(KgNode {
                id: "title:1".to_string(),
                kind: NodeKind::Title,
                label: "Broken".to_string(),
                attrs: Map::new(),
            });
            let mut bad_attrs = Map::new();
            bad_attrs.insert("watched_at".to_string(), json!("not-a-timestamp"));
            graph.edges.push(KgEdge {
                id: "watched:person:x->title:1".to_string(),
                kind: EdgeKind::Watched,
                source: "person:x".to_string(),
                target: "title:1".to_string(),
                weight: None,
                attrs: bad_attrs,
            });
            // A second edge with no watched_at attr at all.
            graph.edges.push(KgEdge {
                id: "watched:person:x->title:missing".to_string(),
                kind: EdgeKind::Watched,
                source: "person:x".to_string(),
                target: "title:1".to_string(),
                weight: None,
                attrs: Map::new(),
            });

            let viz = build_watch_history(&graph, None, 100);
            assert!(
                viz.entries.is_empty(),
                "malformed/missing timestamps must be skipped, not panic"
            );
        }

        #[test]
        fn degrades_to_empty_on_an_empty_graph() {
            let graph = KgGraph::default();
            let viz = build_watch_history(&graph, None, 100);
            assert!(viz.entries.is_empty());
        }
    }

    // -----------------------------------------------------------------
    // 4. Taste-neighbor clusters
    // -----------------------------------------------------------------

    mod taste_clusters {
        use super::*;

        #[test]
        fn groups_similar_taste_and_separates_divergent_taste_with_labels() {
            let friends = TrustedFriends::from_friends([
                FriendIdentity::new("discord-alex", "Alex").opt_in(1),
                FriendIdentity::new("discord-sam", "Sam").opt_in(2),
                FriendIdentity::new("discord-taylor", "Taylor").opt_in(3),
            ]);
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
                        centroid: full_vec(8, 0.98, 0.02),
                    },
                    PersonaRecord {
                        discord_user_id: "discord-taylor".to_string(),
                        persona_id: 3,
                        persona_name: "taylor".to_string(),
                        centroid: full_vec(8, -1.0, 0.0),
                    },
                ],
            };
            let graph = assemble_shared_graph(&friends, &data, 0.5);

            let viz = build_taste_clusters(&graph);

            let alex_id = person_node_id("discord-alex");
            let taylor_id = person_node_id("discord-taylor");

            let alex_cluster = viz
                .clusters
                .iter()
                .find(|c| c.iter().any(|m| m.person_id == alex_id))
                .expect("alex in some cluster");
            assert!(alex_cluster
                .iter()
                .any(|m| m.person_id == person_node_id("discord-sam")));
            assert!(!alex_cluster.iter().any(|m| m.person_id == taylor_id));
            // Labels are resolved, not just ids.
            assert!(alex_cluster.iter().any(|m| m.label == "Alex"));
        }

        #[test]
        fn degrades_to_empty_on_an_empty_graph() {
            let graph = KgGraph::default();
            let viz = build_taste_clusters(&graph);
            assert!(viz.clusters.is_empty());
        }
    }

    // -----------------------------------------------------------------
    // LOAD-BEARING PRIVACY NEGATIVE TEST (viz layer)
    // -----------------------------------------------------------------
    //
    // MUSEX-16's own `crate::kg::assemble::tests` module already proves
    // opted-out relations never enter the *graph*. This proves the property
    // survives one layer further: every MUSEX-17 viz builder run over that
    // same graph also emits none of the opted-out person's ids -- not
    // because the source graph happened to be empty, but because it
    // genuinely contained (and dropped) their relations.
    mod privacy {
        use super::*;

        #[test]
        fn opted_out_users_relations_never_enter_any_viz_output() {
            let friends = TrustedFriends::from_friends([
                FriendIdentity::new("discord-alex", "Alex").opt_in(1),
                FriendIdentity::new("discord-sam", "Sam").opt_in(2),
                // Allowlisted, but never opted in.
                FriendIdentity::new("discord-jamie", "Jamie"),
            ]);

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
                    coview("discord-alex", "discord-sam", "sess-alex-sam"),
                    // Jamie co-viewed WITH an opted-in friend -- still must
                    // be excluded because Jamie's own end isn't opted in.
                    coview("discord-alex", "discord-jamie", "sess-alex-jamie"),
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
                        // Deliberately near-identical to Alex's -- if the
                        // filter were broken this WOULD produce a taste
                        // edge / cluster membership.
                        centroid: full_vec(8, 1.0, 0.0),
                    },
                ],
            };

            // Sanity: the source data genuinely contains Jamie's relations
            // (proves the assertions below test the FILTER, not vacuously
            // empty input).
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

            let jamie_id = person_node_id("discord-jamie");
            let alex_id = person_node_id("discord-alex");

            // 1. Taste-map, queried both AS alex (jamie must never appear
            //    as a neighbor) and AS jamie (must degrade to no-data, not
            //    leak jamie's real personas).
            let alex_taste_map = build_taste_map(&graph, &alex_id);
            assert!(!alex_taste_map
                .neighbors
                .iter()
                .any(|n| n.person_id == jamie_id));
            let jamie_taste_map = build_taste_map(&graph, &jamie_id);
            assert!(jamie_taste_map.label.is_none());
            assert!(jamie_taste_map.personas.is_empty());
            assert!(jamie_taste_map.neighbors.is_empty());

            // 2. Group-dynamics: no node/edge references jamie.
            let group_dynamics = build_group_dynamics(&graph);
            assert!(!group_dynamics.nodes.iter().any(|n| n.id == jamie_id));
            assert!(!group_dynamics
                .edges
                .iter()
                .any(|e| e.source == jamie_id || e.target == jamie_id));
            // Alex/Sam's real co-view DOES appear -- proves this is a real
            // filter, not a bug that drops everyone.
            assert!(group_dynamics.nodes.iter().any(|n| n.id == alex_id));

            // 3. Watch-history: jamie's title never appears anywhere.
            let watch_history = build_watch_history(&graph, None, 100);
            assert!(!watch_history
                .entries
                .iter()
                .any(|e| e.person_id == jamie_id));
            assert!(!watch_history
                .entries
                .iter()
                .any(|e| e.title == "Jamie's Secret Show"));
            assert!(watch_history.entries.iter().any(|e| e.person_id == alex_id));

            // 4. Taste-clusters: no cluster contains jamie, even though her
            //    centroid was seeded near-identical to Alex's.
            let clusters = build_taste_clusters(&graph);
            assert!(!clusters
                .clusters
                .iter()
                .any(|c| c.iter().any(|m| m.person_id == jamie_id)));
        }
    }
}
