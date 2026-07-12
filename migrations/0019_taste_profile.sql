-- MUSE-03: taste_profile + taste_context_centroids (spec §3.4) — the
-- per-account taste model itself, and its context-specific variants
-- (Friday-night != Sunday-morning != phone-commute). Grouped in one
-- migration since both are 1:N-off-accounts taste-model state.
CREATE TABLE taste_profile (
    account_id          bigint PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    genre_affinity      jsonb NOT NULL DEFAULT '{}', -- {genre: weight} recency-weighted, +finish/-abandon
    person_affinity     jsonb NOT NULL DEFAULT '{}', -- {person_id: weight}
    keyword_affinity    jsonb NOT NULL DEFAULT '{}', -- 'slow-burn','one-shot','practical-fx'
    runtime_pref        jsonb,                       -- distribution of finished runtimes (phone vs TV)
    quality_sensitivity jsonb,                       -- from transcode/abandon-on-low-quality signals
    overall_centroid    vector(768),                 -- centroid of loved items (nomic-embed-text, 768-dim)
    computed_at         timestamptz NOT NULL DEFAULT now(),
    model_notes         text                         -- LLM-written summary ("you love cerebral, slow-burn sci-fi...")
);

CREATE TABLE taste_context_centroids (
    account_id  bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    context_key text NOT NULL,     -- 'weekend_evening','weekday_late','phone_short'
    centroid    vector(768) NOT NULL,
    sample_size int NOT NULL,
    PRIMARY KEY (account_id, context_key)
);
