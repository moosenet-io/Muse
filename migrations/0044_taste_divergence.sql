-- MUSE-20: taste_divergence — the "you vs the masses" radar payload (spec
-- §3.7/§4c of specs/S96-muse-foundation.md). MUSE-19 (0041-0043) shipped
-- trending_snapshots/streaming_availability/population_profile but
-- deliberately did NOT create this table (see 0043's header comment) —
-- this migration is that follow-up.
--
-- One row per computation, never upserted — like population_profile, the
-- radar is tracked over time so a consumer can see it move
-- (drifting-mainstream vs going-more-niche) rather than only reading the
-- latest snapshot. See src/repo/taste_divergence.rs (read/write) and
-- src/radar/divergence.rs (the math that produces the JSON payloads).
--
-- Divergence from the spec's reference DDL: the spec's `mainstream_score`
-- doc comment says "0..1 cosine(your centroid, mainstream centroid)" — this
-- crate has no embeddings pipeline wired up yet (embeddings/embedder worker
-- is MUSE-08 scope; see trending/mod.rs's identical divergence note for
-- population_profile.mainstream_centroid), so mainstream_score here is
-- computed from genre/decade distribution overlap instead (see
-- src/radar/divergence.rs::mainstream_score for the documented formula).
-- Same value range (0..1), same intent, no embeddings dependency.
CREATE TABLE taste_divergence (
    id                bigserial PRIMARY KEY,
    account_id        bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    computed_at       timestamptz NOT NULL DEFAULT now(),
    -- radar dimensions (you vs population, shared axes)
    genre_index       jsonb NOT NULL,                    -- {genre: your_share/pop_share}  (>1 over-index, <1 under)
    decade_index      jsonb,
    mainstream_score  real,                               -- 0..1, distribution-overlap based (see divergence.rs)
    adventurousness   real,                               -- 0..1, complement of mainstream_score
    contrarian_index  real,                               -- 0..1, Pearson-correlation based (see divergence.rs)
    -- interesting derived data points
    were_early        jsonb,                              -- [{media_metadata_id, title, watched_at, trended_at, lead_days}]
    blind_spots       jsonb,                              -- [{media_metadata_id, title, best_rank, popularity}]
    guilty_pleasures  jsonb                                -- [{media_metadata_id, title, rewatch_count}]
);
CREATE INDEX ON taste_divergence (account_id, computed_at DESC);
