//! SUBS-01 — pure ranking of provider subtitle candidates.
//!
//! This module exists because the provider tier is the only one where Muse
//! *chooses* which subtitle goes with which video file (see the ordering
//! argument in [`super`]). Everything here is a pure function over already-
//! fetched metadata: no network, no clock, no filesystem, no randomness. Given
//! the same candidate list and the same context it returns the same order,
//! every time — which is what makes it testable, and what makes an operator
//! able to understand why a particular subtitle was offered first.
//!
//! # What is ranked on, in order of weight
//!
//! 1. **Release match** — by a wide margin the most important signal. A
//!    subtitle cut for the same release as the file on disk was timed against
//!    the same encode: same cut, same framerate, same pre-roll. It is very
//!    likely to be in sync with no offset work at all, which is the outcome
//!    this whole feature is trying to reach. A subtitle for a *different*
//!    release of the same film is the exact case that needs an offset, and
//!    sometimes cannot be fixed by an offset at all (a different framerate
//!    needs a rate change, not a shift).
//! 2. **Machine generation (`ai`)** — heavily deprioritised, never hidden.
//!    See [`AI_PENALTY`].
//! 3. **Hearing-impaired match** against the operator's stated preference.
//! 4. **Download count** — a weak popularity prior, log-scaled, used only to
//!    break ties among candidates that are otherwise equivalent.
//!
//! # Why `ai` is a penalty and not a filter
//!
//! Machine-generated subtitles are frequently the *only* thing available for
//! an obscure title, and a transcription-based one is often well-synced even
//! when it is stylistically poor (it was generated FROM audio). Filtering them
//! out entirely would turn "the only subtitle in the world for this film" into
//! "no subtitles found". So they are ranked last and **flagged**, and the flag
//! rides all the way to the operator on [`RankedCandidate::machine_generated`]
//! — the operator decides, with the fact in front of them.

use serde::Serialize;

use super::HearingImpairedPreference;

/// Score awarded when the candidate's `matchedRelease` is the same release as
/// the file on disk. Deliberately larger than every other term combined, so
/// nothing — not a million downloads, not a hearing-impaired preference — can
/// outrank an exact release match.
pub const EXACT_RELEASE_MATCH: i64 = 10_000;

/// Score for a partial release agreement: the release group matches, or the
/// source/resolution tokens agree, but the full release string does not. Worth
/// real weight (same group usually means same source encode) but nowhere near
/// an exact match.
pub const PARTIAL_RELEASE_MATCH: i64 = 2_500;

/// Penalty applied to a machine-generated (`ai: true`) subtitle.
///
/// Sized so that a machine-generated subtitle loses to ANY human subtitle that
/// has even a partial release match, and loses to a human subtitle with no
/// release information at all — but still beats nothing, and still beats a
/// human subtitle for a demonstrably different release when that human
/// subtitle has no other merit. It is a strong thumb on the scale, not a ban.
pub const AI_PENALTY: i64 = 5_000;

/// Score for matching the operator's hearing-impaired preference, and the
/// symmetric penalty for contradicting it. Below the release terms: a
/// correctly-timed subtitle in the wrong SDH flavour is a far better outcome
/// than a badly-timed one in the right flavour.
pub const HEARING_IMPAIRED_MATCH: i64 = 800;

/// Maximum contribution the download-count popularity prior can make.
/// Deliberately small: download count measures what other people picked, which
/// correlates with quality only loosely and with *this file's* sync not at all.
pub const MAX_DOWNLOAD_BONUS: i64 = 400;

/// What the ranker knows about the file it is trying to find subtitles for.
#[derive(Debug, Clone, Default)]
pub struct RankContext {
    /// The release name of the file on disk, e.g.
    /// `"The.Martian.2015.EXTENDED.1080p.BluRay.x264-SPARKS"`. Usually
    /// `media_files.scene_name`, falling back to the filename stem.
    ///
    /// `None` when Muse genuinely does not know — and when it is `None`, NO
    /// candidate receives a release-match score. Guessing a match from a
    /// missing value would put the single heaviest term in the model on a
    /// fabricated basis.
    pub release_name: Option<String>,
    /// The operator's hearing-impaired preference.
    pub hearing_impaired: HearingImpairedPreference,
}

