//! MUSEM-04: quality-tier resolution, custom-format matcher evaluation, and
//! the gates (allowed-tier / size-per-minute / min-format-score) that decide
//! whether one candidate release even survives to the ranking step in
//! [`super::decide_release`].
//!
//! Everything here is pure and I/O-free — every fact a candidate needs
//! (parsed release attributes, size, seeders, freeleech, languages...) comes
//! from [`crate::models::release::Release`] (MUSE-16's rolling grabbability
//! snapshot, already populated by `prowlarr::parse::parse_release_name` +
//! the Prowlarr report-pull worker) plus a target-runtime hint this module
//! adds ([`ReleaseCandidate::runtime_minutes`]) since a release's own row
//! has no notion of "minutes of the movie/episode it is a release OF" —
//! that's a fact about the requested media item, not the release. No new
//! quality/custom-format types are invented: this consumes
//! [`crate::models::quality::QualityDefinition`],
//! [`crate::models::quality::QualityProfile`],
//! [`crate::models::quality::CustomFormat`], and
//! [`crate::models::quality::QualityProfileFormat`] exactly as MUSE-02 shipped
//! them.

use std::collections::BTreeSet;

use serde_json::Value as Json;

use crate::models::quality::{CustomFormat, QualityDefinition, QualityProfile, QualityProfileFormat};
use crate::models::release::Release;

/// One candidate release plus the one piece of context a [`Release`] row
/// can't carry on its own: how long (in minutes) the media item it would
/// satisfy actually runs, needed for the size-per-minute mis-tag guard
/// (blueprint §2). `None` when the caller doesn't know the runtime yet (the
/// size-per-minute gate is then skipped for this candidate rather than
/// guessed at — see [`size_per_minute_ok`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseCandidate {
    pub release: Release,
    pub runtime_minutes: Option<i32>,
}

/// What's already held for the target media item, when [`super::decide_release`]
/// is being asked "is anything here worth upgrading to" rather than "what
/// should I grab for the first time". `None` in
/// [`ScoringPolicy::existing`] means there is nothing on disk yet — every
/// eligible candidate is by definition an improvement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistingRelease {
    pub quality_definition_id: i64,
    pub total_format_score: i32,
}

/// Everything [`super::decide_release`] and [`evaluate_candidate`] need
/// beyond the candidate list, the profile, and the format-score table
/// itself: the two lookup tables a profile's `items`/format-score fields
/// reference by id ([`QualityDefinition`], [`CustomFormat`]), and the
/// optional "what's already held" fact for upgrade decisions.
#[derive(Debug, Clone, Copy)]
pub struct ScoringPolicy<'a> {
    pub definitions: &'a [QualityDefinition],
    pub custom_formats: &'a [CustomFormat],
    pub existing: Option<ExistingRelease>,
}

/// Why a candidate didn't survive gating, or the facts it survived with.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateEvaluation {
    pub quality_definition_id: i64,
    pub quality_key: String,
    /// Position of this candidate's tier within `profile.items` (see
    /// [`tier_position`]) — higher is more preferred. Used as the primary
    /// ranking key.
    pub tier_rank: usize,
    pub total_format_score: i32,
    pub matched_format_ids: BTreeSet<i64>,
}

/// Resolve a candidate's parsed `source`/`resolution` to one
/// [`QualityDefinition`] row. Case-insensitive, exact match on both fields
/// (a `None` resolution on the candidate only matches a definition whose
/// `resolution` is also `None`, e.g. an audio-only or resolution-agnostic
/// tier) — deliberately conservative: a release the parser couldn't pin a
/// source/resolution for does not get fuzzily matched to *some* tier, it
/// resolves to nothing and the caller fails closed.
pub fn resolve_tier<'a>(
    release: &Release,
    definitions: &'a [QualityDefinition],
) -> Option<&'a QualityDefinition> {
    let source = release.source.as_deref()?;
    definitions.iter().find(|d| {
        d.source.eq_ignore_ascii_case(source)
            && opt_eq_ignore_ascii_case(d.resolution.as_deref(), release.resolution.as_deref())
    })
}

