//! MUSE-11: candidate generation — the four independent sources of
//! recommendation candidates, and the de-dup pass that reconciles a title
//! surfaced by more than one source into a single, richer candidate.
//!
//! Every candidate carries [`Candidate::facts`]: plain-English, *grounded*
//! statements about the account's actual data (a real percent-complete, a
//! real affinity weight, a real seeder count, ...) — never an invented
//! detail. `curation::recommend` builds both the deterministic templated
//! rationale and the Chord-LLM prompt directly from this list, so whatever
//! is true here is the only thing either rationale path can ever say.

use std::collections::HashMap;

use sqlx::PgPool;

use crate::error::MuseResult;
use crate::models::availability::Availability;
use crate::models::media_metadata::MediaKind;
use crate::repo;

/// Which of the four MUSE-11 sources produced a candidate. Serialized in
/// kebab-case to match the founding spec's own tag vocabulary verbatim
/// (`[taste/on-deck/gap/available-now]`, S96 MUSE-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateSource {
    /// "More like your taste": nearest library items to the account's
    /// `taste_profile.overall_centroid` (MUSE-08/MUSE-10), excluding
    /// anything already finished.
    Taste,
    /// Continue-watching: a `watch_stats` row that's started but neither
    /// finished nor abandoned.
    OnDeck,
    /// Gap analysis: an engaged-with show whose metadata signals more
    /// content exists beyond the library (see
    /// `repo::media_item::list_show_gap_candidates`).
    Gap,
    /// Availability-aware, not-in-library pick: a trending title Muse has
    /// catalog metadata for but doesn't own, surfaced with its MUSE-16
    /// grabbability.
    AvailableNow,
}

/// One recommendation candidate — a title plus the real signals that
/// justify recommending it, prior to ranking/scoring
/// ([`crate::curation::recommend::score_candidate`]).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub media_metadata_id: i64,
    /// `None` for an [`CandidateSource::AvailableNow`] pick that has no
    /// library instance at all (that's the point of that source).
    pub media_item_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
    pub kind: MediaKind,
    pub source: CandidateSource,
    /// Normalized `[0.0, 1.0]` signal strength from the source itself
    /// (cosine similarity for taste, percent-complete for on-deck, ...) —
    /// the input `recommend::score_candidate` scales by the source's
    /// weight; not a final score.
    pub taste_fit: f64,
    /// Grounded, human-readable real signals backing this candidate. Never
    /// empty for a candidate this module produces — every source attaches
    /// at least one fact.
    pub facts: Vec<String>,
    /// MUSE-16 grabbability rollup, when known. `None` means "not checked"
    /// (in-library taste/on-deck/gap picks don't need it); `Some` with
    /// `release_count == 0` means "checked, nothing found."
    pub availability: Option<Availability>,
}

/// How large a pool to over-fetch from the vector nearest-neighbor lookup
/// before filtering out already-finished items, so a taste-heavy library
/// (lots of finished titles near the centroid) still yields `limit` fresh
/// picks rather than starving out early.
const TASTE_CANDIDATE_POOL_MULTIPLIER: i64 = 4;

/// Genre/decade watch-stats threshold (percent) above which an
/// in-progress show counts as "meaningfully engaged with" for gap analysis,
/// even without a full `finished_count`.
const GAP_ENGAGEMENT_MIN_PERCENT: f32 = 60.0;

/// Return the key with the largest numeric value in a flat `{key: weight}`
/// JSON object (e.g. `taste_profile.genre_affinity`) — used to ground a
/// taste-tier rationale in the account's actual strongest affinity, never a
/// guess. Returns `None` for an empty/non-object value.
pub(crate) fn top_affinity_key(value: &serde_json::Value) -> Option<String> {
    let obj = value.as_object()?;
    obj.iter()
        .filter_map(|(k, v)| v.as_f64().map(|n| (k.clone(), n)))
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(k, _)| k)
}

