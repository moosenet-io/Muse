//! Repo functions for `discussion_threads` / `discussion_posts` — see
//! `migrations/0101_premiere_discussion_threads.sql`.

use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};
use crate::models::premiere_discussion::{
    DiscussionPost, DiscussionThread, NewDiscussionPost, NewDiscussionThread,
};

pub async fn create_thread(
    pool: &PgPool,
    new: &NewDiscussionThread,
) -> MuseResult<DiscussionThread> {
    sqlx::query_as::<_, DiscussionThread>(
        r#"
        INSERT INTO discussion_threads (media_metadata_id, title)
        VALUES ($1, $2)
        RETURNING *
        "#,
    )
    .bind(new.media_metadata_id)
    .bind(&new.title)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

pub async fn get_thread(pool: &PgPool, id: i64) -> MuseResult<DiscussionThread> {
    sqlx::query_as::<_, DiscussionThread>("SELECT * FROM discussion_threads WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(MuseError::Database)?
        .ok_or_else(|| MuseError::NotFound(format!("discussion_thread {id} not found")))
}

/// Every thread for a title, most-recently-created first.
pub async fn list_threads_for_title(
    pool: &PgPool,
    media_metadata_id: i64,
) -> MuseResult<Vec<DiscussionThread>> {
    sqlx::query_as::<_, DiscussionThread>(
        "SELECT * FROM discussion_threads WHERE media_metadata_id = $1 ORDER BY created_at DESC",
    )
    .bind(media_metadata_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

/// Insert a post. Callers MUST have already authorized `discord_user_id` via
/// `crate::discord::identity::TrustedFriends` (opted-in + allowlisted) —
/// this function performs no consent check of its own, matching every other
/// repo module in this crate (`repo::proactive_item`, `repo::watch_stats`,
/// ...): the repo layer stores what the domain layer already decided.
pub async fn create_post(pool: &PgPool, new: &NewDiscussionPost) -> MuseResult<DiscussionPost> {
    sqlx::query_as::<_, DiscussionPost>(
        r#"
        INSERT INTO discussion_posts (thread_id, discord_user_id, body)
        VALUES ($1, $2, $3)
        RETURNING *
        "#,
    )
    .bind(new.thread_id)
    .bind(&new.discord_user_id)
    .bind(&new.body)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// All posts for a thread, oldest first (the natural "read the discussion in
/// order" order for an async, book-club-style thread).
pub async fn list_posts_for_thread(
    pool: &PgPool,
    thread_id: i64,
) -> MuseResult<Vec<DiscussionPost>> {
    sqlx::query_as::<_, DiscussionPost>(
        "SELECT * FROM discussion_posts WHERE thread_id = $1 ORDER BY posted_at ASC",
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}
