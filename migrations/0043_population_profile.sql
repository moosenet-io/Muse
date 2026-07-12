-- MUSE-19/20: aggregate "mainstream" rollup of the trending set (spec
-- §3.7/§4c) — the counterpart to per-account `taste_profile`.
--
-- MUSE-19 (this item) ships the ingest/storage half only: each row records
-- `sample_size` for a window/region at the time it was computed.
-- `genre_distribution`/`decade_distribution`/`runtime_distribution` are
-- written as empty/NULL placeholders, and `mainstream_centroid` (needs
-- resolved-item embeddings, MUSE-03/08 scope) is left NULL. The real
-- distribution math + `mainstream_centroid` + the `taste_divergence` "you
-- vs the masses" radar computation that consumes this table are MUSE-20
-- scope — see `src/trending/mod.rs::compute_population_profile` for the
-- documented seam. Columns exist here per the spec so MUSE-20 doesn't need
-- its own schema migration for them.
--
-- No unique constraint: like `taste_divergence`, this is tracked over time
-- (one row per computation) so a later consumer can see the corpus/radar
-- move, not just its latest value.
CREATE TABLE population_profile (
    id                   bigserial PRIMARY KEY,
    "window"             text NOT NULL,              -- 'week' (day-one cadence)
    region               text NOT NULL DEFAULT 'US',
    genre_distribution   jsonb NOT NULL DEFAULT '{}', -- {genre: share} — MUSE-20 fills this in
    decade_distribution  jsonb,                       -- {decade: share} — MUSE-20
    runtime_distribution jsonb,                       -- MUSE-20
    mainstream_centroid  vector(768),                 -- centroid of the trending set's embeddings — MUSE-20
    sample_size          int,
    computed_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON population_profile ("window", region, computed_at DESC);
