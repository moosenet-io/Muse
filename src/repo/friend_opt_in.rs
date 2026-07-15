//! MUSEX-WIRE-05 (Plane TERM #398, slice 5): repo functions for
//! `friend_opt_in` — see `migrations/0103_friend_opt_in.sql` for the
//! schema/doc. Runtime sqlx only, per `repo::mod`'s crate-wide rule.
//!
//! This is a plain fact table with no consent LOGIC: it records what a
//! friend has consented to, but never itself decides what that consent
//! authorizes. `crate::discord::roster::resolve_trusted_friends` is the
//! ONLY reader that turns a row here into a `FriendIdentity`, and it does
//! so exclusively via `FriendIdentity::opt_in` (the sole production
//! mutator of the private `taste_opt_in` field) — never a raw struct
//! literal or field write. See that module's doc.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::{MuseError, MuseResult};

/// One persisted opt-in fact. `opted_in = true` with `muse_account_id =
/// None` is a defensive-only shape (see the migration doc) — normal writes
/// via [`set_opt_in`] always set both together, same atomicity
/// `FriendIdentity::opt_in` itself enforces at the type level.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct FriendOptIn {
    pub discord_user_id: String,
    pub opted_in: bool,
    pub muse_account_id: Option<i64>,
    pub opted_in_at: Option<DateTime<Utc>>,
}

/// Record consent + link the account atomically — mirrors
/// `FriendIdentity::opt_in`'s "one atomic call" contract at the persistence
/// layer. Upserts keyed by `discord_user_id`: opting in again (e.g. to
/// relink to a different account) simply overwrites the previous row.
pub async fn set_opt_in(
    pool: &PgPool,
    discord_user_id: &str,
    muse_account_id: i64,
) -> MuseResult<FriendOptIn> {
    sqlx::query_as::<_, FriendOptIn>(
        r#"
        INSERT INTO friend_opt_in (discord_user_id, opted_in, muse_account_id, opted_in_at)
        VALUES ($1, true, $2, now())
        ON CONFLICT (discord_user_id) DO UPDATE SET
            opted_in = true,
            muse_account_id = EXCLUDED.muse_account_id,
            opted_in_at = now()
        RETURNING discord_user_id, opted_in, muse_account_id, opted_in_at
        "#,
    )
    .bind(discord_user_id)
    .bind(muse_account_id)
    .fetch_one(pool)
    .await
    .map_err(MuseError::Database)
}

