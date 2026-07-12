-- MUSE-03: embeddings — pgvector recall for library items/people/collections
-- and taste centroids (spec §3.4). Model + dim are stored alongside the
-- vector itself (not just assumed) so a future re-embed (new model, or a
-- dim bump) is detectable rather than silently mixing incompatible spaces.
--
-- Dim is pinned at 768 per S96 §0.7 (nomic-embed-text default). This is a
-- build-time column-width decision -- switching embedding models to a
-- different dimensionality requires a new migration (a new table or an
-- ALTER + full re-embed), not a runtime toggle.
CREATE TABLE embeddings (
    id            bigserial PRIMARY KEY,
    entity_kind   text NOT NULL,           -- 'media_item','person','collection','taste_centroid'
    entity_id     bigint NOT NULL,
    model         text NOT NULL,           -- 'nomic-embed-text'
    dim           int NOT NULL DEFAULT 768,
    embedding     vector(768) NOT NULL,    -- nomic-embed-text, 768-dim (S96 §0.7)
    source_text   text,                    -- what was embedded (title+overview+genres+people)
    embedded_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (entity_kind, entity_id, model)
);

-- HNSW over cosine distance (spec §3.4 default). Chosen over ivfflat: HNSW
-- doesn't need a pre-built-list-count tuned to table size and gives better
-- recall/query-speed tradeoffs at this scale; ivfflat would also require an
-- ANALYZE-worthy row count before the index is useful, which doesn't hold at
-- the small early-Phase-0 library sizes this ships against.
CREATE INDEX embeddings_hnsw ON embeddings USING hnsw (embedding vector_cosine_ops);
CREATE INDEX ON embeddings (entity_kind, entity_id);
