//! MUSE-11: ranking + rationale + the `/recommend`, `/recommend/on_deck`,
//! `/recommend/gaps` axum handlers.
//!
//! ## Ranking
//! `score_candidate` blends a source-tier weight (on-deck outranks gap
//! outranks taste outranks a not-in-library pick — "finish what you
//! started" beats "here's something new") with the candidate's own
//! `taste_fit`, plus an availability bonus/penalty for checked
//! not-in-library picks. See the module's constants for the exact weights.
//!
//! ## Rationale + grounding
//! [`build_rationale`] always starts from [`template_rationale`] — a
//! deterministic sentence built *only* from `Candidate::facts` (real,
//! computed signals; never invented). When a Chord client is configured, it
//! then asks the local LLM to rephrase those same facts into a more natural
//! sentence, with an explicit "don't invent anything beyond these facts"
//! instruction. Any Chord failure (unconfigured, unreachable, malformed
//! response) falls back to the deterministic template — a recommendation
//! never fails just because the LLM is down.
//!
//! ## "Why this" (MUSEX-04, Plane TERM #380)
//! [`RecommendationItem::because`] is a distinct, opt-in surface from the
//! rationale above: a short "because…" line built by
//! `crate::taste_review::because::because_line` from the same reasoning
//! trace MUSET-07's `include_trace` exposes, naming the real top signal(s)
//! that drove the score. Gated behind its own `include_because` flag
//! (`RecommendRequest`/`AccountLimitQuery`) — additive, same posture as
//! `include_trace`, so a caller that doesn't ask for it sees no change.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::curation::candidates::{self, Candidate, CandidateSource};
use crate::error::MuseResult;
use crate::http::AppState;
use crate::models::availability::Availability;
use crate::models::media_metadata::MediaKind;
use crate::taste_model::chord_client::{ChordClient, DEFAULT_MODEL};
use crate::taste_review::because::because_line;
use crate::taste_review::trace::{build_reasoning_trace, ReasoningTrace};

pub const DEFAULT_RECOMMEND_LIMIT: i64 = 10;
pub const MAX_RECOMMEND_LIMIT: i64 = 50;

/// TMDb region MUSE-19's trending ingest is (currently) run for. Not
/// secret-shaped, not host/infra-shaped — a content-region code, so it's a
/// plain constant rather than a `Config` field; a future multi-region build
/// would thread this through from the request/account instead.
const DEFAULT_TRENDING_REGION: &str = "US";

/// Source-tier weight `score_candidate` scales `taste_fit` by — "finish what
/// you started" (on-deck) outranks "there's probably more of this"  (gap),
/// which outranks a fresh "you'd love this" (taste), which outranks a
/// not-in-library pick (real, but requires acquiring something first).
pub(crate) fn source_weight(source: CandidateSource) -> f64 {
    match source {
        CandidateSource::OnDeck => 1.0,
        CandidateSource::Gap => 0.85,
        CandidateSource::Taste => 0.7,
        CandidateSource::AvailableNow => 0.6,
    }
}

/// Score bonus for a not-in-library pick MUSE-16 has confirmed is
/// grabbable right now (`availability.release_count > 0`).
const AVAILABILITY_GRABBABLE_BONUS: f64 = 0.15;
/// Score penalty for a not-in-library pick MUSE-16 has checked and found
/// nothing for. Still ranked (a taste-perfect pick worth knowing about even
/// if it's not grabbable this second), just deprioritized below anything
/// that's actually actionable today.
const AVAILABILITY_UNAVAILABLE_PENALTY: f64 = 0.1;

/// `score = source_weight(source) * taste_fit`, then availability-adjusted
/// for a candidate with a checked [`Availability`] rollup. Never negative
/// (clamped to `0.0`) so a heavily-penalized candidate still sorts, rather
/// than inverting the ranking.
pub fn score_candidate(candidate: &Candidate) -> f64 {
    let mut score = source_weight(candidate.source) * candidate.taste_fit;

    if let Some(availability) = &candidate.availability {
        if availability.release_count > 0 {
            score += AVAILABILITY_GRABBABLE_BONUS;
        } else {
            score -= AVAILABILITY_UNAVAILABLE_PENALTY;
        }
    }

    score.max(0.0)
}

