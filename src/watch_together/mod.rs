//! MUSEX-08 (Plane TERM #384): watch-together session orchestration — the
//! GROUP session that answers "who's on the couch right now, and what
//! should WE watch" by composing three already-shipped MUSEX pieces in
//! sequence, never re-implementing any of them:
//!
//! 1. **Blend** ([`crate::persona::blend::blend_personas`], MUSEX-03) folds
//!    the present members' [`crate::models::persona::Persona`]s into one
//!    session taste vector (or a labeled `NoOverlap` compromise — see
//!    below).
//! 2. **Program** ([`crate::channels::director::program_channel`],
//!    MUSEX-05) turns a real candidate pool into a timed, intent-tagged
//!    [`ChannelSchedule`] — called once per named LOBBY preset (see
//!    [`LOBBY_PRESETS`]) so the group sees a small, genuinely distinct
//!    spread of options rather than one take-it-or-leave-it schedule.
//! 3. **Lobby** presents those 2-3 [`LobbyOption`]s, each EXPLAINED (the
//!    blend's own explanation plus the schedule's real, already-grounded
//!    per-slot rationale — MUSEX-04's `because_line`, reused verbatim, never
//!    re-derived), and [`GroupSession::lock_pick`] records the group's
//!    chosen option.
//!
//! ## Why the director doesn't re-score against the blend vector
//! [`crate::channels::director::program_channel`] takes an ALREADY-SCORED
//! `Vec<DirectorCandidate>` (per that module's own doc: "the director does
//! not invent a second scoring formula") — it has no parameter for a taste
//! vector at all. Re-ranking a candidate pool by cosine similarity to the
//! blend's `session_vector` would need each candidate's own embedding
//! (`repo::embedding`), which is a DB-touching concern this pure module
//! deliberately doesn't take on (matching `persona::blend`'s and
//! `channels::director`'s own "pure math / DB-free" posture, S9). Callers
//! that want blend-weighted candidate ordering build `pool` accordingly
//! before calling [`create_group_session`] (e.g. scoring each candidate
//! against `BlendResult::session_vector` via
//! [`crate::persona::blend::cosine_similarity`] once its embedding is
//! fetched) — this module's contribution is the SESSION-LEVEL orchestration
//! (who's present → blend → program → lobby → lock), not a new scoring
//! formula.
//!
//! ## SERVER-AGNOSTIC — the load-bearing property (AC)
//! This module imports ONLY the abstract persona/blend/director/candidate
//! code (`crate::persona`, `crate::channels::director`,
//! `crate::curation::candidates`, `crate::models::persona`) plus
//! general-purpose crates (`chrono`, `serde`). It never imports the
//! media-server-specific modules of this crate, and never references a
//! media-server play-queue/client type — the lobby/lock deal strictly in
//! abstract "options" (a media id + a runtime + a grounded rationale), never
//! "play this on server X." Turning a locked pick into an actual play
//! command against a real media server is a SEPARATE, later concern
//! (MUSEX-09's server adapter) and is out of scope here on purpose — see
//! the module's own negative test
//! (`orchestration_module_has_zero_server_specific_dependencies`) below,
//! which proves this by scanning this file's own source for any reference
//! to the crate's media-server-specific modules or vocabulary.
//!
//! ## Determinism
//! [`create_group_session`] is a pure function of its inputs: same
//! `members` (any order — [`crate::persona::blend::blend_personas`] sorts
//! by `persona.id` before touching anything) + same `pool` + same
//! [`GroupSessionConstraints`] yields a byte-for-byte identical
//! [`GroupSession`] (same blend, same lobby options, same explanations).
//! [`LOBBY_PRESETS`] is iterated in a fixed order, and each preset's
//! [`ChannelSchedule`] comes from `program_channel`, itself deterministic
//! (seeded, never unseeded randomness or `HashMap` order — see that
//! module's doc). See the `create_group_session_is_order_independent_and_
//! bit_deterministic` test below for the same "widely-different-magnitude"
//! teeth as `persona::blend`'s own determinism test.
//!
//! ## No-overlap: surfaced, never mushed (AC)
//! When [`blend_personas`] reports [`BlendStatus::NoOverlap`], this module
//! does NOT pretend the group has 2-3 genuinely group-tailored options: it
//! returns [`GroupSessionOutcome::NoOverlap`] with the blend's own
//! `suggestion` (split into subgroups / a deliberate compromise) and exactly
//! ONE clearly-labeled "compromise" option built from the blend's labeled
//! compromise vector context — never silently presenting a mush as if it
//! were a real group pick. `Blended`/`SinglePersona` both reach
//! [`GroupSessionOutcome::Ready`] with the full [`LOBBY_PRESETS`] spread.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::channels::director::{
    program_channel, ChannelSchedule, DirectorCandidate, DirectorConstraints, TimeOfDay,
};
use crate::error::{MuseError, MuseResult};
use crate::models::persona::Persona;
use crate::persona::blend::{blend_personas, BlendResult, BlendStatus};

