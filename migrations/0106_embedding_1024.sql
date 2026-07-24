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
-- The 4 derived centroid columns are widened here directly (via drop+add):
-- they are RECOMPUTED from the new embeddings space after cutover
-- (`embed::reembed_1024::recompute_all_centroids`), never re-embedded, so
-- their old 768 values are simply discarded — a 768->1024 in-place cast is
-- impossible in pgvector anyway.

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
-- Step 2: widen the derived centroid columns to vector(1024).
--
-- drop+add is used deliberately (not ALTER TYPE): pgvector cannot cast an
-- existing vector(768) to vector(1024), and these columns are fully
-- recomputed after cutover regardless. sqlx maps rows by column NAME (not
-- ordinal), so the columns moving to the end of their tables is harmless.
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

-- 2d. personas.centroid (NOT NULL, HNSW-indexed). Same NOT-NULL-needs-empty
-- constraint as 2c. Persona ROWS are cleared here (persona_members cascades)
-- and re-derived post-cutover by recompute_all_centroids. This is safe
-- because NO production code path creates personas today (only tests do), so
-- there is nothing to preserve; if that changes before this ships, convert
-- this to a placeholder-fill widen instead. Dropping the column also drops
-- `personas_hnsw`; it is recreated at the new width below.
DELETE FROM personas;
ALTER TABLE personas DROP COLUMN centroid;
ALTER TABLE personas ADD COLUMN centroid vector(1024) NOT NULL;
CREATE INDEX personas_hnsw ON personas USING hnsw (centroid vector_cosine_ops);
