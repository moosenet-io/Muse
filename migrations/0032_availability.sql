-- MUSE-16: availability — per-title "grabbable now" rollup (§4b), recomputed
-- by a worker from `releases`. Keyed on `media_metadata` (the shared,
-- library-independent title) rather than the per-library `media_items`
-- instance, since grabbability is a title-level fact independent of which
-- library a grab would eventually land in — same rationale as `releases`
-- (see 0031_releases.sql).
CREATE TABLE availability (
    media_metadata_id  bigint PRIMARY KEY REFERENCES media_metadata(id) ON DELETE CASCADE,
    best_quality        text,                              -- highest parsed quality currently available
    best_seeders        int,
    release_count       int NOT NULL DEFAULT 0,
    has_freeleech       boolean NOT NULL DEFAULT false,
    cheapest_size_bytes bigint,                             -- smallest acceptable-quality option
    newest_release_at   timestamptz,
    computed_at         timestamptz NOT NULL DEFAULT now()
);