/// A candidate as the ranker sees it — the provider-agnostic subset of the
/// fields ranking actually uses. Keeping this separate from
/// [`super::wyzie::WyzieSubtitle`] is what lets the ranking be tested with no
/// provider types and no HTTP at all, and would let a second provider be added
/// without touching the ranking logic.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Candidate {
    /// Provider's stable id. Also the deterministic final tiebreak.
    pub id: String,
    /// The release this subtitle was cut for, per the provider
    /// (`matchedRelease`). `None`/empty means the provider did not say.
    pub matched_release: Option<String>,
    /// The provider's `ai` flag — machine-generated.
    pub machine_generated: bool,
    pub hearing_impaired: bool,
    pub download_count: i64,
}

/// How strongly a candidate's release agrees with the file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseAgreement {
    /// Same release, normalized.
    Exact,
    /// Same release group, or agreeing source/resolution tokens.
    Partial,
    /// The provider named a release and it does not agree with ours.
    Mismatch,
    /// Not enough information on one side or the other to judge. Scores zero
    /// — neither credited nor penalised.
    Unknown,
}

impl ReleaseAgreement {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Partial => "partial",
            Self::Mismatch => "mismatch",
            Self::Unknown => "unknown",
        }
    }

    fn score(&self) -> i64 {
        match self {
            Self::Exact => EXACT_RELEASE_MATCH,
            Self::Partial => PARTIAL_RELEASE_MATCH,
            // A named mismatch scores zero rather than negative: it is still a
            // real human subtitle for this title, and the exact/partial
            // bonuses above already put every better option ahead of it.
            Self::Mismatch | Self::Unknown => 0,
        }
    }
}

/// A scored candidate, with its score broken into the terms that produced it.
///
/// The breakdown is not decoration. When an operator asks "why did it pick
/// that one", the answer has to be a list of reasons, not a single opaque
/// number — and when a ranking bug is reported, the breakdown is what makes it
/// diagnosable without a debugger.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RankedCandidate {
    pub id: String,
    pub score: i64,
    pub release_agreement: ReleaseAgreement,
    /// Surfaced as its own field, not just folded into `score` — the operator
    /// is entitled to see that a subtitle is machine-generated before they
    /// pick it, however it ranked.
    pub machine_generated: bool,
    pub hearing_impaired: bool,
    pub download_count: i64,
    /// Human-readable reasons, in the order they were applied.
    pub reasons: Vec<String>,
}

/// Rank `candidates` best-first. **Pure.**
///
/// Ties are broken by provider id (ascending) so the order is total and
/// deterministic: two runs over the same input always produce the same list,
/// which matters because this list is shown to an operator who may act on
/// position alone.
pub fn rank_candidates(candidates: &[Candidate], ctx: &RankContext) -> Vec<RankedCandidate> {
    let mut ranked: Vec<RankedCandidate> = candidates.iter().map(|c| score_candidate(c, ctx)).collect();
    ranked.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    ranked
}

/// Score one candidate. Exported so a single candidate can be explained
/// without ranking a whole list.
pub fn score_candidate(candidate: &Candidate, ctx: &RankContext) -> RankedCandidate {
    let mut score = 0i64;
    let mut reasons = Vec::new();

    let agreement = release_agreement(ctx.release_name.as_deref(), candidate.matched_release.as_deref());
    score += agreement.score();
    match agreement {
        ReleaseAgreement::Exact => reasons.push(
            "cut for the same release as the file on disk — very likely already in sync".to_string(),
        ),
        ReleaseAgreement::Partial => {
            reasons.push("same release group or source as the file on disk".to_string())
        }
        ReleaseAgreement::Mismatch => {
            reasons.push("cut for a different release — expect to need a timing offset".to_string())
        }
        ReleaseAgreement::Unknown => {
            reasons.push("no release information to compare against this file".to_string())
        }
    }

    if candidate.machine_generated {
        score -= AI_PENALTY;
        reasons.push("machine-generated (AI) — deprioritised, check it before trusting it".to_string());
    }

    match ctx.hearing_impaired {
        HearingImpairedPreference::Prefer => {
            if candidate.hearing_impaired {
                score += HEARING_IMPAIRED_MATCH;
                reasons.push("hearing-impaired (SDH), as preferred".to_string());
            } else {
                score -= HEARING_IMPAIRED_MATCH;
            }
        }
        HearingImpairedPreference::Avoid => {
            if candidate.hearing_impaired {
                score -= HEARING_IMPAIRED_MATCH;
                reasons.push("hearing-impaired (SDH), which you asked to avoid".to_string());
            } else {
                score += HEARING_IMPAIRED_MATCH;
            }
        }
        HearingImpairedPreference::Indifferent => {}
    }

    let popularity = download_bonus(candidate.download_count);
    score += popularity;
    if popularity > 0 {
        reasons.push(format!("{} downloads", candidate.download_count));
    }

    RankedCandidate {
        id: candidate.id.clone(),
        score,
        release_agreement: agreement,
        machine_generated: candidate.machine_generated,
        hearing_impaired: candidate.hearing_impaired,
        download_count: candidate.download_count,
        reasons,
    }
}

