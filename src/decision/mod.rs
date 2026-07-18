//! MUSEM-04: the release-decision / scoring engine — "what to grab" for one
//! monitored/requested media item, given candidate releases (MUSEM-03's
//! future job), a quality profile, and its custom-format score table
//! (MUSE-02). Pure and deterministic, mirroring
//! [`crate::arr::request::classify_tier`]'s pure-function shape: every
//! signal is passed in by the caller, so [`decide_release`] needs no
//! database/network access of its own and is exhaustively unit-testable.
//!
//! This module intentionally does **not** define its own candidate/quality
//! types — it consumes [`crate::models::quality::QualityDefinition`],
//! [`crate::models::quality::QualityProfile`],
//! [`crate::models::quality::CustomFormat`],
//! [`crate::models::quality::QualityProfileFormat`] (MUSE-02) and
//! [`crate::models::release::Release`] (MUSE-16) exactly as they already
//! exist, so it merges independently of the unmerged MUSEM-01/03 acquisition
//! branches — see [`scoring::ReleaseCandidate`] for the one small addition
//! (`runtime_minutes`) needed on top of `Release`.
//!
//! ## Decision algorithm
//! 1. Every candidate is run through [`scoring::evaluate_candidate`]: resolve
//!    its quality tier, check it's `allowed` in the profile, check
//!    size-per-minute bounds, sum matched custom-format scores, check
//!    `min_format_score`. A candidate that fails any gate is dropped with a
//!    reason (never silently — see [`Decision::Reject`]).
//! 2. Survivors are ranked by, in order: quality-tier rank (profile
//!    preference order), total format score, `proper_repack` (a REPACK/PROPER
//!    beats an equal-in-every-other-way non-repack), seeders (`None` =
//!    unknown, sorted *between* known-positive and zero — never coerced to
//!    `0`, so a private-tracker release with unreported seeders isn't
//!    unfairly sunk), freeleech, smaller size, and finally the release `guid`
//!    for a fully deterministic tiebreak.
//! 3. If [`scoring::ScoringPolicy::existing`] is set (an upgrade decision for
//!    an already-held file, not a first grab): reject outright if the
//!    existing file already meets the profile's cutoff (tier ≥ cutoff AND
//!    format score ≥ `cutoff_format_score`) — "good enough, stop" — or if
//!    `upgrade_allowed` is `false`; otherwise the best survivor must beat the
//!    existing file by at least `min_upgrade_format_score`, or by tier alone.
//! 4. The best remaining candidate is returned as [`Decision::Grab`]; if
//!    nothing survives gating (including an empty candidate list), the
//!    accumulated reasons are returned as [`Decision::Reject`].

pub mod scoring;

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::models::quality::{QualityProfile, QualityProfileFormat};
use scoring::{evaluate_candidate, CandidateEvaluation, ReleaseCandidate, ScorerRegistry, ScoringPolicy};

/// The chosen release plus the facts that justified picking it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseChoice {
    pub release: crate::models::release::Release,
    pub total_score: i32,
    pub quality_tier: String,
    pub reason: String,
}

/// [`decide_release`]'s result: exactly one eligible release, or none —
/// with every rejection reason preserved (never a silent empty list).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Decision {
    Grab(ReleaseChoice),
    Reject { reasons: Vec<String> },
}

