//! MUSEX-15 (Plane TERM #391), part B: async "book-club" style discussion
//! threads for a premiere/title. Persistence follows the exact
//! `repo::premiere_discussion` shape (`migrations/0101_premiere_discussion_threads.sql`);
//! this module is the consent-gated domain layer in front of it, mirroring
//! `crate::promotion::targeting::promote_new_title`'s "consent checked
//! before I/O is even attempted" posture and
//! `crate::conversational`'s "reuse the real seam, invent nothing new" one:
//! posting a message goes through the SAME [`TrustedFriends`] allowlist +
//! opt-in check `crate::premiere::schedule::PremiereEvent::rsvp` uses, and
//! (optionally) announces via the SAME [`DiscordClient`] seam
//! `crate::discord::client`/`crate::promotion::targeting::dispatch_promotions`
//! already establish — no live Discord thread API call is required (or
//! made) here; `DiscordClient::reply` is enough to notify a channel a new
//! thread/post exists, exactly like `dispatch_promotions` uses `reply` for
//! its own plain-text half.

use sqlx::PgPool;

use crate::discord::client::DiscordClient;
use crate::discord::identity::TrustedFriends;
use crate::error::{MuseError, MuseResult};
use crate::models::premiere_discussion::{
    DiscussionPost, DiscussionThread, NewDiscussionPost, NewDiscussionThread,
};
use crate::repo;

/// Create a discussion thread for a title. No consent check needed here —
/// creating a thread carries no friend-specific data (mirrors
/// `crate::premiere::schedule::schedule_premiere` not gating candidate
/// lookup itself, only the invite list).
pub async fn create_thread(
    pool: &PgPool,
    media_metadata_id: i64,
    title: impl Into<String>,
) -> MuseResult<DiscussionThread> {
    repo::premiere_discussion::create_thread(
        pool,
        &NewDiscussionThread {
            media_metadata_id,
            title: title.into(),
        },
    )
    .await
}

/// Post a message to a thread. THE gate: `discord_user_id` must be both
/// allowlisted AND opted-in per `friends` — checked BEFORE
/// `repo::premiere_discussion::create_post` is ever called, so a rejected
/// post never reaches the database at all (same "never even attempted"
/// posture the module doc describes).
pub async fn post_message(
    pool: &PgPool,
    friends: &TrustedFriends,
    thread_id: i64,
    discord_user_id: &str,
    body: impl Into<String>,
) -> MuseResult<DiscussionPost> {
    let Some(friend) = friends.get(discord_user_id) else {
        return Err(MuseError::BadRequest(format!(
            "{discord_user_id} is not an allowlisted friend"
        )));
    };
    if !friend.is_opted_in() {
        return Err(MuseError::BadRequest(format!(
            "{discord_user_id} has not opted in to taste/discussion use"
        )));
    }

    repo::premiere_discussion::create_post(
        pool,
        &NewDiscussionPost {
            thread_id,
            discord_user_id: discord_user_id.to_string(),
            body: body.into(),
        },
    )
    .await
}

pub async fn list_posts(pool: &PgPool, thread_id: i64) -> MuseResult<Vec<DiscussionPost>> {
    repo::premiere_discussion::list_posts_for_thread(pool, thread_id).await
}

