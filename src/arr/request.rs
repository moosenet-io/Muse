//! MUSEX-14 (Plane TERM #390), part B's request-domain boundary: tiered
//! safety for a conversational "please get this" ask, routed only for
//! titles [`crate::conversational`] has already determined are genuinely
//! missing (not owned/available) — see that module's doc for the
//! library-first reasoning this sits downstream of.
//!
//! ## An honest note on scope
//! At the time MUSEX-14 was built, this crate had **no prior media-request/
//! acquisition domain at all** — [`super::client::ArrClient`] is
//! deliberately read-only (`GET` only: `movies`/`series`/`episodes`/
//! `episode_files`), and this module's own parent doc is explicit: "**Never
//! write to *arr** (Phase 0 is acquisition-read-only per the S96 founding
//! spec §1)." So rather than assume a "tiered safety" system that doesn't
//! exist, this module builds the minimum real one: a pure, testable
//! CLASSIFICATION ([`classify_tier`]) of how safe a given missing-title
//! request is to act on, plus a [`MediaRequestSink`] seam (mirrors
//! `crate::discord::client::DiscordClient`'s trait-plus-mock shape) that
//! only [`RequestTier::AutoApprovable`] requests are ever handed to. There
//! is **no live Radarr/Sonarr-writing `MediaRequestSink` implementation
//! shipped in this item** — only [`MockMediaRequestSink`] for tests. Wiring
//! an actual write-capable sink (a real `POST /api/v3/movie` /
//! `POST /api/v3/series` call) is a distinct, separately-reviewable item:
//! it would be the first place this crate ever writes to *arr, and that
//! deserves its own scrutiny rather than riding in as a side effect of a
//! conversational-UX feature.
//!
//! ## The tiers
//! [`classify_tier`] never needs I/O of its own — every signal it consults
//! (`auto_tier_enabled`, a checked [`crate::models::availability::Availability`],
//! whether a matching *arr instance exists for the title's
//! [`crate::models::media_metadata::MediaKind`]) is passed in by the caller,
//! which is what makes it a plain, exhaustively unit-testable function
//! rather than an integration test:
//! - [`RequestTier::Blocked`] — no configured *arr instance can even fulfill
//!   this kind of title (no Sonarr instance for a `Show`, no Radarr instance
//!   for a `Movie`). Structural, not a policy choice — nothing downstream
//!   could act on this regardless of `auto_tier_enabled`.
//! - [`RequestTier::NeedsReview`] — the safe default. Fires whenever
//!   `auto_tier_enabled` is `false` ([`crate::config::Config::arr_request_auto_tier_enabled`]'s
//!   own default), OR MUSE-16 availability was never checked/found nothing
//!   grabbable. A human looks at it before anything is ever submitted.
//! - [`RequestTier::AutoApprovable`] — ONLY when the operator has explicitly
//!   opted in AND a real, checked [`crate::models::availability::Availability`]
//!   confirms the title is grabbable right now
//!   (`release_count > 0` — the exact same signal
//!   `crate::curation::candidates::gather_available_now_candidates` already
//!   grounds its "grabbable now" fact in, never a guess).

use crate::error::MuseResult;
use crate::models::availability::Availability;
use crate::models::media_metadata::MediaKind;

/// How safe [`classify_tier`] judges a missing-title request to be. See the
/// module doc for the full contract of each variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestTier {
    AutoApprovable,
    NeedsReview,
    Blocked,
}

/// THE tiered-safety gate. Pure and total — every input is a value the
/// caller already computed (no I/O here), so this is exhaustively
/// unit-testable. See the module doc for the exact contract of each branch.
pub fn classify_tier(
    auto_tier_enabled: bool,
    availability: Option<&Availability>,
    has_matching_arr_instance: bool,
) -> RequestTier {
    if !has_matching_arr_instance {
        return RequestTier::Blocked;
    }
    if !auto_tier_enabled {
        return RequestTier::NeedsReview;
    }
    match availability {
        Some(a) if a.release_count > 0 => RequestTier::AutoApprovable,
        _ => RequestTier::NeedsReview,
    }
}

/// One missing title [`crate::conversational`] wants requested — the input
/// to [`MediaRequestSink::submit`]. Deliberately carries enough to file a
/// real request (a future write-capable sink would need the tmdb id + kind
/// to pick the right *arr instance) without carrying anything taste/
/// account-shaped — a request is about a TITLE, not about who asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRequestDraft {
    pub tmdb_id: String,
    pub title: String,
    pub kind: MediaKind,
}

