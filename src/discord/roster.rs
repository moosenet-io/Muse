//! MUSEX-WIRE-05 (Plane TERM #398, slice 5): the KEYSTONE that closes the
//! shared honest seam WIRE-01..04 each documented for themselves —
//! [`resolve_trusted_friends`] is the SINGLE sanctioned place PERSISTED
//! (not just allowlisted) production opt-in state enters a
//! [`TrustedFriends`] roster.
//!
//! ## The seam this closes
//! Every wired handler (`discord::bot::discord_respond_handler`,
//! `conversational::conversational_handler`, `premiere::http`,
//! `channels::director_route::channel_director_refresh_handler`) built its
//! roster from `ExperienceSettings.discord_bot.trusted_friends` alone —
//! ALLOWLIST membership only, per `crate::settings::DiscordBotSettings`'s
//! own doc, which can never itself grant `taste_opt_in`. So
//! `FriendIdentity::is_opted_in()` was always `false` in production, and
//! every personalized path fell to its non-personalized default. This
//! module adds the missing piece: `migrations/0103_friend_opt_in.sql` /
//! `repo::friend_opt_in`, the persisted per-friend consent fact table, and
//! this resolver, which reads it.
//!
//! ## Consent-by-construction preserved
//! [`resolve_trusted_friends`] never writes `FriendIdentity`'s private
//! fields directly — there is no way to, from outside `discord::identity`.
//! For each allowlisted friend it looks up the persisted
//! `repo::friend_opt_in` row and, if (and only if) `opted_in = true` AND a
//! `muse_account_id` is present, calls the SANCTIONED
//! `FriendIdentity::new(...).opt_in(account_id)` path — the ONLY production
//! mutator that can ever set `taste_opt_in`. A friend absent from the
//! store, or present with `opted_in = false`, or (the defensive,
//! FK-degrade-only shape `repo::friend_opt_in`'s doc calls out)
//! `opted_in = true` with no linked account, all resolve to a plain
//! `FriendIdentity::new(...)` — not opted in, by construction, same as
//! every WIRE-01..04 roster's default.

use sqlx::PgPool;

use crate::discord::identity::{FriendIdentity, TrustedFriends};
use crate::error::MuseResult;
use crate::repo;
use crate::settings::ExperienceSettings;

/// Build a [`TrustedFriends`] roster reflecting PERSISTED opt-in state: the
/// allowlist still comes from `settings.discord_bot.trusted_friends` (an
/// unlisted friend is never served at all, regardless of any opt-in row —
/// same "allowlist scopes who is served, opt-in scopes what they get"
/// two-gate posture `discord::identity`'s module doc establishes), but each
/// allowlisted friend's consent now comes from `repo::friend_opt_in`
/// instead of always defaulting to not-opted-in.
///
/// This is the ONE place production opt-in state enters a roster. Callers
/// that previously built a roster via
/// `TrustedFriends::from_friends(settings.discord_bot.trusted_friends.iter().map(...))`
/// (every WIRE-01..04 handler) should call this instead to make their
/// personalized arm reachable — see `discord::bot::discord_respond_handler`
/// for the one handler this slice rewires as proof (module doc for the
/// mechanical follow-up on the rest).
pub async fn resolve_trusted_friends(
    pool: &PgPool,
    settings: &ExperienceSettings,
) -> MuseResult<TrustedFriends> {
    let mut friends = Vec::with_capacity(settings.discord_bot.trusted_friends.len());

    for entry in &settings.discord_bot.trusted_friends {
        let identity =
            FriendIdentity::new(entry.discord_user_id.clone(), entry.display_name.clone());

        let opted_in_identity = match repo::friend_opt_in::get(pool, &entry.discord_user_id).await?
        {
            Some(row) if row.opted_in => row
                .muse_account_id
                .map(|account_id| identity.clone().opt_in(account_id)),
            _ => None,
        };

        friends.push(opted_in_identity.unwrap_or(identity));
    }

    Ok(TrustedFriends::from_friends(friends))
}

