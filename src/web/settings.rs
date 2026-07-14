//! MUSEX-18 (Plane TERM #394): `GET`/`PUT /api/settings` — the HTTP surface
//! the Constellation GUI control + tuning panel binds to. Backed by
//! `crate::repo::settings` for persistence and `crate::settings` for the
//! document shape, masking, and confirmation-gate logic.
//!
//! ## Two boundary rules enforced HERE, not just described
//! 1. **Secrets masked on GET.** [`SettingsResponse`] never carries a raw
//!    Discord bot token — only [`crate::settings::mask_discord_token`]'s
//!    placeholder or `None`. The PUT request DTO
//!    ([`SettingsUpdateRequest`]) has no field a client could even use to
//!    SET a token through this endpoint in the first place (same "the type
//!    signature makes the mistake impossible" posture
//!    `crate::web::graph`'s module doc describes for its own privacy fix).
//! 2. **Sensitive toggles are confirmation-gated.** [`evaluate_update`] is
//!    a pure, DB-free function (unit-tested directly, no DB needed) that
//!    decides whether a requested change needs `confirm_sensitive: true` on
//!    the request body — enabling the Discord bot, or widening the sharing
//!    granularity. A request that trips this without confirmation is
//!    rejected before `repo::settings::save` is ever called.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::error::{MuseError, MuseResult};
use crate::http::AppState;
use crate::repo;
use crate::settings::{mask_discord_token, ExperienceSettings, SharingGranularity};

/// The GET response shape — identical to [`ExperienceSettings`] except the
/// Discord bot token presence is surfaced as a MASKED, display-only field
/// (`discord_bot_token_masked`) rather than the token itself, which never
/// lives in [`ExperienceSettings`] at all (see the module doc).
#[derive(Debug, Clone, Serialize)]
pub struct SettingsResponse {
    #[serde(flatten)]
    pub settings: ExperienceSettings,
    /// [`crate::settings::MASKED_SECRET_PLACEHOLDER`] when
    /// `Config::discord_bot_token` is configured, `null` otherwise. NEVER
    /// the real token, NEVER a partial/prefix/suffix of it.
    pub discord_bot_token_masked: Option<&'static str>,
}

/// The PUT request shape. Deliberately does NOT include a token field —
/// there is no way to set a secret through this endpoint (S7: secrets are
/// <secret-manager>-materialized env only, never authored via an API).
/// `confirm_sensitive` defaults to `false` when omitted, so an old/naive
/// client that doesn't know about the confirmation gate fails closed
/// (rejected) rather than silently applying a sensitive change.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SettingsUpdateRequest {
    pub settings: ExperienceSettings,
    pub confirm_sensitive: bool,
}

impl Default for SettingsUpdateRequest {
    fn default() -> Self {
        Self {
            settings: ExperienceSettings::default(),
            confirm_sensitive: false,
        }
    }
}

/// Pure decision: does moving from `current` to `requested` require
/// `confirm_sensitive`, and if the caller didn't set it, is this update
/// rejected? Two sensitive transitions, per the AC:
/// - Discord bot `enabled` flips `false -> true`.
/// - Sharing granularity WIDENS ([`SharingGranularity::widens`]).
///
/// Narrowing, disabling, or any non-sensitive field change is never gated.
/// DB-free and directly unit-tested (see `tests` below) — the handler below
/// is thin glue over this.
pub fn evaluate_update(
    current: &ExperienceSettings,
    requested: &ExperienceSettings,
    confirm_sensitive: bool,
) -> MuseResult<()> {
    let enabling_discord_bot = requested.discord_bot.enabled && !current.discord_bot.enabled;
    let widening_sharing = requested
        .sharing
        .granularity
        .widens(current.sharing.granularity);

    if (enabling_discord_bot || widening_sharing) && !confirm_sensitive {
        let mut reasons = Vec::new();
        if enabling_discord_bot {
            reasons.push("enabling the Discord bot");
        }
        if widening_sharing {
            reasons.push("widening the sharing granularity");
        }
        return Err(MuseError::BadRequest(format!(
            "this update ({}) is a sensitive change and requires confirm_sensitive: true",
            reasons.join(", ")
        )));
    }

    Ok(())
}

/// Turn a loaded/saved [`ExperienceSettings`] plus the current Discord
/// token-configured bit into the GET-safe [`SettingsResponse`].
fn to_response(settings: ExperienceSettings, discord_token_configured: bool) -> SettingsResponse {
    SettingsResponse {
        settings,
        discord_bot_token_masked: mask_discord_token(discord_token_configured),
    }
}

pub async fn get_settings_handler(
    State(state): State<Arc<AppState>>,
) -> MuseResult<Json<SettingsResponse>> {
    let settings = repo::settings::load(&state.pool).await?;
    Ok(Json(to_response(
        settings,
        state.config.discord_bot_token.is_some(),
    )))
}

