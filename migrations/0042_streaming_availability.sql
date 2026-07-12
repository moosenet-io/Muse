-- MUSE-19: where a title streams (TMDb `/watch/providers`) — the "on
-- streaming" grounding for §3.7. Region-aware; day-one only stores offers
-- for the ingest's configured region (see `src/trending/mod.rs`).
--
-- Same media_metadata_id divergence as 0041 (see that file's comment): the
-- spec's `media_item_id` FK is renamed/re-pointed at shared `media_metadata`
-- to match the real MUSE-02 metadata/instance split. Per the spec's own
-- composite primary key, a row can only exist for a title already resolved
-- to `media_metadata` — unresolved trending entries simply have no
-- streaming_availability rows yet.
CREATE TABLE streaming_availability (
    media_metadata_id bigint NOT NULL REFERENCES media_metadata(id) ON DELETE CASCADE,
    provider          text NOT NULL,                 -- 'netflix','prime_video',…
    region            text NOT NULL DEFAULT 'US',
    offer_type        text NOT NULL,                 -- 'flatrate' | 'ads' | 'rent' | 'buy'
    link              text,
    seen_at           timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (media_metadata_id, provider, region, offer_type)
);