/// THE decision entrypoint. See the module doc for the full algorithm. No
/// I/O — `candidates`, `profile`, `format_scores`, and `policy` (which
/// itself carries the quality-definition/custom-format lookup tables and
/// the optional "what's already held" fact) are the entire input.
pub fn decide_release(
    candidates: &[ReleaseCandidate],
    profile: &QualityProfile,
    format_scores: &[QualityProfileFormat],
    policy: &ScoringPolicy<'_>,
) -> Decision {
    if candidates.is_empty() {
        return Decision::Reject {
            reasons: vec!["no candidate releases supplied".to_string()],
        };
    }

    if let Some(existing) = policy.existing {
        if !profile.upgrade_allowed {
            return Decision::Reject {
                reasons: vec![format!(
                    "profile {:?} has upgrade_allowed=false and a file is already held",
                    profile.name
                )],
            };
        }
        if let Some((existing_rank, _allowed)) =
            scoring::tier_position(&profile.items, existing.quality_definition_id)
        {
            let cutoff_rank = profile
                .cutoff_quality_id
                .and_then(|id| scoring::tier_position(&profile.items, id))
                .map(|(rank, _)| rank);
            let at_cutoff_tier = cutoff_rank.map(|c| existing_rank >= c).unwrap_or(true);
            let at_cutoff_score = existing.total_format_score >= profile.cutoff_format_score;
            if at_cutoff_tier && at_cutoff_score {
                return Decision::Reject {
                    reasons: vec![
                        "existing file already meets the profile's cutoff — no upgrade needed"
                            .to_string(),
                    ],
                };
            }
        }
    }

    let registry = ScorerRegistry::deterministic();
    let mut reasons = Vec::new();
    let mut survivors: Vec<(&ReleaseCandidate, CandidateEvaluation)> = Vec::new();

    for candidate in candidates {
        match evaluate_candidate(candidate, profile, format_scores, &registry, policy) {
            Ok(eval) => survivors.push((candidate, eval)),
            Err(reason) => reasons.push(reason),
        }
    }

    if survivors.is_empty() {
        return Decision::Reject { reasons };
    }

    survivors.sort_by(|(a_cand, a_eval), (b_cand, b_eval)| rank_candidates(a_cand, a_eval, b_cand, b_eval));
    // `rank_candidates` orders best-first.
    let (best_candidate, best_eval) = survivors.remove(0);

    if let Some(existing) = policy.existing {
        let (existing_rank, _) =
            scoring::tier_position(&profile.items, existing.quality_definition_id).unwrap_or((0, true));
        let tier_improved = best_eval.tier_rank > existing_rank;
        let score_improved =
            best_eval.total_format_score - existing.total_format_score >= profile.min_upgrade_format_score;
        if !(tier_improved || score_improved) {
            return Decision::Reject {
                reasons: vec![format!(
                    "best candidate ({}) does not improve enough on the existing file to justify an upgrade \
                     (min_upgrade_format_score={})",
                    best_candidate.release.guid, profile.min_upgrade_format_score
                )],
            };
        }
    }

    Decision::Grab(ReleaseChoice {
        release: best_candidate.release.clone(),
        total_score: best_eval.total_format_score,
        quality_tier: best_eval.quality_key.clone(),
        reason: format!(
            "tier {:?} (rank {}), format score {}, {} other candidate(s) rejected",
            best_eval.quality_key,
            best_eval.tier_rank,
            best_eval.total_format_score,
            reasons.len()
        ),
    })
}

/// Best-first comparator (i.e. `Ordering::Less` means `a` ranks better than
/// `b`) implementing the tiebreak chain documented on the module doc.
fn rank_candidates(
    a_cand: &ReleaseCandidate,
    a_eval: &CandidateEvaluation,
    b_cand: &ReleaseCandidate,
    b_eval: &CandidateEvaluation,
) -> Ordering {
    // Higher tier_rank is better -> reverse for ascending sort.
    b_eval
        .tier_rank
        .cmp(&a_eval.tier_rank)
        .then_with(|| b_eval.total_format_score.cmp(&a_eval.total_format_score))
        .then_with(|| {
            b_cand
                .release
                .proper_repack
                .cmp(&a_cand.release.proper_repack)
        })
        .then_with(|| cmp_seeders(a_cand.release.seeders, b_cand.release.seeders))
        .then_with(|| b_cand.release.freeleech.cmp(&a_cand.release.freeleech))
        .then_with(|| cmp_smaller_size_wins(a_cand.release.size_bytes, b_cand.release.size_bytes))
        .then_with(|| a_cand.release.guid.cmp(&b_cand.release.guid))
}