/// Log-scaled popularity bonus, capped at [`MAX_DOWNLOAD_BONUS`].
///
/// Log-scaled because the difference between 10 and 100 downloads is
/// meaningful and the difference between 100,000 and 200,000 is not. Capped so
/// popularity can never accumulate its way past a release-match term.
/// Negative/zero counts contribute nothing rather than being clamped into a
/// penalty — a provider that omits the field should not be punished for it.
fn download_bonus(count: i64) -> i64 {
    if count <= 0 {
        return 0;
    }
    // log10-ish in integer arithmetic: 1→0, 10→~100, 100→~200, 1000→~300,
    // 10_000+→capped at 400.
    let digits = count.to_string().len() as i64;
    ((digits - 1) * 100).clamp(0, MAX_DOWNLOAD_BONUS)
}

/// Judge how well a provider's `matchedRelease` agrees with our file's release
/// name. **Pure**, and the heaviest single input to the score.
///
/// Both sides are normalized first (lowercased, punctuation folded to spaces,
/// tokenised) because the same release is written `The.Martian.2015.1080p.
/// BluRay.x264-SPARKS`, `The Martian 2015 1080p BluRay x264-SPARKS` and
/// `the_martian_2015_1080p_bluray_x264_sparks` by different tools, and a
/// string equality test would call all three different.
pub fn release_agreement(ours: Option<&str>, theirs: Option<&str>) -> ReleaseAgreement {
    let (Some(ours), Some(theirs)) = (ours, theirs) else {
        return ReleaseAgreement::Unknown;
    };
    let ours_tokens = normalize_release(ours);
    let theirs_tokens = normalize_release(theirs);
    if ours_tokens.is_empty() || theirs_tokens.is_empty() {
        return ReleaseAgreement::Unknown;
    }

    if ours_tokens == theirs_tokens {
        return ReleaseAgreement::Exact;
    }

    // The release group is the strongest partial signal: it is the last
    // hyphen-delimited token of a scene name, and the same group's encode of
    // the same title is nearly always the same cut.
    let our_group = release_group(ours);
    let their_group = release_group(theirs);
    if let (Some(a), Some(b)) = (&our_group, &their_group) {
        if a == b {
            return ReleaseAgreement::Partial;
        }
    }

    // Failing that, agreement on the source+resolution tokens (the things that
    // actually determine the cut and the pre-roll) is a weaker partial match.
    let ours_signal = signal_tokens(&ours_tokens);
    let theirs_signal = signal_tokens(&theirs_tokens);
    if !ours_signal.is_empty() && ours_signal == theirs_signal {
        return ReleaseAgreement::Partial;
    }

    ReleaseAgreement::Mismatch
}

