//! MUSEX-06 (Plane TERM #382): serendipity + range control
//! (anti-filter-bubble) — a TUNABLE range control on top of MUSEX-05's
//! `director::DirectorConstraints::serendipity_budget`, plus the core
//! refinement of what "exploration" actually MEANS.
//!
//! ## What this extends from MUSEX-05
//! `director.rs` already had a `serendipity_budget: f64` fraction and a
//! deterministic modular-interval slot reservation
//! (`exploration_interval`/`seed % interval`). What it did NOT have:
//! 1. A first-class, documented "range control" type a future GUI can bind
//!    to directly (`SerendipityRange::from_percent`/`from_fraction`).
//! 2. A guarantee that a low-but-nonzero setting still produces at least
//!    one exploration slot — the old modular reservation could, for a tiny
//!    fraction and a short session, reserve zero slots in practice (a huge
//!    interval never divides evenly into a short slot count).
//! 3. A genuine "taste-adjacent-but-novel" exploration SELECTION. MUSEX-05
//!    classified exploration purely by `CandidateSource::AvailableNow` — a
//!    structurally-reasonable but source-level split, not a taste-adjacency
//!    one. This module ADDS a taste_fit-band classification on top (see
//!    [`is_exploration_eligible`]) so a plain taste-tier pick that happens
//!    to sit at a moderate (not top) affinity is also exploration-eligible
//!    — genuinely near the user's taste vector, not a random pick and not
//!    the safe/known core.
//!
//! This module is a pure, additive refinement: [`is_exploration_eligible`]
//! is a strict superset of MUSEX-05's `source == AvailableNow` rule (every
//! `AvailableNow` candidate is still unconditionally eligible, preserving
//! MUSEX-05's own tests/behavior byte-for-byte), and
//! [`SerendipityRange::reservation_interval`] behaves identically to the old
//! `director::exploration_interval` for the same fraction.

use serde::Serialize;

use crate::curation::candidates::{Candidate, CandidateSource};

/// Lower bound (inclusive) of the "adjacency band" — the range of
/// `Candidate::taste_fit` (a real cosine similarity to the account's taste
/// centroid for [`CandidateSource::Taste`] picks, per
/// `curation::candidates::gather_taste_candidates`) that counts as
/// "taste-adjacent-but-novel."
///
/// Why 0.35, not 0.0: below this, a pick is only weakly related to the
/// account's taste vector at all — scheduling it as "exploration" would be
/// indistinguishable from a genuinely RANDOM pick, which the AC explicitly
/// rules out ("NOT random"). 0.35 is comfortably below the "safe core"
/// (real taste-tier candidates in this codebase's own fixtures score
/// 0.8-0.95 similarity, see `director::tests::dc`/`candidate`) while still
/// meaning "recognizably related."
pub const NOVELTY_LOW: f64 = 0.35;

/// Upper bound (exclusive) of the adjacency band. Above this, a pick is
/// close enough to the taste centroid that it belongs to the safe/known
/// core MUSEX-05's doc explicitly contrasts exploration against ("outside
/// the safe/known core") — scheduling it as "exploration" would just be
/// comfort-food with an exploration label on it, defeating the AC's
/// anti-filter-bubble point. 0.68 sits below this crate's own safe-core
/// fixture values (0.8-0.95) with headroom, and above `NOVELTY_LOW` with
/// enough spread to be a real band, not a hairline.
pub const NOVELTY_HIGH: f64 = 0.68;

/// `true` when `taste_fit` sits in the adjacency band `[NOVELTY_LOW,
/// NOVELTY_HIGH)` — near the taste vector but outside the safe core. Pure,
/// total (any finite/non-finite `f64` handled: NaN never matches a range
/// comparison, so it correctly returns `false`).
pub fn in_adjacency_band(taste_fit: f64) -> bool {
    taste_fit >= NOVELTY_LOW && taste_fit < NOVELTY_HIGH
}

/// Is `candidate` eligible to fill an exploration slot? The core
/// MUSEX-06 refinement: a strict superset of MUSEX-05's rule.
///
/// - [`CandidateSource::AvailableNow`] is ALWAYS eligible (unconditional,
///   unchanged from MUSEX-05) — by construction (see
///   `curation::candidates`'s own doc) it is "a title Muse doesn't own and
///   the account hasn't engaged with," which is exploration regardless of
///   its `taste_fit` (that field is a popularity score for this source, not
///   a taste-vector similarity, so a band check on it would be meaningless
///   — see `gather_available_now_candidates`).
/// - [`CandidateSource::Taste`] is eligible ONLY when its `taste_fit` (a
///   real cosine similarity to the account's taste centroid) falls in
///   [`in_adjacency_band`] — "near the taste vector but outside the safe
///   core," the literal taste-adjacent-but-novel definition.
/// - [`CandidateSource::OnDeck`]/[`CandidateSource::Gap`] are NEVER
///   eligible: their `taste_fit` encodes percent-complete / engagement
///   signals, not taste-vector similarity (see `Candidate::taste_fit`'s own
///   doc), so a band check on them would be comparing incompatible scales.
///   They're also semantically "continue what you started," the opposite
///   of "novel."
pub fn is_exploration_eligible(candidate: &Candidate) -> bool {
    match candidate.source {
        CandidateSource::AvailableNow => true,
        CandidateSource::Taste => in_adjacency_band(candidate.taste_fit),
        CandidateSource::OnDeck | CandidateSource::Gap => false,
    }
}

