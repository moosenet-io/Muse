-- S125 Phase 3 (migration 2 of 2): the CUTOVER. Promotes the backfilled
-- `embedding_1024` column to be THE `embedding` column and rebuilds the HNSW
-- index at 1024 dims.
--
-- =====================  DATA-SAFETY / AUTO-RUN CONTRACT  =====================
-- Muse auto-runs ALL pending migrations at startup (`sqlx::migrate!`), so on
-- an existing vector store 0106 and THIS file would run back-to-back with no
-- backfill in between. That would be catastrophic (drop the populated 768
-- column before 1024 is filled). This migration is therefore made FAIL-SAFE
-- by the guard in Step 0 below: it RAISES and aborts (rolling back this whole
-- migration — no column is touched) whenever the backfill is incomplete.
--
-- Behavior of the guard:
--   * Fresh/empty deploy (no 768 rows): guard finds nothing pending -> passes
--     -> clean cutover to a 1024 schema. Seamless.
--   * Existing store, backfill NOT yet run: guard RAISES -> 0107 rolls back
--     and is recorded as NOT applied -> startup logs the error and continues
--     on the intermediate (0106-applied) schema with the OLD embedding column
--     fully intact. NO DATA LOSS.
--
-- REQUIRED MANUAL SEQUENCE for an existing production store:
--   1. apply 0106                              (adds embedding_1024, widens centroids)
--   2. embed::reembed_1024::backfill_1024      (until count_pending_backfill() == 0)
--   3. apply 0107  (THIS)                      (guard passes, cutover proceeds)
--   4. embed::reembed_1024::recompute_all_centroids
-- ===========================================================================

-- ---------------------------------------------------------------------------
-- Step 0: FAIL-SAFE GUARD. Abort (rolling back this migration) if any row that
-- CAN be backfilled (source_text present) still lacks embedding_1024. This is
-- what makes back-to-back auto-run safe.
-- ---------------------------------------------------------------------------
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM embeddings
        WHERE embedding_1024 IS NULL AND source_text IS NOT NULL
    ) THEN
        RAISE EXCEPTION
            'S125 cutover blocked: embedding_1024 backfill incomplete — run reembed_1024::backfill_1024 first (see this migration''s header for the required manual sequence)';
    END IF;
END $$;

-- ---------------------------------------------------------------------------
-- Step 1: drop rows we could not reproduce. After the guard, the ONLY rows
-- still missing `embedding_1024` are those with no `source_text` to re-embed
-- — their 768 vector cannot be carried forward into the 1024 space, so they
-- are removed. Such rows re-materialize naturally on the next incremental
-- embed pass once their `source_text` is recomposed.
-- ---------------------------------------------------------------------------
DELETE FROM embeddings WHERE embedding_1024 IS NULL;

-- ---------------------------------------------------------------------------
-- Step 2: fold the new model name + dim into the canonical columns. Every
-- surviving row was backfilled with model_1024 = 'qwen3-embedding'.
-- ---------------------------------------------------------------------------
UPDATE embeddings
   SET model = COALESCE(model_1024, model),
       dim   = 1024;

-- ---------------------------------------------------------------------------
-- Step 3: the swap. Drop the old 768 index + column, rename the 1024 column
-- into its place, re-impose NOT NULL, drop the scratch model column, and set
-- the new default dim.
-- ---------------------------------------------------------------------------
DROP INDEX IF EXISTS embeddings_hnsw;
ALTER TABLE embeddings DROP COLUMN embedding;
ALTER TABLE embeddings RENAME COLUMN embedding_1024 TO embedding;
ALTER TABLE embeddings ALTER COLUMN embedding SET NOT NULL;
ALTER TABLE embeddings DROP COLUMN model_1024;
ALTER TABLE embeddings ALTER COLUMN dim SET DEFAULT 1024;

-- ---------------------------------------------------------------------------
-- Step 4: rebuild the HNSW cosine index on the (now 1024-dim) embedding
-- column — same index definition as `0018_embeddings.sql`, new width.
-- ---------------------------------------------------------------------------
CREATE INDEX embeddings_hnsw ON embeddings USING hnsw (embedding vector_cosine_ops);