/// The result of handing a [`MediaRequestDraft`] to a [`MediaRequestSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRequestOutcome {
    pub tier: RequestTier,
    /// `true` only when [`MediaRequestSink::submit`] was actually called
    /// (i.e. `tier == RequestTier::AutoApprovable`) and it returned `Ok`.
    /// `NeedsReview`/`Blocked` always leave this `false` — see the module
    /// doc: the sink is never even called for those tiers, so there is
    /// nothing to bypass.
    pub submitted: bool,
    /// Human-readable audit trail: why this tier, why (not) submitted.
    pub reason: String,
}

/// The request-submission seam — mirrors
/// `crate::discord::client::DiscordClient`'s trait-plus-mock shape. See the
/// module doc: no implementation of this trait in the current build ever
/// performs a live *arr write.
#[async_trait::async_trait]
pub trait MediaRequestSink: Send + Sync {
    async fn submit(&self, draft: &MediaRequestDraft) -> MuseResult<()>;
}

/// A deterministic, network-free [`MediaRequestSink`] for tests. Records
/// every draft it receives — the seam
/// `crate::conversational`'s tests inspect to prove a request was (or, for
/// `NeedsReview`/`Blocked`, was NOT) submitted.
#[derive(Debug, Default)]
pub struct MockMediaRequestSink {
    pub submitted: std::sync::Mutex<Vec<MediaRequestDraft>>,
}

