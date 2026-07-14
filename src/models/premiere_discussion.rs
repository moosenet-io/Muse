//! MUSEX-15 (Plane TERM #391), part B: `discussion_threads` /
//! `discussion_posts` — async, per-title "book-club" style discussion. See
//! `migrations/0101_premiere_discussion_threads.sql` and
//! `crate::premiere::discussion` for the consent-gated domain layer that
//! sits in front of these tables (this module is data-shape only, same
//! posture as `crate::models::proactive_item`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct DiscussionThread {
    pub id: i64,
    pub media_metadata_id: i64,
    pub title: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewDiscussionThread {
    pub media_metadata_id: i64,
    pub title: String,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct DiscussionPost {
    pub id: i64,
    pub thread_id: i64,
    pub discord_user_id: String,
    pub body: String,
    pub posted_at: DateTime<Utc>,
}

/// The input to a persisted post. Deliberately does NOT carry any
/// opt-in/consent flag of its own — by the time `repo::premiere_discussion::create_post`
/// is called, `crate::premiere::discussion::post_message` has already
/// verified `discord_user_id` is an allowlisted, opted-in friend via
/// `crate::discord::identity::TrustedFriends`. This mirrors
/// `crate::promotion::targeting::promote_new_title`'s "consent checked
/// before I/O is even attempted" posture.
#[derive(Debug, Clone)]
pub struct NewDiscussionPost {
    pub thread_id: i64,
    pub discord_user_id: String,
    pub body: String,
}