/// MUSE-11 taste-tier candidates: nearest library items to the account's
/// `taste_profile.overall_centroid` (the MUSE-10 taste model), excluding
/// anything the account has already finished. Returns an empty `Vec` (never
/// an error) when the account has no taste profile yet or no computed
/// centroid — a cold-start account simply gets nothing from this source,
/// same graceful-degrade posture as every other optional signal in this
/// crate.
pub async fn gather_taste_candidates(
    pool: &PgPool,
    account_id: i64,
    limit: i64,
) -> MuseResult<Vec<Candidate>> {
    let Some(profile) = repo::taste::get_profile(pool, account_id).await? else {
        return Ok(Vec::new());
    };
    let Some(centroid) = profile.overall_centroid.clone() else {
        return Ok(Vec::new());
    };

    let top_genre = top_affinity_key(&profile.genre_affinity);

    let pool_size = (limit * TASTE_CANDIDATE_POOL_MULTIPLIER).max(limit);
    let matches = crate::embed::nearest(pool, centroid.as_slice().to_vec(), pool_size).await?;

    let mut out = Vec::new();
    for m in matches {
        if out.len() as i64 >= limit {
            break;
        }

        let media_item_id = m.entity_id;

        // Already-finished titles aren't fresh picks for the taste tier —
        // on-deck/gap own "you're into this," taste owns "you'd love this
        // too." A stats-lookup failure (no row yet) means "never watched,"
        // which is fine to recommend.
        if let Ok(Some(stats)) =
            repo::watch_stats::get_watch_stats(pool, account_id, media_item_id).await
        {
            if stats.finished_count > 0 {
                continue;
            }
        }

        let Ok(item) = repo::media_item::get(pool, media_item_id).await else {
            continue; // stale embedding pointing at a since-deleted item
        };
        let Ok(meta) = repo::media_metadata::get(pool, item.media_metadata_id).await else {
            continue;
        };

        // pgvector cosine distance ranges [0.0, 2.0]; map to a [0.0, 1.0]
        // similarity so every source's `taste_fit` is on the same scale.
        let similarity = (1.0 - (m.distance / 2.0)).clamp(0.0, 1.0);

        let mut facts = vec![format!(
            "it's a {:.0}% match to your overall taste profile",
            similarity * 100.0
        )];
        if let Some(genre) = &top_genre {
            facts.push(format!("you rate {genre} highly"));
        }

        out.push(Candidate {
            media_metadata_id: meta.id,
            media_item_id: Some(media_item_id),
            title: meta.title,
            year: meta.year,
            kind: meta.kind,
            source: CandidateSource::Taste,
            taste_fit: similarity,
            facts,
            availability: None,
        });
    }

    Ok(out)
}

/// MUSE-11 on-deck / continue-watching candidates: thin wrapper over
/// `repo::watch_stats::list_on_deck`, translated into [`Candidate`]s with a
/// fact grounded in the actual `avg_percent`.
///
/// ## Consent at the source (MUSEX-WIRE-01, Plane TERM #398)
/// This is a taste-source PRIMITIVE, not a consent gate — it takes a bare
/// `account_id` and does no opt-in check of its own, same as every other
/// `gather_*` function in this module. It is deliberately `pub(crate)`, not
/// `pub`: the MUSEX-CAP-SEC capstone finding (finding 1) was that this
/// function's old `pub` visibility let a caller wire straight to it and
/// bypass opt-in-by-construction with no compile error. `pub(crate)` makes
/// that impossible for any caller outside this crate, while the two real
/// in-crate callers stay exactly as they were: `crate::curation::recommend`'s
/// `/recommend` HTTP handlers (the account-owner's own dashboard — no
/// Discord-friend consent domain applies there, this crate's
/// `taste_opt_in` model is specifically about a *friend* accessing
/// *someone else's* taste) and `crate::discord::bot::respond`, which is the
/// SANCTIONED, opted-in-identity-gated door for the Discord-friend
/// consent domain — see that function's module doc for how
/// `decide_response_mode`'s `TasteAware` arm is the only path that can ever
/// reach this function with a friend-resolved `account_id`.
pub(crate) async fn gather_on_deck_candidates(
    pool: &PgPool,
    account_id: i64,
    limit: i64,
) -> MuseResult<Vec<Candidate>> {
    let rows = repo::watch_stats::list_on_deck(pool, account_id, limit).await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let percent = r.avg_percent.unwrap_or(0.0).clamp(0.0, 100.0);
            Candidate {
                media_metadata_id: r.media_metadata_id,
                media_item_id: Some(r.media_item_id),
                title: r.title,
                year: r.year,
                kind: r.kind,
                source: CandidateSource::OnDeck,
                taste_fit: (percent as f64 / 100.0).clamp(0.0, 1.0),
                facts: vec![format!("you're {percent:.0}% through it — pick it back up")],
                availability: None,
            }
        })
        .collect())
}

/// MUSE-11 gap-analysis candidates: thin wrapper over
/// `repo::media_item::list_show_gap_candidates`, translated into
/// [`Candidate`]s with a fact grounded in the actual `next_airing`/`status`.
pub async fn gather_gap_candidates(
    pool: &PgPool,
    account_id: i64,
    limit: i64,
) -> MuseResult<Vec<Candidate>> {
    let rows = repo::media_item::list_show_gap_candidates(pool, account_id, limit).await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let fact = if let Some(next) = r.next_airing {
                format!("a new episode is scheduled for {}", next.date_naive())
            } else if let Some(status) = &r.status {
                format!("its status (\"{status}\") means it isn't done airing yet")
            } else {
                "you're deep into this show".to_string()
            };

            Candidate {
                media_metadata_id: r.media_metadata_id,
                media_item_id: Some(r.media_item_id),
                title: r.title,
                year: r.year,
                kind: MediaKind::Show,
                source: CandidateSource::Gap,
                taste_fit: (r.avg_percent.unwrap_or(GAP_ENGAGEMENT_MIN_PERCENT) as f64 / 100.0)
                    .clamp(0.0, 1.0),
                facts: vec![fact],
                availability: None,
            }
        })
        .collect())
}