#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::settings::TrustedFriendEntry;

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

    async fn seed_account(pool: &PgPool) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO accounts (username, friendly_name, is_home_user, is_primary) \
             VALUES ($1, $2, false, false) RETURNING id",
        )
        .bind(format!("wire05-roster-test-{}", uuid_ish()))
        .bind("WIRE-05 Roster Test Account")
        .fetch_one(pool)
        .await
        .expect("seed account");
        row.0
    }

    fn uuid_ish() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    }

    fn settings_with(entries: Vec<TrustedFriendEntry>) -> ExperienceSettings {
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot.enabled = true;
        settings.discord_bot.trusted_friends = entries;
        settings
    }

    #[tokio::test]
    async fn persisted_opted_in_friend_resolves_to_is_opted_in_true_via_opt_in() {
        let Some(pool) =
            test_pool_or_skip("persisted_opted_in_friend_resolves_to_is_opted_in_true_via_opt_in")
                .await
        else {
            return;
        };
        let account_id = seed_account(&pool).await;
        let discord_user_id = format!("discord-wire05-roster-opted-{}", uuid_ish());

        repo::friend_opt_in::set_opt_in(&pool, &discord_user_id, account_id)
            .await
            .expect("set_opt_in");

        let settings = settings_with(vec![TrustedFriendEntry {
            discord_user_id: discord_user_id.clone(),
            display_name: "Alex".to_string(),
        }]);

        let roster = resolve_trusted_friends(&pool, &settings)
            .await
            .expect("resolve_trusted_friends");

        let friend = roster.get(&discord_user_id).expect("friend on roster");
        assert!(
            friend.is_opted_in(),
            "a persisted-opted-in friend must resolve as opted in"
        );
        assert_eq!(friend.linked_account(), Some(account_id));
    }

    #[tokio::test]
    async fn non_persisted_friend_resolves_to_not_opted_in() {
        let Some(pool) = test_pool_or_skip("non_persisted_friend_resolves_to_not_opted_in").await
        else {
            return;
        };
        let discord_user_id = format!("discord-wire05-roster-never-{}", uuid_ish());

        let settings = settings_with(vec![TrustedFriendEntry {
            discord_user_id: discord_user_id.clone(),
            display_name: "Sam".to_string(),
        }]);

        let roster = resolve_trusted_friends(&pool, &settings)
            .await
            .expect("resolve_trusted_friends");

        let friend = roster.get(&discord_user_id).expect("friend on roster");
        assert!(
            !friend.is_opted_in(),
            "a friend with no persisted opt-in row must not be opted in"
        );
    }

    #[tokio::test]
    async fn opted_out_friend_resolves_to_not_opted_in() {
        let Some(pool) = test_pool_or_skip("opted_out_friend_resolves_to_not_opted_in").await
        else {
            return;
        };
        let account_id = seed_account(&pool).await;
        let discord_user_id = format!("discord-wire05-roster-optedout-{}", uuid_ish());

        repo::friend_opt_in::set_opt_in(&pool, &discord_user_id, account_id)
            .await
            .expect("set_opt_in");
        repo::friend_opt_in::clear_opt_in(&pool, &discord_user_id)
            .await
            .expect("clear_opt_in");

        let settings = settings_with(vec![TrustedFriendEntry {
            discord_user_id: discord_user_id.clone(),
            display_name: "Jamie".to_string(),
        }]);

        let roster = resolve_trusted_friends(&pool, &settings)
            .await
            .expect("resolve_trusted_friends");

        let friend = roster.get(&discord_user_id).expect("friend on roster");
        assert!(
            !friend.is_opted_in(),
            "an opted-out friend must resolve as not opted in"
        );
        assert!(friend.linked_account().is_none());
    }

    #[tokio::test]
    async fn non_allowlisted_discord_user_id_never_appears_on_the_roster_even_if_opted_in() {
        // A friend_opt_in row for someone NOT in
        // settings.discord_bot.trusted_friends must never leak onto the
        // roster — the allowlist gate runs first, same two-gate posture
        // `discord::identity`'s module doc documents.
        let Some(pool) = test_pool_or_skip(
            "non_allowlisted_discord_user_id_never_appears_on_the_roster_even_if_opted_in",
        )
        .await
        else {
            return;
        };
        let account_id = seed_account(&pool).await;
        let not_allowlisted_id = format!("discord-wire05-roster-notallowlisted-{}", uuid_ish());

        repo::friend_opt_in::set_opt_in(&pool, &not_allowlisted_id, account_id)
            .await
            .expect("set_opt_in");

        let settings = settings_with(vec![]);

        let roster = resolve_trusted_friends(&pool, &settings)
            .await
            .expect("resolve_trusted_friends");

        assert!(roster.get(&not_allowlisted_id).is_none());
        assert!(roster.is_empty());
    }
}
