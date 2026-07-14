//! MUSEX-15 (Plane TERM #391), part C: engagement-tiered request budgets.
//! A friend earns request headroom based on TWO real signals — do they
//! actually watch what they bring in (watch-through), and does the
//! household actually like it (household-loved)? — and that tier MODULATES
//! (never bypasses) [`crate::arr::request`]'s existing tiered-safety gate.
//!
//! ## Honest limitation: no request-log table yet
//! This crate has no `(friend, requested title)` ledger — MUSEX-14 shipped
//! the request DOMAIN ([`crate::arr::request`]) but never a persisted log of
//! who asked for what (see that module's own "no live *arr-writing sink
//! shipped" honest-limitation note, which this inherits). So
//! [`gather_engagement_counts`] approximates "what a friend brought" via
//! the titles their LINKED muse account has a real `watch_stats` signal on
//! (repo::watch_stats::list_watch_stats_for_account) — the closest existing,
//! real signal — cross-referenced against household `ratings` on those SAME
//! titles. This is a deliberate v0 approximation, same explicit-limitation
//! posture `crate::conversational`'s module doc takes for its own "Missing
//! titles" section; wiring a real request ledger once one exists is a
//! natural follow-up, not done in this pass.
//!
//! ## Budgets modulate, never bypass, `arr::request`
//! [`submit_with_engagement_budget`] calls
//! [`crate::arr::request::classify_tier`] FIRST — the exact same pure
//! classification `crate::conversational::handle_conversational_request`
//! uses — and only ever adjusts [`crate::arr::request::RequestTier::AutoApprovable`]
//! DOWN to [`crate::arr::request::RequestTier::NeedsReview`] when the friend
//! is over budget. It can never turn a structural
//! [`crate::arr::request::RequestTier::Blocked`] into anything else, and it
//! can never turn `NeedsReview`/`Blocked` INTO `AutoApprovable` — budget is
//! strictly a brake, never an accelerator, which is what makes "does not
//! bypass the gate" a property of the call graph (`classify_tier` is always
//! consulted first), not just of a returned value.

use sqlx::PgPool;

use crate::arr::request::{
    classify_tier, MediaRequestDraft, MediaRequestOutcome, MediaRequestSink, RequestTier,
};
use crate::error::MuseResult;
use crate::models::availability::Availability;
use crate::repo;

/// How engaged a friend is, mapped to a request budget. Ordered
/// (`Starter < Trusted < Curator`) so a future caller can compare tiers
/// directly if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EngagementTier {
    Starter,
    Trusted,
    Curator,
}

/// GUI/config-tunable weights + thresholds behind [`compute_tier`] and
/// [`budget_for_tier`] — see `crate::config::Config`'s own
/// `premiere_engagement_*`/`premiere_*_budget` fields for the env-backed
/// defaults. Kept as its own small struct (rather than threading eight loose
/// `f64`/`u32` params through every call) so a caller builds it once from
/// `Config` and reuses it across many friends in one pass.
#[derive(Debug, Clone, Copy)]
pub struct EngagementTierConfig {
    pub watch_through_weight: f64,
    pub household_love_weight: f64,
    pub trusted_threshold: f64,
    pub curator_threshold: f64,
    pub starter_budget: u32,
    pub trusted_budget: u32,
    pub curator_budget: u32,
}

/// Raw counts behind a friend's engagement score — plain integers, not
/// pre-divided floats, so [`compute_tier`]'s aggregation stays pure/testable
/// and all DB access lives only in [`gather_engagement_counts`]. See the
/// module doc's honest-limitation note for exactly what "touched"/"loved"
/// approximate.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EngagementCounts {
    /// `watch_stats` rows for this friend's linked account with
    /// `finished_count > 0` — titles they actually watched through.
    pub watched: u32,
    /// Total `watch_stats` rows for this friend's linked account — titles
    /// they have ANY signal on at all (the watch-through denominator).
    pub touched: u32,
    /// Household `ratings` rows, on titles this friend's account has
    /// touched, that clear `Config::premiere_loved_rating_threshold`.
    pub household_loved: u32,
    /// Total household `ratings` rows found on titles this friend's account
    /// has touched (the household-love denominator).
    pub household_rated: u32,
}