pub async fn put_settings_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SettingsUpdateRequest>,
) -> MuseResult<Json<SettingsResponse>> {
    let current = repo::settings::load(&state.pool).await?;
    evaluate_update(&current, &req.settings, req.confirm_sensitive)?;
    let saved = repo::settings::save(&state.pool, &req.settings).await?;
    Ok(Json(to_response(
        saved,
        state.config.discord_bot_token.is_some(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{DiscordBotSettings, SharingSettings};

    fn base() -> ExperienceSettings {
        ExperienceSettings::default()
    }

    // --- evaluate_update: confirmation gate, DB-free -----------------------

    #[test]
    fn enabling_discord_bot_without_confirmation_is_rejected() {
        let current = base();
        let mut requested = base();
        requested.discord_bot = DiscordBotSettings {
            enabled: true,
            ..current.discord_bot.clone()
        };

        let result = evaluate_update(&current, &requested, false);
        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::BadRequest(msg) => assert!(msg.contains("Discord bot")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn enabling_discord_bot_with_confirmation_succeeds() {
        let current = base();
        let mut requested = base();
        requested.discord_bot = DiscordBotSettings {
            enabled: true,
            ..current.discord_bot.clone()
        };

        assert!(evaluate_update(&current, &requested, true).is_ok());
    }

    #[test]
    fn disabling_discord_bot_never_needs_confirmation() {
        let mut current = base();
        current.discord_bot.enabled = true;
        let mut requested = base();
        requested.discord_bot.enabled = false;

        assert!(evaluate_update(&current, &requested, false).is_ok());
    }

    #[test]
    fn widening_sharing_without_confirmation_is_rejected() {
        let current = base(); // Private
        let mut requested = base();
        requested.sharing = SharingSettings {
            granularity: SharingGranularity::Public,
        };

        let result = evaluate_update(&current, &requested, false);
        assert!(result.is_err());
        match result.unwrap_err() {
            MuseError::BadRequest(msg) => assert!(msg.contains("sharing")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn widening_sharing_with_confirmation_succeeds() {
        let current = base();
        let mut requested = base();
        requested.sharing = SharingSettings {
            granularity: SharingGranularity::Public,
        };

        assert!(evaluate_update(&current, &requested, true).is_ok());
    }

    #[test]
    fn narrowing_sharing_never_needs_confirmation() {
        let mut current = base();
        current.sharing = SharingSettings {
            granularity: SharingGranularity::Public,
        };
        let mut requested = base();
        requested.sharing = SharingSettings {
            granularity: SharingGranularity::Private,
        };

        assert!(evaluate_update(&current, &requested, false).is_ok());
    }

    #[test]
    fn a_non_sensitive_change_never_needs_confirmation() {
        let current = base();
        let mut requested = base();
        requested.channel_director.serendipity_percent = 55.0;
        requested.adaptation_loop.aggressiveness = 0.9;

        assert!(evaluate_update(&current, &requested, false).is_ok());
    }

    #[test]
    fn both_sensitive_changes_at_once_are_reported_together() {
        let current = base();
        let mut requested = base();
        requested.discord_bot.enabled = true;
        requested.sharing = SharingSettings {
            granularity: SharingGranularity::Public,
        };

        let result = evaluate_update(&current, &requested, false);
        match result.unwrap_err() {
            MuseError::BadRequest(msg) => {
                assert!(msg.contains("Discord bot"));
                assert!(msg.contains("sharing"));
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    // --- secret masking, DB-free ---------------------------------------------

    #[test]
    fn response_masks_the_token_when_configured() {
        let response = to_response(base(), true);
        assert_eq!(
            response.discord_bot_token_masked,
            Some(crate::settings::MASKED_SECRET_PLACEHOLDER)
        );
        // Sanity: the placeholder is not derived from, and does not equal,
        // any secret-shaped literal this test could confuse with a leak.
        assert!(!crate::settings::MASKED_SECRET_PLACEHOLDER.starts_with("sk-"));
    }

    #[test]
    fn response_has_no_masked_placeholder_when_unconfigured() {
        let response = to_response(base(), false);
        assert_eq!(response.discord_bot_token_masked, None);
    }

    #[test]
    fn response_serializes_with_no_raw_token_field_at_all() {
        // Belt-and-suspenders: serialize the response to JSON and confirm
        // there is no key whose name suggests a raw token, only the masked
        // one.
        let response = to_response(base(), true);
        let json = serde_json::to_value(&response).expect("serialize");
        let obj = json.as_object().expect("object");
        assert!(obj.contains_key("discord_bot_token_masked"));
        assert!(!obj.contains_key("discord_bot_token"));
        assert!(!obj.contains_key("token"));
    }
}
