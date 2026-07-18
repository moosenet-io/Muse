//! MUSEX-WIRE-03 (Plane TERM #398, slice 3): the production HTTP door onto
//! `crate::premiere::schedule` — `POST /premiere` (schedule an event, returns
//! the announce embed) and `POST /premiere/rsvp` (record an RSVP). Mirrors
//! WIRE-01 (`crate::discord::bot::run_discord_respond` /
//! `discord_respond_handler`) and WIRE-02 (`crate::conversational`)
//! EXACTLY: a settings-gated `run_premiere_*` wrapper that returns inert
//! BEFORE any premiere work, wrapping an axum handler that re-checks the
//! SAME gate before building the roster, so the ROUTE (not just the helper)
//! is inert-first.
//!
//! ## Settings toggle choice (documented, per the item's instructions)
//! `ExperienceSettings` has no toggle named literally "premiere". Of the
//! per-subsystem toggles, [`ExperienceSettings::is_watch_together_enabled`]
//! is the semantically-correct one: `crate::premiere`'s own module doc
//! states a premiere is "the SCHEDULED, ANNOUNCED flavor of the same
//! underlying idea (a group watch session for one title)" as
//! `crate::watch_together::GroupSession` — same subsystem, different
//! lifecycle. `discord_bot.enabled` was considered and rejected: it gates
//! whether the BOT SPEAKS at all (WIRE-01's flow), not whether scheduled
//! watch events exist; a premiere can be scheduled and RSVP'd via this HTTP
//! surface without the Discord bot subsystem being involved at all (the
//! announce embed is returned to the caller, not necessarily posted by the
//! bot). So gating on `watch_together` (AND the master switch, via the same
//! accessor) is correct.
//!
//! ## Honest seam (same posture as WIRE-01)
//! There is no persisted `premiere_events`/`premiere_rsvps` store yet —
//! `PremiereEvent` (`crate::premiere::schedule`) is a pure, in-memory value
//! with no repo layer, unlike `crate::premiere::discussion`'s threads/posts
//! (which DO persist via `crate::repo::premiere_discussion`). Adding that
//! store is real, separately-reviewable follow-up work (the natural next
//! WIRE slice), not done here — this item wires the entry point onto the
//! EXISTING domain logic, unchanged. Consequently `POST /premiere/rsvp`
//! must be given the same scheduling parameters `POST /premiere` was
//! called with (title/candidate facts/scheduled time/invitee list) so it
//! can deterministically rebuild the identical [`PremiereEvent`] and call
//! its real [`PremiereEvent::rsvp`] — this is NOT re-implementing the
//! consent gate, it is re-deriving the exact same pure value
//! [`schedule_premiere`] already produces from the same inputs, then
//! calling the unmodified method on it. And exactly like WIRE-01's Discord
//! roster (`ExperienceSettings::discord_bot.trusted_friends` grants
//! allowlist membership only, never opt-in), the roster available in
//! production today can never produce an opted-in invitee — so in
//! production this route reliably schedules an event with `invited_count:
//! 0` until a real per-friend opt-in persistence layer lands. The
//! `run_premiere_schedule`/`run_premiere_rsvp` tests below exercise the
//! opted-in arm directly against a constructed roster, proving the gate +
//! pipeline are correct even though production can't yet drive a real
//! friend into that state.
//!
//! ## No `{id}`/`:id` route here
//! Because there is no persisted per-event id, this slice adds no
//! `GET /premiere/{id}`-shaped route at all — sidestepping the known axum
//! 0.7 `{id}`-brace-route bug entirely (see
//! `muse_axum_brace_route_bug` memory) rather than needing the `:id`
//! workaround. Both routes added here (`POST /premiere`, `POST
//! /premiere/rsvp`) are static paths.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::curation::candidates::{Candidate, CandidateSource};
use crate::discord::client::RichEmbed;
use crate::discord::identity::{FriendIdentity, TrustedFriends};
use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::media_metadata::MediaKind;
use crate::premiere::schedule::{
    build_announce_embed, schedule_premiere, PremiereEvent, RsvpStatus,
};
use crate::settings::ExperienceSettings;
use crate::taste_model::chord_client::ChordClient;

