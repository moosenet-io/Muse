-- MPRB-05 (S130-A): make `media_files.media_info` a VERSIONED probe document,
-- and record per-file what the probe actually did.
--
-- ## Migration number
-- The spec (`specs/S130-A-maestro-probe.md`) names `0109_media_files_probe.sql`.
-- That number was already TAKEN by `0109_artwork_renditions.sql` before this item
-- was written; `main` ended at `0112_rendition_marks.sql`, so this is 0113. The
-- spec is 91 commits stale and says so in its own banner — the number was checked
-- against the tree, not inherited from the document.
--
-- ## What this delivers
-- `0009_media_files.sql` line 28 promised a described `media_info`; nothing has
-- ever populated it beyond `{"container": "<file extension>"}` written by
-- `src/library/scan.rs` from the FILENAME, never from the file's contents. That
-- is the founding defect this epic exists to fix. `0009` is NOT edited here.
--
-- ## Additive only, and that is load-bearing
-- Every statement is `IF NOT EXISTS` / `DROP … IF EXISTS` + `ADD`, so:
--   * running it twice is a no-op, and
--   * running it against a live service is safe — a binary that predates it is
--     unaffected, because nothing it reads changes shape.
-- The S127 lesson applies in the other direction though: a binary that POSTDATES
-- it does `SELECT *` into `MediaFile`, which now expects these columns. Ship the
-- migration WITH (or before) the deploy, never after.

ALTER TABLE media_files
    -- Mirrors `media_info -> 'schema_version'`. It exists ONLY so the backfill
    -- queue predicate is indexable: `jsonb ->> 'schema_version'` is not, absent a
    -- functional index. The DOCUMENT is authoritative and the column is a hint;
    -- `repo::media_file::set_probe_result` writes both in one statement so they
    -- cannot diverge, and `MediaFile::stored_media_info()` reads the document.
    ADD COLUMN IF NOT EXISTS media_info_version int,

    -- When the probe last ran. Set on success AND on failure — "we tried at
    -- 04:12 and it failed" is a fact worth as much as a successful probe.
    ADD COLUMN IF NOT EXISTS probed_at          timestamptz,

    -- ok | suspicious | unreadable | probe_failed. See the CHECK below.
    ADD COLUMN IF NOT EXISTS probe_state        text,

    -- One column answers "what is wrong with this file" for ALL of the unhappy
    -- states: the `ProbeError` description for a failure, the suspicion
    -- description for a result that parsed but looks wrong.
    ADD COLUMN IF NOT EXISTS probe_error        text,

    -- Bounds the backfill. `list_needing_probe` takes `max_attempts` and stops
    -- returning a file that has burned it, so 16,000 items cannot become an
    -- infinite retry loop over the handful that will never parse.
    ADD COLUMN IF NOT EXISTS probe_attempts     int NOT NULL DEFAULT 0;

-- `probe_state` is CHECK-constrained TEXT, not a Postgres enum, and the choice is
-- deliberate. The crate's existing enum (`release_type_kind`) is a stable domain
-- type. This one will plausibly gain a variant, and widening a checked text column
-- is a migration whose deploy can be ordered independently of the code that uses
-- it — `ALTER TYPE … ADD VALUE` cannot even run inside a transaction block.
--
-- NULL is permitted and MEANS "never probed": every row that predates this
-- migration is in exactly that state, and a CHECK only rejects FALSE, so NULL
-- passes without a special-cased default that would claim a probe that never ran.
ALTER TABLE media_files
    DROP CONSTRAINT IF EXISTS media_files_probe_state_values;
ALTER TABLE media_files
    ADD CONSTRAINT media_files_probe_state_values CHECK (
        probe_state IS NULL
        OR probe_state IN ('ok', 'suspicious', 'unreadable', 'probe_failed')
    );

-- The backfill queue predicate. Keyset pagination on `id` (never OFFSET, which
-- degrades quadratically over a library this size), so `id` is the indexed column
-- and the predicate is what makes the index small.
--
-- Note the version literal is `1`. A partial-index predicate must be constant, so
-- bumping MEDIA_INFO_SCHEMA_VERSION to 2 requires a NEW migration adding a `< 2`
-- index — that is the intended cost, and it is recorded here so the next author
-- does not discover it by watching a re-probe sweep seq-scan the table.
CREATE INDEX IF NOT EXISTS media_files_needs_probe_idx
    ON media_files (id)
    WHERE media_info_version IS NULL OR media_info_version < 1;

-- The audit predicate: the two states that mean "a human should look at this".
-- `suspicious` is deliberately here and NOT in the queue index above: a suspicious
-- result IS a stored, complete probe (it counts as probed for completion) and is
-- ALSO a file needing attention. Conflating those two questions is what makes a
-- backfill look finished when it is not.
CREATE INDEX IF NOT EXISTS media_files_probe_attention_idx
    ON media_files (id)
    WHERE probe_state IN ('probe_failed', 'suspicious');

COMMENT ON COLUMN media_files.media_info IS
    'Versioned probe document (src/media/doc.rs, MediaInfoDoc). Read it ONLY through '
    'MediaFile::stored_media_info() — a grep-guard test fails the build on ad-hoc key '
    'access elsewhere. Rows written before S130-A carry the legacy {"container": "<ext>"} '
    'shape and no schema_version; a v1 document is a strict superset of that, so the GUI '
    'keeps working throughout the window between deploy and backfill completion.';
COMMENT ON COLUMN media_files.media_info_version IS
    'Indexable mirror of media_info->>schema_version. The document wins on disagreement.';
COMMENT ON COLUMN media_files.probe_state IS
    'ok | suspicious | unreadable | probe_failed. The two failure spellings come from '
    'media::probe::ProbeState::as_str() (MPRB-02) — this list is not a second '
    'classification of the same errors, and a test asserts the two agree.';
