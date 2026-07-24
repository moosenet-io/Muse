-- S125 Phase 3 (migration 1 of 2): additive widen to qwen3-embedding(1024).
--
-- This is the SAFE, non-cutover half. It ADDS a nullable 1024-dim column
-- alongside the existing 768-dim `embeddings.embedding` so the orchestrator's
-- re-embed backfill (`embed::reembed_1024::backfill_1024`) can populate it
-- WITHOUT taking recall offline. The actual swap (drop old, rename new,
-- rebuild HNSW) lives in the SEPARATE migration `0107_embedding_1024_cutover.sql`
-- so the backfill can run in between. Do NOT run 0107 until
-- `embed::reembed_1024::count_pending_backfill` returns 0.
--
-- The derived centroid columns are widened here: they are RECOMPUTED from
-- the new embeddings space after cutover
-- (`embed::reembed_1024::recompute_all_centroids`), never re-embedded, so
-- their old 768 values are discarded — a 768->1024 in-place cast is
-- impossible in pgvector anyway. Two widen strategies are used, deliberately:
--   * PURE DERIVED CACHES (overall_centroid, mainstream_centroid,
--     taste_context_centroids) — cleared/nulled here; nothing irrecoverable
--     lives in them, and recompute regenerates them wholesale.
--   * personas — NON-DESTRUCTIVE widen (S125 review finding): persona ROWS
--     and `persona_members` are PRESERVED. A persona row can hold
--     irrecoverable operator/user intent (an explicit or shared persona's
--     name + `defining_signals`) that `recompute_all_centroids` does NOT
--     reconstruct (it only re-derives account-owned DERIVED personas). We
--     therefore drop NOT NULL and null the 768 centroid in place rather than
--     deleting the rows. See step 2d.

-- ---------------------------------------------------------------------------
-- Step 1: embeddings — additive new-column, backfilled by the orchestrator.
-- Both nullable: the backfill fills them; the cutover migration enforces
-- NOT NULL on `embedding` after promoting it. `model_1024` records the new
-- model name ('qwen3-embedding') per backfilled row so the cutover can fold
-- it into `model`.
-- ---------------------------------------------------------------------------
ALTER TABLE embeddings ADD COLUMN embedding_1024 vector(1024);
ALTER TABLE embeddings ADD COLUMN model_1024 text;

-- ---------------------------------------------------------------------------
-- Step 2: widen the derived centroid columns to vector(1024). pgvector cannot
-- cast a stored vector(768) to vector(1024), so every widen either recreates
-- the column (pure-derived caches, 2a-2c) or retypes it with `USING NULL`
-- (personas, 2d — preserves rows). sqlx maps rows by column NAME (not
-- ordinal), so a recreated column moving to the end of its table is harmless.
-- ---------------------------------------------------------------------------

-- 2a. taste_profile.overall_centroid (nullable) — recomputed by recompute_taste.
ALTER TABLE taste_profile DROP COLUMN overall_centroid;
ALTER TABLE taste_profile ADD COLUMN overall_centroid vector(1024);

-- 2b. population_profile.mainstream_centroid (nullable). NOTE: no code path
-- ever populates this (MUSE-20's mainstream-centroid math was never built);
-- it stays NULL, just at the new width.
ALTER TABLE population_profile DROP COLUMN mainstream_centroid;
ALTER TABLE population_profile ADD COLUMN mainstream_centroid vector(1024);

-- 2c. taste_context_centroids.centroid (NOT NULL) — per-context taste
-- centroids, fully recomputed by recompute_taste. Adding a NOT NULL column
-- requires an empty table, so the derived rows are cleared first (they are
-- reproduced from scratch after cutover — no information is lost).
DELETE FROM taste_context_centroids;
ALTER TABLE taste_context_centroids DROP COLUMN centroid;
ALTER TABLE taste_context_centroids ADD COLUMN centroid vector(1024) NOT NULL;

-- 2d. personas.centroid — NON-DESTRUCTIVE widen (S125 review finding). Unlike
-- the pure-derived caches above, persona ROWS may encode irrecoverable
-- operator/user intent (explicit or shared personas: their name +
-- `defining_signals` + `persona_members` membership), and
-- recompute_all_centroids only re-derives account-owned DERIVED personas. So
-- we PRESERVE every persona row and every persona_members row here:
--   1. drop the HNSW index (can't index a column mid-retype),
--   2. drop the NOT NULL constraint on centroid,
--   3. retype 768 -> vector(1024) with `USING NULL` — the only legal
--      conversion (a 768->1024 cast is impossible), which nulls the stale
--      centroid on every row IN PLACE without touching the rest of the row,
--   4. recreate the HNSW index (NULLs are simply not indexed).
-- recompute_all_centroids refills DERIVED personas' centroids post-cutover.
-- Any explicit/shared persona keeps its row + membership with a NULL centroid
-- until manually re-derived from `defining_signals.source_media_item_ids`
-- (there are none in production today, so nothing is left NULL in practice).
DROP INDEX IF EXISTS personas_hnsw;
ALTER TABLE personas ALTER COLUMN centroid DROP NOT NULL;
ALTER TABLE personas ALTER COLUMN centroid TYPE vector(1024) USING NULL::vector(1024);
CREATE INDEX personas_hnsw ON personas USING hnsw (centroid vector_cosine_ops);