/// Lowercase, fold separators to spaces, drop empties. Returns tokens so
/// comparisons are order-sensitive but separator-insensitive.
fn normalize_release(s: &str) -> Vec<String> {
    s.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// The trailing `-GROUP` of a scene release name, lowercased.
///
/// Returns `None` when there is no hyphen, or when the trailing token looks
/// like part of the title rather than a group (contains a space after
/// normalization, or is purely numeric — `Blade.Runner.2049` must not yield a
/// group of `2049`).
fn release_group(s: &str) -> Option<String> {
    let tail = s.rsplit('-').next()?.trim();
    if tail.is_empty() || tail == s.trim() {
        return None;
    }
    // Strip a trailing file extension a filename-derived release name may
    // still carry (`...-SPARKS.mkv`).
    let tail = tail.split('.').next().unwrap_or(tail);
    let lowered = tail.to_ascii_lowercase();
    if lowered.is_empty() || lowered.chars().all(|c| c.is_ascii_digit()) || lowered.contains(' ') {
        return None;
    }
    Some(lowered)
}

/// The subset of tokens that describe the *encode* rather than the title:
/// resolution, source and edition. These are what determine whether two
/// releases share a cut and a pre-roll, which is what determines sync.
fn signal_tokens(tokens: &[String]) -> Vec<String> {
    const SIGNALS: &[&str] = &[
        "2160p", "1080p", "720p", "480p", "bluray", "blu", "bdrip", "brrip", "webrip", "web",
        "webdl", "hdtv", "dvdrip", "remux", "extended", "theatrical", "unrated", "directors",
        "imax", "proper", "repack",
    ];
    let mut found: Vec<String> = tokens
        .iter()
        .filter(|t| SIGNALS.contains(&t.as_str()))
        .cloned()
        .collect();
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = "The.Martian.2015.EXTENDED.1080p.BluRay.x264-SPARKS";

    fn ctx() -> RankContext {
        RankContext {
            release_name: Some(OURS.to_string()),
            hearing_impaired: HearingImpairedPreference::Indifferent,
        }
    }

    fn candidate(id: &str) -> Candidate {
        Candidate {
            id: id.to_string(),
            ..Candidate::default()
        }
    }

    // ---------- release agreement ----------

    #[test]
    fn an_identical_release_is_an_exact_match_regardless_of_separators() {
        assert_eq!(release_agreement(Some(OURS), Some(OURS)), ReleaseAgreement::Exact);
        assert_eq!(
            release_agreement(Some(OURS), Some("The Martian 2015 EXTENDED 1080p BluRay x264 SPARKS")),
            ReleaseAgreement::Exact,
            "dots vs spaces must not defeat the match"
        );
        assert_eq!(
            release_agreement(Some(OURS), Some("the_martian_2015_extended_1080p_bluray_x264_sparks")),
            ReleaseAgreement::Exact
        );
    }

    #[test]
    fn the_same_release_group_is_a_partial_match() {
        assert_eq!(
            release_agreement(Some(OURS), Some("The.Martian.2015.1080p.BluRay.x264-SPARKS")),
            ReleaseAgreement::Partial
        );
    }

    #[test]
    fn a_different_release_is_a_mismatch_not_an_unknown() {
        // This distinction matters: a NAMED different release is information
        // ("expect to need an offset"), whereas unknown is the absence of it.
        assert_eq!(
            release_agreement(Some(OURS), Some("The.Martian.2015.720p.WEBRip.x264-RARBG")),
            ReleaseAgreement::Mismatch
        );
    }

    /// The SCORING consequence of an unknown release, which is the thing that
    /// actually matters: with our own release unknown, no candidate may
    /// receive the release bonus, so a release-claiming candidate must not
    /// outrank one that claims nothing.
    #[test]
    fn no_candidate_receives_a_release_bonus_when_our_own_release_is_unknown() {
        let blind = RankContext {
            release_name: None,
            hearing_impaired: HearingImpairedPreference::Indifferent,
        };
        let claims_a_release = Candidate {
            matched_release: Some(OURS.into()),
            ..candidate("claims")
        };
        let claims_nothing = candidate("silent");

        let ranked = rank_candidates(&[claims_a_release, claims_nothing], &blind);
        assert!(
            ranked.iter().all(|r| r.release_agreement == ReleaseAgreement::Unknown),
            "with our release unknown, every agreement must be Unknown: {ranked:?}"
        );
        assert_eq!(
            ranked[0].score, ranked[1].score,
            "the heaviest term in the model must not fire on a value we do not have"
        );
        assert!(
            ranked[0].score < PARTIAL_RELEASE_MATCH,
            "no release bonus may have been awarded"
        );
    }

    #[test]
    fn missing_release_information_on_either_side_is_unknown_never_a_match() {
        // The heaviest term in the model must never fire on a fabricated
        // basis. If we do not know our own release, nothing gets the bonus.
        assert_eq!(release_agreement(None, Some(OURS)), ReleaseAgreement::Unknown);
        assert_eq!(release_agreement(Some(OURS), None), ReleaseAgreement::Unknown);
        assert_eq!(release_agreement(None, None), ReleaseAgreement::Unknown);
        assert_eq!(release_agreement(Some(""), Some(OURS)), ReleaseAgreement::Unknown);
        assert_eq!(release_agreement(Some("..."), Some(OURS)), ReleaseAgreement::Unknown);
    }

    #[test]
    fn a_year_is_never_mistaken_for_a_release_group() {
        // `Blade.Runner.2049` ends in `-`-less digits; if the group extractor
        // returned "2049" then every 2049-suffixed title would "match".
        assert_eq!(release_group("Blade.Runner.2049.2017.1080p.BluRay-GROUP"), Some("group".into()));
        assert_eq!(release_group("Blade Runner 2049"), None);
        assert_eq!(release_group("Movie-2049"), None, "a numeric tail is not a group");
        assert_eq!(release_group("NoHyphenHere"), None);
    }

    #[test]
    fn a_release_group_survives_a_trailing_file_extension() {
        assert_eq!(release_group("The.Martian.2015-SPARKS.mkv"), Some("sparks".into()));
    }

    // ---------- scoring ----------

    #[test]
    fn an_exact_release_match_outranks_everything_else_combined() {
        // The central ranking rule. A subtitle cut for our exact release must
        // win even against a wildly more popular, SDH-matching alternative.
        let exact = Candidate {
            matched_release: Some(OURS.into()),
            download_count: 1,
            ..candidate("a")
        };
        let popular_mismatch = Candidate {
            matched_release: Some("The.Martian.2015.720p.WEBRip-RARBG".into()),
            download_count: 9_999_999,
            hearing_impaired: true,
            ..candidate("b")
        };
        let ranked = rank_candidates(&[popular_mismatch, exact], &ctx());
        assert_eq!(ranked[0].id, "a", "the exact release match must rank first");
        assert_eq!(ranked[0].release_agreement, ReleaseAgreement::Exact);
    }

    #[test]
    fn a_partial_match_beats_a_mismatch_and_loses_to_an_exact_match() {
        let exact = Candidate {
            matched_release: Some(OURS.into()),
            ..candidate("exact")
        };
        let partial = Candidate {
            matched_release: Some("The.Martian.2015.1080p.BluRay.x264-SPARKS".into()),
            ..candidate("partial")
        };
        let mismatch = Candidate {
            matched_release: Some("Something.Else.720p-OTHER".into()),
            ..candidate("mismatch")
        };
        let ranked = rank_candidates(&[mismatch, partial, exact], &ctx());
        assert_eq!(
            ranked.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["exact", "partial", "mismatch"]
        );
    }

    #[test]
    fn a_machine_generated_subtitle_is_deprioritised_but_never_dropped() {
        let ai = Candidate {
            machine_generated: true,
            download_count: 50_000,
            ..candidate("ai")
        };
        let human = candidate("human");
        let ranked = rank_candidates(&[ai, human], &ctx());

        assert_eq!(ranked.len(), 2, "an AI subtitle must never be filtered out of the list");
        assert_eq!(ranked[0].id, "human", "a human subtitle must outrank a machine-generated one");
        assert_eq!(ranked[1].id, "ai");
        assert!(
            ranked[1].machine_generated,
            "the AI flag must reach the operator, not just the score"
        );
        assert!(
            ranked[1].reasons.iter().any(|r| r.contains("machine-generated")),
            "the operator must be told why it ranked low: {:?}",
            ranked[1].reasons
        );
    }

    #[test]
    fn an_ai_subtitle_still_wins_when_it_is_the_only_candidate_or_the_only_release_match() {
        // The penalty must not be so large it turns "the only subtitle in the
        // world" into "nothing found".
        let only = Candidate {
            machine_generated: true,
            ..candidate("only")
        };
        let ranked = rank_candidates(&[only], &ctx());
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].id, "only");

        // And an AI subtitle for our EXACT release beats a human one for a
        // different release: sync is the thing being optimised.
        let ai_exact = Candidate {
            machine_generated: true,
            matched_release: Some(OURS.into()),
            ..candidate("ai-exact")
        };
        let human_mismatch = Candidate {
            matched_release: Some("Other.Release.720p-XYZ".into()),
            ..candidate("human-other")
        };
        let ranked = rank_candidates(&[human_mismatch, ai_exact], &ctx());
        assert_eq!(ranked[0].id, "ai-exact");
    }

    #[test]
    fn the_ai_penalty_cannot_be_bought_off_with_downloads() {
        // MAX_DOWNLOAD_BONUS must stay far below AI_PENALTY, or popularity
        // would launder a machine-generated subtitle back to the top.
        assert!(
            MAX_DOWNLOAD_BONUS < AI_PENALTY,
            "download popularity must never outweigh the machine-generated penalty"
        );
        assert!(
            HEARING_IMPAIRED_MATCH < AI_PENALTY,
            "an SDH preference must never outweigh the machine-generated penalty"
        );
        assert!(
            MAX_DOWNLOAD_BONUS + HEARING_IMPAIRED_MATCH < PARTIAL_RELEASE_MATCH,
            "no combination of soft signals may outrank a release match"
        );
        assert!(
            PARTIAL_RELEASE_MATCH < EXACT_RELEASE_MATCH,
            "a partial match must never reach an exact one"
        );
    }

    #[test]
    fn hearing_impaired_preference_moves_candidates_within_equal_release_agreement() {
        let sdh = Candidate {
            hearing_impaired: true,
            ..candidate("sdh")
        };
        let plain = candidate("plain");

        let prefer = RankContext {
            hearing_impaired: HearingImpairedPreference::Prefer,
            ..ctx()
        };
        assert_eq!(rank_candidates(&[plain.clone(), sdh.clone()], &prefer)[0].id, "sdh");

        let avoid = RankContext {
            hearing_impaired: HearingImpairedPreference::Avoid,
            ..ctx()
        };
        assert_eq!(rank_candidates(&[sdh.clone(), plain.clone()], &avoid)[0].id, "plain");

        let indifferent = ctx();
        let ranked = rank_candidates(&[sdh, plain], &indifferent);
        assert_eq!(
            ranked[0].score, ranked[1].score,
            "with no preference stated, SDH must not move the score at all"
        );
    }

    #[test]
    fn download_count_only_breaks_ties_and_is_log_scaled_and_capped() {
        assert_eq!(download_bonus(0), 0);
        assert_eq!(download_bonus(-5), 0, "a missing/negative count is never a penalty");
        assert!(download_bonus(10) > download_bonus(1));
        assert!(download_bonus(1_000) > download_bonus(100));
        assert_eq!(download_bonus(10_000_000), MAX_DOWNLOAD_BONUS, "must be capped");
        assert!(download_bonus(i64::MAX) <= MAX_DOWNLOAD_BONUS);

        let popular = Candidate {
            download_count: 100_000,
            ..candidate("popular")
        };
        let obscure = Candidate {
            download_count: 2,
            ..candidate("obscure")
        };
        assert_eq!(rank_candidates(&[obscure, popular], &ctx())[0].id, "popular");
    }

    #[test]
    fn ranking_is_deterministic_and_total_even_for_identical_candidates() {
        // The list is shown to a human who may act on position alone, so the
        // same input must always produce the same order.
        let a = candidate("bbb");
        let b = candidate("aaa");
        let c = candidate("ccc");
        let first = rank_candidates(&[a.clone(), b.clone(), c.clone()], &ctx());
        let second = rank_candidates(&[c, a, b], &ctx());
        assert_eq!(first, second);
        assert_eq!(
            first.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["aaa", "bbb", "ccc"],
            "ties break on provider id, ascending"
        );
    }

    #[test]
    fn an_empty_candidate_list_ranks_to_an_empty_list_not_a_panic() {
        assert!(rank_candidates(&[], &ctx()).is_empty());
    }

    #[test]
    fn every_ranked_candidate_carries_a_reason() {
        // "Why this one" must always be answerable.
        let ranked = rank_candidates(&[candidate("x")], &ctx());
        assert!(!ranked[0].reasons.is_empty());
    }

    #[test]
    fn scoring_does_not_overflow_on_a_hostile_download_count() {
        let hostile = Candidate {
            download_count: i64::MAX,
            machine_generated: true,
            ..candidate("hostile")
        };
        let ranked = rank_candidates(&[hostile], &ctx());
        assert_eq!(ranked.len(), 1);
    }
}