/// `None` (unknown) sorts *between* any known positive value and zero: an
/// unreported private-tracker seeder count must never be coerced to `0`
/// (edge case in the item spec) and unfairly sink the candidate below every
/// release with a real, low-but-known seeder count, while a candidate with
/// a *known* higher seeder count still legitimately outranks an unknown one.
/// Order (best to worst): higher known count > unknown > known zero/low.
fn cmp_seeders(a: Option<i32>, b: Option<i32>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(a), None) => {
            if a > 0 {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (None, Some(b)) => {
            if b > 0 {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (None, None) => Ordering::Equal,
    }
}

fn cmp_smaller_size_wins(a: Option<i64>, b: Option<i64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::quality::{CustomFormat, QualityDefinition, QualityProfileFormat};
    use crate::models::release::Release;
    use chrono::Utc;
    use serde_json::json;

    fn definition(id: i64, key: &str, source: &str, resolution: Option<&str>) -> QualityDefinition {
        QualityDefinition {
            id,
            quality_key: key.to_string(),
            title: key.to_string(),
            source: source.to_string(),
            resolution: resolution.map(|r| r.to_string()),
            modifier: "none".to_string(),
            min_size_mb_per_min: None,
            max_size_mb_per_min: None,
            preferred_size_mb_per_min: None,
            sort_order: id as i32,
            created_at: Utc::now(),
        }
    }

    fn definition_with_size_bounds(
        id: i64,
        key: &str,
        source: &str,
        resolution: Option<&str>,
        min: f32,
        max: f32,
    ) -> QualityDefinition {
        let mut d = definition(id, key, source, resolution);
        d.min_size_mb_per_min = Some(min);
        d.max_size_mb_per_min = Some(max);
        d
    }

    /// `items` low-to-high preference; every entry `allowed:true` unless
    /// overridden.
    fn profile_items(ordered_ids: &[i64]) -> serde_json::Value {
        json!(ordered_ids
            .iter()
            .map(|id| json!({ "quality": { "id": id }, "allowed": true }))
            .collect::<Vec<_>>())
    }

    fn profile(items: serde_json::Value, cutoff_quality_id: Option<i64>) -> QualityProfile {
        QualityProfile {
            id: 1,
            name: "Test Profile".to_string(),
            cutoff_quality_id,
            items,
            language: None,
            upgrade_allowed: true,
            min_format_score: 0,
            cutoff_format_score: 0,
            min_upgrade_format_score: 1,
            natural_language_intent: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn release(guid: &str, source: &str, resolution: Option<&str>) -> Release {
        Release {
            id: 0,
            media_metadata_id: None,
            episode_id: None,
            indexer_id: 1,
            guid: guid.to_string(),
            title: format!("Some.Title.2020.{resolution}.{source}", resolution = resolution.unwrap_or(""), source = source),
            info_url: None,
            download_url: None,
            info_hash: None,
            size_bytes: None,
            publish_date: None,
            seeders: None,
            leechers: None,
            grabs: None,
            freeleech: false,
            freeleech_pct: None,
            categories: vec![],
            parsed_title: None,
            parsed_year: None,
            quality: None,
            resolution: resolution.map(|r| r.to_string()),
            source: Some(source.to_string()),
            video_codec: None,
            audio_codec: None,
            audio_channels: None,
            hdr: vec![],
            edition: None,
            release_group: None,
            proper_repack: false,
            languages: vec![],
            subtitles: vec![],
            parse_confidence: None,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            expires_at: None,
        }
    }

    fn candidate(r: Release) -> ReleaseCandidate {
        ReleaseCandidate {
            release: r,
            runtime_minutes: None,
        }
    }

    fn no_formats() -> Vec<CustomFormat> {
        vec![]
    }
    fn no_scores() -> Vec<QualityProfileFormat> {
        vec![]
    }

    // --- allowed-tier filtering -------------------------------------------

    #[test]
    fn rejects_a_disallowed_tier() {
        let definitions = vec![definition(1, "web-1080p", "WEB", Some("1080p"))];
        let items = json!([{ "quality": { "id": 1 }, "allowed": false }]);
        let p = profile(items, None);
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: None,
        };
        let candidates = vec![candidate(release("r1", "WEB", Some("1080p")))];
        let decision = decide_release(&candidates, &p, &no_scores(), &policy);
        match decision {
            Decision::Reject { reasons } => assert!(reasons[0].contains("not allowed")),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    // --- size-per-minute rejection -----------------------------------------

    #[test]
    fn rejects_size_per_minute_out_of_bounds() {
        let definitions = vec![definition_with_size_bounds(1, "web-1080p", "WEB", Some("1080p"), 10.0, 20.0)];
        let items = profile_items(&[1]);
        let p = profile(items, None);
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: None,
        };
        let mut r = release("r1", "WEB", Some("1080p"));
        // 100 minutes * 5 MB/min = 500MB, way below the 10-20 MB/min bound.
        r.size_bytes = Some(500 * 1024 * 1024);
        let candidates = vec![ReleaseCandidate {
            release: r,
            runtime_minutes: Some(100),
        }];
        let decision = decide_release(&candidates, &p, &no_scores(), &policy);
        match decision {
            Decision::Reject { reasons } => assert!(reasons[0].contains("size-per-minute")),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    // --- min-format-score rejection -----------------------------------------

    #[test]
    fn rejects_below_min_format_score() {
        let definitions = vec![definition(1, "web-1080p", "WEB", Some("1080p"))];
        let items = profile_items(&[1]);
        let mut p = profile(items, None);
        p.min_format_score = 10;
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: None,
        };
        let candidates = vec![candidate(release("r1", "WEB", Some("1080p")))];
        let decision = decide_release(&candidates, &p, &no_scores(), &policy);
        match decision {
            Decision::Reject { reasons } => assert!(reasons[0].contains("min_format_score")),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    // --- ranking: higher tier beats higher-seeder-lower-tier -----------------

    #[test]
    fn higher_tier_beats_higher_seeder_lower_tier() {
        let definitions = vec![
            definition(1, "web-720p", "WEB", Some("720p")),
            definition(2, "web-1080p", "WEB", Some("1080p")),
        ];
        let items = profile_items(&[1, 2]); // 2 (1080p) ranks higher
        let p = profile(items, None);
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: None,
        };
        let mut low_tier_high_seeders = release("low", "WEB", Some("720p"));
        low_tier_high_seeders.seeders = Some(1000);
        let mut high_tier_low_seeders = release("high", "WEB", Some("1080p"));
        high_tier_low_seeders.seeders = Some(1);
        let candidates = vec![candidate(low_tier_high_seeders), candidate(high_tier_low_seeders)];
        let decision = decide_release(&candidates, &p, &no_scores(), &policy);
        match decision {
            Decision::Grab(choice) => assert_eq!(choice.release.guid, "high"),
            other => panic!("expected Grab, got {other:?}"),
        }
    }

    // --- REPACK beats equal non-repack ---------------------------------------

    #[test]
    fn repack_beats_equal_non_repack() {
        let definitions = vec![definition(1, "web-1080p", "WEB", Some("1080p"))];
        let items = profile_items(&[1]);
        let p = profile(items, None);
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: None,
        };
        let plain = release("plain", "WEB", Some("1080p"));
        let mut repack = release("repack", "WEB", Some("1080p"));
        repack.proper_repack = true;
        let candidates = vec![candidate(plain), candidate(repack)];
        let decision = decide_release(&candidates, &p, &no_scores(), &policy);
        match decision {
            Decision::Grab(choice) => assert_eq!(choice.release.guid, "repack"),
            other => panic!("expected Grab, got {other:?}"),
        }
    }

    // --- cutoff stops an upgrade ---------------------------------------------

    #[test]
    fn cutoff_stops_an_upgrade() {
        let definitions = vec![
            definition(1, "web-720p", "WEB", Some("720p")),
            definition(2, "web-1080p", "WEB", Some("1080p")),
        ];
        let items = profile_items(&[1, 2]);
        let mut p = profile(items, Some(1)); // cutoff at tier 1 (rank 0)
        p.cutoff_format_score = 0;
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: Some(scoring::ExistingRelease {
                quality_definition_id: 1,
                total_format_score: 0,
            }),
        };
        // A better candidate exists, but the existing file already meets
        // cutoff, so no upgrade should be offered.
        let candidates = vec![candidate(release("better", "WEB", Some("1080p")))];
        let decision = decide_release(&candidates, &p, &no_scores(), &policy);
        match decision {
            Decision::Reject { reasons } => assert!(reasons[0].contains("cutoff")),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    // --- empty candidates -----------------------------------------------------

    #[test]
    fn empty_candidates_rejects() {
        let definitions = vec![definition(1, "web-1080p", "WEB", Some("1080p"))];
        let p = profile(profile_items(&[1]), None);
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: None,
        };
        let decision = decide_release(&[], &p, &no_scores(), &policy);
        match decision {
            Decision::Reject { reasons } => assert!(!reasons.is_empty()),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    // --- all-rejected has non-empty reasons ------------------------------------

    #[test]
    fn all_rejected_has_non_empty_reasons() {
        let definitions = vec![definition(1, "web-1080p", "WEB", Some("1080p"))];
        let items = json!([{ "quality": { "id": 1 }, "allowed": false }]);
        let p = profile(items, None);
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: None,
        };
        let candidates = vec![
            candidate(release("r1", "WEB", Some("1080p"))),
            candidate(release("r2", "WEB", Some("1080p"))),
        ];
        let decision = decide_release(&candidates, &p, &no_scores(), &policy);
        match decision {
            Decision::Reject { reasons } => assert_eq!(reasons.len(), 2),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    // --- unknown/unparseable quality fails closed ------------------------------

    #[test]
    fn unresolvable_quality_fails_closed_not_default_grab() {
        let definitions = vec![definition(1, "web-1080p", "WEB", Some("1080p"))];
        let p = profile(profile_items(&[1]), None);
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: None,
        };
        // No source/resolution parsed at all.
        let mut r = release("unknown", "UNKNOWNSRC", None);
        r.source = None;
        r.resolution = None;
        let candidates = vec![candidate(r)];
        let decision = decide_release(&candidates, &p, &no_scores(), &policy);
        match decision {
            Decision::Reject { reasons } => assert!(reasons[0].contains("unresolvable")),
            other => panic!("expected Reject (fail-closed), got {other:?}"),
        }
    }

    // --- deterministic tiebreak by guid -----------------------------------------

    #[test]
    fn deterministic_tiebreak_by_guid() {
        let definitions = vec![definition(1, "web-1080p", "WEB", Some("1080p"))];
        let p = profile(profile_items(&[1]), None);
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: None,
        };
        // Identical in every scored dimension -> guid "aaa" sorts first.
        let candidates = vec![
            candidate(release("zzz", "WEB", Some("1080p"))),
            candidate(release("aaa", "WEB", Some("1080p"))),
        ];
        let decision = decide_release(&candidates, &p, &no_scores(), &policy);
        match decision {
            Decision::Grab(choice) => assert_eq!(choice.release.guid, "aaa"),
            other => panic!("expected Grab, got {other:?}"),
        }
    }

    // --- null seeders treated as unknown, not zero -------------------------------

    #[test]
    fn null_seeders_not_coerced_to_zero() {
        let definitions = vec![definition(1, "web-1080p", "WEB", Some("1080p"))];
        let p = profile(profile_items(&[1]), None);
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &no_formats(),
            existing: None,
        };
        let mut known_zero = release("known_zero", "WEB", Some("1080p"));
        known_zero.seeders = Some(0);
        let unknown = release("unknown_seeders", "WEB", Some("1080p"));
        // unknown.seeders stays None
        let candidates = vec![candidate(known_zero), candidate(unknown)];
        let decision = decide_release(&candidates, &p, &no_scores(), &policy);
        match decision {
            Decision::Grab(choice) => assert_eq!(choice.release.guid, "unknown_seeders"),
            other => panic!("expected Grab, got {other:?}"),
        }
    }

    // --- custom-format scoring feeds into min-format-score / ranking --------------

    #[test]
    fn custom_format_score_influences_ranking() {
        let definitions = vec![definition(1, "web-1080p", "WEB", Some("1080p"))];
        let p = profile(profile_items(&[1]), None);
        let formats = vec![CustomFormat {
            id: 1,
            name: "x265".to_string(),
            specifications: json!([{
                "implementation": "ReleaseTitleSpecification",
                "required": true,
                "negate": false,
                "fields": { "value": "x265" }
            }]),
            include_when_renaming: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }];
        let scores = vec![QualityProfileFormat {
            quality_profile_id: 1,
            custom_format_id: 1,
            score: 50,
        }];
        let policy = ScoringPolicy {
            definitions: &definitions,
            custom_formats: &formats,
            existing: None,
        };
        let mut plain = release("plain", "WEB", Some("1080p"));
        plain.title = "Some.Title.2020.WEB.1080p".to_string();
        let mut x265 = release("x265-release", "WEB", Some("1080p"));
        x265.title = "Some.Title.2020.WEB.1080p.x265".to_string();
        let candidates = vec![candidate(plain), candidate(x265)];
        let decision = decide_release(&candidates, &p, &scores, &policy);
        match decision {
            Decision::Grab(choice) => {
                assert_eq!(choice.release.guid, "x265-release");
                assert_eq!(choice.total_score, 50);
            }
            other => panic!("expected Grab, got {other:?}"),
        }
    }
}
