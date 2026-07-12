//! MUSE-12: the five event-driven proactive-content generators, plus the
//! cooldown/dedup orchestrator that turns their output into `proactive_items`
//! rows.
//!
//! Every generator returns [`GeneratedItem`]s carrying `facts`: plain-English
//! statements grounded in real, computed signals — the same discipline
//! `curation::candidates::Candidate::facts` uses for MUSE-11. [`build_message`]
//! builds both the deterministic template (always available) and, when Chord
//! is configured, a natural-language phrasing explicitly instructed to invent
//! nothing beyond those facts. A Chord failure never blocks generation — it
//! just falls back to the template, same posture as `curation::recommend`.
//!
//! Each generator is independently fallible (a DB error, a missing signal) —
//! [`generate_for_account`] treats a single generator's failure as "produced
//! no candidates this pass" (logged, not propagated), so one broken/degraded
//! signal source never stops the others from running.

use chrono::{DateTime, Datelike, Duration, Timelike, Utc, Weekday};
use serde_json::{json, Value as Json};
use sqlx::PgPool;

use crate::curation::candidates;
use crate::enrichment::cache::{kind as enrichment_kind, GetsGoodPayload};
use crate::error::MuseResult;
use crate::models::proactive_item::{NewProactiveItemDeduped, ProactiveItem};
use crate::repo;
use crate::taste_model::chord_client::{ChordClient, DEFAULT_MODEL};

/// `proactive_items.kind` values this module produces.
pub mod kind {
    pub const NEW_SEASON: &str = "new_season";
    pub const FRIDAY_EVENING: &str = "friday_evening";
    pub const ABANDON_INSIGHT: &str = "abandon_insight";
    pub const GRAB_WINDOW: &str = "grab_window";
    pub const ZEITGEIST: &str = "zeitgeist";
}

/// TMDb region MUSE-19's trending ingest is (currently) run for — mirrors
/// `curation::recommend::DEFAULT_TRENDING_REGION` (same constant, kept local
/// rather than shared since it's a content-region default, not
/// infra-shaped, and this module has no other dependency on that one).
const DEFAULT_TRENDING_REGION: &str = "US";

/// How many candidates each generator considers per account before cooldown
/// filtering — generous enough that a busy account still gets a few fresh
/// nudges per pass, small enough this stays a lightweight per-tick query.
const GENERATOR_CANDIDATE_LIMIT: i64 = 10;

/// `does_it_get_good`'s `patience_payoff` threshold above which the
/// abandonment-insight generator treats forum consensus as "worth another
/// shot" grounding, even with no specific episode number extracted.
const ABANDON_PATIENCE_THRESHOLD: f32 = 0.4;

/// A `taste_divergence` snapshot older than this is treated as too stale to
/// ground a zeitgeist nudge in ("you were early" / "trending now" claims
/// should reflect a reasonably current radar computation, not a months-old
/// one) — the generator skips rather than risk a stale claim.
const ZEITGEIST_MAX_STALENESS_DAYS: i64 = 30;

/// How many `were_early`/`blind_spots` entries the zeitgeist generator
/// surfaces per pass — these lists are already ranked (see
/// `radar::divergence`), so a small prefix is the "most interesting"
/// subset, not an arbitrary cut.
const ZEITGEIST_ENTRY_LIMIT: usize = 3;

/// Default cooldown window per `kind` — how long the orchestrator waits
/// before letting the same `(account, kind, dedup_key)` fire again. Also
/// used to set a candidate's `expires_at` when a generator doesn't compute
/// its own (double the cooldown: an unread nudge should go stale well
/// before it would've been eligible to repeat anyway).
pub fn cooldown_days(item_kind: &str) -> i64 {
    match item_kind {
        k if k == kind::NEW_SEASON => 14,
        k if k == kind::FRIDAY_EVENING => 5,
        k if k == kind::ABANDON_INSIGHT => 21,
        k if k == kind::GRAB_WINDOW => 3,
        k if k == kind::ZEITGEIST => 7,
        _ => 7,
    }
}

/// One generator's output, prior to cooldown filtering + message phrasing.
#[derive(Debug, Clone)]
pub struct GeneratedItem {
    pub kind: &'static str,
    pub media_item_id: Option<i64>,
    /// The "same nudge" identity within `kind` — e.g. a `media_metadata_id`,
    /// or a synthetic key for a non-title-scoped nudge. Never empty.
    pub dedup_key: String,
    pub subject_title: String,
    /// The lead phrase `template_message` puts before the title (e.g. "You
    /// were early on", "It's grabbable right now:").
    pub headline_lead: &'static str,
    /// Grounded, human-readable real signals — never empty for a candidate
    /// a generator emits. Both `template_message` and the Chord prompt in
    /// `build_message` are built *only* from this list.
    pub facts: Vec<String>,
    pub priority: i32,
    pub earliest_at: Option<DateTime<Utc>>,
    /// `None` lets [`generate_for_account`] fall back to
    /// `now + 2 * cooldown_days(kind)`.
    pub expires_at: Option<DateTime<Utc>>,
    /// Structured payload stored in `proactive_items.body` alongside the
    /// headline — the rationale/facts plus any generator-specific detail
    /// (media_metadata_id, lead_days, episode number, ...).
    pub body_extra: Json,
}

