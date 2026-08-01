-- SUBS-01 review gate (codex): applying an offset twice compounded.
--
-- `apply_confirmed_offset` repointed `storage_path` at the ADJUSTED copy, and
-- the apply route read its source through `readable_path()`, which prefers
-- `storage_path`. So a +1000ms adjustment followed by a +2000ms one read the
-- already-shifted text and shifted it again — the file held +3000ms while the
-- row recorded 2000. `offset_ms` is documented as the offset currently APPLIED,
-- an absolute value, so the only consistent reading is that every adjustment
-- must derive from the same pristine text.
--
-- This column is that pristine text: the subtitle exactly as Muse first
-- obtained it, never rewritten by an adjustment. NULL for an embedded track
-- (there is no separate file) and for a sidecar (the sidecar itself is the
-- immutable original, and it lives in the read-only library).
--
-- Idempotent, like every migration here: Muse applies these at startup.
ALTER TABLE subtitle_selections
    ADD COLUMN IF NOT EXISTS original_storage_path text;

-- Backfill: any row whose offset has never been applied still has a pristine
-- storage_path, so it is its own original. A row with an applied offset that
-- predates this column cannot have its original recovered here — it is left
-- NULL, and the code falls back rather than guessing.
UPDATE subtitle_selections
   SET original_storage_path = storage_path
 WHERE original_storage_path IS NULL
   AND storage_path IS NOT NULL
   AND offset_ms = 0;

COMMENT ON COLUMN subtitle_selections.original_storage_path IS
    'The subtitle as first obtained, never rewritten by an adjustment. Every adjustment is '
    'derived from THIS, so offset_ms stays absolute and repeated applies do not compound.';
