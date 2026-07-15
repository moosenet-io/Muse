//! MUSEX-13 (Plane TERM #389): the Discord bot core — a genuinely bespoke
//! social surface for Muse, NOT a Requestrr/Notifiarr command-table reskin.
//!
//! ## Why this module exists
//! Every other MUSEX experience-layer item (persona, recommend/because,
//! the conversational assistant) lives inside Muse's own brain. This module
//! is the first surface that lets a *friend* — a trusted human outside the
//! Plex-account model this crate otherwise assumes — talk to that brain
//! from Discord. It deliberately reuses the real brain
//! ([`crate::curation::recommend`], [`crate::persona`],
//! [`crate::taste_review::trace`]) rather than inventing a second,
//! Discord-specific recommendation/rationale path — see [`bot`]'s module
//! doc for how.
//!
//! ## The four pieces (mirrors the seam pattern established by
//! `crate::cultural::source::TrendSource` / `crate::watch_together::sync::ServerSyncPrimitive`)
//! - [`identity`] — the per-Discord-user record: PRIVATE, DEFAULT-FALSE
//!   consent (`taste_opt_in` + the linked Muse account, granted only,
//!   atomically, via `FriendIdentity::opt_in` and read via
//!   `is_opted_in`/`linked_account`), plus the [`identity::TrustedFriends`]
//!   allowlist that scopes who is served at all.
//! - [`client`] — the [`client::DiscordClient`] trait: a real,
//!   `DISCORD_BOT_TOKEN`-config-gated implementation (inert when
//!   unconfigured — Muse has no live Discord integration yet, so this is a
//!   documented best-effort client, never exercised by a live call in this
//!   crate's own test suite) plus [`client::MockDiscordClient`] for tests.
//! - [`client::RichEmbed`] — the server-agnostic embed struct (title +
//!   poster art URL + synopsis) a [`client::DiscordClient`] renders.
//! - [`bot`] — the brain-driven interaction logic: [`bot::decide_response_mode`]
//!   is the load-bearing, default-private gate; [`bot::build_generic_reply`]
//!   and [`bot::build_taste_reply`] are the two possible outputs, and their
//!   *type signatures* — not just runtime checks — are what make "a
//!   non-opted-in friend's reply can never carry taste/watch-data" provable
//!   by construction (see that module's doc).

pub mod bot;
pub mod client;
pub mod identity;
pub mod opt_in_route;
pub mod roster;

pub use bot::{decide_response_mode, ResponseMode};
pub use client::{DiscordClient, MockDiscordClient, RealDiscordClient, RichEmbed};
pub use identity::{FriendIdentity, TrustedFriends};
pub use opt_in_route::{friend_opt_in_handler, friend_opt_out_handler};
pub use roster::resolve_trusted_friends;