fn opt_eq_ignore_ascii_case(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        (None, None) => true,
        _ => false,
    }
}

/// Find a quality definition's position + `allowed` flag within a profile's
/// `items` JSON (blueprint §2: `[{quality:{id,...}, items:[...nested
/// group...], allowed:bool}, ...]`). Walks one level of group nesting
/// (`items` inside an entry) so a tier that only appears inside a named
/// quality-group (e.g. "WEB 1080p" grouping WEBDL+WEBRip) still resolves.
///
/// **Order convention** (undocumented in the live *arr data recon, so fixed
/// here and applied consistently): index 0 is the *least* preferred allowed
/// tier and the last index is the *most* preferred — i.e. the array reads
/// low-to-high preference. This only needs to be internally consistent for
/// ranking to be correct; it does not need to match a specific *arr UI
/// convention since [`super::decide_release`] never compares its ranking
/// against a live *arr instance.
pub fn tier_position(items: &Json, quality_definition_id: i64) -> Option<(usize, bool)> {
    let array = items.as_array()?;
    for (idx, entry) in array.iter().enumerate() {
        let entry_allowed = entry.get("allowed").and_then(Json::as_bool).unwrap_or(true);
        if entry_matches_id(entry, quality_definition_id) {
            return Some((idx, entry_allowed));
        }
        if let Some(nested) = entry.get("items").and_then(Json::as_array) {
            if let Some(leaf) = nested.iter().find(|n| entry_matches_id(n, quality_definition_id)) {
                // Honor the leaf item's OWN `allowed` flag (review: codex —
                // a quality explicitly disallowed inside an otherwise-
                // allowed group must still be rejected), falling back to
                // the group's flag when the leaf doesn't specify its own. A
                // group disallowed as a whole disallows every member
                // regardless of a leaf's own flag.
                let leaf_allowed = leaf.get("allowed").and_then(Json::as_bool).unwrap_or(entry_allowed);
                return Some((idx, entry_allowed && leaf_allowed));
            }
        }
    }
    None
}

fn entry_matches_id(entry: &Json, quality_definition_id: i64) -> bool {
    entry
        .get("quality")
        .and_then(|q| q.get("id"))
        .and_then(Json::as_i64)
        == Some(quality_definition_id)
}

/// The blueprint §2 mis-tag guard: a candidate's size-per-minute must fall
/// within the resolved tier's `[min, max]` MB-per-minute bounds, when both
/// the tier defines bounds AND the candidate has enough information
/// (`size_bytes` + [`ReleaseCandidate::runtime_minutes`]) to compute a
/// value. Missing information on either side is *not* a rejection — it
/// means the gate has nothing to check, not that the candidate failed it.
pub fn size_per_minute_ok(definition: &QualityDefinition, candidate: &ReleaseCandidate) -> bool {
    let (Some(size_bytes), Some(minutes)) = (candidate.release.size_bytes, candidate.runtime_minutes)
    else {
        return true;
    };
    if minutes <= 0 {
        return true;
    }
    let mb_per_min = (size_bytes as f64 / (1024.0 * 1024.0)) / minutes as f64;
    if let Some(min) = definition.min_size_mb_per_min {
        if mb_per_min < min as f64 {
            return false;
        }
    }
    if let Some(max) = definition.max_size_mb_per_min {
        if mb_per_min > max as f64 {
            return false;
        }
    }
    true
}

/// A pluggable release scorer — the charter's AI seam (module doc:
/// "a local-LLM scorer can later be registered alongside the static
/// scorers"). Each scorer independently judges which `custom_formats` rows
/// it thinks a candidate matches; [`ScorerRegistry::matched_format_ids`]
/// unions every scorer's verdict before the profile's per-format score
/// table is summed, so adding a new scorer never requires touching
/// [`evaluate_candidate`] or [`super::decide_release`].
pub trait Scorer: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, candidate: &ReleaseCandidate, custom_formats: &[CustomFormat]) -> BTreeSet<i64>;
}