// --- run_premiere_schedule: the settings-gated wrapper onto schedule_premiere ---

/// The real output of a successful (gate-passed) schedule call: the
/// [`PremiereEvent`] itself (so a caller/test can inspect `invited_count`,
/// `is_invited`, etc.) plus its rendered announce embed.
pub struct PremiereScheduleOutcome {
    pub event: PremiereEvent,
    pub embed: RichEmbed,
}

/// MUSEX-WIRE-03: the settings-gated, PRODUCTION-WIRED entry point onto
/// [`schedule_premiere`]. Mirrors `run_discord_respond`'s inert-when-off
/// contract: gated on [`ExperienceSettings::is_watch_together_enabled`]
/// BEFORE `schedule_premiere` (and therefore the rationale/embed/invite
/// work it does) runs at all.
///
/// Does not duplicate or weaken [`schedule_premiere`]'s own consent gate
/// (opted-in-friends-only invites) — this wraps it with the SECOND,
/// independent gate the experience-layer settings panel requires: a friend
/// can be allowlisted+opted-in and a title can be perfectly schedulable,
/// and still nothing happens if the operator has switched the
/// watch-together subsystem (or the master switch) off.
pub async fn run_premiere_schedule(
    settings: &ExperienceSettings,
    friends: &TrustedFriends,
    chord: Option<&ChordClient>,
    candidate: &Candidate,
    scheduled_at: DateTime<Utc>,
    invitee_discord_ids: &[&str],
    public_base_url: Option<&str>,
) -> Option<PremiereScheduleOutcome> {
    if !settings.is_watch_together_enabled() {
        return None;
    }
    let event =
        schedule_premiere(chord, candidate, scheduled_at, friends, invitee_discord_ids).await;
    let embed = build_announce_embed(&event, public_base_url);
    Some(PremiereScheduleOutcome { event, embed })
}

/// MUSEX-WIRE-03: the settings-gated, PRODUCTION-WIRED entry point onto
/// [`PremiereEvent::rsvp`]. Same inert-when-off contract as
/// [`run_premiere_schedule`] — gated BEFORE the event is even rebuilt (so a
/// disabled subsystem does no `schedule_premiere`/rationale work either,
/// not just no RSVP write).
///
/// Consent enforcement is entirely [`PremiereEvent::rsvp`]'s own, UNCHANGED
/// logic — see the module doc's "Honest seam" for why this rebuilds the
/// event from the same scheduling parameters rather than reading persisted
/// state. Returns `Ok(None)` when the subsystem is off, `Ok(Some(status))`
/// on a recorded RSVP, and propagates [`PremiereEvent::rsvp`]'s
/// `Err(MuseError::BadRequest(_))` verbatim for a non-invited (not
/// allowlisted / not opted-in / not on this event's guest list) caller —
/// i.e. rejected with zero effect, never silently recorded.
pub async fn run_premiere_rsvp(
    settings: &ExperienceSettings,
    friends: &TrustedFriends,
    chord: Option<&ChordClient>,
    candidate: &Candidate,
    scheduled_at: DateTime<Utc>,
    invitee_discord_ids: &[&str],
    discord_user_id: &str,
    status: RsvpStatus,
) -> MuseResult<Option<RsvpStatus>> {
    if !settings.is_watch_together_enabled() {
        return Ok(None);
    }
    let mut event =
        schedule_premiere(chord, candidate, scheduled_at, friends, invitee_discord_ids).await;
    event.rsvp(discord_user_id, status)?;
    Ok(Some(status))
}

// --- HTTP DTOs ---------------------------------------------------------------

/// Wire shape for [`RsvpStatus`] — that type deliberately carries no serde
/// derives (it's a pure domain enum), so this is the HTTP-facing mirror,
/// same pattern `crate::settings::QuestionFrequency` uses for a production
/// type with no `serde` derive of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RsvpStatusDto {
    Going,
    NotGoing,
    Maybe,
}

impl From<RsvpStatusDto> for RsvpStatus {
    fn from(dto: RsvpStatusDto) -> Self {
        match dto {
            RsvpStatusDto::Going => RsvpStatus::Going,
            RsvpStatusDto::NotGoing => RsvpStatus::NotGoing,
            RsvpStatusDto::Maybe => RsvpStatus::Maybe,
        }
    }
}

