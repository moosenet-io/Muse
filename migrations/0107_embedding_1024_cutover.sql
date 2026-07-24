-- S125 Phase 3 (migration 2 of 2): the CUTOVER. Promotes the backfilled
-- `embedding_1024` column to be THE `embedding` column and rebuilds the HNSW
-- index at 1024 dims.
--
-- PRECONDITION (orchestrator-enforced, NOT expressible in SQL): the re-embed
-- backfill (`embed::reembed_1024::backfill_1024`) has run and
-- `embed::reembed_1024::count_pending_backfill` returns 0 — i.e. every row
-- that CAN be re-embedded (source_text present) now has `embedding_1024`.
-- Running this before then would DELETE still-unbackfilled rows (see step 1).
--
-- ORDER in the overall sequence:
--   0106 (add col + widen centroids) -> backfill_1024 -> [THIS] -> recompute_all_centroids

-- ---------------------------------------------------------------------------
-- Step 1: drop rows we could not reproduce. Any row still missing
-- `embedding_1024` here has no `source_text` to re-embed (or repeatedly
-- failed the backfill) — its 768 vector cannot be carried forward into the
-- 1024 space, so it is removed. Such rows re-materialize naturally on the
-- next incremental embed pass once their `source_text` is recomposed.
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