// --- message building (template + optional Chord phrasing) ----------------

/// Deterministic, always-available message — every word traces to
/// `item.facts`. This is both the LLM-down fallback and the ground truth
/// `build_message`'s Chord prompt is grounded in.
pub fn template_message(item: &GeneratedItem) -> String {
    format!(
        "{} \"{}\" — {}.",
        item.headline_lead,
        item.subject_title,
        item.facts.join("; ")
    )
}

/// Produce the delivered headline for one generated item: the templated
/// sentence when no Chord client is configured or the call fails, otherwise
/// a Chord-phrased sentence explicitly instructed to use only the given
/// facts. Never errors — generation must never fail just because the local
/// model is down or busy (<host>'s GPU may be held by the resident
/// `lemonade-coder` production serve).
pub async fn build_message(chord: Option<&ChordClient>, item: &GeneratedItem) -> String {
    let template = template_message(item);

    let Some(client) = chord else {
        return template;
    };

    let system = "You are Lumina, a warm, concise personal assistant relaying one proactive media \
        suggestion from Muse (a private media companion). Write ONE short, natural-sounding sentence. \
        You MUST ground the sentence ONLY in the facts listed below — never invent a plot detail, \
        rating, episode count, or signal that isn't listed. Do not add a preamble or explanation, just \
        the one sentence.";
    let user = format!(
        "Title: {}\nFacts: {}\nWrite the one-sentence proactive nudge now.",
        item.subject_title,
        item.facts.join("; ")
    );

    match client.chat_completion(DEFAULT_MODEL, system, &user).await {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(
                error = %e,
                kind = item.kind,
                dedup_key = %item.dedup_key,
                "MUSE-12: chord message phrasing failed; falling back to the templated message"
            );
            template
        }
    }
}

// --- next-Friday-evening helper --------------------------------------------

/// The next Friday at 20:00 UTC strictly after `now` (today counts if it's
/// Friday and still before 20:00). A deliberate simplification — Muse has
/// no per-account timezone yet, so "Friday evening" is UTC-anchored rather
/// than locale-aware; see the MUSE-12 spec-divergence note in the module
/// build report.
fn next_friday_evening(now: DateTime<Utc>) -> DateTime<Utc> {
    let today = now.date_naive();
    let mut days_ahead = (Weekday::Fri.num_days_from_monday() as i64
        - today.weekday().num_days_from_monday() as i64)
        .rem_euclid(7);
    if days_ahead == 0 && now.hour() >= 20 {
        days_ahead = 7;
    }
    let target_date = today + Duration::days(days_ahead);
    target_date
        .and_hms_opt(20, 0, 0)
        .expect("20:00 is always a valid time")
        .and_utc()
}

// --- generator 1: new season / gap -----------------------------------------

/// A followed show with a new season/next episode out (MUSE-11 gap analysis
/// + `media_metadata.next_airing`/`status`).
pub async fn generate_new_season(pool: &PgPool, account_id: i64) -> MuseResult<Vec<GeneratedItem>> {
    let rows = repo::media_item::list_show_gap_candidates(pool, account_id, GENERATOR_CANDIDATE_LIMIT).await?;

    let mut out = Vec::new();
    for r in rows {
        let fact = if let Some(next) = r.next_airing {
            format!("a new episode is scheduled for {}", next.date_naive())
        } else if let Some(status) = &r.status {
            format!("its status (\"{status}\") means it isn't done airing yet")
        } else {
            continue; // no grounded "more to watch" signal for this row
        };

        out.push(GeneratedItem {
            kind: kind::NEW_SEASON,
            media_item_id: Some(r.media_item_id),
            dedup_key: r.media_metadata_id.to_string(),
            subject_title: r.title,
            headline_lead: "There's more to watch:",
            facts: vec![fact],
            priority: 5,
            earliest_at: None,
            expires_at: None,
            body_extra: json!({
                "media_metadata_id": r.media_metadata_id,
                "next_airing": r.next_airing,
                "status": r.status,
            }),
        });
    }
    Ok(out)
}

// --- generator 2: Friday-evening / time-of-day ------------------------------