impl EngagementCounts {
    /// Fraction `[0.0, 1.0]` of touched titles this friend actually watched
    /// through. `0.0` (never `NaN`) when they've touched nothing yet — a
    /// cold-start friend earns no watch-through credit, not an error.
    pub fn watch_through_rate(&self) -> f64 {
        if self.touched == 0 {
            return 0.0;
        }
        f64::from(self.watched) / f64::from(self.touched)
    }

    /// Fraction `[0.0, 1.0]` of household ratings (on this friend's touched
    /// titles) that were loved. `0.0` when the household never rated
    /// anything this friend touched — no evidence of a good pick, not
    /// evidence of a bad one, same conservative-default posture
    /// `arr::request::classify_tier` takes for unchecked availability.
    pub fn household_love_rate(&self) -> f64 {
        if self.household_rated == 0 {
            return 0.0;
        }
        f64::from(self.household_loved) / f64::from(self.household_rated)
    }
}

/// The pure classification: a weighted composite of the two rates against
/// `config`'s thresholds. Total, exhaustive, no I/O — exactly the same
/// "caller already computed every input" posture `arr::request::classify_tier`
/// documents for itself.
pub fn compute_tier(counts: &EngagementCounts, config: &EngagementTierConfig) -> EngagementTier {
    let composite = counts.watch_through_rate() * config.watch_through_weight
        + counts.household_love_rate() * config.household_love_weight;

    if composite >= config.curator_threshold {
        EngagementTier::Curator
    } else if composite >= config.trusted_threshold {
        EngagementTier::Trusted
    } else {
        EngagementTier::Starter
    }
}

/// The request budget a tier earns — pure lookup into `config`.
pub fn budget_for_tier(tier: EngagementTier, config: &EngagementTierConfig) -> u32 {
    match tier {
        EngagementTier::Starter => config.starter_budget,
        EngagementTier::Trusted => config.trusted_budget,
        EngagementTier::Curator => config.curator_budget,
    }
}

/// Gather the real counts behind a friend's engagement score for
/// `account_id` (their linked Muse account — see
/// `discord::identity::FriendIdentity::linked_account`), cross-referenced
/// against `household_account_ids`' ratings. See the module doc's honest
/// limitation for exactly what this approximates. Never errors on "no
/// signal yet" — a cold-start account simply yields all-zero counts.
pub async fn gather_engagement_counts(
    pool: &PgPool,
    account_id: i64,
    household_account_ids: &[i64],
    loved_rating_threshold: f32,
) -> MuseResult<EngagementCounts> {
    let stats = repo::watch_stats::list_watch_stats_for_account(pool, account_id).await?;
    let touched = stats.len() as u32;
    let watched = stats.iter().filter(|s| s.finished_count > 0).count() as u32;

    let touched_item_ids: std::collections::HashSet<i64> =
        stats.iter().map(|s| s.media_item_id).collect();

    let mut household_rated = 0u32;
    let mut household_loved = 0u32;
    for &household_account_id in household_account_ids {
        // The household's own household-member accounts are exactly what
        // this friend's taste is judged against -- excluding this friend's
        // own account would need its id to be known here, but a friend's
        // `muse_account_id` is only ever the target of promotion/scoring in
        // this crate, never itself a "household" seat, so no self-rating
        // double-count risk exists in practice; callers should still pass
        // only genuine household seats.
        let ratings =
            repo::watch_stats::list_ratings_for_account(pool, household_account_id).await?;
        for r in ratings {
            if !touched_item_ids.contains(&r.media_item_id) {
                continue;
            }
            let Some(value) = r.rating else { continue };
            household_rated += 1;
            if value >= loved_rating_threshold {
                household_loved += 1;
            }
        }
    }

    Ok(EngagementCounts {
        watched,
        touched,
        household_loved,
        household_rated,
    })
}