impl From<RsvpStatus> for RsvpStatusDto {
    fn from(status: RsvpStatus) -> Self {
        match status {
            RsvpStatus::Going => RsvpStatusDto::Going,
            RsvpStatus::NotGoing => RsvpStatusDto::NotGoing,
            RsvpStatus::Maybe => RsvpStatusDto::Maybe,
        }
    }
}

/// `POST /premiere` request body. Carries the minimal
/// [`crate::curation::candidates::Candidate`] fields
/// [`crate::curation::recommend::build_rationale`] actually grounds its
/// rationale in (`title`/`facts`), plus the scheduling fields
/// [`schedule_premiere`] needs. `source` is not caller-supplied: every
/// premiere scheduled through this route is a deliberate, operator/friend-
/// initiated pick rather than an output of one of the four MUSE-11 ranking
/// sources, so it is hardcoded to [`CandidateSource::Taste`] (the same
/// source `crate::premiere::schedule`'s own tests use).
#[derive(Debug, Deserialize)]
pub struct PremiereScheduleRequest {
    pub media_metadata_id: i64,
    pub title: String,
    pub kind: MediaKind,
    pub year: Option<i32>,
    #[serde(default)]
    pub facts: Vec<String>,
    #[serde(default)]
    pub taste_fit: f64,
    pub scheduled_at: DateTime<Utc>,
    pub invitee_discord_ids: Vec<String>,
}

impl PremiereScheduleRequest {
    fn to_candidate(&self) -> Candidate {
        Candidate {
            media_metadata_id: self.media_metadata_id,
            media_item_id: None,
            title: self.title.clone(),
            year: self.year,
            kind: self.kind,
            source: CandidateSource::Taste,
            taste_fit: self.taste_fit,
            facts: self.facts.clone(),
            availability: None,
        }
    }
}

/// `POST /premiere` response — inert (`scheduled: false`, every other field
/// `None`) when the subsystem is off, mirroring
/// `crate::discord::bot::DiscordRespondResponse`'s all-`None` inert shape.
#[derive(Debug, Serialize)]
pub struct PremiereScheduleResponse {
    pub scheduled: bool,
    pub title: Option<String>,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub rationale: Option<String>,
    pub embed_title: Option<String>,
    pub embed_poster_url: Option<String>,
    pub embed_synopsis: Option<String>,
    pub invited_count: Option<usize>,
}

fn to_schedule_response(outcome: Option<PremiereScheduleOutcome>) -> PremiereScheduleResponse {
    match outcome {
        None => PremiereScheduleResponse {
            scheduled: false,
            title: None,
            scheduled_at: None,
            rationale: None,
            embed_title: None,
            embed_poster_url: None,
            embed_synopsis: None,
            invited_count: None,
        },
        Some(outcome) => PremiereScheduleResponse {
            scheduled: true,
            title: Some(outcome.event.title.clone()),
            scheduled_at: Some(outcome.event.scheduled_at),
            rationale: Some(outcome.event.rationale.clone()),
            embed_title: Some(outcome.embed.title),
            embed_poster_url: outcome.embed.poster_url,
            embed_synopsis: Some(outcome.embed.synopsis),
            invited_count: Some(outcome.event.invited_count()),
        },
    }
}

/// `POST /premiere` — the production HTTP door onto [`run_premiere_schedule`].
/// Inert-first ordering (identical to
/// `crate::discord::bot::discord_respond_handler`): the settings load is the
/// one unavoidable pool read (the toggle is the persisted source of truth),
/// and the gate is re-checked immediately after that load — BEFORE the
/// roster is built or [`schedule_premiere`] is called — so the ROUTE, not
/// just the helper, is inert-first.
pub async fn premiere_schedule_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PremiereScheduleRequest>,
) -> MuseResult<Json<PremiereScheduleResponse>> {
    let settings = crate::repo::settings::load(&state.pool).await?;

    if !settings.is_watch_together_enabled() {
        return Ok(Json(to_schedule_response(None)));
    }

    // Enabled path only. MUSEX-WIRE-05/06 keystone: resolve PERSISTED
    // per-friend opt-in state (not just allowlist membership) so a
    // genuinely opted-in friend reaches the personalized/invited path —
    // see `crate::discord::roster::resolve_trusted_friends`'s own module
    // doc, which names this handler as one of its intended callers.
    let friends = crate::discord::roster::resolve_trusted_friends(&state.pool, &settings).await?;

    let chord = ChordClient::from_config(&state.config);
    let candidate = req.to_candidate();
    let invitee_ids: Vec<&str> = req.invitee_discord_ids.iter().map(String::as_str).collect();

    let outcome = run_premiere_schedule(
        &settings,
        &friends,
        chord.as_ref(),
        &candidate,
        req.scheduled_at,
        &invitee_ids,
        state.config.public_base_url.as_deref(),
    )
    .await;

    Ok(Json(to_schedule_response(outcome)))
}