/// A taste-fit suggestion timed to the account's context-centroid peak
/// (MUSE-10 `taste_context_centroids` — the weekend/weekday x time-of-day
/// bucket with the most observed sessions, when it's an evening bucket).
pub async fn generate_friday_evening(pool: &PgPool, account_id: i64) -> MuseResult<Vec<GeneratedItem>> {
    let centroids = repo::taste::list_context_centroids(pool, account_id).await?;
    let Some(peak) = centroids
        .iter()
        .filter(|c| c.context_key.ends_with("_evening") && c.sample_size > 0)
        .max_by_key(|c| c.sample_size)
    else {
        return Ok(Vec::new()); // no evening-context signal yet -- cold start
    };

    let taste_candidates = candidates::gather_taste_candidates(pool, account_id, 3).await?;
    let Some(pick) = taste_candidates.into_iter().next() else {
        return Ok(Vec::new()); // no fresh taste pick to suggest
    };

    let mut facts = vec![format!(
        "you tend to watch in a {} context ({} sessions on record)",
        peak.context_key, peak.sample_size
    )];
    facts.extend(pick.facts.clone());

    Ok(vec![GeneratedItem {
        kind: kind::FRIDAY_EVENING,
        media_item_id: pick.media_item_id,
        dedup_key: format!("{}:{}", peak.context_key, pick.media_metadata_id),
        subject_title: pick.title,
        headline_lead: "For your kind of evening, how about",
        facts,
        priority: 5,
        earliest_at: Some(next_friday_evening(Utc::now())),
        expires_at: None,
        body_extra: json!({
            "media_metadata_id": pick.media_metadata_id,
            "context_key": peak.context_key,
            "context_sample_size": peak.sample_size,
        }),
    }])
}

// --- generator 3: abandonment insight ---------------------------------------

/// A show the account abandoned that either "gets good at ep N" (MUSE-14
/// `does_it_get_good` enrichment) or that other accounts pushed through and
/// finished — a nudge to give it another shot.
pub async fn generate_abandon_insight(pool: &PgPool, account_id: i64) -> MuseResult<Vec<GeneratedItem>> {
    let rows = repo::watch_stats::list_abandoned(pool, account_id, GENERATOR_CANDIDATE_LIMIT).await?;

    let mut out = Vec::new();
    for r in rows {
        let mut facts = Vec::new();
        let mut gets_good_at_episode = None;

        let enrichment = repo::external_enrichment::list_for_media_item(pool, r.media_item_id).await?;
        if let Some(row) = enrichment
            .iter()
            .find(|e| e.kind == enrichment_kind::DOES_IT_GET_GOOD)
        {
            if let Ok(payload) = serde_json::from_value::<GetsGoodPayload>(row.payload.clone()) {
                if let Some(ep) = payload.gets_good_at_episode {
                    facts.push(format!("community consensus says it gets good at episode {ep}"));
                    gets_good_at_episode = Some(ep);
                } else if payload.patience_payoff.unwrap_or(0.0) >= ABANDON_PATIENCE_THRESHOLD {
                    facts.push("forum consensus says it's worth pushing through the slow start".to_string());
                }
            }
        }

        let others_finished = repo::watch_stats::count_other_accounts_finished(pool, r.media_item_id, account_id)
            .await
            .unwrap_or(0);
        if others_finished > 0 {
            facts.push(format!(
                "{others_finished} other account{} in your household finished it",
                if others_finished == 1 { "" } else { "s" }
            ));
        }

        if facts.is_empty() {
            continue; // no grounded reason to re-nudge -- an abandoned title alone isn't a signal
        }

        if let Some(percent) = r.avg_percent {
            facts.push(format!("you got {percent:.0}% through it"));
        }

        out.push(GeneratedItem {
            kind: kind::ABANDON_INSIGHT,
            media_item_id: Some(r.media_item_id),
            dedup_key: r.media_metadata_id.to_string(),
            subject_title: r.title,
            headline_lead: "You put this on pause:",
            facts,
            priority: if gets_good_at_episode.is_some() { 6 } else { 5 },
            earliest_at: None,
            expires_at: None,
            body_extra: json!({
                "media_metadata_id": r.media_metadata_id,
                "gets_good_at_episode": gets_good_at_episode,
                "others_finished": others_finished,
                "avg_percent": r.avg_percent,
            }),
        });
    }
    Ok(out)
}

// --- generator 4: grab-window / freeleech -----------------------------------

/// A not-in-library, taste-relevant title that is grabbable NOW (MUSE-16
/// availability), especially freeleech.
pub async fn generate_grab_window(pool: &PgPool) -> MuseResult<Vec<GeneratedItem>> {
    let rows = candidates::gather_available_now_candidates(pool, DEFAULT_TRENDING_REGION, GENERATOR_CANDIDATE_LIMIT)
        .await?;

    let mut out = Vec::new();
    for c in rows {
        let Some(availability) = &c.availability else { continue };
        if availability.release_count <= 0 {
            continue; // checked, nothing grabbable -- not a grab-window nudge
        }

        let freeleech = availability.has_freeleech;
        // `c.facts` (from `candidates::gather_available_now_candidates`) already
        // carries the real, computed popularity + grabbability sentences —
        // reused verbatim rather than re-derived, so this generator never
        // states a number it didn't itself compute.
        let facts = c.facts.clone();

        out.push(GeneratedItem {
            kind: kind::GRAB_WINDOW,
            media_item_id: c.media_item_id,
            dedup_key: c.media_metadata_id.to_string(),
            subject_title: c.title,
            headline_lead: if freeleech {
                "Freeleech grab window open:"
            } else {
                "It's grabbable right now:"
            },
            facts,
            priority: if freeleech { 7 } else { 5 },
            earliest_at: None,
            expires_at: None,
            body_extra: json!({
                "media_metadata_id": c.media_metadata_id,
                "release_count": availability.release_count,
                "best_seeders": availability.best_seeders,
                "has_freeleech": freeleech,
            }),
        });
    }
    Ok(out)
}