// --- present members -----------------------------------------------------

/// One present member of the group session: the account plus the
/// [`Persona`] (already resolved by the caller — see
/// [`present_members_for_accounts`] for the DB-backed resolution path) that
/// represents their taste in this session.
#[derive(Debug, Clone)]
pub struct PresentMember {
    pub account_id: i64,
    pub persona: Persona,
}

// --- lobby options ---------------------------------------------------------

/// A named lobby-option preset: a `(key, display label, serendipity
/// budget)` triple. Three distinct, deterministic serendipity levels are
/// enough to give the group a genuinely different spread (safe / balanced /
/// adventurous) without inventing a second scoring axis — every option
/// programs the SAME candidate pool through the SAME real
/// [`program_channel`], varying only the one constraint MUSEX-05/06 already
/// expose for exactly this purpose.
pub const LOBBY_PRESETS: [(&str, &str, f64); 3] = [
    ("safe", "Safe pick", 0.0),
    ("balanced", "Balanced pick", 0.15),
    ("adventurous", "Adventurous pick", 0.35),
];

/// The one compromise preset used for a [`BlendStatus::NoOverlap`] group —
/// deliberately just ONE, deliberately the safest (no serendipity reach on
/// top of an already-uncertain compromise vector), and deliberately labeled
/// as a compromise rather than reusing [`LOBBY_PRESETS`]'s framing.
const NO_OVERLAP_COMPROMISE_PRESET: (&str, &str, f64) = ("compromise", "Compromise pick", 0.0);

/// One option in the lobby: a real, timed [`ChannelSchedule`] plus a
/// grounded, human-readable explanation of why it's being offered.
#[derive(Debug, Clone, Serialize)]
pub struct LobbyOption {
    /// Stable within one [`GroupSession`] (the preset key) — what
    /// [`GroupSession::lock_pick`] takes.
    pub option_id: String,
    pub label: &'static str,
    pub serendipity_budget: f64,
    pub schedule: ChannelSchedule,
    /// Grounded in the blend's own explanation plus the schedule's real
    /// facts (slot count, total runtime, opening title + its own
    /// already-computed rationale) — never invented prose.
    pub explanation: String,
}

/// The group's locked pick — the terminal state [`GroupSession::lock_pick`]
/// records. Deliberately just a media-id + rationale carrier: turning this
/// into an actual play command is MUSEX-09's server-adapter concern, not
/// this module's.
#[derive(Debug, Clone, Serialize)]
pub struct LockedPick {
    pub option_id: String,
    pub schedule: ChannelSchedule,
    pub locked_at: DateTime<Utc>,
}

/// How [`create_group_session`] resolved the group's programming — see the
/// module doc's "No-overlap: surfaced, never mushed" section.
#[derive(Debug, Clone, Serialize)]
pub enum GroupSessionOutcome {
    /// The blend genuinely overlapped (`Blended` or `SinglePersona`): the
    /// full [`LOBBY_PRESETS`] spread is offered.
    Ready { options: Vec<LobbyOption> },
    /// [`BlendStatus::NoOverlap`]: exactly one labeled compromise option,
    /// plus the blend's own human-readable split/compromise suggestion.
    NoOverlap {
        suggestion: String,
        compromise_options: Vec<LobbyOption>,
    },
}

/// Everything [`create_group_session`] needs beyond the members and the
/// candidate pool — the same shape as
/// [`crate::channels::director::DirectorConstraints`] minus
/// `serendipity_budget` (each [`LOBBY_PRESETS`]/`NO_OVERLAP_COMPROMISE_PRESET`
/// entry supplies its own).
#[derive(Debug, Clone)]
pub struct GroupSessionConstraints {
    pub start_at: DateTime<Utc>,
    pub end_by: DateTime<Utc>,
    pub time_of_day: TimeOfDay,
    pub max_slots: usize,
    pub seed: u64,
}

/// The whole GROUP session: who's present, the blend those personas
/// produced, the lobby it opened into, and (once the group has decided) the
/// locked pick.
#[derive(Debug, Clone)]
pub struct GroupSession {
    pub members: Vec<PresentMember>,
    pub blend: BlendResult,
    pub outcome: GroupSessionOutcome,
    pub locked: Option<LockedPick>,
}

impl GroupSession {
    /// The lobby's current options, regardless of which [`GroupSessionOutcome`]
    /// variant this session landed in — the one place a caller needs to look
    /// to render "here's what's on offer."
    pub fn options(&self) -> &[LobbyOption] {
        match &self.outcome {
            GroupSessionOutcome::Ready { options } => options,
            GroupSessionOutcome::NoOverlap {
                compromise_options, ..
            } => compromise_options,
        }
    }