/// Score every candidate and sort descending by score. Ties are broken by
/// insertion order (`sort_by`'s stability) rather than a secondary key —
/// deliberately not specified further, since the spec doesn't call for a
/// specific tie-break and an unstable one would make output non-reproducible
/// across otherwise-identical runs.
pub fn rank_candidates(candidates: Vec<Candidate>) -> Vec<(Candidate, f64)> {
    let mut scored: Vec<(Candidate, f64)> = candidates
        .into_iter()
        .map(|c| {
            let score = score_candidate(&c);
            (c, score)
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

/// Build the deterministic, always-available rationale sentence directly
/// from `candidate.facts` — every word traces to a real, computed signal.
/// This is both the LLM-down fallback and the ground truth `build_rationale`
/// grounds its Chord prompt in.
pub fn template_rationale(candidate: &Candidate) -> String {
    let facts = candidate.facts.join("; ");
    match candidate.source {
        CandidateSource::OnDeck => format!("Continue \"{}\" — {facts}.", candidate.title),
        CandidateSource::Gap => format!("\"{}\" — {facts}.", candidate.title),
        CandidateSource::Taste => {
            format!("\"{}\" is recommended because {facts}.", candidate.title)
        }
        CandidateSource::AvailableNow => format!("\"{}\" — {facts}.", candidate.title),
    }
}

/// Produce the rationale for one candidate: the templated sentence when no
/// Chord client is configured or the call fails, otherwise a Chord-phrased
/// sentence explicitly instructed to use only the given facts. Never errors
/// — a rationale-generation problem degrades to the template, it never fails
/// the recommendation itself.
pub async fn build_rationale(chord: Option<&ChordClient>, candidate: &Candidate) -> String {
    let template = template_rationale(candidate);

    let Some(client) = chord else {
        return template;
    };

    let system = "You are Muse, a private media curation assistant. Write ONE short, natural-sounding \
        sentence recommending the given title to the account. You MUST ground the sentence ONLY in the \
        facts listed below — never invent a plot detail, rating, cast member, or signal that isn't listed. \
        Do not add a preamble or explanation, just the one sentence.";
    let user = format!(
        "Title: {}\nFacts: {}\nWrite the one-sentence recommendation now.",
        candidate.title,
        candidate.facts.join("; ")
    );

    match client.chat_completion(DEFAULT_MODEL, system, &user).await {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(
                error = %e,
                media_metadata_id = candidate.media_metadata_id,
                "MUSE-11: chord rationale generation failed; falling back to the templated rationale"
            );
            template
        }
    }
}

fn clamp_limit(requested: Option<i64>) -> i64 {
    requested
        .unwrap_or(DEFAULT_RECOMMEND_LIMIT)
        .clamp(1, MAX_RECOMMEND_LIMIT)
}

/// One ranked recommendation, as returned over HTTP.
#[derive(Debug, Serialize)]
pub struct RecommendationItem {
    pub media_metadata_id: i64,
    pub media_item_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
    pub kind: MediaKind,
    pub source: CandidateSource,
    pub score: f64,
    pub rationale: String,
    pub availability: Option<Availability>,
    /// MUSET-07 (Plane TERM #372): the INTERROGABLE reasoning trace behind
    /// this recommendation — which signals drove it, their weights, and the
    /// path/rule that produced it (see `crate::taste_review::trace`). Only
    /// populated when the caller opts in via `RecommendRequest::include_trace`
    /// / the on-deck/gap query's `include_trace`; `None` (and therefore
    /// omitted from the JSON entirely) otherwise, so a normal caller's
    /// response shape is byte-for-byte unchanged by this feature existing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<ReasoningTrace>,
    /// MUSEX-04 (Plane TERM #380): a concise, Lumina-voiced "because…" line
    /// naming the real top signal(s) behind this recommendation — see
    /// `crate::taste_review::because::because_line`. Grounded strictly in
    /// the same signals `trace` above is built from (never fabricated).
    /// Only populated when the caller opts in via
    /// `RecommendRequest::include_because` / the on-deck/gap query's
    /// `include_because`; `None` (and therefore omitted from the JSON
    /// entirely) otherwise, so a caller that doesn't ask for it gets
    /// byte-for-byte the same response shape as before this feature
    /// existed — same additive posture as `trace`/`include_trace`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub because: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RecommendResponse {
    pub items: Vec<RecommendationItem>,
}

async fn score_and_explain(
    chord: Option<&ChordClient>,
    ranked: Vec<(Candidate, f64)>,
    limit: i64,
    include_trace: bool,
    include_because: bool,
) -> Vec<RecommendationItem> {
    let mut items = Vec::with_capacity(ranked.len().min(limit.max(0) as usize));
    for (candidate, score) in ranked.into_iter().take(limit.max(0) as usize) {
        let rationale = build_rationale(chord, &candidate).await;
        // MUSEX-04: `because_line` is a pure function of the same trace
        // `include_trace` already builds, so when both flags are set we
        // build the trace once and reuse it for both — never recomputed,
        // never a second source of truth for "which signals drove this."
        let full_trace =
            (include_trace || include_because).then(|| build_reasoning_trace(&candidate, score));
        let because = if include_because {
            full_trace.as_ref().map(because_line)
        } else {
            None
        };
        let trace = if include_trace { full_trace } else { None };
        items.push(RecommendationItem {
            media_metadata_id: candidate.media_metadata_id,
            media_item_id: candidate.media_item_id,
            title: candidate.title,
            year: candidate.year,
            kind: candidate.kind,
            source: candidate.source,
            score,
            rationale,
            availability: candidate.availability,
            trace,
            because,
        });
    }
    items
}

#[derive(Debug, Deserialize)]
pub struct RecommendRequest {
    pub account_id: i64,
    /// Free-text context hint (e.g. "friday night", "kids are asleep") —
    /// accepted per spec but not yet incorporated into ranking (a v0
    /// divergence: MUSE-11 has no context-aware scoring formula defined
    /// yet, only the taste-context centroids MUSE-10 already computes,
    /// which this v0 doesn't consult). Reserved for a follow-up.
    #[serde(default)]
    #[allow(dead_code)]
    pub context: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// When `false` (the default), not-in-library picks MUSE-16 has
    /// checked and found nothing grabbable for are dropped entirely rather
    /// than merely deprioritized — most callers want actionable picks only.
    #[serde(default)]
    pub include_unavailable: bool,
    /// MUSET-07: when `true`, each returned item carries its
    /// [`RecommendationItem::trace`] — the interrogable reasoning trace an
    /// adversarial review can critique. Defaults to `false` (omitted),
    /// keeping the default response shape unchanged.
    #[serde(default)]
    pub include_trace: bool,
    /// MUSEX-04: when `true`, each returned item carries its
    /// [`RecommendationItem::because`] — the concise "because…" narration
    /// line. Independent of `include_trace` (a caller can ask for the
    /// human-readable line without the full machine-shaped trace, or vice
    /// versa). Defaults to `false` (omitted), keeping the default response
    /// shape unchanged.
    #[serde(default)]
    pub include_because: bool,
}

/// `POST /recommend` — the full MUSE-11 ranked list: on-deck + gap + taste +
/// availability-aware not-in-library candidates, deduplicated, scored, and
/// explained. Strictly scoped to `req.account_id` — never blends another
/// account's signals in (multi-user isolation, per spec).
pub async fn recommend_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RecommendRequest>,
) -> MuseResult<Json<RecommendResponse>> {
    let limit = clamp_limit(req.limit);

    let mut pool_candidates = Vec::new();
    pool_candidates
        .extend(candidates::gather_on_deck_candidates(&state.pool, req.account_id, limit).await?);
    pool_candidates
        .extend(candidates::gather_gap_candidates(&state.pool, req.account_id, limit).await?);
    pool_candidates
        .extend(candidates::gather_taste_candidates(&state.pool, req.account_id, limit).await?);

    let mut not_in_library =
        candidates::gather_available_now_candidates(&state.pool, DEFAULT_TRENDING_REGION, limit)
            .await?;
    if !req.include_unavailable {
        not_in_library.retain(|c| {
            c.availability
                .as_ref()
                .map(|a| a.release_count > 0)
                .unwrap_or(false)
        });
    }
    pool_candidates.extend(not_in_library);

    let deduped = candidates::dedup_candidates(pool_candidates);
    let ranked = rank_candidates(deduped);

    let chord = ChordClient::from_config(&state.config);
    let items = score_and_explain(
        chord.as_ref(),
        ranked,
        limit,
        req.include_trace,
        req.include_because,
    )
    .await;

    Ok(Json(RecommendResponse { items }))
}

#[derive(Debug, Deserialize)]
pub struct AccountLimitQuery {
    pub account_id: i64,
    #[serde(default)]
    pub limit: Option<i64>,
    /// MUSET-07: same opt-in trace flag as `RecommendRequest::include_trace`.
    #[serde(default)]
    pub include_trace: bool,
    /// MUSEX-04: same opt-in "because…" narration flag as
    /// `RecommendRequest::include_because`.
    #[serde(default)]
    pub include_because: bool,
}

/// `GET /recommend/on_deck?account_id=` — continue-watching only.
pub async fn on_deck_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AccountLimitQuery>,
) -> MuseResult<Json<RecommendResponse>> {
    let limit = clamp_limit(q.limit);
    let candidates =
        candidates::gather_on_deck_candidates(&state.pool, q.account_id, limit).await?;
    let ranked = rank_candidates(candidates);
    let chord = ChordClient::from_config(&state.config);
    let items = score_and_explain(
        chord.as_ref(),
        ranked,
        limit,
        q.include_trace,
        q.include_because,
    )
    .await;
    Ok(Json(RecommendResponse { items }))
}