// --- generator 5: zeitgeist / were-early -------------------------------------

/// "You were early on Y" / "X is trending and matches your taste — worth
/// the hype?", grounded in the account's latest MUSE-20 `taste_divergence`
/// radar snapshot.
pub async fn generate_zeitgeist(pool: &PgPool, account_id: i64) -> MuseResult<Vec<GeneratedItem>> {
    let Some(divergence) = repo::taste_divergence::latest_divergence(pool, account_id).await? else {
        return Ok(Vec::new()); // radar never computed yet
    };

    let now = Utc::now();
    if (now - divergence.computed_at).num_days() > ZEITGEIST_MAX_STALENESS_DAYS {
        return Ok(Vec::new()); // too stale to ground a "trending now" claim
    }

    let mut out = Vec::new();

    if let Some(were_early) = divergence.were_early.as_ref().and_then(|v| v.as_array()) {
        for entry in were_early.iter().take(ZEITGEIST_ENTRY_LIMIT) {
            let (Some(media_metadata_id), Some(title)) =
                (entry["media_metadata_id"].as_i64(), entry["title"].as_str())
            else {
                continue;
            };
            let lead_days = entry["lead_days"].as_i64().unwrap_or(0);

            out.push(GeneratedItem {
                kind: kind::ZEITGEIST,
                media_item_id: None,
                dedup_key: format!("were_early:{media_metadata_id}"),
                subject_title: title.to_string(),
                headline_lead: "You were early on",
                facts: vec![format!("you watched it {lead_days} days before it started trending")],
                priority: 5,
                earliest_at: None,
                expires_at: None,
                body_extra: json!({
                    "media_metadata_id": media_metadata_id,
                    "signal": "were_early",
                    "lead_days": lead_days,
                }),
            });
        }
    }

    if let Some(blind_spots) = divergence.blind_spots.as_ref().and_then(|v| v.as_array()) {
        for entry in blind_spots.iter().take(ZEITGEIST_ENTRY_LIMIT) {
            let (Some(media_metadata_id), Some(title)) =
                (entry["media_metadata_id"].as_i64(), entry["title"].as_str())
            else {
                continue;
            };
            let popularity = entry["popularity"].as_f64();

            let fact = match popularity {
                Some(p) => format!("it's trending now (popularity {p:.0}) and everyone's talking about it"),
                None => "it's trending now and everyone's talking about it".to_string(),
            };

            out.push(GeneratedItem {
                kind: kind::ZEITGEIST,
                media_item_id: None,
                dedup_key: format!("blind_spot:{media_metadata_id}"),
                subject_title: title.to_string(),
                headline_lead: "Worth the hype?",
                facts: vec![fact],
                priority: 4,
                earliest_at: None,
                expires_at: None,
                body_extra: json!({
                    "media_metadata_id": media_metadata_id,
                    "signal": "blind_spot",
                    "popularity": popularity,
                }),
            });
        }
    }

    Ok(out)
}

// --- orchestrator: cooldown/dedup + persist ---------------------------------

/// Run all five generators for one account, drop anything within its
/// kind's cooldown window (also making a same-tick re-run idempotent — see
/// the module doc), phrase each survivor's message, and persist it.
/// Never fails outright: an individual generator erroring (a down
/// dependency) is logged and treated as "produced nothing" so the rest of
/// the pass still runs.
pub async fn generate_for_account(
    pool: &PgPool,
    chord: Option<&ChordClient>,
    account_id: i64,
) -> MuseResult<Vec<ProactiveItem>> {
    let now = Utc::now();
    let mut candidates_out: Vec<GeneratedItem> = Vec::new();

    run_generator("new_season", account_id, &mut candidates_out, generate_new_season(pool, account_id).await);
    run_generator(
        "friday_evening",
        account_id,
        &mut candidates_out,
        generate_friday_evening(pool, account_id).await,
    );
    run_generator(
        "abandon_insight",
        account_id,
        &mut candidates_out,
        generate_abandon_insight(pool, account_id).await,
    );
    run_generator("grab_window", account_id, &mut candidates_out, generate_grab_window(pool).await);
    run_generator("zeitgeist", account_id, &mut candidates_out, generate_zeitgeist(pool, account_id).await);

    let mut created = Vec::new();
    for item in candidates_out {
        let since = now - Duration::days(cooldown_days(item.kind));
        let existing =
            repo::proactive_item::find_recent_by_dedup_key(pool, Some(account_id), item.kind, &item.dedup_key, since)
                .await?;
        if existing.is_some() {
            continue; // within cooldown, or this exact nudge already fired this pass -- idempotent re-run
        }

        let headline = build_message(chord, &item).await;
        let expires_at = item.expires_at.unwrap_or_else(|| now + Duration::days(cooldown_days(item.kind) * 2));

        let new = NewProactiveItemDeduped {
            account_id: Some(account_id),
            kind: item.kind.to_string(),
            media_item_id: item.media_item_id,
            headline,
            body: Some(item.body_extra.clone()),
            priority: item.priority,
            earliest_at: item.earliest_at,
            expires_at: Some(expires_at),
            dedup_key: item.dedup_key.clone(),
        };

        let row = repo::proactive_item::create_with_dedup(pool, &new).await?;
        created.push(row);
    }

    Ok(created)
}