/// MUSE-11 availability-aware, not-in-library candidates: trending titles
/// Muse has catalog metadata for but nobody owns, joined against MUSE-16
/// `availability` so the rationale can say "grabbable now (N seeders)" vs
/// "not currently available" — a real, checked signal either way, never a
/// guess.
pub async fn gather_available_now_candidates(
    pool: &PgPool,
    region: &str,
    limit: i64,
) -> MuseResult<Vec<Candidate>> {
    let rows = repo::trending::list_trending_not_in_library(pool, region, limit).await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let availability = repo::availability::get(pool, r.media_metadata_id)
            .await
            .ok();
        let grabbable = availability
            .as_ref()
            .map(|a| a.release_count > 0)
            .unwrap_or(false);

        let mut facts = vec![format!(
            "it's trending right now (popularity {:.0})",
            r.popularity.unwrap_or(0.0)
        )];
        match &availability {
            Some(a) if grabbable => {
                let freeleech = if a.has_freeleech { ", freeleech" } else { "" };
                facts.push(format!(
                    "grabbable now ({} seeders{freeleech})",
                    a.best_seeders.unwrap_or(0)
                ));
            }
            Some(_) => facts.push("not currently available".to_string()),
            None => facts.push("availability hasn't been checked yet".to_string()),
        }

        let popularity_score = (r.popularity.unwrap_or(0.0) as f64 / 100.0).clamp(0.0, 1.0);

        out.push(Candidate {
            media_metadata_id: r.media_metadata_id,
            media_item_id: None,
            title: r.title,
            year: r.year,
            kind: r.kind,
            source: CandidateSource::AvailableNow,
            taste_fit: popularity_score,
            facts,
            availability,
        });
    }

    Ok(out)
}

/// Priority order used to pick which source "wins" a `media_metadata_id`
/// two-or-more sources both surfaced (lower = wins). On-deck/gap (already in
/// progress / already owned) outrank a fresh taste pick, which in turn
/// outranks a not-in-library pick of the *same* title — that shouldn't
/// normally happen (an owned title can't also be not-in-library), but the
/// ordering is total regardless so `dedup_candidates` never has to guess.
fn source_priority(source: CandidateSource) -> u8 {
    match source {
        CandidateSource::OnDeck => 0,
        CandidateSource::Gap => 1,
        CandidateSource::Taste => 2,
        CandidateSource::AvailableNow => 3,
    }
}

/// Collapse candidates from multiple sources that name the same
/// `media_metadata_id` into one, keeping the highest-priority source's
/// fields but merging every source's `facts` so no grounded signal is lost.
/// Order of the output is unspecified (ranking happens afterward in
/// `recommend::rank_candidates`).
pub fn dedup_candidates(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut by_id: HashMap<i64, Candidate> = HashMap::new();

    for candidate in candidates {
        match by_id.get_mut(&candidate.media_metadata_id) {
            None => {
                by_id.insert(candidate.media_metadata_id, candidate);
            }
            Some(existing) => {
                if source_priority(candidate.source) < source_priority(existing.source) {
                    let mut merged = candidate;
                    merged.facts.extend(existing.facts.drain(..));
                    *existing = merged;
                } else {
                    existing.facts.extend(candidate.facts);
                }
            }
        }
    }

    by_id.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: i64, source: CandidateSource, fact: &str) -> Candidate {
        Candidate {
            media_metadata_id: id,
            media_item_id: Some(id),
            title: format!("Title {id}"),
            year: Some(2020),
            kind: MediaKind::Movie,
            source,
            taste_fit: 0.5,
            facts: vec![fact.to_string()],
            availability: None,
        }
    }

    #[test]
    fn top_affinity_key_picks_the_max_weight() {
        let value = serde_json::json!({"scifi": 1.2, "horror": 3.4, "comedy": 0.1});
        assert_eq!(top_affinity_key(&value), Some("horror".to_string()));
    }

    #[test]
    fn top_affinity_key_returns_none_for_empty_object() {
        assert_eq!(top_affinity_key(&serde_json::json!({})), None);
    }

    #[test]
    fn dedup_candidates_keeps_higher_priority_source() {
        let candidates = vec![
            candidate(1, CandidateSource::Taste, "taste fact"),
            candidate(1, CandidateSource::OnDeck, "on-deck fact"),
        ];

        let deduped = dedup_candidates(candidates);
        assert_eq!(deduped.len(), 1);
        let winner = &deduped[0];
        assert_eq!(
            winner.source,
            CandidateSource::OnDeck,
            "on-deck must win over taste for the same title"
        );
        assert!(winner.facts.iter().any(|f| f == "on-deck fact"));
        assert!(
            winner.facts.iter().any(|f| f == "taste fact"),
            "facts from the losing source must still be merged in, not discarded"
        );
    }

    #[test]
    fn dedup_candidates_leaves_distinct_titles_untouched() {
        let candidates = vec![
            candidate(1, CandidateSource::Taste, "fact a"),
            candidate(2, CandidateSource::Gap, "fact b"),
        ];

        let deduped = dedup_candidates(candidates);
        assert_eq!(deduped.len(), 2);
    }
}
