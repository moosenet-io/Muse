//! MUSEX-15 (Plane TERM #391), part A: scheduled premiere events — a
//! programmed watch-together announced as an EVENT (title + time + RSVP +
//! a grounded "why this pairing" rationale), not merely a lobby someone
//! happens to open. See the module doc for how this composes with
//! `crate::watch_together`.
//!
//! ## Consent, by construction (same posture as `crate::promotion`)
//! [`schedule_premiere`] builds its invite list from
//! [`TrustedFriends::opted_in_friends`] intersected with the caller's
//! requested invitee ids — a friend who is not allowlisted or not opted in
//! can never enter [`PremiereEvent`]'s `invited` set, full stop. Because
//! [`PremiereEvent::rsvp`] only ever accepts an id already in `invited`,
//! that same guarantee propagates to RSVP: there is no code path by which a
//! non-opted-in friend's RSVP is recorded, mirroring
//! `crate::promotion::targeting::promote_new_title`'s "never enters the loop
//! at all" argument for its own consent gate.
//!
//! ## Rationale: grounded, never fabricated
//! [`schedule_premiere`] gets its rationale from
//! [`crate::curation::recommend::build_rationale`] — the SAME
//! facts-grounded (optionally Chord-phrased) rationale function
//! `crate::promotion::targeting::promote_new_title` and
//! `crate::discord::bot::respond`'s `TasteAware` arm already use. This
//! module invents no second "why we picked this" text generator.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::curation::candidates::Candidate;
use crate::curation::recommend::build_rationale;
use crate::discord::client::RichEmbed;
use crate::discord::identity::TrustedFriends;
use crate::error::{MuseError, MuseResult};
use crate::models::media_metadata::MediaKind;
use crate::taste_model::chord_client::ChordClient;

/// A friend's response to a premiere invite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsvpStatus {
    Going,
    NotGoing,
    Maybe,
}

/// One scheduled premiere event: a title, a time, who's invited (opted-in
/// friends only — see the module doc), who has RSVP'd and how, and the
/// grounded rationale for this pairing.
#[derive(Debug, Clone)]
pub struct PremiereEvent {
    pub title: String,
    pub media_metadata_id: i64,
    pub kind: MediaKind,
    pub scheduled_at: DateTime<Utc>,
    /// Grounded in `candidate.facts` via `build_rationale` — never invented
    /// prose (see the module doc).
    pub rationale: String,
    /// PRIVATE by construction, same rationale as
    /// `discord::identity::FriendIdentity`'s private consent fields: the
    /// only way in is [`schedule_premiere`]'s opted-in-filtered build, so
    /// there is no public setter a caller could use to sneak a
    /// non-opted-in id into this set.
    invited: HashSet<String>,
    rsvps: HashMap<String, RsvpStatus>,
}

impl PremiereEvent {
    /// Whether `discord_user_id` was actually invited (i.e. was both
    /// allowlisted and opted-in at schedule time).
    pub fn is_invited(&self, discord_user_id: &str) -> bool {
        self.invited.contains(discord_user_id)
    }

    pub fn invited_count(&self) -> usize {
        self.invited.len()
    }

    /// Record an RSVP. Returns [`MuseError::BadRequest`] — never panics,
    /// never silently records anything — when `discord_user_id` was not
    /// invited (not allowlisted, not opted-in, or simply not on this
    /// event's guest list). This is the enforcement point the module doc's
    /// "propagates to RSVP" claim rests on.
    pub fn rsvp(&mut self, discord_user_id: &str, status: RsvpStatus) -> MuseResult<()> {
        if !self.invited.contains(discord_user_id) {
            return Err(MuseError::BadRequest(format!(
                "{discord_user_id} is not an opted-in invitee of this premiere event"
            )));
        }
        self.rsvps.insert(discord_user_id.to_string(), status);
        Ok(())
    }

    pub fn rsvp_status(&self, discord_user_id: &str) -> Option<RsvpStatus> {
        self.rsvps.get(discord_user_id).copied()
    }

    pub fn going_count(&self) -> usize {
        self.rsvps
            .values()
            .filter(|s| **s == RsvpStatus::Going)
            .count()
    }

    pub fn rsvp_count(&self) -> usize {
        self.rsvps.len()
    }
}

/// Schedule a premiere event for `candidate`, inviting whichever of
/// `invitee_discord_ids` are ALSO opted-in per `friends` — see the module
/// doc. A requested invitee who isn't allowlisted/opted-in is simply absent
/// from the resulting event, exactly like
/// `crate::promotion::targeting::promote_new_title`'s non-opted-in friends
/// are absent from its output, never an error.
pub async fn schedule_premiere(
    chord: Option<&ChordClient>,
    candidate: &Candidate,
    scheduled_at: DateTime<Utc>,
    friends: &TrustedFriends,
    invitee_discord_ids: &[&str],
) -> PremiereEvent {
    let rationale = build_rationale(chord, candidate).await;

    let invited: HashSet<String> = friends
        .opted_in_friends()
        .map(|f| f.discord_user_id.clone())
        .filter(|id| invitee_discord_ids.contains(&id.as_str()))
        .collect();

    PremiereEvent {
        title: candidate.title.clone(),
        media_metadata_id: candidate.media_metadata_id,
        kind: candidate.kind,
        scheduled_at,
        rationale,
        invited,
        rsvps: HashMap::new(),
    }
}