/// Fold one generator's `MuseResult` into the shared candidate pool,
/// logging (not propagating) an error — the graceful-degrade contract
/// every generator shares.
fn run_generator(
    label: &str,
    account_id: i64,
    out: &mut Vec<GeneratedItem>,
    result: MuseResult<Vec<GeneratedItem>>,
) {
    match result {
        Ok(items) => out.extend(items),
        Err(e) => tracing::warn!(
            error = %e,
            generator = label,
            account_id,
            "MUSE-12: proactive generator failed for this pass; skipping (graceful degrade)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_item(kind: &'static str, facts: Vec<&str>) -> GeneratedItem {
        GeneratedItem {
            kind,
            media_item_id: Some(1),
            dedup_key: "42".to_string(),
            subject_title: "Arrival".to_string(),
            headline_lead: "There's more to watch:",
            facts: facts.into_iter().map(str::to_string).collect(),
            priority: 5,
            earliest_at: None,
            expires_at: None,
            body_extra: json!({}),
        }
    }

    // --- template_message / build_message -----------------------------

    #[test]
    fn template_message_cites_lead_subject_and_facts() {
        let item = sample_item(kind::NEW_SEASON, vec!["a new episode is scheduled for 2026-08-01"]);
        let msg = template_message(&item);
        assert!(msg.starts_with("There's more to watch: \"Arrival\""));
        assert!(msg.contains("a new episode is scheduled for 2026-08-01"));
    }

    #[tokio::test]
    async fn build_message_falls_back_to_template_when_no_chord_configured() {
        let item = sample_item(kind::ABANDON_INSIGHT, vec!["it gets good at episode 4"]);
        let msg = build_message(None, &item).await;
        assert_eq!(msg, template_message(&item));
    }

    #[tokio::test]
    async fn build_message_uses_chord_output_when_available() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"choices": [{"message": {"role": "assistant", "content": "Give Arrival another shot — it gets good at episode 4."}}]}"#);
        });

        let client = ChordClient::new(server.base_url()).expect("client should construct");
        let item = sample_item(kind::ABANDON_INSIGHT, vec!["it gets good at episode 4"]);
        let msg = build_message(Some(&client), &item).await;

        assert_eq!(msg, "Give Arrival another shot — it gets good at episode 4.");
    }

    #[tokio::test]
    async fn build_message_falls_back_to_template_when_chord_call_fails() {
        use httpmock::prelude::*;

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(500).body("model not loaded");
        });

        let client = ChordClient::new(server.base_url()).expect("client should construct");
        let item = sample_item(kind::GRAB_WINDOW, vec!["grabbable now (40 seeders, freeleech)"]);
        let msg = build_message(Some(&client), &item).await;

        assert_eq!(
            msg,
            template_message(&item),
            "a failing chord call must never fail generation — it degrades to the template"
        );
    }

    // --- cooldown_days ---------------------------------------------------

    #[test]
    fn cooldown_days_covers_every_known_kind() {
        assert_eq!(cooldown_days(kind::NEW_SEASON), 14);
        assert_eq!(cooldown_days(kind::FRIDAY_EVENING), 5);
        assert_eq!(cooldown_days(kind::ABANDON_INSIGHT), 21);
        assert_eq!(cooldown_days(kind::GRAB_WINDOW), 3);
        assert_eq!(cooldown_days(kind::ZEITGEIST), 7);
    }

    // --- next_friday_evening ----------------------------------------------

    #[test]
    fn next_friday_evening_from_a_monday_lands_on_the_same_week_friday() {
        let monday = Utc.with_ymd_and_hms(2026, 7, 13, 9, 0, 0).unwrap(); // a Monday
        let next = next_friday_evening(monday);
        assert_eq!(next.weekday(), Weekday::Fri);
        assert_eq!(next.date_naive(), Utc.with_ymd_and_hms(2026, 7, 17, 0, 0, 0).unwrap().date_naive());
        assert_eq!(next.hour(), 20);
    }

    #[test]
    fn next_friday_evening_from_friday_before_20_00_is_today() {
        let friday_afternoon = Utc.with_ymd_and_hms(2026, 7, 17, 14, 0, 0).unwrap(); // a Friday, 14:00
        let next = next_friday_evening(friday_afternoon);
        assert_eq!(next.date_naive(), friday_afternoon.date_naive());
        assert_eq!(next.hour(), 20);
    }

    #[test]
    fn next_friday_evening_from_friday_after_20_00_rolls_to_next_week() {
        let friday_night = Utc.with_ymd_and_hms(2026, 7, 17, 21, 0, 0).unwrap(); // a Friday, 21:00 -- already past
        let next = next_friday_evening(friday_night);
        assert_eq!(next.weekday(), Weekday::Fri);
        assert!(next.date_naive() > friday_night.date_naive());
        assert_eq!((next.date_naive() - friday_night.date_naive()).num_days(), 7);
    }

    // --- live-DB tests ---------------------------------------------------
    //
    // Gated on MUSE_TEST_DATABASE_URL: skip cleanly (never fail) when unset,
    // per the MUSE-02 build constraint. Every seeded row/account uses a
    // per-test UUID suffix and every assertion is scoped to that suffix's
    // own ids — the shared muse_test DB accumulates rows across the whole
    // suite, so a global "list everything" assertion would flake.

    async fn live_pool() -> Option<sqlx::PgPool> {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!("MUSE_TEST_DATABASE_URL not set — skipping proactive::generators live-DB test");
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

    /// Seed a bare account with one movie-kind library — the minimum shape
    /// every generator test below builds a title on top of.
    async fn seed_account_and_library(pool: &sqlx::PgPool, suffix: &str) -> (crate::models::account::Account, crate::models::library::Library) {
        use crate::models::account::NewAccount;
        use crate::models::library::{LibraryKind, NewLibrary};

        let account = repo::account::create(
            pool,
            &NewAccount {
                plex_account_id: Some(format!("muse12-account-{suffix}")),
                username: Some(format!("muse12_{suffix}")),
                friendly_name: Some("MUSE-12 Test Account".to_string()),
                is_home_user: false,
                is_primary: false,
            },
        )
        .await
        .expect("create account");

        let library = repo::library::create(
            pool,
            &NewLibrary {
                name: format!("muse12-library-{suffix}"),
                kind: LibraryKind::Tv,
                root_folder: format!("/media/muse12-test-{suffix}"),
                source_arr_name: None,
                source_arr_url: None,
            },
        )
        .await
        .expect("create library");

        (account, library)
    }

    #[tokio::test]
    async fn new_season_generator_fires_only_when_next_airing_is_grounded() {
        let Some(pool) = live_pool().await else { return };
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::watch_stats::NewWatchStats;
        use uuid::Uuid;

        let suffix = Uuid::new_v4().simple().to_string();
        let (account, library) = seed_account_and_library(&pool, &suffix).await;

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Show,
                tmdb_id: Some(format!("muse12-newseason-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Muse-12 New Season Show {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: Some("continuing".to_string()),
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(45),
                year: Some(2024),
                images: serde_json::json!({}),
            },
        )
        .await
        .expect("upsert media_metadata");

        let item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/media/muse12-test-{suffix}/show.mkv"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("muse12-newseason-rk-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        // Not yet engaged with -- shouldn't fire.
        let before = generate_new_season(&pool, account.id).await.expect("generate_new_season");
        assert!(
            before.iter().all(|g| g.dedup_key != metadata.id.to_string()),
            "an unwatched show must not trigger a new-season nudge"
        );

        repo::watch_stats::upsert_watch_stats(
            &pool,
            &NewWatchStats {
                account_id: account.id,
                media_item_id: item.id,
                play_count: 3,
                finished_count: 1,
                rewatch_count: 0,
                total_watched_ms: 45 * 60 * 1000 * 3,
                avg_percent: Some(0.9),
                last_watched_at: Some(Utc::now()),
                abandoned: false,
                first_watched_at: Some(Utc::now() - Duration::days(10)),
            },
        )
        .await
        .expect("upsert watch_stats");

        let after = generate_new_season(&pool, account.id).await.expect("generate_new_season");
        let hit = after
            .iter()
            .find(|g| g.dedup_key == metadata.id.to_string())
            .expect("engaged, continuing show should trigger a new-season nudge");
        assert_eq!(hit.kind, kind::NEW_SEASON);
        assert!(hit.facts.iter().any(|f| f.contains("continuing")));
    }

    #[tokio::test]
    async fn abandon_insight_generator_grounds_in_gets_good_and_other_account_signals() {
        let Some(pool) = live_pool().await else { return };
        use crate::enrichment::cache::{kind as ek, source as es, GetsGoodPayload};
        use crate::models::external_enrichment::NewExternalEnrichment;
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::watch_stats::NewWatchStats;
        use uuid::Uuid;

        let suffix = Uuid::new_v4().simple().to_string();
        let (account, library) = seed_account_and_library(&pool, &suffix).await;
        let (other_account, _) = seed_account_and_library(&pool, &format!("{suffix}-other")).await;

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Show,
                tmdb_id: Some(format!("muse12-abandon-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Muse-12 Slow Starter {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(45),
                year: Some(2023),
                images: serde_json::json!({}),
            },
        )
        .await
        .expect("upsert media_metadata");

        let item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/media/muse12-test-{suffix}/slow-starter.mkv"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("muse12-abandon-rk-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        repo::watch_stats::upsert_watch_stats(
            &pool,
            &NewWatchStats {
                account_id: account.id,
                media_item_id: item.id,
                play_count: 1,
                finished_count: 0,
                rewatch_count: 0,
                total_watched_ms: 10 * 60 * 1000,
                avg_percent: Some(0.1),
                last_watched_at: Some(Utc::now() - Duration::days(5)),
                abandoned: true,
                first_watched_at: Some(Utc::now() - Duration::days(5)),
            },
        )
        .await
        .expect("upsert abandoned watch_stats");

        // No grounding signal yet -- must not fire.
        let before = generate_abandon_insight(&pool, account.id).await.expect("generate_abandon_insight");
        assert!(
            before.iter().all(|g| g.dedup_key != metadata.id.to_string()),
            "an abandoned title with no grounding signal must not trigger a nudge"
        );

        let gets_good = GetsGoodPayload {
            gets_good_at_episode: Some(4),
            patience_payoff: Some(0.8),
            summary: "picks up around episode 4".to_string(),
            url: None,
            source_count: 3,
        };
        repo::external_enrichment::upsert(
            &pool,
            &NewExternalEnrichment {
                media_item_id: item.id,
                kind: ek::DOES_IT_GET_GOOD.to_string(),
                source: es::SEARXNG.to_string(),
                payload: serde_json::to_value(&gets_good).unwrap(),
                confidence: Some(0.8),
                ttl_seconds: 30 * 24 * 3600,
            },
        )
        .await
        .expect("upsert gets_good enrichment");

        repo::watch_stats::upsert_watch_stats(
            &pool,
            &NewWatchStats {
                account_id: other_account.id,
                media_item_id: item.id,
                play_count: 5,
                finished_count: 1,
                rewatch_count: 0,
                total_watched_ms: 45 * 60 * 1000 * 5,
                avg_percent: Some(0.97),
                last_watched_at: Some(Utc::now()),
                abandoned: false,
                first_watched_at: Some(Utc::now() - Duration::days(30)),
            },
        )
        .await
        .expect("upsert other account's finished watch_stats");

        let after = generate_abandon_insight(&pool, account.id).await.expect("generate_abandon_insight");
        let hit = after
            .iter()
            .find(|g| g.dedup_key == metadata.id.to_string())
            .expect("abandoned title with a gets-good signal + another finisher should trigger a nudge");
        assert!(hit.facts.iter().any(|f| f.contains("episode 4")));
        assert!(hit.facts.iter().any(|f| f.contains("other account")));
    }

    #[tokio::test]
    async fn zeitgeist_generator_reads_were_early_from_the_latest_radar_snapshot() {
        let Some(pool) = live_pool().await else { return };
        use crate::models::taste_divergence::NewTasteDivergence;
        use uuid::Uuid;

        let suffix = Uuid::new_v4().simple().to_string();
        let (account, _library) = seed_account_and_library(&pool, &suffix).await;

        let fake_metadata_id: i64 = 987654321; // no FK on taste_divergence's JSON payload -- a synthetic id is fine
        let were_early = serde_json::json!([{
            "media_metadata_id": fake_metadata_id,
            "title": format!("Muse-12 Were-Early Pick {suffix}"),
            "watched_at": Utc::now(),
            "trended_at": Utc::now(),
            "lead_days": 45,
        }]);

        repo::taste_divergence::insert_divergence(
            &pool,
            &NewTasteDivergence {
                account_id: account.id,
                genre_index: serde_json::json!({}),
                decade_index: None,
                mainstream_score: Some(0.5),
                adventurousness: Some(0.5),
                contrarian_index: Some(0.5),
                were_early: were_early.clone(),
                blind_spots: serde_json::json!([]),
                guilty_pleasures: serde_json::json!([]),
            },
        )
        .await
        .expect("insert taste_divergence");

        let items = generate_zeitgeist(&pool, account.id).await.expect("generate_zeitgeist");
        let hit = items
            .iter()
            .find(|g| g.dedup_key == format!("were_early:{fake_metadata_id}"))
            .expect("a fresh were_early entry should trigger a zeitgeist nudge");
        assert_eq!(hit.kind, kind::ZEITGEIST);
        assert!(hit.facts.iter().any(|f| f.contains("45 days")));
    }

    #[tokio::test]
    async fn friday_evening_generator_is_empty_for_a_cold_start_account() {
        let Some(pool) = live_pool().await else { return };
        use uuid::Uuid;

        let suffix = Uuid::new_v4().simple().to_string();
        let (account, _library) = seed_account_and_library(&pool, &suffix).await;

        // No taste_context_centroids, no taste_profile -- nothing to ground
        // a Friday-evening nudge in yet.
        let items = generate_friday_evening(&pool, account.id).await.expect("generate_friday_evening");
        assert!(items.is_empty(), "a cold-start account must not get a Friday-evening nudge");
    }

    #[tokio::test]
    async fn generate_for_account_is_idempotent_across_a_same_window_rerun() {
        let Some(pool) = live_pool().await else { return };
        use crate::enrichment::cache::{kind as ek, source as es, GetsGoodPayload};
        use crate::models::external_enrichment::NewExternalEnrichment;
        use crate::models::media_item::NewMediaItem;
        use crate::models::media_metadata::{MediaKind, NewMediaMetadata};
        use crate::models::watch_stats::NewWatchStats;
        use uuid::Uuid;

        let suffix = Uuid::new_v4().simple().to_string();
        let (account, library) = seed_account_and_library(&pool, &suffix).await;

        let metadata = repo::media_metadata::upsert_by_tmdb(
            &pool,
            &NewMediaMetadata {
                kind: MediaKind::Show,
                tmdb_id: Some(format!("muse12-idempotent-{suffix}")),
                tvdb_id: None,
                imdb_id: None,
                provider_ids: serde_json::json!({}),
                title: format!("Muse-12 Idempotent Rerun Show {suffix}"),
                sort_title: None,
                original_title: None,
                original_language: None,
                status: None,
                overview: None,
                studio: None,
                network: None,
                runtime_minutes: Some(45),
                year: Some(2022),
                images: serde_json::json!({}),
            },
        )
        .await
        .expect("upsert media_metadata");

        let item = repo::media_item::upsert(
            &pool,
            &NewMediaItem {
                library_id: library.id,
                media_metadata_id: metadata.id,
                path: format!("/media/muse12-test-{suffix}/idempotent.mkv"),
                monitored: true,
                quality_profile_id: None,
                minimum_availability: None,
                plex_rating_key: Some(format!("muse12-idempotent-rk-{suffix}")),
                added_at: None,
            },
        )
        .await
        .expect("upsert media_item");

        repo::watch_stats::upsert_watch_stats(
            &pool,
            &NewWatchStats {
                account_id: account.id,
                media_item_id: item.id,
                play_count: 1,
                finished_count: 0,
                rewatch_count: 0,
                total_watched_ms: 5 * 60 * 1000,
                avg_percent: Some(0.05),
                last_watched_at: Some(Utc::now()),
                abandoned: true,
                first_watched_at: Some(Utc::now()),
            },
        )
        .await
        .expect("upsert abandoned watch_stats");

        let gets_good = GetsGoodPayload {
            gets_good_at_episode: Some(2),
            patience_payoff: Some(0.9),
            summary: "picks up fast".to_string(),
            url: None,
            source_count: 2,
        };
        repo::external_enrichment::upsert(
            &pool,
            &NewExternalEnrichment {
                media_item_id: item.id,
                kind: ek::DOES_IT_GET_GOOD.to_string(),
                source: es::SEARXNG.to_string(),
                payload: serde_json::to_value(&gets_good).unwrap(),
                confidence: Some(0.9),
                ttl_seconds: 30 * 24 * 3600,
            },
        )
        .await
        .expect("upsert gets_good enrichment");

        let first_pass = generate_for_account(&pool, None, account.id).await.expect("first generate_for_account pass");
        let created_this_test: Vec<_> = first_pass
            .iter()
            .filter(|i| i.dedup_key.as_deref() == Some(metadata.id.to_string().as_str()))
            .collect();
        assert_eq!(created_this_test.len(), 1, "first pass should create exactly one abandon_insight item for this show");

        let created_item = created_this_test[0];
        assert_eq!(created_item.status, "pending");

        // --- MUSE-12 contract: GET /proactive/pending's underlying query --
        let pending = repo::proactive_item::list_pending_for_account(&pool, account.id, Utc::now())
            .await
            .expect("list_pending_for_account");
        assert!(
            pending.iter().any(|p| p.id == created_item.id),
            "a freshly generated item must be pending"
        );

        // Idempotent re-run: same cooldown window, same dedup_key -> no new row.
        let second_pass = generate_for_account(&pool, None, account.id).await.expect("second generate_for_account pass");
        assert!(
            second_pass
                .iter()
                .all(|i| i.dedup_key.as_deref() != Some(metadata.id.to_string().as_str())),
            "re-running the generator within the cooldown window must not create a duplicate pending item"
        );

        // --- MUSE-12 contract: POST /proactive/{id}/ack ---
        let acked = repo::proactive_item::ack(
            &pool,
            created_item.id,
            repo::proactive_item::AckOutcome::Dismissed,
            Utc::now(),
        )
        .await
        .expect("ack as dismissed");
        assert_eq!(acked.status, "dismissed");
        assert!(acked.dismissed_at.is_some());

        // `status` is authoritative for ack outcome -- re-fetch directly
        // rather than relying on `list_pending_for_account`'s
        // `delivered_at`-based filter (MUSE-03, unchanged), which a
        // `dismissed` outcome (no `delivered_at` write) doesn't affect.
        let refetched = repo::proactive_item::get(&pool, created_item.id).await.expect("get acked item");
        assert_eq!(refetched.status, "dismissed");
        assert!(refetched.dismissed_at.is_some());
    }
}
