## Release-decision engine

MUSEM-04 adds `src/decision/` — the pure, deterministic "what to grab" engine that will sit
between a future targeted Prowlarr search (MUSEM-03) and a future qBittorrent grab
(MUSEM-02): given a set of candidate releases, a `quality_profiles` row, and its
`quality_profile_formats` score table, pick the single best *eligible* release, or reject with
reasons. Mirrors `arr::request::classify_tier`'s shape — **no I/O of its own**; every signal is
passed in by the caller, which is what makes it exhaustively unit-testable.

- `decision::decide_release(candidates, profile, format_scores, policy) -> Decision` — the
  entrypoint. `Decision` is `Grab(ReleaseChoice)` or `Reject { reasons: Vec<String> }` (never a
  silent empty result).
- `decision::scoring::ReleaseCandidate` wraps the existing `models::release::Release` row (MUSE-16's
  rolling Prowlarr-report snapshot) plus a `runtime_minutes` hint — the one fact a release row
  can't carry on its own (how long the media item it would satisfy runs), needed for the blueprint
  §2 size-per-minute mis-tag guard. No new candidate/quality types are invented — this consumes
  `models::quality::{QualityDefinition, QualityProfile, CustomFormat, QualityProfileFormat}`
  exactly as MUSE-02 shipped them.
- **Gates** (any failure rejects that candidate with a reason, never a default grab):
  quality-tier resolution (source+resolution → a `quality_definitions` row; unresolvable = fail
  closed), the tier's `allowed` flag in `profile.items`, size-per-minute bounds, and
  `min_format_score`.
- **Custom-format scoring**: `decision::scoring::CustomFormatScorer` evaluates
  `custom_formats.specifications` (blueprint §6/§7.5) against a candidate — `ReleaseTitleSpecification`/
  `EditionSpecification` (case-insensitive substring, deliberately no regex engine — no new
  dependency, same conservative-matching philosophy as `prowlarr::parse`), `SourceSpecification`,
  `ResolutionSpecification`, `LanguageSpecification`, `SizeSpecification`, and
  `IndexerFlagSpecification` (freeleech only, since `Release` doesn't carry a raw indexer-flag
  list). Combining rule mirrors *arr: all `required` specs must match, and at least one
  non-required spec must match if any exist.
- **Scorer registry seam**: matching is behind a `decision::scoring::Scorer` trait +
  `ScorerRegistry` (currently just `CustomFormatScorer::deterministic()`) specifically so a future
  local-LLM/taste scorer (the charter's Phase-1 AI release-selection seam) can register alongside
  the static scorers without `decide_release` itself changing.
- **Ranking** of gate-surviving candidates: quality-tier rank (the profile's `items` preference
  order) → total format score → `proper_repack` (REPACK/PROPER beats an otherwise-equal
  non-repack) → seeders (`None`/unknown is never coerced to `0` — sorted between a known-positive
  count and a known-zero one, so an unreported private-tracker seeder count isn't unfairly sunk)
  → freeleech → smaller size → the release `guid` as a fully deterministic final tiebreak.
- **Upgrade decisions**: `ScoringPolicy::existing` (`Some(ExistingRelease{quality_definition_id,
  total_format_score})`) turns the call into "is anything here worth upgrading to". If the
  existing file already meets the profile's cutoff (tier ≥ `cutoff_quality_id`'s rank AND format
  score ≥ `cutoff_format_score`), or `upgrade_allowed` is `false`, `decide_release` rejects
  outright ("good enough, stop") without even reaching ranking; otherwise the best candidate must
  beat the existing file by tier or by at least `min_upgrade_format_score`.

As of MUSEM-05, `decision::decide_release` IS wired — see the next section.

