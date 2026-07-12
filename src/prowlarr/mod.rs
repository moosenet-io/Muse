//! Read-only Prowlarr availability client (MUSE-16).
//!
//! Mirrors the *arr report-pull mechanism (§4b of
//! `specs/S96-muse-foundation.md`) so Muse knows what's actually grabbable
//! *now* — not just what exists in a catalog. Three pieces:
//! - [`client::ProwlarrClient`] — the typed, read-only Prowlarr v1 API
//!   client (indexer listing, RSS-mode report-pull, bounded targeted
//!   search), with tracker-etiquette rate-limiting built in.
//! - [`rate_limit::RateLimiter`] — the polite-interval + hourly-cap guard
//!   the client enforces.
//! - [`parse::parse_release_name`] — the deterministic release-name parser
//!   v0 that populates `releases.parsed_*`.
//! - [`scheduler::is_due`] — the persisted (DB-backed) per-indexer
//!   scheduling decision the report-pull worker uses (MUSE-17).
//! - [`worker::run_tick`] / [`worker::spawn_report_pull_worker`] — the
//!   scheduled report-pull worker itself (MUSE-17): ties the client, parser,
//!   and repo layer together into one pull -> parse -> upsert -> rollup
//!   pass, run on a background loop.
//!
//! This module makes no writes to Prowlarr and never grabs anything; the
//! persistence side (upserting into `indexers`/`releases`/`availability`)
//! lives in `repo::indexer`/`repo::release`/`repo::availability`.

mod client;
mod models;
mod parse;
mod rate_limit;
mod scheduler;
mod worker;

pub use client::ProwlarrClient;
pub use models::{ProwlarrCapabilities, ProwlarrCategory, ProwlarrIndexer, ProwlarrRelease};
pub use parse::{parse_release_name, ParsedRelease};
pub use rate_limit::RateLimiter;
pub use worker::{spawn_report_pull_worker, TickSummary};
