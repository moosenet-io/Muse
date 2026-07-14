//! MUSE-05: multi-instance Radarr/Sonarr (*arr API v3) ingest client.
//!
//! The operator runs **8 *arr instances** sharded by root folder/purpose
//! (`ARR-BLUEPRINT.md` §1): 5 Radarr (radarr, radarr_foreign, radarr_anime,
//! radarr_uhd, radarr_animated) + 3 Sonarr (sonarr, sonarr_anime,
//! sonarr_animated). This module is a *pure, read-only* HTTP client
//! ([`client::ArrClient`]) plus an ingest routine ([`ingest::run`]) that maps
//! *arr API responses onto the MUSE-02 core schema
//! (`libraries`/`media_metadata`/`media_items`/`seasons`/`episodes`/
//! `media_files`) via the existing `repo::*` layer — mirroring the MUSE-04
//! `plex` module's shape (typed client + `#[cfg(test)]` httpmock parsing
//! tests), but for N configured instances instead of one server.
//!
//! **Never write to *arr** (Phase 0 is acquisition-read-only per the S96
//! founding spec §1). **Never hardcode instance URLs/keys** — the fleet is
//! described by [`config::ArrInstanceConfig`], loaded from
//! `MUSE_ARR_INSTANCES` (JSON) via [`crate::config::Config::arr_instances`].
//!
//! One *arr instance (`radarr_animated`, per the operator) is currently
//! offline; [`ingest::run`] degrades gracefully — an unreachable/erroring
//! instance is logged and skipped, never aborting ingest for the rest of the
//! fleet (see [`ingest::IngestSummary`]).
//!
//! [`request`] (MUSEX-14, Plane TERM #390) adds the tiered-safety
//! CLASSIFICATION for a conversational "please get this" ask — it does
//! **not** relax the "never write to *arr" rule above. [`request::classify_tier`]
//! only decides which [`request::RequestTier`] a missing title falls into;
//! actually submitting a request is delegated to a [`request::MediaRequestSink`]
//! seam with no live Radarr/Sonarr-writing implementation shipped here, same
//! posture as this module's own read-only [`ArrClient`].

pub mod client;
pub mod config;
pub mod ingest;
pub mod models;
pub mod request;

pub use client::ArrClient;
pub use config::{ArrInstanceConfig, ArrKind};
pub use ingest::{run, IngestSummary};
pub use request::{
    classify_tier, MediaRequestDraft, MediaRequestOutcome, MediaRequestSink, RequestTier,
};