/// Budget-aware wrapper around [`crate::arr::request::classify_tier`] +
/// [`crate::arr::request::submit_if_appropriate`]'s own dispatch logic — see
/// the module doc's "Budgets modulate, never bypass" section for the exact
/// contract. `requests_used_this_window` is the friend's already-consumed
/// request count for whatever window the caller tracks (this function takes
/// no opinion on the window's length — that's `Config::premiere_announce_cadence_secs`
/// or an operator's own bookkeeping, not this pure/DB-light function's
/// concern).
pub async fn submit_with_engagement_budget(
    sink: &dyn MediaRequestSink,
    draft: MediaRequestDraft,
    auto_tier_enabled: bool,
    availability: Option<&Availability>,
    has_matching_arr_instance: bool,
    budget: u32,
    requests_used_this_window: u32,
) -> MuseResult<MediaRequestOutcome> {
    let base_tier = classify_tier(auto_tier_enabled, availability, has_matching_arr_instance);

    let over_budget = requests_used_this_window >= budget;
    let tier = if base_tier == RequestTier::AutoApprovable && over_budget {
        RequestTier::NeedsReview
    } else {
        base_tier
    };

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
            reason: if base_tier == RequestTier::AutoApprovable && over_budget {
                format!(
                    "would auto-approve, but the requesting friend is over their engagement \
                     budget ({requests_used_this_window}/{budget} used this window) — routed to \
                     manual review instead"
                )
            } else {
                "queued for manual review — auto-tier is disabled or availability wasn't \
                 confirmed grabbable"
                    .to_string()
            },
        }),
        RequestTier::AutoApprovable => {
            sink.submit(&draft).await?;
            Ok(MediaRequestOutcome {
                tier,
                submitted: true,
                reason: format!(
                    "auto-approved: operator opted in, MUSE-16 confirmed it's grabbable right \
                     now, and the requesting friend is within their engagement budget \
                     ({requests_used_this_window}/{budget} used this window)"
                ),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arr::request::MockMediaRequestSink;
    use crate::models::media_metadata::MediaKind;

    fn config() -> EngagementTierConfig {
        EngagementTierConfig {
            watch_through_weight: 0.5,
            household_love_weight: 0.5,
            trusted_threshold: 0.4,
            curator_threshold: 0.7,
            starter_budget: 1,
            trusted_budget: 3,
            curator_budget: 6,
        }
    }

    fn high_engagement_counts() -> EngagementCounts {
        EngagementCounts {
            watched: 9,
            touched: 10,
            household_loved: 8,
            household_rated: 10,
        }
    }

    fn low_engagement_counts() -> EngagementCounts {
        EngagementCounts {
            watched: 1,
            touched: 10,
            household_loved: 0,
            household_rated: 10,
        }
    }

    // --- compute_tier / budget_for_tier: pure, non-vacuous -------------------

    #[test]
    fn high_and_low_engagement_fixtures_actually_differ() {
        // Non-vacuous fixture sanity, per the AC: prove the two friends'
        // engagement genuinely diverges before relying on it below.
        let high = high_engagement_counts();
        let low = low_engagement_counts();
        assert!(high.watch_through_rate() > low.watch_through_rate());
        assert!(high.household_love_rate() > low.household_love_rate());
    }

    #[test]
    fn high_engagement_friend_earns_a_higher_tier_and_larger_budget_than_a_low_engagement_friend() {
        let cfg = config();
        let high_tier = compute_tier(&high_engagement_counts(), &cfg);
        let low_tier = compute_tier(&low_engagement_counts(), &cfg);

        assert!(
            high_tier > low_tier,
            "high engagement ({high_tier:?}) must outrank low engagement ({low_tier:?})"
        );
        assert_eq!(high_tier, EngagementTier::Curator);
        assert_eq!(low_tier, EngagementTier::Starter);

        let high_budget = budget_for_tier(high_tier, &cfg);
        let low_budget = budget_for_tier(low_tier, &cfg);
        assert!(
            high_budget > low_budget,
            "a higher engagement tier must earn a larger budget: {high_budget} vs {low_budget}"
        );
    }

    #[test]
    fn a_friend_with_zero_signal_defaults_to_starter_not_a_panic() {
        let cfg = config();
        let tier = compute_tier(&EngagementCounts::default(), &cfg);
        assert_eq!(tier, EngagementTier::Starter);
    }

    // --- submit_with_engagement_budget: modulates, never bypasses ------------

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

    #[tokio::test]
    async fn an_over_budget_friend_is_not_auto_approved_even_when_otherwise_eligible() {
        let sink = MockMediaRequestSink::new();
        // Every classify_tier input says AutoApprovable... but the friend
        // has already used their entire budget this window.
        let outcome = submit_with_engagement_budget(
            &sink,
            draft(),
            true,
            Some(&availability(5)),
            true,
            /* budget */ 2,
            /* requests_used_this_window */ 2,
        )
        .await
        .unwrap();

        assert_eq!(outcome.tier, RequestTier::NeedsReview);
        assert!(!outcome.submitted);
        assert_eq!(
            sink.submitted_count(),
            0,
            "an over-budget friend's request must never reach the sink"
        );
    }

    #[tokio::test]
    async fn an_in_budget_friend_who_is_legitimately_auto_approvable_still_gets_submitted() {
        let sink = MockMediaRequestSink::new();
        let outcome = submit_with_engagement_budget(
            &sink,
            draft(),
            true,
            Some(&availability(5)),
            true,
            /* budget */ 3,
            /* requests_used_this_window */ 1,
        )
        .await
        .unwrap();

        assert_eq!(outcome.tier, RequestTier::AutoApprovable);
        assert!(outcome.submitted);
        assert_eq!(sink.submitted_count(), 1);
    }

    #[tokio::test]
    async fn budget_can_never_upgrade_a_structurally_blocked_request() {
        let sink = MockMediaRequestSink::new();
        // No matching *arr instance -> structurally Blocked. A huge unused
        // budget must not change that.
        let outcome = submit_with_engagement_budget(
            &sink,
            draft(),
            true,
            Some(&availability(5)),
            /* has_matching_arr_instance */ false,
            /* budget */ 1000,
            /* requests_used_this_window */ 0,
        )
        .await
        .unwrap();

        assert_eq!(outcome.tier, RequestTier::Blocked);
        assert!(!outcome.submitted);
        assert_eq!(sink.submitted_count(), 0);
    }

    #[tokio::test]
    async fn budget_can_never_upgrade_a_needs_review_request_into_auto_approvable() {
        let sink = MockMediaRequestSink::new();
        // auto_tier_enabled = false -> NeedsReview regardless of budget.
        let outcome = submit_with_engagement_budget(
            &sink,
            draft(),
            false,
            Some(&availability(5)),
            true,
            /* budget */ 1000,
            /* requests_used_this_window */ 0,
        )
        .await
        .unwrap();

        assert_eq!(outcome.tier, RequestTier::NeedsReview);
        assert!(!outcome.submitted);
        assert_eq!(sink.submitted_count(), 0);
    }
}

/// DB-backed coverage for [`gather_engagement_counts`]: seed two friends'
/// accounts with genuinely different watch-through + household-love
/// signals and confirm the gathered counts reflect that divergence. Gated
/// per `MUSE_TEST_DATABASE_URL`, same convention as every other `db_gated`
/// suite in this crate.
#[cfg(test)]
mod db_gated {
    use super::*;
    use crate::models::account::NewAccount;
    use crate::models::library::{LibraryKind, NewLibrary};
    use crate::models::media_item::NewMediaItem;
    use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
    use crate::models::watch_stats::NewWatchStats;
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

    async fn seed_account(pool: &PgPool, label: &str) -> i64 {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        repo::account::create(
            pool,
            &NewAccount {
                plex_account_id: Some(format!("plex-{suffix}")),
                username: Some(format!("user-{suffix}")),
                friendly_name: Some(format!("{label} Probe")),
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account")
        .id
    }

    async fn seed_item(pool: &PgPool) -> i64 {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let library = repo::library::create(
            pool,
            &NewLibrary {
                name: format!("lib-{suffix}"),
                kind: LibraryKind::Movie,
                root_folder: format!("/movies-{suffix}"),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");
        let metadata = repo::media_metadata::upsert_by_tmdb(
            pool,
            &NewMediaMetadata {
                kind: MediaKind::Movie,
                tmdb_id: Some(format!("tmdb-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: json!({}),
                title: format!("MUSEX15EngagementProbe{suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: Some("a real, seeded synopsis".to_string()),
                studio: None,
                network: None,
                runtime_minutes: Some(100),
                year: Some(2024),
                images: json!({}),
            },
        )
        .await
        .expect("create media_metadata");
        repo::media_item::upsert(
            pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/movies-{suffix}/movie.mkv"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("plexkey-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("create media_item")
        .id
    }

    #[tokio::test]
    async fn gathered_counts_reflect_real_watch_through_and_household_love_divergence() {
        let Some(pool) = test_pool_or_skip(
            "gathered_counts_reflect_real_watch_through_and_household_love_divergence",
        )
        .await
        else {
            return;
        };

        let high_engagement_friend = seed_account(&pool, "HighEngagement").await;
        let low_engagement_friend = seed_account(&pool, "LowEngagement").await;
        let household_member = seed_account(&pool, "HouseholdMember").await;

        let loved_item = seed_item(&pool).await;
        let unloved_item = seed_item(&pool).await;

        // High-engagement friend: finished both titles they touched.
        repo::watch_stats::upsert_watch_stats(
            &pool,
            &NewWatchStats {
                account_id: high_engagement_friend,
                media_item_id: loved_item,
                play_count: 1,
                finished_count: 1,
                rewatch_count: 0,
                total_watched_ms: 6_000_000,
                avg_percent: Some(100.0),
                last_watched_at: Some(chrono::Utc::now()),
                abandoned: false,
                first_watched_at: Some(chrono::Utc::now()),
            },
        )
        .await
        .expect("seed high-engagement watch_stats");

        // Low-engagement friend: touched the same title but never finished
        // it (abandoned).
        repo::watch_stats::upsert_watch_stats(
            &pool,
            &NewWatchStats {
                account_id: low_engagement_friend,
                media_item_id: unloved_item,
                play_count: 1,
                finished_count: 0,
                rewatch_count: 0,
                total_watched_ms: 100_000,
                avg_percent: Some(5.0),
                last_watched_at: Some(chrono::Utc::now()),
                abandoned: true,
                first_watched_at: Some(chrono::Utc::now()),
            },
        )
        .await
        .expect("seed low-engagement watch_stats");

        // Household loved what the high-engagement friend brought, and
        // panned what the low-engagement friend brought.
        repo::watch_stats::upsert_rating(
            &pool,
            household_member,
            loved_item,
            9.5,
            chrono::Utc::now(),
        )
        .await
        .expect("seed loved rating");
        repo::watch_stats::upsert_rating(
            &pool,
            household_member,
            unloved_item,
            2.0,
            chrono::Utc::now(),
        )
        .await
        .expect("seed unloved rating");

        let high_counts =
            gather_engagement_counts(&pool, high_engagement_friend, &[household_member], 7.0)
                .await
                .expect("gather_engagement_counts should not error");
        let low_counts =
            gather_engagement_counts(&pool, low_engagement_friend, &[household_member], 7.0)
                .await
                .expect("gather_engagement_counts should not error");

        assert!(
            high_counts.watch_through_rate() > low_counts.watch_through_rate(),
            "fixture bug: the high-engagement friend must actually out-watch-through the low one: \
             {high_counts:?} vs {low_counts:?}"
        );
        assert!(
            high_counts.household_love_rate() > low_counts.household_love_rate(),
            "fixture bug: the household must actually love the high-engagement friend's pick more: \
             {high_counts:?} vs {low_counts:?}"
        );

        let cfg = EngagementTierConfig {
            watch_through_weight: 0.5,
            household_love_weight: 0.5,
            trusted_threshold: 0.4,
            curator_threshold: 0.7,
            starter_budget: 1,
            trusted_budget: 3,
            curator_budget: 6,
        };
        let high_tier = compute_tier(&high_counts, &cfg);
        let low_tier = compute_tier(&low_counts, &cfg);
        assert!(
            high_tier > low_tier,
            "real seeded data must produce a higher tier for the high-engagement friend: \
             {high_tier:?} vs {low_tier:?}"
        );
    }
}