/// `POST /premiere/rsvp` request body. See the module doc's "Honest seam"
/// for why this repeats the original schedule parameters rather than
/// referencing a persisted event id — there is no persisted event yet, so
/// this deterministically rebuilds the identical [`PremiereEvent`]
/// [`schedule_premiere`] would have produced for the original `POST
/// /premiere` call, then RSVPs against it.
#[derive(Debug, Deserialize)]
pub struct PremiereRsvpRequest {
    pub media_metadata_id: i64,
    pub title: String,
    pub kind: MediaKind,
    pub year: Option<i32>,
    #[serde(default)]
    pub facts: Vec<String>,
    #[serde(default)]
    pub taste_fit: f64,
    pub scheduled_at: DateTime<Utc>,
    pub invitee_discord_ids: Vec<String>,
    pub discord_user_id: String,
    pub status: RsvpStatusDto,
}

impl PremiereRsvpRequest {
    fn to_candidate(&self) -> Candidate {
        Candidate {
            media_metadata_id: self.media_metadata_id,
            media_item_id: None,
            title: self.title.clone(),
            year: self.year,
            kind: self.kind,
            source: CandidateSource::Taste,
            taste_fit: self.taste_fit,
            facts: self.facts.clone(),
            availability: None,
        }
    }
}

/// `POST /premiere/rsvp` response — `recorded: false` when the subsystem is
/// off (inert, mirrors the schedule response's shape). A rejected RSVP
/// (not invited) does NOT reach this DTO at all — it short-circuits as a
/// `400 Bad Request` via [`crate::error::MuseError::BadRequest`], the same
/// way `crate::premiere::discussion::post_message`'s consent rejection
/// surfaces to its callers.
#[derive(Debug, Serialize)]
pub struct PremiereRsvpResponse {
    pub recorded: bool,
    pub status: Option<RsvpStatusDto>,
}

fn to_rsvp_response(outcome: Option<RsvpStatus>) -> PremiereRsvpResponse {
    match outcome {
        None => PremiereRsvpResponse {
            recorded: false,
            status: None,
        },
        Some(status) => PremiereRsvpResponse {
            recorded: true,
            status: Some(status.into()),
        },
    }
}

