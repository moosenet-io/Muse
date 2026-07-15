//! MUSEX-15 (Plane TERM #391): premiere events + engagement tiers — three
//! OPT-IN, GUI/config-tunable capabilities layered on top of already-shipped
//! MUSEX pieces, inventing no second consent model, rationale generator, or
//! request-safety gate:
//!
//! - [`schedule`] (part A) — scheduled premiere events: a title + time +
//!   RSVP + a grounded "why this pairing" rationale, announced via the
//!   `crate::discord` [`crate::discord::client::RichEmbed`] shape. Only
//!   opted-in/allowlisted friends (`crate::discord::identity::TrustedFriends`)
//!   can be invited or RSVP — see that module's doc for exactly how that's
//!   provable by construction, the same posture
//!   `crate::promotion::targeting` documents for itself.
//! - [`discussion`] (part B) — async, per-title "book-club" style discussion
//!   threads, persisted via `crate::repo::premiere_discussion` following
//!   this crate's standard repo-layer pattern. Posting is gated by the SAME
//!   `TrustedFriends` allowlist/opt-in check `schedule::PremiereEvent::rsvp`
//!   uses.
//! - [`engagement`] (part C) — engagement-tiered request budgets, computed
//!   from real watch-through + household-loved signals, that MODULATE
//!   (never bypass) `crate::arr::request`'s existing tiered-safety gate —
//!   see that submodule's doc for the exact "budget is strictly a brake,
//!   never an accelerator" contract.
//!
//! ## Why premieres live beside, not inside, `watch_together`
//! `crate::watch_together::GroupSession` answers "who's on the couch right
//! now" — an ad hoc, present-tense lobby. A premiere is the SCHEDULED,
//! ANNOUNCED flavor of the same underlying idea (a group watch session for
//! one title), but its lifecycle (announce ahead of time -> collect RSVPs
//! over days -> the event itself -> an async discussion afterward) is
//! different enough from a live lobby's (present members -> blend -> lock a
//! pick -> play now) that folding it into `GroupSession` would mean
//! threading a whole "not everyone is present yet" state machine through a
//! module that currently assumes they are. `schedule::PremiereEvent`
//! deliberately reuses `watch_together`'s neighboring primitives
//! (`crate::discord`'s consent/embed seam, `crate::curation::recommend`'s
//! rationale) rather than `GroupSession` itself; wiring "lock a premiere's
//! RSVP'd members into an actual `GroupSession` when the event starts" is a
//! natural, separately-reviewable follow-up, not done in this pass.

pub mod discussion;
pub mod engagement;
pub mod schedule;

// MUSEX-WIRE-01 (Plane TERM #398): `compute_tier`/`budget_for_tier`/
// `gather_engagement_counts` are no longer re-exported here — they became
// `pub(crate)` in `engagement` (consent-at-source, see that module's doc).
// The sanctioned doors onto their output are `resolve_friend_budget` /
// `resolve_friend_budget_from_counts` below, unchanged.
pub use engagement::{
    resolve_friend_budget, resolve_friend_budget_from_counts, submit_with_engagement_budget,
    EngagementCounts, EngagementTier, EngagementTierConfig, RequestBudget,
};
pub use schedule::{build_announce_embed, schedule_premiere, PremiereEvent, RsvpStatus};
