-- MUSE-03: external_enrichment (spec §3.5) — Terminus-tool-suite enrichment
-- cache (forum sentiment, renewal news, trailers, deals, critic scores).
-- CASCADE on media_item_id (unlike the telemetry/taste tables above): this
-- is purely a derived cache row keyed to a library item, with no standalone
-- historical value once the item is gone.
CREATE TABLE external_enrichment (
    id            bigserial PRIMARY KEY,
    media_item_id bigint NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    kind          text NOT NULL,     -- 'forum_sentiment','does_it_get_good','renewal_news','trailer','deal','critic_score'
    source        text NOT NULL,     -- 'reddit','letterboxd','searxng','news','metacritic'
    payload       jsonb NOT NULL,    -- normalized: {score, summary, url, gets_good_at_episode, ...}
    confidence    real,
    fetched_at    timestamptz NOT NULL DEFAULT now(),
    ttl_seconds   int NOT NULL DEFAULT 604800,
    UNIQUE (media_item_id, kind, source)
);
CREATE INDEX ON external_enrichment (fetched_at);
