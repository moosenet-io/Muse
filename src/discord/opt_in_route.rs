//! MUSEX-WIRE-05 (Plane TERM #398, slice 5): the production HTTP doors that
//! WRITE to `repo::friend_opt_in` — `POST /friends/opt-in` and
//! `POST /friends/opt-out`. These are the only production entry points that
//! ever call `repo::friend_opt_in::set_opt_in`/`clear_opt_in`; everything
//! downstream (`discord::roster::resolve_trusted_friends`) only READS what
//! these routes write.
//!
//! ## Gating (mirrors WIRE-01..04's inert-first posture)
//! Settings load is the one unavoidable pool read; the
//! [`ExperienceSettings::is_discord_bot_enabled`] gate — master switch AND
//! the `discord_bot` per-subsystem toggle, the same gate
//! `discord::bot::discord_respond_handler` uses — is checked immediately
//! after, BEFORE any `friend_opt_in` write. A disabled subsystem answers
//! inert (`recorded: false`), never an error, never a write.
//!
//! A SECOND, consent-specific gate follows: the caller's `discord_user_id`
//! must already be on the `discord_bot.trusted_friends` ALLOWLIST. This
//! isn't optional plumbing — it's the same two-gate posture
//! `discord::identity`'s module doc establishes ("the allowlist scopes who
//! is served AT ALL, opt-in is considered only after"): a stranger with no
//! allowlist membership can never create a `friend_opt_in` row, because a
//! row for a non-allowlisted id could never be reached by
//! `resolve_trusted_friends` anyway (it walks the allowlist, not the opt-in
//! table) — rejecting it up front avoids silently accepting writes that can
//! never do anything, and avoids a confusing "recorded: true" for a request
//! that in fact grants no access.
//!
//! No `:id`/`{id}` path param is used (static routes only), same posture
//! WIRE-03/WIRE-04 document for themselves re: the axum-0.7 `{id}` bug.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::error::MuseResult;
use crate::http::AppState;
use crate::repo;

/// `POST /friends/opt-in` request body: the friend consenting, and the
/// Muse account whose taste/watch-data they're consenting to have used.
/// Mirrors `FriendIdentity::opt_in`'s own two-arguments-in-one-call shape
/// (account linkage and consent are one atomic decision, never separable).
#[derive(Debug, Deserialize)]
pub struct FriendOptInRequest {
    pub discord_user_id: String,
    pub muse_account_id: i64,
}

/// `POST /friends/opt-out` request body.
#[derive(Debug, Deserialize)]
pub struct FriendOptOutRequest {
    pub discord_user_id: String,
}

/// Shared response shape for both routes — inert (`recorded: false`) when
/// the subsystem is off or the caller isn't allowlisted, mirroring every
/// WIRE-01..04 response's all-inert-on-gate-fail shape.
#[derive(Debug, Serialize)]
pub struct FriendOptResponse {
    pub recorded: bool,
    pub discord_user_id: String,
    pub opted_in: bool,
}

fn is_allowlisted(settings: &crate::settings::ExperienceSettings, discord_user_id: &str) -> bool {
    settings
        .discord_bot
        .trusted_friends
        .iter()
        .any(|f| f.discord_user_id == discord_user_id)
}

/// `POST /friends/opt-in` — records consent + the linked account via
/// `repo::friend_opt_in::set_opt_in`. Inert-first: the settings gate and
/// the allowlist-membership check both run BEFORE the write.
pub async fn friend_opt_in_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FriendOptInRequest>,
) -> MuseResult<Json<FriendOptResponse>> {
    let settings = repo::settings::load(&state.pool).await?;

    if !settings.is_discord_bot_enabled() || !is_allowlisted(&settings, &req.discord_user_id) {
        return Ok(Json(FriendOptResponse {
            recorded: false,
            discord_user_id: req.discord_user_id,
            opted_in: false,
        }));
    }

    let row =
        repo::friend_opt_in::set_opt_in(&state.pool, &req.discord_user_id, req.muse_account_id)
            .await?;

    Ok(Json(FriendOptResponse {
        recorded: true,
        discord_user_id: row.discord_user_id,
        opted_in: row.opted_in,
    }))
}