/// The only scorer this item ships: deterministic evaluation of
/// `custom_formats.specifications` (blueprint §6/§7.5) against a candidate's
/// [`Release`] fields. Mirrors *arr's own "required specs must all match,
/// AND at least one optional spec matches if any optional specs exist"
/// combining rule. Supported `implementation` values (documented
/// simplification — not full parity with every *arr specification variant,
/// since `Release` doesn't carry every raw field *arr's file-probing does):
/// `ReleaseTitleSpecification` (case-insensitive substring match on title —
/// deliberately no regex engine, same "no external crate, conservative
/// matching" philosophy `prowlarr::parse` already uses), `SourceSpecification`,
/// `ResolutionSpecification`, `EditionSpecification` (case-insensitive
/// substring on edition), `LanguageSpecification`, `SizeSpecification`
/// (`min_mb`/`max_mb` fields), `IndexerFlagSpecification` (only
/// `"freeleech"` is modeled, since `Release` only carries a `freeleech`
/// bool, not a raw flag list). An unrecognized `implementation` never
/// matches (fail-closed — a misconfigured/unknown rule cannot silently
/// accumulate score).
pub struct CustomFormatScorer;

impl Scorer for CustomFormatScorer {
    fn name(&self) -> &'static str {
        "custom_format"
    }

    fn matches(&self, candidate: &ReleaseCandidate, custom_formats: &[CustomFormat]) -> BTreeSet<i64> {
        custom_formats
            .iter()
            .filter(|cf| custom_format_matches(cf, candidate))
            .map(|cf| cf.id)
            .collect()
    }
}

fn custom_format_matches(format: &CustomFormat, candidate: &ReleaseCandidate) -> bool {
    let specs = match format.specifications.as_array() {
        Some(s) => s.as_slice(),
        None => return false,
    };
    if specs.is_empty() {
        return false;
    }

    let all_required_match = specs
        .iter()
        .filter(|s| spec_required(s))
        .all(|s| spec_matches(s, candidate));
    if !all_required_match {
        return false;
    }

    let mut optional = specs.iter().filter(|s| !spec_required(s)).peekable();
    if optional.peek().is_none() {
        return true;
    }
    optional.any(|s| spec_matches(s, candidate))
}

fn spec_required(spec: &Json) -> bool {
    spec.get("required").and_then(Json::as_bool).unwrap_or(false)
}

fn spec_matches(spec: &Json, candidate: &ReleaseCandidate) -> bool {
    let negate = spec.get("negate").and_then(Json::as_bool).unwrap_or(false);
    let implementation = spec.get("implementation").and_then(Json::as_str).unwrap_or("");
    let fields = spec.get("fields").cloned().unwrap_or(Json::Null);
    let raw = spec_implementation_matches(implementation, &fields, candidate);
    if negate {
        !raw
    } else {
        raw
    }
}

fn spec_implementation_matches(implementation: &str, fields: &Json, candidate: &ReleaseCandidate) -> bool {
    let release = &candidate.release;
    match implementation {
        "ReleaseTitleSpecification" => field_substring_matches(fields, &release.title),
        "EditionSpecification" => match &release.edition {
            Some(edition) => field_substring_matches(fields, edition),
            None => false,
        },
        "SourceSpecification" => field_str_eq(fields, release.source.as_deref()),
        "ResolutionSpecification" => field_str_eq(fields, release.resolution.as_deref()),
        "LanguageSpecification" => {
            let Some(value) = fields.get("value").and_then(Json::as_str) else {
                return false;
            };
            release.languages.iter().any(|l| l.eq_ignore_ascii_case(value))
        }
        "IndexerFlagSpecification" => {
            let flag = fields.get("value").and_then(Json::as_str).unwrap_or("");
            flag.eq_ignore_ascii_case("freeleech") && release.freeleech
        }
        "SizeSpecification" => {
            let Some(size_bytes) = release.size_bytes else {
                return false;
            };
            let size_mb = size_bytes as f64 / (1024.0 * 1024.0);
            let min_ok = fields
                .get("min_mb")
                .and_then(Json::as_f64)
                .map(|min| size_mb >= min)
                .unwrap_or(true);
            let max_ok = fields
                .get("max_mb")
                .and_then(Json::as_f64)
                .map(|max| size_mb <= max)
                .unwrap_or(true);
            min_ok && max_ok
        }
        _ => false,
    }
}

