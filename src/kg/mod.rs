//! MUSEX-16 (Plane TERM #392): the Muse-side watch-history + group-dynamics
//! knowledge graph — a NODE/EDGE model over watch-history, personas, and
//! co-viewing, exported in a serializable shape suitable for the Atlas KG
//! build path ([`crate::kg::model`]), assembled PRIVACY-SCOPED from real
//! repo data ([`crate::kg::assemble`]), and queryable for group-dynamics /
//! taste-neighbor questions ([`crate::kg::query`]).
//!
//! ## The Atlas KG is external — this module produces an EXPORT, not a server
//! The Atlas per-project code knowledge graph (`scribe::graph`, `kg_*`
//! tools) is a Terminus capability, entirely outside this crate. Muse has no
//! `cargo`/network path to a live KG server, so this module does not call
//! `kg_*` anything and does not stand one up — it only ASSEMBLES the
//! node/edge structure ([`model::KgGraph`]) an external Atlas build step
//! could ingest, and answers the group-dynamics/taste-neighbor questions
//! in-process over that assembled structure ([`query`]). This mirrors the
//! "pure math, no live systems" posture `crate::persona::blend` and
//! `crate::watch_together`'s orchestration documents for themselves (S9).
//!
//! ## Privacy scoping, BY CONSTRUCTION (the load-bearing property)
//! [`assemble::assemble_shared_graph`] takes a
//! [`crate::discord::identity::TrustedFriends`] allowlist and filters every
//! node/edge through [`crate::discord::identity::TrustedFriends::opted_in_friends`]
//! — the SAME accessor `crate::promotion::targeting::promote_new_title` and
//! `crate::premiere::schedule::schedule_premiere` already use to keep a
//! non-opted-in friend out of their respective outputs. Filtering happens
//! BEFORE any shared node/edge is constructed (an opted-out/non-opted-in
//! person's id is checked against the opted-in set up front, and only
//! opted-in ids ever become a `person:` node id or appear as an edge
//! endpoint) — never a post-hoc redaction pass over an already-built graph.
//! See [`assemble`]'s module doc and its `db_gated`-free unit tests
//! (particularly the negative test) for the full argument.
//!
//! ## Node/edge kinds
//! Nodes: person, persona, title, session ([`model::NodeKind`]). Edges:
//! co-view (watched-together), taste-edge (taste-neighbor), watched
//! (person→title), persona-of (person→persona) ([`model::EdgeKind`]). Every
//! id is stable and namespaced (`person:{discord_user_id}`,
//! `persona:{persona_id}`, `title:{media_item_id}`, `session:{session_key}`)
//! so re-assembling from the same source data twice yields byte-identical
//! ids — see [`model`]'s id-helper functions.

pub mod assemble;
pub mod model;
pub mod query;
pub mod viz;

pub use assemble::{
    assemble_shared_graph, CoViewRecord, GraphSourceData, PersonaRecord, WatchRecord,
};
pub use model::{EdgeKind, KgEdge, KgGraph, KgNode, NodeKind};
pub use query::{bridge_between, co_view_adjacency, taste_neighbor_clusters};
pub use viz::{
    build_group_dynamics, build_taste_clusters, build_taste_map, build_watch_history,
    GroupDynamicsViz, TasteClusterViz, TasteMapViz, WatchHistoryViz,
};