/// `POST /friends/opt-out` — clears consent via
/// `repo::friend_opt_in::clear_opt_in`. Unlike opt-in, opting OUT is always
/// safe to honor even for a friend who has since been removed from the
/// allowlist (there's no harm in clearing a row for someone no longer
/// served), so this route gates ONLY on the subsystem toggle, not
/// allowlist membership.
pub async fn friend_opt_out_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<FriendOptOutRequest>,
) -> MuseResult<Json<FriendOptResponse>> {
    let settings = repo::settings::load(&state.pool).await?;

    if !settings.is_discord_bot_enabled() {
        return Ok(Json(FriendOptResponse {
            recorded: false,
            discord_user_id: req.discord_user_id,
            opted_in: false,
        }));
    }

    let recorded = repo::friend_opt_in::clear_opt_in(&state.pool, &req.discord_user_id).await?;

    Ok(Json(FriendOptResponse {
        recorded: recorded.is_some(),
        discord_user_id: req.discord_user_id,
        opted_in: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{DiscordBotSettings, ExperienceSettings, TrustedFriendEntry};

    fn enabled_settings_with_friend(discord_user_id: &str) -> ExperienceSettings {
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot = DiscordBotSettings {
            enabled: true,
            trusted_friends: vec![TrustedFriendEntry {
                discord_user_id: discord_user_id.to_string(),
                display_name: "Alex".to_string(),
            }],
            ..DiscordBotSettings::default()
        };
        settings
    }

    #[test]
    fn is_allowlisted_true_for_a_listed_friend() {
        let settings = enabled_settings_with_friend("discord-alex");
        assert!(is_allowlisted(&settings, "discord-alex"));
    }

    #[test]
    fn is_allowlisted_false_for_a_stranger() {
        let settings = enabled_settings_with_friend("discord-alex");
        assert!(!is_allowlisted(&settings, "discord-total-stranger"));
    }

    #[test]
    fn friend_opt_in_request_deserializes() {
        let req: FriendOptInRequest =
            serde_json::from_str(r#"{"discord_user_id":"discord-alex","muse_account_id":7}"#)
                .expect("should deserialize");
        assert_eq!(req.discord_user_id, "discord-alex");
        assert_eq!(req.muse_account_id, 7);
    }

    #[test]
    fn friend_opt_out_request_deserializes() {
        let req: FriendOptOutRequest =
            serde_json::from_str(r#"{"discord_user_id":"discord-alex"}"#)
                .expect("should deserialize");
        assert_eq!(req.discord_user_id, "discord-alex");
    }
}

/// DB-backed handler-level coverage — same `db_gated` idiom
/// `channels::director_route`/`discord::bot` use: skips cleanly without
/// `MUSE_TEST_DATABASE_URL`, otherwise drives the REAL handlers end-to-end
/// against a live pool (settings persisted + loaded, `friend_opt_in` rows
/// actually written/read).
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::settings::{DiscordBotSettings, ExperienceSettings, TrustedFriendEntry};

    async fn test_pool_or_skip(test_name: &str) -> Option<sqlx::PgPool> {
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

    fn test_app_state(pool: sqlx::PgPool) -> Arc<AppState> {
        let config = crate::config::Config::default();
        Arc::new(AppState {
            pool,
            enrichment: crate::enrichment::EnrichmentService::from_config(&config),
            config,
            plex: None,
            prowlarr: None,
            arr_instances: Vec::new(),
            tmdb: None,
            embed: None,
            download: None,
            cast_controller: None,
        })
    }

    async fn seed_account(pool: &sqlx::PgPool) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO accounts (username, friendly_name, is_home_user, is_primary) \
             VALUES ($1, $2, false, false) RETURNING id",
        )
        .bind(format!("wire05-route-test-{}", uuid_ish()))
        .bind("WIRE-05 Route Test Account")
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

    #[tokio::test]
    async fn opt_in_route_is_inert_when_discord_bot_disabled() {
        let Some(pool) = test_pool_or_skip("opt_in_route_is_inert_when_discord_bot_disabled").await
        else {
            return;
        };
        let account_id = seed_account(&pool).await;
        let discord_user_id = format!("discord-wire05-route-disabled-{}", uuid_ish());

        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot = DiscordBotSettings {
            enabled: false,
            trusted_friends: vec![TrustedFriendEntry {
                discord_user_id: discord_user_id.clone(),
                display_name: "Alex".to_string(),
            }],
            ..DiscordBotSettings::default()
        };
        repo::settings::save(&pool, &settings)
            .await
            .expect("save disabled settings");

        let state = test_app_state(pool.clone());
        let req = FriendOptInRequest {
            discord_user_id: discord_user_id.clone(),
            muse_account_id: account_id,
        };

        let Json(response) = friend_opt_in_handler(State(state), Json(req))
            .await
            .expect("a disabled route must return Ok, never an error");

        assert!(!response.recorded);
        assert!(!response.opted_in);

        let row = repo::friend_opt_in::get(&pool, &discord_user_id)
            .await
            .expect("get");
        assert!(
            row.is_none(),
            "a disabled subsystem must never write a friend_opt_in row"
        );
    }

    #[tokio::test]
    async fn opt_in_route_is_inert_for_a_non_allowlisted_caller() {
        let Some(pool) =
            test_pool_or_skip("opt_in_route_is_inert_for_a_non_allowlisted_caller").await
        else {
            return;
        };
        let account_id = seed_account(&pool).await;
        let stranger_id = format!("discord-wire05-route-stranger-{}", uuid_ish());

        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot = DiscordBotSettings {
            enabled: true,
            trusted_friends: vec![], // stranger is NOT on the allowlist
            ..DiscordBotSettings::default()
        };
        repo::settings::save(&pool, &settings)
            .await
            .expect("save settings");

        let state = test_app_state(pool.clone());
        let req = FriendOptInRequest {
            discord_user_id: stranger_id.clone(),
            muse_account_id: account_id,
        };

        let Json(response) = friend_opt_in_handler(State(state), Json(req))
            .await
            .expect("route must return Ok, never an error");

        assert!(
            !response.recorded,
            "a non-allowlisted caller must never be recorded"
        );

        let row = repo::friend_opt_in::get(&pool, &stranger_id)
            .await
            .expect("get");
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn opt_in_then_opt_out_round_trips_through_the_real_routes() {
        let Some(pool) =
            test_pool_or_skip("opt_in_then_opt_out_round_trips_through_the_real_routes").await
        else {
            return;
        };
        let account_id = seed_account(&pool).await;
        let discord_user_id = format!("discord-wire05-route-roundtrip-{}", uuid_ish());

        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.discord_bot = DiscordBotSettings {
            enabled: true,
            trusted_friends: vec![TrustedFriendEntry {
                discord_user_id: discord_user_id.clone(),
                display_name: "Alex".to_string(),
            }],
            ..DiscordBotSettings::default()
        };
        repo::settings::save(&pool, &settings)
            .await
            .expect("save settings");

        let state = test_app_state(pool.clone());

        let Json(opt_in_response) = friend_opt_in_handler(
            State(state.clone()),
            Json(FriendOptInRequest {
                discord_user_id: discord_user_id.clone(),
                muse_account_id: account_id,
            }),
        )
        .await
        .expect("opt-in should succeed");
        assert!(opt_in_response.recorded);
        assert!(opt_in_response.opted_in);

        let persisted = repo::friend_opt_in::get(&pool, &discord_user_id)
            .await
            .expect("get")
            .expect("row should exist after opt-in");
        assert!(persisted.opted_in);
        assert_eq!(persisted.muse_account_id, Some(account_id));

        let Json(opt_out_response) = friend_opt_out_handler(
            State(state),
            Json(FriendOptOutRequest {
                discord_user_id: discord_user_id.clone(),
            }),
        )
        .await
        .expect("opt-out should succeed");
        assert!(opt_out_response.recorded);
        assert!(!opt_out_response.opted_in);

        let persisted_after = repo::friend_opt_in::get(&pool, &discord_user_id)
            .await
            .expect("get")
            .expect("row should still exist, just cleared");
        assert!(!persisted_after.opted_in);
        assert!(persisted_after.muse_account_id.is_none());
    }
}
