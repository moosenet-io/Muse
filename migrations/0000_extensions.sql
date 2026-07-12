-- MUSE-02: extensions needed by the arr-shaped core schema.
--
-- pgvector (`CREATE EXTENSION vector`) is deliberately NOT enabled here — the
-- embeddings/taste tables are MUSE-03/08 scope. pg_trgm powers fuzzy title
-- lookups (trigram GIN index) as a fallback alongside future vector search.
CREATE EXTENSION IF NOT EXISTS pg_trgm;