/// Announce a newly-created thread through a [`DiscordClient`] — thin glue
/// only, same posture as `crate::promotion::targeting::dispatch_promotions`:
/// all the consent/creation logic already happened, this just notifies.
pub async fn announce_thread(
    discord: &dyn DiscordClient,
    channel_id: &str,
    thread: &DiscussionThread,
) -> MuseResult<()> {
    discord
        .reply(
            channel_id,
            &format!(
                "A new discussion thread is open for \"{}\" — post your thoughts any time.",
                thread.title
            ),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::client::MockDiscordClient;

    #[tokio::test]
    async fn announce_thread_sends_exactly_one_reply_naming_the_title() {
        let mock = MockDiscordClient::new();
        let thread = DiscussionThread {
            id: 1,
            media_metadata_id: 42,
            title: "Severance".to_string(),
            created_at: chrono::Utc::now(),
        };

        announce_thread(&mock, "channel-1", &thread).await.unwrap();

        assert_eq!(mock.reply_call_count(), 1);
        let calls = mock.reply_calls.lock().unwrap();
        assert!(calls[0].1.contains("Severance"));
    }
}

/// DB-backed end-to-end coverage: create a thread, post from an opted-in
/// friend (persisted + retrievable), and prove a non-opted-in friend's post
/// never reaches the database. Gated per `MUSE_TEST_DATABASE_URL`, same
/// convention as `crate::promotion::targeting::db_gated` /
/// `crate::conversational::db_gated` — skips cleanly, never a hard failure,
/// when no test database is configured.
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::discord::identity::FriendIdentity;
    use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
    use serde_json::json;

    async fn test_pool_or_skip(test_name: &str) -> Option<PgPool> {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping {test_name} \
                 (expected in the default test run; this harness does not \
                 require a live DB)"
            );
            return None;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");
        Some(pool)
    }

    async fn seed_title(pool: &PgPool) -> (i64, String) {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let title = format!("MUSEX15PremiereDiscussionProbe{suffix}");
        let metadata = repo::media_metadata::upsert_by_tmdb(
            pool,
            &NewMediaMetadata {
                kind: MediaKind::Show,
                tmdb_id: Some(format!("tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: json!({}),
                title: title.clone(),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: Some("a real, seeded synopsis".to_string()),
                studio: None,
                network: None,
                runtime_minutes: Some(50),
                year: Some(2024),
                images: json!({}),
            },
        )
        .await
        .expect("create media_metadata");
        (metadata.id, title)
    }

    #[tokio::test]
    async fn opted_in_friend_post_is_persisted_and_retrievable() {
        let Some(pool) =
            test_pool_or_skip("opted_in_friend_post_is_persisted_and_retrievable").await
        else {
            return;
        };

        let (media_metadata_id, title) = seed_title(&pool).await;
        let thread = create_thread(&pool, media_metadata_id, title.clone())
            .await
            .expect("create_thread should not error");

        let friends =
            TrustedFriends::from_friends([FriendIdentity::new("discord-alex", "Alex").opt_in(1)]);

        post_message(
            &pool,
            &friends,
            thread.id,
            "discord-alex",
            "can't wait for this",
        )
        .await
        .expect("post_message should not error for an opted-in friend");

        let posts = list_posts(&pool, thread.id)
            .await
            .expect("list_posts should not error");
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].discord_user_id, "discord-alex");
        assert_eq!(posts[0].body, "can't wait for this");
    }

    /// LOAD-BEARING PRIVACY NEGATIVE TEST: a non-opted-in (but allowlisted)
    /// friend's post attempt is rejected and never reaches the database.
    #[tokio::test]
    async fn non_opted_in_friend_post_is_rejected_and_never_persisted() {
        let Some(pool) =
            test_pool_or_skip("non_opted_in_friend_post_is_rejected_and_never_persisted").await
        else {
            return;
        };

        let (media_metadata_id, title) = seed_title(&pool).await;
        let thread = create_thread(&pool, media_metadata_id, title)
            .await
            .expect("create_thread should not error");

        // Allowlisted but not opted in.
        let friends = TrustedFriends::from_friends([FriendIdentity::new("discord-jamie", "Jamie")]);
        assert!(
            !friends.get("discord-jamie").unwrap().is_opted_in(),
            "sanity: not opted in"
        );

        let result = post_message(
            &pool,
            &friends,
            thread.id,
            "discord-jamie",
            "spoilers ahead",
        )
        .await;
        assert!(
            result.is_err(),
            "a non-opted-in friend's post must be rejected"
        );

        let posts = list_posts(&pool, thread.id)
            .await
            .expect("list_posts should not error");
        assert!(
            posts.is_empty(),
            "a rejected post must never reach the database: {posts:?}"
        );
    }
}