/// `POST /premiere/rsvp` — the production HTTP door onto
/// [`run_premiere_rsvp`]. Same inert-first ordering as
/// [`premiere_schedule_handler`]: settings loaded once, gate checked
/// immediately, BEFORE the roster is built or the event is rebuilt.
pub async fn premiere_rsvp_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PremiereRsvpRequest>,
) -> MuseResult<Json<PremiereRsvpResponse>> {
    let settings = crate::repo::settings::load(&state.pool).await?;

    if !settings.is_watch_together_enabled() {
        return Ok(Json(to_rsvp_response(None)));
    }

    // Same keystone swap as `premiere_schedule_handler` above — see there
    // for the rationale.
    let friends = crate::discord::roster::resolve_trusted_friends(&state.pool, &settings).await?;

    let chord = ChordClient::from_config(&state.config);
    let candidate = req.to_candidate();
    let invitee_ids: Vec<&str> = req.invitee_discord_ids.iter().map(String::as_str).collect();
    let status: RsvpStatus = req.status.into();

    let outcome = run_premiere_rsvp(
        &settings,
        &friends,
        chord.as_ref(),
        &candidate,
        req.scheduled_at,
        &invitee_ids,
        &req.discord_user_id,
        status,
    )
    .await?;

    Ok(Json(to_rsvp_response(outcome)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SubsystemToggle;
    use chrono::Duration as ChronoDuration;

    fn candidate() -> Candidate {
        Candidate {
            media_metadata_id: 42,
            media_item_id: None,
            title: "Severance".to_string(),
            year: Some(2022),
            kind: MediaKind::Show,
            source: CandidateSource::Taste,
            taste_fit: 0.9,
            facts: vec!["it's a 95% match to your taste profile".to_string()],
            availability: None,
        }
    }

    fn friends() -> TrustedFriends {
        TrustedFriends::from_friends([
            FriendIdentity::new("discord-alex", "Alex").opt_in(1),
            FriendIdentity::new("discord-not-opted-in", "Jamie"),
        ])
    }

    fn enabled_settings() -> ExperienceSettings {
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.watch_together = SubsystemToggle { enabled: true };
        settings
    }

    fn disabled_settings_master_off() -> ExperienceSettings {
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = false;
        settings.watch_together = SubsystemToggle { enabled: true };
        settings
    }

    fn disabled_settings_subsystem_off() -> ExperienceSettings {
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.watch_together = SubsystemToggle { enabled: false };
        settings
    }

    // --- run_premiere_schedule: inert-first ---------------------------------

    #[tokio::test]
    async fn run_premiere_schedule_is_inert_when_watch_together_disabled() {
        let settings = disabled_settings_subsystem_off();
        let friends = friends();

        let outcome = run_premiere_schedule(
            &settings,
            &friends,
            None,
            &candidate(),
            Utc::now() + ChronoDuration::days(3),
            &["discord-alex"],
            None,
        )
        .await;

        assert!(
            outcome.is_none(),
            "a disabled watch_together subsystem must schedule nothing"
        );
    }

    #[tokio::test]
    async fn run_premiere_schedule_is_inert_when_master_switch_off() {
        let settings = disabled_settings_master_off();
        let friends = friends();

        let outcome = run_premiere_schedule(
            &settings,
            &friends,
            None,
            &candidate(),
            Utc::now() + ChronoDuration::days(3),
            &["discord-alex"],
            None,
        )
        .await;

        assert!(
            outcome.is_none(),
            "the master switch alone must be enough to make this inert"
        );
    }

    /// Mirror-image sanity check (same idiom `run_discord_respond`'s own
    /// tests use): WITH the gate enabled and a genuinely opted-in invitee,
    /// this must actually run the pipeline and produce a real, grounded
    /// outcome — proving the two inert tests above assert something real,
    /// not a vacuously-always-None function.
    #[tokio::test]
    async fn run_premiere_schedule_runs_the_real_pipeline_when_enabled() {
        let settings = enabled_settings();
        let friends = friends();
        let scheduled_at = Utc::now() + ChronoDuration::days(3);

        let outcome = run_premiere_schedule(
            &settings,
            &friends,
            None,
            &candidate(),
            scheduled_at,
            &["discord-alex", "discord-not-opted-in"],
            Some("http://example.invalid"),
        )
        .await
        .expect("enabled subsystem must schedule the event");

        assert_eq!(outcome.event.title, "Severance");
        assert_eq!(outcome.event.scheduled_at, scheduled_at);
        assert_eq!(
            outcome.event.invited_count(),
            1,
            "only the opted-in requested invitee is invited"
        );
        assert!(outcome.event.is_invited("discord-alex"));
        assert!(!outcome.event.is_invited("discord-not-opted-in"));

        // Grounded rationale, not fabricated: the module doc's promise —
        // build_rationale (no Chord client here) falls back to the
        // templated rationale, which is built from `candidate.facts`.
        assert!(outcome.event.rationale.contains("95% match"));
        assert_eq!(outcome.embed.title, "Severance");
        assert!(outcome.embed.synopsis.contains("95% match"));
    }

    // --- run_premiere_rsvp: inert-first + consent ---------------------------

    #[tokio::test]
    async fn run_premiere_rsvp_is_inert_when_watch_together_disabled() {
        let settings = disabled_settings_subsystem_off();
        let friends = friends();

        let result = run_premiere_rsvp(
            &settings,
            &friends,
            None,
            &candidate(),
            Utc::now() + ChronoDuration::days(3),
            &["discord-alex"],
            "discord-alex",
            RsvpStatus::Going,
        )
        .await;

        assert!(result.is_ok(), "a disabled subsystem must never error");
        assert!(
            result.unwrap().is_none(),
            "a disabled subsystem must record no RSVP"
        );
    }

    #[tokio::test]
    async fn run_premiere_rsvp_is_inert_when_master_switch_off() {
        let settings = disabled_settings_master_off();
        let friends = friends();

        let result = run_premiere_rsvp(
            &settings,
            &friends,
            None,
            &candidate(),
            Utc::now() + ChronoDuration::days(3),
            &["discord-alex"],
            "discord-alex",
            RsvpStatus::Going,
        )
        .await;

        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn run_premiere_rsvp_records_an_opted_in_invitees_rsvp_when_enabled() {
        let settings = enabled_settings();
        let friends = friends();

        let result = run_premiere_rsvp(
            &settings,
            &friends,
            None,
            &candidate(),
            Utc::now() + ChronoDuration::days(3),
            &["discord-alex"],
            "discord-alex",
            RsvpStatus::Going,
        )
        .await
        .expect("an opted-in invitee's RSVP must not error");

        assert_eq!(result, Some(RsvpStatus::Going));
    }

    /// LOAD-BEARING PRIVACY NEGATIVE TEST — mirrors
    /// `crate::premiere::schedule::tests::non_opted_in_friends_rsvp_attempt_is_rejected_with_zero_effect`:
    /// a non-opted-in (but allowlisted) friend's RSVP attempt is rejected
    /// with zero effect, even with the subsystem fully enabled.
    #[tokio::test]
    async fn run_premiere_rsvp_rejects_a_non_opted_in_invitee_even_when_enabled() {
        let settings = enabled_settings();
        let friends = friends();

        let result = run_premiere_rsvp(
            &settings,
            &friends,
            None,
            &candidate(),
            Utc::now() + ChronoDuration::days(3),
            &["discord-alex", "discord-not-opted-in"],
            "discord-not-opted-in",
            RsvpStatus::Going,
        )
        .await;

        assert!(
            result.is_err(),
            "a non-opted-in friend's RSVP must be rejected, got {result:?}"
        );
    }

    #[tokio::test]
    async fn run_premiere_rsvp_rejects_an_unknown_caller_even_when_enabled() {
        let settings = enabled_settings();
        let friends = friends();

        let result = run_premiere_rsvp(
            &settings,
            &friends,
            None,
            &candidate(),
            Utc::now() + ChronoDuration::days(3),
            &["discord-alex"],
            "discord-total-stranger",
            RsvpStatus::Going,
        )
        .await;

        assert!(
            result.is_err(),
            "a caller who was never invited must be rejected, got {result:?}"
        );
    }

    // --- DTO shape / conversion ----------------------------------------------

    #[test]
    fn to_schedule_response_of_none_is_all_inert_fields() {
        let response = to_schedule_response(None);
        assert!(!response.scheduled);
        assert!(response.title.is_none());
        assert!(response.invited_count.is_none());
    }

    #[test]
    fn to_rsvp_response_of_none_is_inert() {
        let response = to_rsvp_response(None);
        assert!(!response.recorded);
        assert!(response.status.is_none());
    }

    #[test]
    fn to_rsvp_response_of_some_carries_the_status() {
        let response = to_rsvp_response(Some(RsvpStatus::Maybe));
        assert!(response.recorded);
        assert_eq!(response.status, Some(RsvpStatusDto::Maybe));
    }

    #[test]
    fn rsvp_status_dto_round_trips_through_the_domain_type() {
        for status in [RsvpStatus::Going, RsvpStatus::NotGoing, RsvpStatus::Maybe] {
            let dto: RsvpStatusDto = status.into();
            let back: RsvpStatus = dto.into();
            assert_eq!(back, status);
        }
    }
}

/// DB-backed handler-level coverage: drives the REAL `premiere_schedule_handler`/
/// `premiere_rsvp_handler` async fns end-to-end (settings persisted + loaded
/// from a live pool), the same shape
/// `crate::discord::bot`'s `db_gated` handler tests use — proving the ROUTE
/// (not just the `run_premiere_*` helpers) is inert-first when disabled.
/// `db_gated` because the handler's settings load genuinely needs a live
/// pool (the toggle is the DB-persisted source of truth); skips cleanly,
/// never a hard failure, when `MUSE_TEST_DATABASE_URL` isn't set.
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::settings::{DiscordBotSettings, SubsystemToggle, TrustedFriendEntry};

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

    /// Minimal real `accounts` row — enough to satisfy
    /// `friend_opt_in.muse_account_id`'s `REFERENCES accounts(id)` FK (see
    /// `migrations/0103_friend_opt_in.sql`'s doc). Unlike
    /// `discord::bot`'s `seed_on_deck_account`, this test doesn't need
    /// on-deck taste data — only a valid account id for `set_opt_in`.
    async fn seed_account(pool: &sqlx::PgPool) -> i64 {
        use crate::models::account::NewAccount;
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let account = crate::repo::account::create(
            pool,
            &NewAccount {
                plex_account_id: Some(format!("plex-{suffix}")),
                username: Some(format!("user-{suffix}")),
                friendly_name: Some("WIRE06 Premiere Proof".to_string()),
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account");
        account.id
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
        })
    }

    fn schedule_request(suffix: &str) -> PremiereScheduleRequest {
        PremiereScheduleRequest {
            media_metadata_id: 42,
            title: format!("MUSEXWIRE03-{suffix}"),
            kind: MediaKind::Show,
            year: Some(2022),
            facts: vec!["it's a 95% match to your taste profile".to_string()],
            taste_fit: 0.9,
            scheduled_at: Utc::now() + chrono::Duration::days(3),
            invitee_discord_ids: vec!["discord-1".to_string()],
        }
    }

    #[tokio::test]
    async fn premiere_schedule_handler_route_is_inert_when_subsystem_disabled() {
        let Some(pool) =
            test_pool_or_skip("premiere_schedule_handler_route_is_inert_when_subsystem_disabled")
                .await
        else {
            return;
        };

        // Persist a DISABLED settings doc that STILL carries a non-empty
        // roster: if the gate did not precede the roster/pipeline work, the
        // handler would resolve "discord-1" and proceed — the inert
        // response below proves the route returns BEFORE any of that.
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.watch_together = SubsystemToggle { enabled: false };
        settings.discord_bot = DiscordBotSettings {
            trusted_friends: vec![TrustedFriendEntry {
                discord_user_id: "discord-1".to_string(),
                display_name: "Alex".to_string(),
            }],
            ..settings.discord_bot
        };
        crate::repo::settings::save(&pool, &settings)
            .await
            .expect("save disabled settings");

        let state = test_app_state(pool);
        let req = schedule_request("disabled");

        let Json(response) = premiere_schedule_handler(State(state), Json(req))
            .await
            .expect("a disabled route must return an inert Ok, never an error");

        assert!(!response.scheduled, "a disabled route must not schedule");
        assert!(response.title.is_none());
        assert!(response.invited_count.is_none());
    }

    #[tokio::test]
    async fn premiere_schedule_handler_route_runs_when_enabled() {
        let Some(pool) =
            test_pool_or_skip("premiere_schedule_handler_route_runs_when_enabled").await
        else {
            return;
        };

        // ENABLED subsystem with an allowlisted roster entry that has NO
        // persisted `friend_opt_in` row (MUSEX-WIRE-05/06 keystone):
        // allowlist membership alone still never grants opt-in — see
        // `crate::discord::roster::resolve_trusted_friends`'s doc — so
        // `invited_count` is 0 here. What this proves is that the GATE
        // passed and the real pipeline ran (rationale/embed populated from
        // the real candidate), not that a friend was invited. Contrast
        // with `premiere_schedule_handler_route_invites_a_persisted_opted_in_friend`
        // below, which is identical except for one
        // `repo::friend_opt_in::set_opt_in` write and reaches
        // `invited_count == 1` instead.
        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.watch_together = SubsystemToggle { enabled: true };
        crate::repo::settings::save(&pool, &settings)
            .await
            .expect("save enabled settings");

        let state = test_app_state(pool);
        let req = schedule_request("enabled");

        let Json(response) = premiere_schedule_handler(State(state), Json(req))
            .await
            .expect("an enabled route must not error");

        assert!(response.scheduled, "an enabled route must schedule");
        assert!(response
            .title
            .as_deref()
            .unwrap()
            .starts_with("MUSEXWIRE03-"));
        assert_eq!(response.invited_count, Some(0));
        assert!(response.rationale.unwrap().contains("95% match"));
    }

    /// MUSEX-WIRE-06 follow-up proof (the swap this recovery item adds):
    /// same setup as `_route_runs_when_enabled` above, plus exactly one
    /// extra write — `repo::friend_opt_in::set_opt_in`, the same
    /// persistence `POST /friends/opt-in` performs — and the REAL
    /// production route now invites the friend (`invited_count == 1`)
    /// instead of 0. Proves `premiere_schedule_handler` reads
    /// `resolve_trusted_friends`'s PERSISTED opt-in state, not just the
    /// allowlist.
    #[tokio::test]
    async fn premiere_schedule_handler_route_invites_a_persisted_opted_in_friend() {
        let Some(pool) = test_pool_or_skip(
            "premiere_schedule_handler_route_invites_a_persisted_opted_in_friend",
        )
        .await
        else {
            return;
        };

        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.watch_together = SubsystemToggle { enabled: true };
        settings.discord_bot = DiscordBotSettings {
            trusted_friends: vec![TrustedFriendEntry {
                discord_user_id: "discord-wire06-opted-in".to_string(),
                display_name: "Alex".to_string(),
            }],
            ..settings.discord_bot
        };
        crate::repo::settings::save(&pool, &settings)
            .await
            .expect("save enabled settings");

        // The ONE extra step: persist real consent via the sanctioned repo
        // write (the same one `POST /friends/opt-in` performs) — never a
        // raw `FriendIdentity` field write. `muse_account_id` is a real FK
        // into `accounts` (see `migrations/0103_friend_opt_in.sql`), so a
        // genuine account is seeded first.
        let account_id = seed_account(&pool).await;
        crate::repo::friend_opt_in::set_opt_in(&pool, "discord-wire06-opted-in", account_id)
            .await
            .expect("set_opt_in should succeed");

        let state = test_app_state(pool);
        let mut req = schedule_request("wire06-opted-in");
        req.invitee_discord_ids = vec!["discord-wire06-opted-in".to_string()];

        let Json(response) = premiere_schedule_handler(State(state), Json(req))
            .await
            .expect("the enabled, persisted-opted-in path must not error");

        assert!(response.scheduled);
        assert_eq!(
            response.invited_count,
            Some(1),
            "a persisted-opted-in friend's REAL route must be invited through \
             resolve_trusted_friends, got: {response:?}"
        );
    }

    #[tokio::test]
    async fn premiere_rsvp_handler_route_is_inert_when_subsystem_disabled() {
        let Some(pool) =
            test_pool_or_skip("premiere_rsvp_handler_route_is_inert_when_subsystem_disabled").await
        else {
            return;
        };

        let mut settings = ExperienceSettings::default();
        settings.master_enabled = true;
        settings.watch_together = SubsystemToggle { enabled: false };
        crate::repo::settings::save(&pool, &settings)
            .await
            .expect("save disabled settings");

        let state = test_app_state(pool);
        let req = PremiereRsvpRequest {
            media_metadata_id: 42,
            title: "MUSEXWIRE03-rsvp-disabled".to_string(),
            kind: MediaKind::Show,
            year: Some(2022),
            facts: vec!["it's a 95% match to your taste profile".to_string()],
            taste_fit: 0.9,
            scheduled_at: Utc::now() + chrono::Duration::days(3),
            invitee_discord_ids: vec!["discord-1".to_string()],
            discord_user_id: "discord-1".to_string(),
            status: RsvpStatusDto::Going,
        };

        let Json(response) = premiere_rsvp_handler(State(state), Json(req))
            .await
            .expect("a disabled route must return an inert Ok, never an error");

        assert!(!response.recorded, "a disabled route must record no RSVP");
        assert!(response.status.is_none());
    }
}
