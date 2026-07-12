-- MUSE-02: custom formats — the named, scored matcher-rule registry
-- (ARR-BLUEPRINT §2/§6/§7.5: "quality/preference intent = a named scorer
-- registry + a per-library score table, not a hardcoded ruleset").
--
-- This is a documented SEAM only in MUSE-02: `specifications` holds matcher
-- rule definitions (regex/field specs against parsed release attributes,
-- same shape family as *arr's ReleaseTitleSpecification /
-- SourceSpecification / etc.) but nothing evaluates them yet — the scorer
-- itself (including a Phase-1 local-LLM taste-aware scorer as "just another
-- rule") is out of scope for this migration/spec item.
CREATE TABLE custom_formats (
    id                          bigserial PRIMARY KEY,
    name                        text NOT NULL UNIQUE,
    specifications              jsonb NOT NULL DEFAULT '[]', -- [{implementation, negate, required, fields:{...}}, ...]
    include_when_renaming       boolean NOT NULL DEFAULT false,
    created_at                  timestamptz NOT NULL DEFAULT now(),
    updated_at                  timestamptz NOT NULL DEFAULT now()
);

-- Quality profiles — per-library "what to grab, what to prefer" policy.
-- Divergence from spec §3.2: `cutoff` becomes a real FK to
-- quality_definitions (not a free-text quality string), and the
-- FormatItems scoring table is modeled as its own join
-- (quality_profile_formats) rather than a jsonb blob, so the seam in
-- custom_formats above is queryable.
CREATE TABLE quality_profiles (
    id                        bigserial PRIMARY KEY,
    name                      text NOT NULL UNIQUE,
    cutoff_quality_id         bigint REFERENCES quality_definitions(id),
    items                     jsonb NOT NULL DEFAULT '[]', -- ordered allowed-qualities list (incl. nested quality groups)
    language                  text,
    upgrade_allowed           boolean NOT NULL DEFAULT true,
    min_format_score          int NOT NULL DEFAULT 0,
    cutoff_format_score       int NOT NULL DEFAULT 0,
    min_upgrade_format_score  int NOT NULL DEFAULT 1,
    natural_language_intent   text,                        -- Phase-1: "small, good-enough, no HDR"
    created_at                timestamptz NOT NULL DEFAULT now(),
    updated_at                timestamptz NOT NULL DEFAULT now()
);

-- Per-profile custom-format score table (*arr QualityProfiles.FormatItems parity).
CREATE TABLE quality_profile_formats (
    quality_profile_id bigint NOT NULL REFERENCES quality_profiles(id) ON DELETE CASCADE,
    custom_format_id   bigint NOT NULL REFERENCES custom_formats(id) ON DELETE CASCADE,
    score               int NOT NULL DEFAULT 0,
    PRIMARY KEY (quality_profile_id, custom_format_id)
);