/// `GET /recommend/gaps?account_id=` — gap analysis only.
pub async fn gaps_handler(
    State(state): State<Arc<AppState>>,
    Query(q): Query<AccountLimitQuery>,
) -> MuseResult<Json<RecommendResponse>> {
    let limit = clamp_limit(q.limit);
    let candidates = candidates::gather_gap_candidates(&state.pool, q.account_id, limit).await?;
    let ranked = rank_candidates(candidates);
    let chord = ChordClient::from_config(&state.config);
    let items = score_and_explain(
        chord.as_ref(),
        ranked,
        limit,
        q.include_trace,
        q.include_because,
    )
    .await;
    Ok(Json(RecommendResponse { items }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(
        source: CandidateSource,
        taste_fit: f64,
        availability: Option<Availability>,
    ) -> Candidate {
        Candidate {
            media_metadata_id: 1,
            media_item_id: Some(1),
            title: "Arrival".to_string(),
            year: Some(2016),
            kind: MediaKind::Movie,
            source,
            taste_fit,
            facts: vec![
                "it's a 92% match to your overall taste profile".to_string(),
                "you rate sci-fi highly".to_string(),
            ],
            availability,
        }
    }

    fn availability(
        release_count: i32,
        best_seeders: Option<i32>,
        freeleech: bool,
    ) -> Availability {
        Availability {
            media_metadata_id: 1,
            best_quality: Some("1080p".to_string()),
            best_seeders,
            release_count,
            has_freeleech: freeleech,
            cheapest_size_bytes: None,
            newest_release_at: None,
            computed_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn on_deck_outranks_gap_outranks_taste_outranks_available_now_at_equal_taste_fit() {
        let on_deck = score_candidate(&candidate(CandidateSource::OnDeck, 0.8, None));
        let gap = score_candidate(&candidate(CandidateSource::Gap, 0.8, None));
        let taste = score_candidate(&candidate(CandidateSource::Taste, 0.8, None));
        let available_now = score_candidate(&candidate(CandidateSource::AvailableNow, 0.8, None));

        assert!(
            on_deck > gap,
            "on-deck ({on_deck}) must outrank gap ({gap}) at equal taste_fit"
        );
        assert!(
            gap > taste,
            "gap ({gap}) must outrank taste ({taste}) at equal taste_fit"
        );
        assert!(
            taste > available_now,
            "taste ({taste}) must outrank an unchecked available-now pick ({available_now}) at equal taste_fit"
        );
    }

    #[test]
    fn grabbable_availability_boosts_score_above_unavailable() {
        let grabbable = score_candidate(&candidate(
            CandidateSource::AvailableNow,
            0.5,
            Some(availability(3, Some(40), false)),
        ));
        let unavailable = score_candidate(&candidate(
            CandidateSource::AvailableNow,
            0.5,
            Some(availability(0, None, false)),
        ));

        assert!(
            grabbable > unavailable,
            "a grabbable-now pick ({grabbable}) must rank above a checked-but-unavailable one ({unavailable})"
        );
    }

    #[test]
    fn score_candidate_never_goes_negative() {
        let unavailable = candidate(
            CandidateSource::AvailableNow,
            0.0,
            Some(availability(0, None, false)),
        );
        assert_eq!(score_candidate(&unavailable), 0.0);
    }

    #[test]
    fn rank_candidates_sorts_descending_by_score() {
        let candidates = vec![
            candidate(CandidateSource::AvailableNow, 0.5, None),
            candidate(CandidateSource::OnDeck, 0.9, None),
            candidate(CandidateSource::Taste, 0.6, None),
        ];

        let ranked = rank_candidates(candidates);
        let scores: Vec<f64> = ranked.iter().map(|(_, s)| *s).collect();
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
        assert_eq!(
            scores, sorted,
            "rank_candidates must return descending score order"
        );
        assert_eq!(
            ranked[0].0.source,
            CandidateSource::OnDeck,
            "on-deck should win this mix"
        );
    }

    #[tokio::test]
    async fn score_and_explain_omits_because_by_default() {
        let ranked = rank_candidates(vec![candidate(CandidateSource::Taste, 0.92, None)]);
        let items = score_and_explain(None, ranked, 10, false, false).await;

        assert_eq!(
            items[0].because, None,
            "MUSEX-04: because must be None unless include_because is set — additive, unchanged default shape"
        );
        assert!(items[0].trace.is_none());
    }

    #[tokio::test]
    async fn score_and_explain_populates_because_when_opted_in() {
        let ranked = rank_candidates(vec![candidate(CandidateSource::Taste, 0.92, None)]);
        let items = score_and_explain(None, ranked, 10, false, true).await;

        let because = items[0]
            .because
            .as_ref()
            .expect("include_because=true must populate RecommendationItem::because");
        assert!(
            because.contains("92% match to your overall taste profile"),
            "because must be grounded in the candidate's real top fact: {because}"
        );
        // Independent of include_trace: because can be requested without
        // the full machine-shaped trace.
        assert!(items[0].trace.is_none());
    }

    #[tokio::test]
    async fn score_and_explain_because_and_trace_are_independently_opt_in() {
        let ranked = rank_candidates(vec![candidate(CandidateSource::Taste, 0.92, None)]);
        let items = score_and_explain(None, ranked, 10, true, false).await;

        assert!(
            items[0].trace.is_some(),
            "include_trace alone must still populate trace"
        );
        assert_eq!(
            items[0].because, None,
            "include_trace alone must not implicitly populate because"
        );
    }

    #[tokio::test]
    async fn score_and_explain_because_matches_direct_because_line_call() {
        // MUSEX-04 ties directly to MUSET-07's trace: the because line
        // computed through the recommend pipeline must be identical to
        // calling `because_line` directly on the same candidate's trace —
        // no separate/divergent computation path.
        let c = candidate(CandidateSource::Taste, 0.92, None);
        let score = score_candidate(&c);
        let expected = because_line(&build_reasoning_trace(&c, score));

        let ranked = rank_candidates(vec![c]);
        let items = score_and_explain(None, ranked, 10, false, true).await;

        assert_eq!(items[0].because.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn template_rationale_cites_the_real_signals_in_facts() {
        let c = candidate(CandidateSource::Taste, 0.92, None);
        let rationale = template_rationale(&c);

        assert!(rationale.contains("Arrival"));
        assert!(
            rationale.contains("92% match to your overall taste profile"),
            "rationale must cite the actual computed match percentage: {rationale}"
        );
        assert!(
            rationale.contains("you rate sci-fi highly"),
            "rationale must cite the actual top genre affinity: {rationale}"
        );
    }

    #[test]
    fn template_rationale_on_deck_cites_percent_complete() {
        let mut c = candidate(CandidateSource::OnDeck, 0.6, None);
        c.facts = vec!["you're 61% through it — pick it back up".to_string()];
        let rationale = template_rationale(&c);
        assert!(
            rationale.contains("61%"),
            "on-deck rationale must cite the real percent-complete: {rationale}"
        );
    }

    #[tokio::test]
    async fn build_rationale_falls_back_to_template_when_no_chord_configured() {
        let c = candidate(CandidateSource::Taste, 0.92, None);
        let rationale = build_rationale(None, &c).await;
        assert_eq!(rationale, template_rationale(&c));
    }

    #[tokio::test]
    async fn build_rationale_uses_chord_output_when_available() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"choices": [{"message": {"role": "assistant", "content": "You'll love Arrival — it's a near-perfect match for your sci-fi taste."}}]}"#);
        });

        let client = ChordClient::new(server.base_url()).expect("client should construct");
        let c = candidate(CandidateSource::Taste, 0.92, None);
        let rationale = build_rationale(Some(&client), &c).await;

        assert_eq!(
            rationale,
            "You'll love Arrival — it's a near-perfect match for your sci-fi taste."
        );
    }

    #[tokio::test]
    async fn build_rationale_falls_back_to_template_when_chord_call_fails() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(500).body("model not loaded");
        });

        let client = ChordClient::new(server.base_url()).expect("client should construct");
        let c = candidate(CandidateSource::Taste, 0.92, None);
        let rationale = build_rationale(Some(&client), &c).await;

        assert_eq!(
            rationale,
            template_rationale(&c),
            "a failing chord call must never fail the recommendation — it degrades to the template"
        );
    }
}