    /// Record the group's chosen option. Idempotent-by-design: locking the
    /// same `option_id` twice (or a different one, changing their mind
    /// before playback starts) simply overwrites [`GroupSession::locked`] —
    /// this module has no playback-started concept to protect against
    /// (that lives with MUSEX-09's server adapter). Returns
    /// [`MuseError::NotFound`] for an `option_id` that isn't in the current
    /// lobby.
    pub fn lock_pick(&mut self, option_id: &str, now: DateTime<Utc>) -> MuseResult<&LockedPick> {
        let schedule = self
            .options()
            .iter()
            .find(|o| o.option_id == option_id)
            .map(|o| o.schedule.clone())
            .ok_or_else(|| MuseError::NotFound(format!("no lobby option with id {option_id:?}")))?;
        self.locked = Some(LockedPick {
            option_id: option_id.to_string(),
            schedule,
            locked_at: now,
        });
        Ok(self.locked.as_ref().expect("just set"))
    }
}

/// Build one grounded [`LobbyOption`] explanation from the blend's own
/// explanation plus the schedule's real facts. Never invents a claim beyond
/// what `blend.explanation` and the schedule itself (slot count, total
/// runtime, the opening slot's title + its own already-computed rationale)
/// already say.
fn describe_option(blend_explanation: &str, label: &str, schedule: &ChannelSchedule) -> String {
    if schedule.slots.is_empty() {
        return format!(
            "{label}: nothing in the candidate pool fits this session's time budget. \
             {blend_explanation}"
        );
    }
    let total_minutes: i64 = schedule.slots.iter().map(|s| s.runtime_ms).sum::<i64>() / 60_000;
    let first = &schedule.slots[0];
    format!(
        "{label} ({} slot{}, ~{} min): opens with \"{}\" -- {} {}",
        schedule.slots.len(),
        if schedule.slots.len() == 1 { "" } else { "s" },
        total_minutes,
        first.title,
        first.rationale,
        blend_explanation,
    )
}

/// Program one [`LobbyOption`] for `preset` against `pool` (cloned — every
/// preset programs the SAME pool independently, since `program_channel`
/// consumes its input) and `constraints`.
fn build_option(
    preset: (&'static str, &'static str, f64),
    pool: &[DirectorCandidate],
    constraints: &GroupSessionConstraints,
    blend_explanation: &str,
) -> LobbyOption {
    let (key, label, serendipity_budget) = preset;
    let director_constraints = DirectorConstraints {
        start_at: constraints.start_at,
        end_by: constraints.end_by,
        time_of_day: constraints.time_of_day,
        serendipity_budget,
        max_slots: constraints.max_slots,
        seed: constraints.seed,
    };
    let schedule = program_channel(pool.to_vec(), &director_constraints);
    let explanation = describe_option(blend_explanation, label, &schedule);
    LobbyOption {
        option_id: key.to_string(),
        label,
        serendipity_budget,
        schedule,
        explanation,
    }
}

/// Create a [`GroupSession`] from the present members' personas: blend
/// ([`blend_personas`], MUSEX-03), then program the lobby
/// ([`program_channel`], MUSEX-05) — see the module doc for the full
/// composition and the no-overlap handling.
///
/// `pool` should already be de-duplicated/roughly score-ordered, exactly as
/// [`program_channel`] itself expects (this module doesn't re-sort it
/// either — see the module doc's "Why the director doesn't re-score"
/// section).
pub fn create_group_session(
    members: Vec<PresentMember>,
    pool: Vec<DirectorCandidate>,
    constraints: &GroupSessionConstraints,
) -> GroupSession {
    let personas: Vec<Persona> = members.iter().map(|m| m.persona.clone()).collect();
    let blend = blend_personas(&personas);

    let outcome = match &blend.status {
        BlendStatus::NoOverlap { suggestion } => GroupSessionOutcome::NoOverlap {
            suggestion: suggestion.clone(),
            compromise_options: vec![build_option(
                NO_OVERLAP_COMPROMISE_PRESET,
                &pool,
                constraints,
                &blend.explanation,
            )],
        },
        BlendStatus::Blended | BlendStatus::SinglePersona => GroupSessionOutcome::Ready {
            options: LOBBY_PRESETS
                .iter()
                .map(|&preset| build_option(preset, &pool, constraints, &blend.explanation))
                .collect(),
        },
    };

    GroupSession {
        members,
        blend,
        outcome,
        locked: None,
    }
}

// --- DB-backed resolution (the only DB-touching surface in this module) ---