/// A tunable serendipity/range control: the fraction of a programmed
/// channel's slots reserved for exploration, 0.0 (pure-safe, no
/// exploration ever) to 1.0 (every slot is an exploration attempt). This is
/// the value a future GUI's "how adventurous tonight?" slider sets — see
/// [`SerendipityRange::from_percent`] for the 0-100 mapping a UI naturally
/// wants.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SerendipityRange {
    fraction: f64,
}

impl SerendipityRange {
    /// Build from a raw 0.0-1.0 fraction. Non-finite (`NaN`/`inf`) or
    /// out-of-range input is clamped to `[0.0, 1.0]` rather than panicking
    /// or propagating — a range control must never be able to crash a
    /// schedule build from a bad caller value.
    pub fn from_fraction(fraction: f64) -> Self {
        let f = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self { fraction: f }
    }

    /// Build from a 0-100 percent value (the natural unit for a GUI
    /// slider). `percent / 100.0`, then the same clamp as
    /// [`Self::from_fraction`].
    pub fn from_percent(percent: f64) -> Self {
        Self::from_fraction(percent / 100.0)
    }

    /// The pure-safe control: 0% exploration, ever. Equivalent to
    /// `SerendipityRange::from_fraction(0.0)`, named for readability at call
    /// sites (`SerendipityRange::pure_safe()` reads better than a bare
    /// `0.0` at a glance).
    pub fn pure_safe() -> Self {
        Self::from_fraction(0.0)
    }

    /// The already-clamped `[0.0, 1.0]` fraction.
    pub fn fraction(&self) -> f64 {
        self.fraction
    }

    /// `true` only for the exact 0% setting — the ONE case the ≥1
    /// exploration-slot guarantee explicitly does NOT apply to (AC: "A LOW
    /// setting still GUARANTEES ≥1 non-obvious slot per session UNLESS set
    /// to pure-safe (0%)").
    pub fn is_pure_safe(&self) -> bool {
        self.fraction <= 0.0
    }

    /// How many slots apart a modular exploration reservation lands, for
    /// this fraction. Identical shape to MUSEX-05's `director::
    /// exploration_interval` (moved here so the range-control type owns its
    /// own reservation math): `<= 0.0` disables modular reservation
    /// (`usize::MAX`, never hit by `%`); `>= 1.0` reserves every slot;
    /// otherwise the nearest interval to `1.0 / fraction`, floored at 1.
    ///
    /// NOTE: this is the CADENCE a nonzero fraction settles into once the
    /// ≥1 guarantee (implemented in `director::program_channel`, not here —
    /// it needs live scheduling state the range control alone doesn't have)
    /// has already produced its first exploration slot. A tiny fraction
    /// still gets a huge interval here (rare steady-state reservations),
    /// which is correct: the GUARANTEE is a one-time floor, not a promise
    /// that a 1% setting behaves like a 25% one after slot 0.
    pub fn reservation_interval(&self) -> usize {
        if self.fraction <= 0.0 {
            return usize::MAX;
        }
        if self.fraction >= 1.0 {
            return 1;
        }
        ((1.0 / self.fraction).round() as usize).max(1)
    }
}