/// Opt-out: symmetric with [`set_opt_in`] — clears both `opted_in` and the
/// linked account, same "no residual link an opt-in check could
/// accidentally bypass" posture `FriendIdentity::opt_out` documents for
/// itself. A no-op (returns `Ok(None)`) when the friend never had a row —
/// opting out of consent that was never granted is not an error.
pub async fn clear_opt_in(pool: &PgPool, discord_user_id: &str) -> MuseResult<Option<FriendOptIn>> {
    sqlx::query_as::<_, FriendOptIn>(
        r#"
        UPDATE friend_opt_in
        SET opted_in = false, muse_account_id = NULL
        WHERE discord_user_id = $1
        RETURNING discord_user_id, opted_in, muse_account_id, opted_in_at
        "#,
    )
    .bind(discord_user_id)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

/// The persisted opt-in row for one friend, or `None` if no row exists yet
/// (never opted in, or opted in and then the row was never created —
/// equivalent states for a caller, both read as "not opted in").
pub async fn get(pool: &PgPool, discord_user_id: &str) -> MuseResult<Option<FriendOptIn>> {
    sqlx::query_as::<_, FriendOptIn>(
        "SELECT discord_user_id, opted_in, muse_account_id, opted_in_at \
         FROM friend_opt_in WHERE discord_user_id = $1",
    )
    .bind(discord_user_id)
    .fetch_optional(pool)
    .await
    .map_err(MuseError::Database)
}

/// Every friend currently opted in — an operator-facing enumeration, not on
/// any production consent-decision path (mirrors
/// `crate::discord::identity::TrustedFriends::opted_in_friends`'s doc
/// distinguishing "the roster" from "an enumeration of it").
pub async fn list_opted_in(pool: &PgPool) -> MuseResult<Vec<FriendOptIn>> {
    sqlx::query_as::<_, FriendOptIn>(
        "SELECT discord_user_id, opted_in, muse_account_id, opted_in_at \
         FROM friend_opt_in WHERE opted_in = true ORDER BY discord_user_id",
    )
    .fetch_all(pool)
    .await
    .map_err(MuseError::Database)
}

#[cfg(test)]
mod db_gated {
    use super::*;

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

    /// Creates a real `accounts` row so `muse_account_id` FK inserts are
    /// valid — `friend_opt_in.muse_account_id` references `accounts(id)`.
    async fn seed_account(pool: &PgPool) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO accounts (username, friendly_name, is_home_user, is_primary) \
             VALUES ($1, $2, false, false) RETURNING id",
        )
        .bind(format!("wire05-test-{}", uuid_ish()))
        .bind("WIRE-05 Test Account")
        .fetch_one(pool)
        .await
        .expect("seed account");
        row.0
    }

    // A small dependency-free unique-ish suffix so repeated test runs don't
    // collide on the (implicitly unique-ish) username above.
    fn uuid_ish() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    }

    #[tokio::test]
    async fn set_opt_in_then_get_round_trips_opted_in_true_and_linked_account() {
        let Some(pool) =
            test_pool_or_skip("set_opt_in_then_get_round_trips_opted_in_true_and_linked_account")
                .await
        else {
            return;
        };
        let account_id = seed_account(&pool).await;
        let discord_user_id = format!("discord-wire05-{}", uuid_ish());

        let written = set_opt_in(&pool, &discord_user_id, account_id)
            .await
            .expect("set_opt_in should succeed");
        assert!(written.opted_in);
        assert_eq!(written.muse_account_id, Some(account_id));

        let fetched = get(&pool, &discord_user_id)
            .await
            .expect("get should succeed")
            .expect("row should exist after set_opt_in");
        assert!(fetched.opted_in);
        assert_eq!(fetched.muse_account_id, Some(account_id));
        assert!(fetched.opted_in_at.is_some());
    }

    #[tokio::test]
    async fn clear_opt_in_flips_opted_in_false_and_unlinks_account() {
        let Some(pool) =
            test_pool_or_skip("clear_opt_in_flips_opted_in_false_and_unlinks_account").await
        else {
            return;
        };
        let account_id = seed_account(&pool).await;
        let discord_user_id = format!("discord-wire05-{}", uuid_ish());

        set_opt_in(&pool, &discord_user_id, account_id)
            .await
            .expect("set_opt_in should succeed");

        let cleared = clear_opt_in(&pool, &discord_user_id)
            .await
            .expect("clear_opt_in should succeed")
            .expect("row should exist to clear");
        assert!(!cleared.opted_in);
        assert!(cleared.muse_account_id.is_none());

        let fetched = get(&pool, &discord_user_id)
            .await
            .expect("get should succeed")
            .expect("row should still exist, just cleared");
        assert!(!fetched.opted_in);
        assert!(fetched.muse_account_id.is_none());
    }

    #[tokio::test]
    async fn clear_opt_in_on_a_never_opted_in_friend_is_a_no_op() {
        let Some(pool) =
            test_pool_or_skip("clear_opt_in_on_a_never_opted_in_friend_is_a_no_op").await
        else {
            return;
        };
        let discord_user_id = format!("discord-wire05-never-{}", uuid_ish());

        let result = clear_opt_in(&pool, &discord_user_id)
            .await
            .expect("clear_opt_in must not error on a missing row");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_is_none_for_a_friend_with_no_row() {
        let Some(pool) = test_pool_or_skip("get_is_none_for_a_friend_with_no_row").await else {
            return;
        };
        let discord_user_id = format!("discord-wire05-nonexistent-{}", uuid_ish());

        let result = get(&pool, &discord_user_id)
            .await
            .expect("get must not error on a missing row");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_opted_in_excludes_opted_out_friends() {
        let Some(pool) = test_pool_or_skip("list_opted_in_excludes_opted_out_friends").await else {
            return;
        };
        let account_id = seed_account(&pool).await;
        let opted_in_id = format!("discord-wire05-listed-{}", uuid_ish());
        let opted_out_id = format!("discord-wire05-notlisted-{}", uuid_ish());

        set_opt_in(&pool, &opted_in_id, account_id)
            .await
            .expect("set_opt_in for the opted-in friend");
        set_opt_in(&pool, &opted_out_id, account_id)
            .await
            .expect("set_opt_in for the soon-to-opt-out friend");
        clear_opt_in(&pool, &opted_out_id)
            .await
            .expect("clear_opt_in for the opted-out friend");

        let listed = list_opted_in(&pool).await.expect("list_opted_in");
        let ids: Vec<&str> = listed.iter().map(|f| f.discord_user_id.as_str()).collect();
        assert!(ids.contains(&opted_in_id.as_str()));
        assert!(!ids.contains(&opted_out_id.as_str()));
    }
}
