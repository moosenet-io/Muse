-- FOUNDRY-06: which titles the operator has marked for Path B renditions.
--
-- Path B's ladder (FOUNDRY-03) has existed with NO TRIGGER: `plan_ladder` had
-- no caller outside the foundry module, so renditions could not be produced at
-- all. That was safe — the operator's constraint is that renditions are
-- generated ONLY for titles they mark, never library-wide, and "no trigger"
-- satisfies that trivially — but it also meant the feature did not work.
--
-- This table IS the enforcement point. A rendition run reads marks; it never
-- enumerates the library. The absence of a row is the absence of consent.

CREATE TABLE IF NOT EXISTS rendition_marks (
    id             bigserial PRIMARY KEY,

    -- The marked SCOPE, exactly as the operator chose it in the UI: a movie, a
    -- season, or a whole show. Stored rather than expanded at mark time,
    -- because a season marked today should cover an episode that arrives
    -- tomorrow — expanding on write would silently miss it.
    scope          text NOT NULL,

    -- Absolute path of the marked thing: a file for `movie`, a directory for
    -- `season`/`show`. Expansion to individual files happens at RUN time and
    -- is counted there, so the number of encodes a mark implies is visible
    -- before anything runs.
    path           text NOT NULL,

    -- Which rungs. A subset of mobile/web/tv/hifi, never "all by default" —
    -- the operator's whole point was NOT generating four versions of
    -- everything.
    rungs          text[] NOT NULL,

    -- Who marked it and when, so an unexpected rendition can be traced back to
    -- a decision rather than to the system.
    marked_by      text,
    created_at     timestamptz NOT NULL DEFAULT now(),

    -- Set when the operator un-marks. Kept rather than deleted so "why does
    -- this title have renditions?" remains answerable after the fact.
    revoked_at     timestamptz
);

-- A scope must be one of the three the UI can express. Free text here would
-- let a typo'd scope silently match nothing at run time, which reads as "the
-- mark did nothing" rather than as an error.
ALTER TABLE rendition_marks
    DROP CONSTRAINT IF EXISTS rendition_marks_scope_values;
ALTER TABLE rendition_marks
    ADD CONSTRAINT rendition_marks_scope_values CHECK (
        scope IN ('movie', 'season', 'show')
    );

-- At least one rung, or the mark expresses nothing. An empty array would
-- produce a run that examines the title and emits no renditions, which is
-- indistinguishable from a bug.
ALTER TABLE rendition_marks
    DROP CONSTRAINT IF EXISTS rendition_marks_rungs_nonempty;
ALTER TABLE rendition_marks
    ADD CONSTRAINT rendition_marks_rungs_nonempty CHECK (
        -- `cardinality`, NOT `array_length`. array_length() returns NULL for an
        -- empty array, and a CHECK passes on NULL (it only rejects FALSE) — so
        -- `array_length(rungs,1) >= 1` accepted the exact value it was written
        -- to reject. Caught by running this migration against a real Postgres
        -- rather than by reading it.
        cardinality(rungs) >= 1
    );

-- Rungs must be REAL rung names, by the same reasoning `scope` has a value
-- check: a typo'd rung silently matches nothing at run time, which reads as
-- "the mark did nothing" rather than as an error. Opus raised the
-- inconsistency at the FOUNDRY-06 gate — the migration argued the case for
-- `scope` and then did not apply it here.
--
-- `<@` also rejects ARRAY[NULL], which slips past a cardinality check with
-- length 1.
ALTER TABLE rendition_marks
    DROP CONSTRAINT IF EXISTS rendition_marks_rung_values;
ALTER TABLE rendition_marks
    ADD CONSTRAINT rendition_marks_rung_values CHECK (
        rungs <@ ARRAY['mobile', 'web', 'tv', 'hifi']::text[]
    );

-- One LIVE mark per path. Re-marking a path updates the existing row rather
-- than stacking duplicates that would each expand to the same encodes.
CREATE UNIQUE INDEX IF NOT EXISTS rendition_marks_one_live_per_path
    ON rendition_marks (path)
    WHERE revoked_at IS NULL;

CREATE INDEX IF NOT EXISTS rendition_marks_live_idx
    ON rendition_marks (created_at)
    WHERE revoked_at IS NULL;

COMMENT ON TABLE rendition_marks IS
    'Path B consent. A rendition run reads THIS; it never enumerates the library. The '
    'absence of a row is the absence of consent — which is why the run path must have no '
    'way to produce a candidate list from anywhere else.';
COMMENT ON COLUMN rendition_marks.scope IS
    'movie | season | show — stored unexpanded so a season marked today covers an episode '
    'that arrives tomorrow.';
