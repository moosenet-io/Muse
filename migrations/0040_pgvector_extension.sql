-- MUSE-19: population_profile.mainstream_centroid (below, 0043) needs
-- pgvector. Guarded with IF NOT EXISTS since MUSE-03/08 (library
-- embeddings) may create this extension independently depending on merge
-- order across branches — this migration must be a no-op if that already
-- ran, and a from-scratch enabler if it hasn't.
CREATE EXTENSION IF NOT EXISTS vector;