impl Default for SerendipityRange {
    /// 0% — pure-safe. Matches MUSEX-05's own `DirectorConstraints` (whose
    /// `serendipity_budget: f64` field defaults to whatever the caller sets
    /// — most tests/presets that want "no exploration" use `0.0`
    /// explicitly), so constructing a range control with `Default::default()`
    /// never surprises an existing caller with unexpected exploration.
    fn default() -> Self {
        Self::pure_safe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::media_metadata::MediaKind;

    fn candidate(source: CandidateSource, taste_fit: f64) -> Candidate {
        Candidate {
            media_metadata_id: 1,
            media_item_id: Some(1),
            title: "Title".to_string(),
            year: Some(2020),
            kind: MediaKind::Movie,
            source,
            taste_fit,
            facts: vec!["a fact".to_string()],
            availability: None,
        }
    }

    // --- fraction / percent construction + clamping ------------------------

    #[test]
    fn from_fraction_clamps_out_of_range() {
        assert_eq!(SerendipityRange::from_fraction(-0.5).fraction(), 0.0);
        assert_eq!(SerendipityRange::from_fraction(1.5).fraction(), 1.0);
        assert_eq!(SerendipityRange::from_fraction(0.3).fraction(), 0.3);
    }

    #[test]
    fn from_fraction_rejects_non_finite() {
        assert_eq!(SerendipityRange::from_fraction(f64::NAN).fraction(), 0.0);
        assert_eq!(
            SerendipityRange::from_fraction(f64::INFINITY).fraction(),
            0.0
        );
    }

    #[test]
    fn from_percent_maps_0_to_100_onto_0_to_1() {
        assert_eq!(SerendipityRange::from_percent(0.0).fraction(), 0.0);
        assert_eq!(SerendipityRange::from_percent(25.0).fraction(), 0.25);
        assert_eq!(SerendipityRange::from_percent(100.0).fraction(), 1.0);
        // out-of-range percent still clamps at the fraction stage
        assert_eq!(SerendipityRange::from_percent(150.0).fraction(), 1.0);
        assert_eq!(SerendipityRange::from_percent(-10.0).fraction(), 0.0);
    }

    #[test]
    fn pure_safe_and_default_agree_and_are_zero() {
        assert_eq!(SerendipityRange::pure_safe(), SerendipityRange::default());
        assert!(SerendipityRange::pure_safe().is_pure_safe());
        assert_eq!(SerendipityRange::pure_safe().fraction(), 0.0);
    }

    #[test]
    fn is_pure_safe_is_false_for_any_positive_fraction() {
        assert!(!SerendipityRange::from_fraction(0.01).is_pure_safe());
        assert!(!SerendipityRange::from_fraction(1.0).is_pure_safe());
    }

    // --- reservation interval (unchanged shape from MUSEX-05) --------------

    #[test]
    fn reservation_interval_matches_documented_shape() {
        assert_eq!(
            SerendipityRange::pure_safe().reservation_interval(),
            usize::MAX
        );
        assert_eq!(
            SerendipityRange::from_fraction(1.0).reservation_interval(),
            1
        );
        assert_eq!(
            SerendipityRange::from_fraction(1.5).reservation_interval(),
            1
        );
        assert_eq!(
            SerendipityRange::from_fraction(0.25).reservation_interval(),
            4
        );
        assert_eq!(
            SerendipityRange::from_fraction(0.5).reservation_interval(),
            2
        );
    }

    // --- adjacency band ------------------------------------------------------

    #[test]
    fn adjacency_band_boundaries() {
        assert!(!in_adjacency_band(NOVELTY_LOW - 0.01));
        assert!(in_adjacency_band(NOVELTY_LOW));
        assert!(in_adjacency_band((NOVELTY_LOW + NOVELTY_HIGH) / 2.0));
        assert!(!in_adjacency_band(NOVELTY_HIGH));
        assert!(!in_adjacency_band(NOVELTY_HIGH + 0.01));
    }

    #[test]
    fn adjacency_band_excludes_nan() {
        assert!(!in_adjacency_band(f64::NAN));
    }

    // --- exploration eligibility: the core "not random / not top-safe" -----

    #[test]
    fn available_now_is_always_eligible_regardless_of_taste_fit() {
        // Low, mid, AND high taste_fit — AvailableNow's taste_fit is a
        // popularity score, not a taste-vector similarity, so the band
        // never gates it (preserves MUSEX-05 unconditionally).
        for taste_fit in [0.0, 0.3, 0.5, 0.9, 1.0] {
            assert!(is_exploration_eligible(&candidate(
                CandidateSource::AvailableNow,
                taste_fit
            )));
        }
    }

    #[test]
    fn taste_candidate_in_band_is_eligible() {
        assert!(is_exploration_eligible(&candidate(
            CandidateSource::Taste,
            0.5
        )));
    }

    #[test]
    fn taste_candidate_in_the_safe_core_is_not_eligible() {
        // A near-top-affinity taste pick is exactly the "safe/known core"
        // the AC says exploration must sit OUTSIDE of.
        assert!(!is_exploration_eligible(&candidate(
            CandidateSource::Taste,
            0.9
        )));
    }

    #[test]
    fn taste_candidate_too_dissimilar_is_not_eligible() {
        // Below NOVELTY_LOW would be indistinguishable from a random pick —
        // the AC explicitly rules that out.
        assert!(!is_exploration_eligible(&candidate(
            CandidateSource::Taste,
            0.05
        )));
    }

    #[test]
    fn on_deck_and_gap_are_never_eligible_even_in_band() {
        // Their taste_fit is percent-complete / engagement, not taste
        // similarity — a band check on them would compare incompatible
        // scales, so they're excluded outright regardless of value.
        assert!(!is_exploration_eligible(&candidate(
            CandidateSource::OnDeck,
            0.5
        )));
        assert!(!is_exploration_eligible(&candidate(
            CandidateSource::Gap,
            0.5
        )));
    }
}