/// Resolve `account_ids` (the "who's present" roster) into
/// [`PresentMember`]s by looking up each account's persona named
/// `persona_name` (e.g. `"primary"` — see
/// [`crate::channels::director::list_director_presets`]'s own persona-name
/// convention) via [`crate::repo::persona::get_by_name_for_account`] — the
/// existing MUSEX-02 addressability seam, not a new lookup path. An account
/// with no persona under that name is skipped cleanly (not an error): a
/// present member Muse has no taste data for yet simply doesn't contribute
/// to the blend, rather than failing the whole group session.
pub async fn present_members_for_accounts(
    pool: &sqlx::PgPool,
    account_ids: &[i64],
    persona_name: &str,
) -> MuseResult<Vec<PresentMember>> {
    let mut members = Vec::with_capacity(account_ids.len());
    for &account_id in account_ids {
        if let Some(persona) =
            crate::repo::persona::get_by_name_for_account(pool, account_id, persona_name).await?
        {
            members.push(PresentMember {
                account_id,
                persona,
            });
        }
    }
    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curation::candidates::{Candidate, CandidateSource};
    use crate::models::embedding::EMBEDDING_DIM;
    use crate::models::media_metadata::MediaKind;
    use crate::models::persona::PERSONA_KIND_DERIVED;
    use chrono::Duration as ChronoDuration;
    use serde_json::json;

    fn persona(id: i64, name: &str, centroid: Vec<f32>, top_genres: &[(&str, i64)]) -> Persona {
        let genres_json: Vec<serde_json::Value> = top_genres
            .iter()
            .map(|(g, c)| json!({"genre": g, "count": c}))
            .collect();
        Persona {
            id,
            account_id: Some(id),
            name: name.to_string(),
            kind: PERSONA_KIND_DERIVED.to_string(),
            centroid: pgvector::Vector::from(centroid),
            defining_signals: json!({
                "context_key": null,
                "top_genres": genres_json,
                "source_media_item_ids": [],
            }),
            metadata: json!({}),
            sample_size: 3,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn full_width(components: &[(usize, f32)]) -> Vec<f32> {
        let mut v = vec![0.0f32; EMBEDDING_DIM as usize];
        for (i, value) in components {
            v[*i] = *value;
        }
        v
    }

    fn candidate(id: i64, source: CandidateSource, taste_fit: f64, fact: &str) -> Candidate {
        Candidate {
            media_metadata_id: id,
            media_item_id: Some(id),
            title: format!("Title {id}"),
            year: Some(2020),
            kind: MediaKind::Movie,
            source,
            taste_fit,
            facts: vec![fact.to_string()],
            availability: None,
        }
    }

    fn dc(id: i64, source: CandidateSource, score: f64, runtime_ms: i64) -> DirectorCandidate {
        DirectorCandidate {
            candidate: candidate(
                id,
                source,
                score,
                "it's a 92% match to your overall taste profile",
            ),
            score,
            runtime_ms,
        }
    }

    const THIRTY_MIN_MS: i64 = 30 * 60_000;

    fn base_constraints() -> GroupSessionConstraints {
        let start = DateTime::parse_from_rfc3339("2026-07-14T18:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        GroupSessionConstraints {
            start_at: start,
            end_by: start + ChronoDuration::hours(4),
            time_of_day: TimeOfDay::Evening,
            max_slots: 0,
            seed: 7,
        }
    }

    fn base_pool() -> Vec<DirectorCandidate> {
        let mut pool: Vec<DirectorCandidate> = (1..=8)
            .map(|i| dc(i, CandidateSource::Taste, 0.9, THIRTY_MIN_MS))
            .collect();
        pool.extend((100..=104).map(|i| dc(i, CandidateSource::AvailableNow, 0.3, THIRTY_MIN_MS)));
        pool
    }

    // ------------------------------------------------------------------
    // Creates a group session from present members' personas
    // ------------------------------------------------------------------

    #[test]
    fn group_session_from_present_members_blends_their_personas() {
        let a = persona(
            1,
            "a",
            full_width(&[(0, 1.0)]),
            &[("horror", 5), ("comedy", 2)],
        );
        let b = persona(
            2,
            "b",
            full_width(&[(0, 1.0)]),
            &[("comedy", 9), ("horror", 1)],
        );
        let members = vec![
            PresentMember {
                account_id: 10,
                persona: a,
            },
            PresentMember {
                account_id: 20,
                persona: b,
            },
        ];
        let session = create_group_session(members, base_pool(), &base_constraints());
        assert_eq!(session.members.len(), 2);
        assert_eq!(session.blend.status, BlendStatus::Blended);
        assert!(
            session.blend.explanation.contains("horror")
                && session.blend.explanation.contains("comedy"),
            "the blend explanation must name the genres shared by every present member: {}",
            session.blend.explanation
        );
    }

    // ------------------------------------------------------------------
    // Programs from the blend: 2-3 explained options
    // ------------------------------------------------------------------

    #[test]
    fn ready_outcome_presents_the_full_lobby_preset_spread_explained() {
        let a = persona(1, "a", full_width(&[(0, 1.0)]), &[("horror", 5)]);
        let b = persona(2, "b", full_width(&[(0, 1.0)]), &[("horror", 3)]);
        let members = vec![
            PresentMember {
                account_id: 10,
                persona: a,
            },
            PresentMember {
                account_id: 20,
                persona: b,
            },
        ];
        let session = create_group_session(members, base_pool(), &base_constraints());

        let GroupSessionOutcome::Ready { options } = &session.outcome else {
            panic!(
                "expected Ready outcome for overlapping personas, got {:?}",
                session.outcome
            );
        };
        assert_eq!(
            options.len(),
            3,
            "must present between 2 and 3 explained options"
        );
        for opt in options {
            assert!(
                !opt.explanation.is_empty(),
                "every option must be EXPLAINED"
            );
            assert!(
                !opt.schedule.slots.is_empty(),
                "with a real non-empty pool every preset should schedule something"
            );
        }
        // Distinct serendipity levels actually produce a genuinely different
        // spread (not three copies of the same schedule) for this pool.
        let explore_counts: Vec<usize> = options
            .iter()
            .map(|o| o.schedule.exploration_slot_count)
            .collect();
        assert!(
            explore_counts.iter().any(|&c| c > 0),
            "at least one of the safe/balanced/adventurous presets must reserve exploration slots: {explore_counts:?}"
        );
        assert_eq!(
            options[0].serendipity_budget, 0.0,
            "the safe preset must be genuinely zero-serendipity"
        );
    }

    #[test]
    fn options_returns_the_current_lobby_regardless_of_outcome_variant() {
        let a = persona(1, "a", full_width(&[(0, 1.0)]), &[]);
        let b = persona(2, "b", full_width(&[(0, 1.0)]), &[]);
        let members = vec![
            PresentMember {
                account_id: 10,
                persona: a,
            },
            PresentMember {
                account_id: 20,
                persona: b,
            },
        ];
        let session = create_group_session(members, base_pool(), &base_constraints());
        assert_eq!(session.options().len(), 3);
    }

    // ------------------------------------------------------------------
    // No-overlap: surfaced, never mushed (negative test)
    // ------------------------------------------------------------------

    #[test]
    fn no_overlap_group_gets_one_labeled_compromise_not_a_full_lobby() {
        // Perfectly opposed centroids, exactly like persona::blend's own
        // no-overlap fixture -- the weakest pairwise cosine similarity is
        // -1.0, at/below the no-overlap threshold.
        let a = persona(1, "horror-fan", full_width(&[(0, 1.0)]), &[("horror", 5)]);
        let b = persona(
            2,
            "anti-horror",
            full_width(&[(0, -1.0)]),
            &[("romance", 5)],
        );
        let members = vec![
            PresentMember {
                account_id: 10,
                persona: a,
            },
            PresentMember {
                account_id: 20,
                persona: b,
            },
        ];
        let session = create_group_session(members, base_pool(), &base_constraints());

        match &session.outcome {
            GroupSessionOutcome::NoOverlap {
                suggestion,
                compromise_options,
            } => {
                assert!(
                    !suggestion.is_empty(),
                    "a NoOverlap outcome must carry an actionable suggestion"
                );
                assert!(
                    suggestion.contains("subgroup")
                        || suggestion.contains("split")
                        || suggestion.contains("compromise"),
                    "suggestion should propose a compromise or a split: {suggestion}"
                );
                assert_eq!(
                    compromise_options.len(),
                    1,
                    "a no-overlap group must get exactly ONE labeled compromise option, never a \
                     full lobby pretending the group genuinely agrees"
                );
                assert_eq!(compromise_options[0].option_id, "compromise");
                assert_eq!(compromise_options[0].serendipity_budget, 0.0);
            }
            other => panic!(
                "genuinely opposed personas must surface as NoOverlap, never silently mushed into \
                 a full lobby: {other:?}"
            ),
        }
    }

    #[test]
    fn no_members_present_is_surfaced_not_fabricated() {
        let session = create_group_session(vec![], base_pool(), &base_constraints());
        match &session.outcome {
            GroupSessionOutcome::NoOverlap {
                compromise_options, ..
            } => {
                assert_eq!(compromise_options.len(), 1);
            }
            other => panic!("an empty roster must degrade to NoOverlap, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Lock the group's pick
    // ------------------------------------------------------------------

    #[test]
    fn lock_pick_records_the_groups_chosen_option() {
        let a = persona(1, "a", full_width(&[(0, 1.0)]), &[]);
        let b = persona(2, "b", full_width(&[(0, 1.0)]), &[]);
        let members = vec![
            PresentMember {
                account_id: 10,
                persona: a,
            },
            PresentMember {
                account_id: 20,
                persona: b,
            },
        ];
        let mut session = create_group_session(members, base_pool(), &base_constraints());
        assert!(session.locked.is_none());

        let now = Utc::now();
        let locked = session
            .lock_pick("balanced", now)
            .expect("balanced is a real option");
        assert_eq!(locked.option_id, "balanced");
        assert_eq!(locked.locked_at, now);
        assert!(session.locked.is_some());
        assert_eq!(session.locked.as_ref().unwrap().option_id, "balanced");
    }

    #[test]
    fn lock_pick_rejects_an_unknown_option_id() {
        let a = persona(1, "a", full_width(&[(0, 1.0)]), &[]);
        let b = persona(2, "b", full_width(&[(0, 1.0)]), &[]);
        let members = vec![
            PresentMember {
                account_id: 10,
                persona: a,
            },
            PresentMember {
                account_id: 20,
                persona: b,
            },
        ];
        let mut session = create_group_session(members, base_pool(), &base_constraints());
        let err = session.lock_pick("not-a-real-option", Utc::now());
        assert!(
            err.is_err(),
            "an unknown option_id must be rejected, not silently accepted"
        );
        assert!(
            session.locked.is_none(),
            "a rejected lock must not mutate session state"
        );
    }

    #[test]
    fn lock_pick_can_change_the_groups_mind_before_playback() {
        let a = persona(1, "a", full_width(&[(0, 1.0)]), &[]);
        let b = persona(2, "b", full_width(&[(0, 1.0)]), &[]);
        let members = vec![
            PresentMember {
                account_id: 10,
                persona: a,
            },
            PresentMember {
                account_id: 20,
                persona: b,
            },
        ];
        let mut session = create_group_session(members, base_pool(), &base_constraints());
        session.lock_pick("safe", Utc::now()).unwrap();
        session.lock_pick("adventurous", Utc::now()).unwrap();
        assert_eq!(session.locked.as_ref().unwrap().option_id, "adventurous");
    }

    // ------------------------------------------------------------------
    // Determinism, WITH TEETH (same idiom as persona::blend's own test)
    // ------------------------------------------------------------------

    #[test]
    fn create_group_session_is_order_independent_and_bit_deterministic() {
        // Same magnitude-spread teeth as persona::blend's determinism test:
        // dimension 0's values differ by more than 2^53 across ids 1/2/3, so
        // a naive caller-order sum would visibly differ across input orders.
        let a = persona(1, "a", full_width(&[(0, 1e17), (1, 3.0)]), &[("horror", 3)]);
        let b = persona(2, "b", full_width(&[(0, 1.0), (1, 3.0)]), &[("horror", 2)]);
        let c = persona(
            3,
            "c",
            full_width(&[(0, -1e17), (1, 3.0)]),
            &[("horror", 1)],
        );

        let make_members = |order: &[Persona]| -> Vec<PresentMember> {
            order
                .iter()
                .cloned()
                .map(|p| PresentMember {
                    account_id: p.id,
                    persona: p,
                })
                .collect()
        };

        let forward = make_members(&[a.clone(), b.clone(), c.clone()]);
        let shuffled = make_members(&[c.clone(), a.clone(), b.clone()]);
        let reversed = make_members(&[c.clone(), b.clone(), a.clone()]);

        let s_forward = create_group_session(forward, base_pool(), &base_constraints());
        let s_shuffled = create_group_session(shuffled, base_pool(), &base_constraints());
        let s_reversed = create_group_session(reversed, base_pool(), &base_constraints());

        assert_eq!(
            s_forward.blend.session_vector.as_slice(),
            s_shuffled.blend.session_vector.as_slice(),
            "the blend inside a group session must be bit-identical regardless of member order"
        );
        assert_eq!(
            s_forward.blend.session_vector.as_slice(),
            s_reversed.blend.session_vector.as_slice()
        );
        assert_eq!(s_forward.blend.explanation, s_shuffled.blend.explanation);

        let ids = |s: &GroupSession| -> Vec<Vec<i64>> {
            s.options()
                .iter()
                .map(|o| {
                    o.schedule
                        .slots
                        .iter()
                        .map(|slot| slot.media_metadata_id)
                        .collect()
                })
                .collect()
        };
        assert_eq!(
            ids(&s_forward),
            ids(&s_shuffled),
            "the lobby options themselves must be identical regardless of member order"
        );
        assert_eq!(ids(&s_forward), ids(&s_reversed));
    }

    #[test]
    fn same_inputs_twice_yields_the_same_lobby() {
        let a = persona(1, "a", full_width(&[(0, 1.0)]), &[("comedy", 4)]);
        let b = persona(2, "b", full_width(&[(0, 1.0)]), &[("comedy", 2)]);
        let build = || {
            let members = vec![
                PresentMember {
                    account_id: 10,
                    persona: a.clone(),
                },
                PresentMember {
                    account_id: 20,
                    persona: b.clone(),
                },
            ];
            create_group_session(members, base_pool(), &base_constraints())
        };
        let s1 = build();
        let s2 = build();
        let ids = |s: &GroupSession| -> Vec<Vec<i64>> {
            s.options()
                .iter()
                .map(|o| {
                    o.schedule
                        .slots
                        .iter()
                        .map(|slot| slot.media_metadata_id)
                        .collect()
                })
                .collect()
        };
        assert_eq!(ids(&s1), ids(&s2));
        assert_eq!(s1.blend.explanation, s2.blend.explanation);
    }

    // ------------------------------------------------------------------
    // Empty candidate pool degrades cleanly
    // ------------------------------------------------------------------

    #[test]
    fn empty_pool_yields_empty_schedules_not_a_panic() {
        let a = persona(1, "a", full_width(&[(0, 1.0)]), &[]);
        let b = persona(2, "b", full_width(&[(0, 1.0)]), &[]);
        let members = vec![
            PresentMember {
                account_id: 10,
                persona: a,
            },
            PresentMember {
                account_id: 20,
                persona: b,
            },
        ];
        let session = create_group_session(members, vec![], &base_constraints());
        for opt in session.options() {
            assert!(opt.schedule.slots.is_empty());
            assert!(
                !opt.explanation.is_empty(),
                "even an empty schedule must still be explained"
            );
        }
    }

    // ------------------------------------------------------------------
    // SERVER-AGNOSTIC: the negative test proving zero server dependency
    // ------------------------------------------------------------------

    /// This module's own source, scanned for any reference to this crate's
    /// media-server-specific modules or vocabulary. The orchestration
    /// (session -> blend -> program -> lobby -> lock) must be reachable
    /// with ZERO knowledge of which server (if any) will eventually play
    /// the locked pick -- turning a pick into a play command is MUSEX-09's
    /// separate server-adapter concern. If this test ever fails, the
    /// orchestration module has grown a server-specific dependency and the
    /// AC is violated.
    #[test]
    fn orchestration_module_has_zero_server_specific_dependencies() {
        // Scan only the NON-TEST portion of this file: the test modules
        // themselves necessarily contain this scan's own needle strings (as
        // string literals in the `forbidden` array below), so scanning the
        // whole file would always fail against itself. Splitting at the
        // FIRST `#[cfg(test)]` marker (right before `mod tests`, the first
        // of this file's two test modules) scopes the check to everything
        // above it -- the actual production code, which must be genuinely
        // server-agnostic.
        let source = include_str!("mod.rs");
        let (production_code, _test_code) = source
            .split_once("#[cfg(test)]")
            .expect("this file has a #[cfg(test)] marker before its test module");

        let forbidden = [
            "crate::plex",
            "crate::plex_control",
            "crate::streaming",
            "crate::tuner",
            "PlexClient",
            "MediaServerClient",
            "jellyfin",
            "Jellyfin",
        ];
        for needle in forbidden {
            assert!(
                !production_code.contains(needle),
                "watch_together::mod's production code must have ZERO server-specific \
                 dependencies -- found forbidden reference {needle:?}"
            );
        }
    }

    /// The functional complement to the source-scan above: run the WHOLE
    /// session -> blend -> program -> lobby -> lock path end-to-end using
    /// only synthetic personas and candidates -- no server client, no
    /// server-play type, anywhere in any signature this test touches. That
    /// this compiles and runs at all (with no server type ever named) is
    /// itself the proof that the orchestration doesn't need one.
    #[test]
    fn full_session_lifecycle_runs_with_zero_server_dependency() {
        let a = persona(1, "solo-fan", full_width(&[(0, 1.0)]), &[("comedy", 3)]);
        let members = vec![PresentMember {
            account_id: 10,
            persona: a,
        }];
        let mut session = create_group_session(members, base_pool(), &base_constraints());
        assert_eq!(session.blend.status, BlendStatus::SinglePersona);
        let first_option_id = session.options()[0].option_id.clone();
        session
            .lock_pick(&first_option_id, Utc::now())
            .expect("locking a real lobby option must succeed");
        assert!(session.locked.is_some());
    }
}

/// DB-gated end-to-end proof: seeds two real accounts + two real personas,
/// resolves them via [`present_members_for_accounts`] (the one DB-touching
/// function in this module), and confirms [`create_group_session`] blends
/// and programs from the REAL fetched personas -- not fabricated ones.
/// Gated on `MUSE_TEST_DATABASE_URL`, identical skip-when-unset posture as
/// every other live-DB test in this crate (`channels::director::live_tests`,
/// `curation::live_tests`) -- never a live system, never a hardcoded DSN.
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::models::account::NewAccount;
    use crate::models::persona::{NewPersona, PERSONA_KIND_EXPLICIT};
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    #[tokio::test]
    async fn present_members_for_accounts_resolves_real_seeded_personas() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 present_members_for_accounts_resolves_real_seeded_personas"
            );
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();

        let account_a = crate::repo::account::create(
            &pool,
            &NewAccount {
                plex_account_id: None,
                username: Some(format!("musex08-a-{suffix}")),
                friendly_name: Some("MUSEX-08 fixture A".to_string()),
                is_home_user: true,
                is_primary: false,
            },
        )
        .await
        .expect("create account A");

        let account_b = crate::repo::account::create(
            &pool,
            &NewAccount {
                plex_account_id: None,
                username: Some(format!("musex08-b-{suffix}")),
                friendly_name: Some("MUSEX-08 fixture B".to_string()),
                is_home_user: true,
                is_primary: false,
            },
        )
        .await
        .expect("create account B");

        let persona_a = crate::repo::persona::upsert_for_account(
            &pool,
            &NewPersona {
                account_id: Some(account_a.id),
                name: "primary".to_string(),
                kind: PERSONA_KIND_EXPLICIT.to_string(),
                centroid: pgvector::Vector::from(vec![
                    0.1f32;
                    crate::models::embedding::EMBEDDING_DIM
                        as usize
                ]),
                defining_signals: serde_json::json!({
                    "context_key": null,
                    "top_genres": [{"genre": "comedy", "count": 3}],
                    "source_media_item_ids": [],
                }),
                metadata: serde_json::json!({}),
                sample_size: 3,
            },
        )
        .await
        .expect("upsert persona A");

        let persona_b = crate::repo::persona::upsert_for_account(
            &pool,
            &NewPersona {
                account_id: Some(account_b.id),
                name: "primary".to_string(),
                kind: PERSONA_KIND_EXPLICIT.to_string(),
                centroid: pgvector::Vector::from(vec![
                    0.1f32;
                    crate::models::embedding::EMBEDDING_DIM
                        as usize
                ]),
                defining_signals: serde_json::json!({
                    "context_key": null,
                    "top_genres": [{"genre": "comedy", "count": 5}],
                    "source_media_item_ids": [],
                }),
                metadata: serde_json::json!({}),
                sample_size: 3,
            },
        )
        .await
        .expect("upsert persona B");

        let members = present_members_for_accounts(&pool, &[account_a.id, account_b.id], "primary")
            .await
            .expect("resolve present members");

        assert_eq!(members.len(), 2);
        let resolved_ids: Vec<i64> = members.iter().map(|m| m.persona.id).collect();
        assert!(resolved_ids.contains(&persona_a.id));
        assert!(resolved_ids.contains(&persona_b.id));

        let session = create_group_session(members, vec![], &base_constraints());
        assert_eq!(
            session.blend.status,
            BlendStatus::Blended,
            "two identical-centroid real personas must blend, not no-overlap"
        );

        sqlx::query("DELETE FROM personas WHERE id = ANY($1)")
            .bind(vec![persona_a.id, persona_b.id])
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM accounts WHERE id = ANY($1)")
            .bind(vec![account_a.id, account_b.id])
            .execute(&pool)
            .await
            .ok();
    }

    /// An account with no persona under the requested name is skipped
    /// cleanly, not an error -- the documented degrade in
    /// [`present_members_for_accounts`]'s doc comment.
    #[tokio::test]
    async fn present_members_for_accounts_skips_an_account_with_no_matching_persona() {
        let Ok(database_url) = std::env::var("MUSE_TEST_DATABASE_URL") else {
            eprintln!(
                "MUSE_TEST_DATABASE_URL not set — skipping \
                 present_members_for_accounts_skips_an_account_with_no_matching_persona"
            );
            return;
        };

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("connect to MUSE_TEST_DATABASE_URL");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly");

        let suffix = Uuid::new_v4().simple().to_string();
        let account = crate::repo::account::create(
            &pool,
            &NewAccount {
                plex_account_id: None,
                username: Some(format!("musex08-none-{suffix}")),
                friendly_name: None,
                is_home_user: true,
                is_primary: false,
            },
        )
        .await
        .expect("create account");

        let members = present_members_for_accounts(&pool, &[account.id], "no-such-persona")
            .await
            .expect("resolve present members");
        assert!(
            members.is_empty(),
            "an account with no matching persona must be skipped, not error"
        );

        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(account.id)
            .execute(&pool)
            .await
            .ok();
    }
}