/// Render the announce embed: title + scheduled time + the grounded
/// rationale — the "why this pairing" surface the module doc promises.
/// Poster art follows the exact same `{base}/art/{kind}/{media_metadata_id}`
/// convention `discord::bot::build_rich_embed` uses (never a raw upstream
/// URL — see that function's own doc).
pub fn build_announce_embed(event: &PremiereEvent, public_base_url: Option<&str>) -> RichEmbed {
    let poster_url = public_base_url.map(|base| {
        let kind = match event.kind {
            MediaKind::Movie => "movie",
            MediaKind::Show => "show",
        };
        format!(
            "{}/art/{kind}/{}",
            base.trim_end_matches('/'),
            event.media_metadata_id
        )
    });
    RichEmbed {
        title: event.title.clone(),
        poster_url,
        synopsis: format!(
            "Premiering {} — {}",
            event.scheduled_at.to_rfc3339(),
            event.rationale
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curation::candidates::CandidateSource;
    use crate::discord::identity::FriendIdentity;
    use chrono::Duration as ChronoDuration;

    fn candidate() -> Candidate {
        Candidate {
            media_metadata_id: 42,
            media_item_id: Some(42),
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
            FriendIdentity::new("discord-sam", "Sam").opt_in(2),
            FriendIdentity::new("discord-not-opted-in", "Jamie"),
        ])
    }

    #[tokio::test]
    async fn schedule_premiere_invites_only_opted_in_requested_friends() {
        let friends = friends();
        let event = schedule_premiere(
            None,
            &candidate(),
            Utc::now() + ChronoDuration::days(3),
            &friends,
            &[
                "discord-alex",
                "discord-sam",
                "discord-not-opted-in",
                "discord-unknown",
            ],
        )
        .await;

        assert_eq!(event.invited_count(), 2);
        assert!(event.is_invited("discord-alex"));
        assert!(event.is_invited("discord-sam"));
        assert!(!event.is_invited("discord-not-opted-in"));
        assert!(!event.is_invited("discord-unknown"));
    }

    #[tokio::test]
    async fn rsvp_from_two_opted_in_friends_is_recorded_and_embed_carries_title_time_rationale() {
        let friends = friends();
        let scheduled_at = Utc::now() + ChronoDuration::days(3);
        let mut event = schedule_premiere(
            None,
            &candidate(),
            scheduled_at,
            &friends,
            &["discord-alex", "discord-sam"],
        )
        .await;

        event.rsvp("discord-alex", RsvpStatus::Going).unwrap();
        event.rsvp("discord-sam", RsvpStatus::Maybe).unwrap();

        assert_eq!(event.rsvp_count(), 2);
        assert_eq!(event.going_count(), 1);
        assert_eq!(event.rsvp_status("discord-alex"), Some(RsvpStatus::Going));
        assert_eq!(event.rsvp_status("discord-sam"), Some(RsvpStatus::Maybe));

        let embed = build_announce_embed(&event, Some("http://example.invalid"));
        assert_eq!(embed.title, "Severance");
        assert!(embed.synopsis.contains(&scheduled_at.to_rfc3339()));
        assert!(embed.synopsis.contains("95% match"));
        assert_eq!(
            embed.poster_url.as_deref(),
            Some("http://example.invalid/art/show/42")
        );
    }

    /// LOAD-BEARING PRIVACY NEGATIVE TEST — mirrors
    /// `crate::promotion::targeting::db_gated::non_opted_in_friend_with_a_known_good_match_gets_zero_promotions`:
    /// a non-opted-in friend's RSVP attempt has zero effect, even though
    /// they're a real, allowlisted `TrustedFriends` entry.
    #[tokio::test]
    async fn non_opted_in_friends_rsvp_attempt_is_rejected_with_zero_effect() {
        let friends = friends();
        let mut event = schedule_premiere(
            None,
            &candidate(),
            Utc::now() + ChronoDuration::days(3),
            &friends,
            &["discord-alex", "discord-not-opted-in"],
        )
        .await;

        // Sanity: the non-opted-in friend was never invited in the first
        // place.
        assert!(!event.is_invited("discord-not-opted-in"));

        let result = event.rsvp("discord-not-opted-in", RsvpStatus::Going);
        assert!(
            result.is_err(),
            "a non-opted-in friend's RSVP must be rejected"
        );
        assert_eq!(
            event.rsvp_count(),
            0,
            "a rejected RSVP must have zero effect on recorded RSVPs"
        );
        assert!(event.rsvp_status("discord-not-opted-in").is_none());
    }
}