impl MockMediaRequestSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn submitted_count(&self) -> usize {
        self.submitted.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl MediaRequestSink for MockMediaRequestSink {
    async fn submit(&self, draft: &MediaRequestDraft) -> MuseResult<()> {
        self.submitted.lock().unwrap().push(draft.clone());
        Ok(())
    }
}

/// MUSEX-WIRE-02 (Plane TERM #398, slice 2): the production placeholder
/// [`MediaRequestSink`] wired to `POST /conversational`
/// (`crate::conversational::conversational_handler`). This is deliberately
/// NOT a live Radarr/Sonarr-writing sink — per this module's own doc, no
/// such sink has shipped yet, and inventing an acknowledgment for a
/// request that never actually reaches *arr would be dishonest. It is safe
/// to wire as a real production dependency today because
/// [`classify_tier`]'s current callers ([`crate::conversational`],
/// `crate::premiere::engagement`) never pass a confirmed
/// [`crate::models::availability::Availability`] into
/// [`submit_if_appropriate`] — so `submit` is structurally UNREACHABLE from
/// production right now (see [`classify_tier`]'s doc: "never
/// `AutoApprovable` in practice"). If that ever changes (a real
/// availability check gets wired in), `submit` returning `Ok(())` with no
/// side effect would start silently lying about a request having been
/// filed — so this type's own doc is the trip wire: a future PR that wires
/// live availability into the conversational/premiere paths MUST replace
/// this with (or gate it behind) a real write-capable sink first.
#[derive(Debug, Default)]
pub struct NoopMediaRequestSink;

#[async_trait::async_trait]
impl MediaRequestSink for NoopMediaRequestSink {
    async fn submit(&self, _draft: &MediaRequestDraft) -> MuseResult<()> {
        Ok(())
    }
}

/// Classify `draft`, and — ONLY when [`classify_tier`] returns
/// [`RequestTier::AutoApprovable`] — hand it to `sink`. `NeedsReview` and
/// `Blocked` never touch `sink` at all (not "the sink chooses not to
/// submit" — it is never invoked), which is what makes "tiered safety
/// intact" a property of the call graph, not just of a returned value.
pub async fn submit_if_appropriate(
    sink: &dyn MediaRequestSink,
    draft: MediaRequestDraft,
    auto_tier_enabled: bool,
    availability: Option<&Availability>,
    has_matching_arr_instance: bool,
) -> MuseResult<MediaRequestOutcome> {
    let tier = classify_tier(auto_tier_enabled, availability, has_matching_arr_instance);

    match tier {
        RequestTier::Blocked => Ok(MediaRequestOutcome {
            tier,
            submitted: false,
            reason: format!(
                "no configured *arr instance can fulfill a {:?} request — blocked",
                draft.kind
            ),
        }),
        RequestTier::NeedsReview => Ok(MediaRequestOutcome {
            tier,
            submitted: false,
            reason: "queued for manual review — auto-tier is disabled or availability wasn't \
                      confirmed grabbable"
                .to_string(),
        }),
        RequestTier::AutoApprovable => {
            sink.submit(&draft).await?;
            Ok(MediaRequestOutcome {
                tier,
                submitted: true,
                reason: "auto-approved: operator opted in and MUSE-16 confirmed it's grabbable \
                          right now"
                    .to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn availability(release_count: i32) -> Availability {
        Availability {
            media_metadata_id: 1,
            best_quality: None,
            best_seeders: Some(10),
            release_count,
            has_freeleech: false,
            cheapest_size_bytes: None,
            newest_release_at: None,
            computed_at: chrono::Utc::now(),
        }
    }

    fn draft() -> MediaRequestDraft {
        MediaRequestDraft {
            tmdb_id: "tmdb-1".to_string(),
            title: "Sicario".to_string(),
            kind: MediaKind::Movie,
        }
    }

    // --- classify_tier: pure, exhaustive ------------------------------------

    #[test]
    fn no_matching_arr_instance_is_always_blocked_regardless_of_other_signals() {
        assert_eq!(
            classify_tier(true, Some(&availability(5)), false),
            RequestTier::Blocked
        );
        assert_eq!(classify_tier(false, None, false), RequestTier::Blocked);
    }

    #[test]
    fn auto_tier_disabled_is_needs_review_even_with_confirmed_availability() {
        assert_eq!(
            classify_tier(false, Some(&availability(5)), true),
            RequestTier::NeedsReview
        );
    }

    #[test]
    fn auto_tier_enabled_with_unchecked_availability_is_needs_review_not_auto() {
        assert_eq!(classify_tier(true, None, true), RequestTier::NeedsReview);
    }

    #[test]
    fn auto_tier_enabled_with_checked_but_unavailable_is_needs_review_not_auto() {
        assert_eq!(
            classify_tier(true, Some(&availability(0)), true),
            RequestTier::NeedsReview
        );
    }

    #[test]
    fn auto_tier_enabled_with_confirmed_grabbable_is_auto_approvable() {
        assert_eq!(
            classify_tier(true, Some(&availability(3)), true),
            RequestTier::AutoApprovable
        );
    }

    // --- submit_if_appropriate: the sink is only ever called for AutoApprovable ---

    #[tokio::test]
    async fn needs_review_never_calls_the_sink() {
        let sink = MockMediaRequestSink::new();
        let outcome = submit_if_appropriate(&sink, draft(), false, Some(&availability(5)), true)
            .await
            .unwrap();

        assert_eq!(outcome.tier, RequestTier::NeedsReview);
        assert!(!outcome.submitted);
        assert_eq!(
            sink.submitted_count(),
            0,
            "the sink must never be invoked for a NeedsReview request"
        );
    }

    #[tokio::test]
    async fn blocked_never_calls_the_sink() {
        let sink = MockMediaRequestSink::new();
        let outcome = submit_if_appropriate(&sink, draft(), true, Some(&availability(5)), false)
            .await
            .unwrap();

        assert_eq!(outcome.tier, RequestTier::Blocked);
        assert!(!outcome.submitted);
        assert_eq!(sink.submitted_count(), 0);
    }

    #[tokio::test]
    async fn auto_approvable_submits_exactly_once_through_the_sink() {
        let sink = MockMediaRequestSink::new();
        let d = draft();
        let outcome = submit_if_appropriate(&sink, d.clone(), true, Some(&availability(3)), true)
            .await
            .unwrap();

        assert_eq!(outcome.tier, RequestTier::AutoApprovable);
        assert!(outcome.submitted);
        assert_eq!(sink.submitted_count(), 1);
        let submitted = sink.submitted.lock().unwrap();
        assert_eq!(submitted[0], d);
    }

    #[tokio::test]
    async fn default_posture_never_auto_submits_without_explicit_opt_in() {
        // Mirrors Config::arr_request_auto_tier_enabled's own default
        // (`false`): with no operator opt-in, even a confirmed-grabbable
        // title never reaches the sink.
        let sink = MockMediaRequestSink::new();
        let outcome = submit_if_appropriate(&sink, draft(), false, Some(&availability(10)), true)
            .await
            .unwrap();

        assert!(!outcome.submitted);
        assert_eq!(sink.submitted_count(), 0);
    }
}