fn field_str_eq(fields: &Json, actual: Option<&str>) -> bool {
    let (Some(expected), Some(actual)) = (fields.get("value").and_then(Json::as_str), actual) else {
        return false;
    };
    expected.eq_ignore_ascii_case(actual)
}

/// Case-insensitive substring match — the documented simplification for
/// `ReleaseTitleSpecification`/`EditionSpecification` (no regex engine; see
/// the [`CustomFormatScorer`] doc). An empty `value` never matches
/// (fail-closed, not "matches everything").
fn field_substring_matches(fields: &Json, haystack: &str) -> bool {
    let Some(pattern) = fields.get("value").and_then(Json::as_str) else {
        return false;
    };
    if pattern.is_empty() {
        return false;
    }
    haystack.to_lowercase().contains(&pattern.to_lowercase())
}

/// Union every registered [`Scorer`]'s verdict, then look up each matched
/// format's score for this profile (unscored/unknown-to-this-profile
/// formats contribute 0, not an error — a profile is free to leave a
/// format unscored).
pub struct ScorerRegistry {
    scorers: Vec<Box<dyn Scorer>>,
}

impl ScorerRegistry {
    /// The deterministic scorer set this item ships (just
    /// [`CustomFormatScorer`]). A future LLM/taste scorer registers here
    /// too — see the [`Scorer`] doc.
    pub fn deterministic() -> Self {
        Self {
            scorers: vec![Box::new(CustomFormatScorer)],
        }
    }

    pub fn matched_format_ids(&self, candidate: &ReleaseCandidate, custom_formats: &[CustomFormat]) -> BTreeSet<i64> {
        let mut ids = BTreeSet::new();
        for scorer in &self.scorers {
            ids.extend(scorer.matches(candidate, custom_formats));
        }
        ids
    }
}

/// Run every gate (tier resolution, allowed, size-per-minute, min-format-
/// score) for one candidate against `profile`. `Ok` carries everything
/// [`super::decide_release`] needs to rank the candidate; `Err` carries the
/// human-readable reason it was rejected.
pub fn evaluate_candidate(
    candidate: &ReleaseCandidate,
    profile: &QualityProfile,
    format_scores: &[QualityProfileFormat],
    registry: &ScorerRegistry,
    policy: &ScoringPolicy<'_>,
) -> Result<CandidateEvaluation, String> {
    let release = &candidate.release;

    let definition = resolve_tier(release, policy.definitions).ok_or_else(|| {
        format!(
            "{}: unresolvable quality (source={:?}, resolution={:?}) — no matching quality_definitions row",
            release.guid, release.source, release.resolution
        )
    })?;

    let (tier_rank, allowed) = tier_position(&profile.items, definition.id).ok_or_else(|| {
        format!(
            "{}: quality tier {:?} is not present in profile {:?}'s items",
            release.guid, definition.quality_key, profile.name
        )
    })?;
    if !allowed {
        return Err(format!(
            "{}: quality tier {:?} is not allowed by profile {:?}",
            release.guid, definition.quality_key, profile.name
        ));
    }

    if !size_per_minute_ok(definition, candidate) {
        return Err(format!(
            "{}: size-per-minute out of bounds for tier {:?}",
            release.guid, definition.quality_key
        ));
    }

    let matched_format_ids = registry.matched_format_ids(candidate, policy.custom_formats);
    let total_format_score: i32 = format_scores
        .iter()
        .filter(|fs| fs.quality_profile_id == profile.id && matched_format_ids.contains(&fs.custom_format_id))
        .map(|fs| fs.score)
        .sum();

    if total_format_score < profile.min_format_score {
        return Err(format!(
            "{}: total format score {} below profile min_format_score {}",
            release.guid, total_format_score, profile.min_format_score
        ));
    }

    Ok(CandidateEvaluation {
        quality_definition_id: definition.id,
        quality_key: definition.quality_key.clone(),
        tier_rank,
        total_format_score,
        matched_format_ids,
    })
}
