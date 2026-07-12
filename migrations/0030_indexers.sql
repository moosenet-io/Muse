-- MUSE-16: indexers — a read-only mirror of Prowlarr's own indexer registry
-- (§4b of the founding spec). Prowlarr owns indexer credentials/rate-limits;
-- Muse never talks to trackers directly, only through Prowlarr's API, and
-- this table exists so the report-pull worker knows which indexers exist,
-- what categories they support, and how politely to poll each one.
CREATE TABLE indexers (
    id                       bigserial PRIMARY KEY,
    prowlarr_id              int NOT NULL UNIQUE,
    name                     text NOT NULL,
    protocol                 text,                     -- 'torrent' | 'usenet'
    privacy                  text,                     -- 'public' | 'private' | 'semiPrivate'
    enabled                  boolean NOT NULL DEFAULT true,
    categories               int[] NOT NULL DEFAULT '{}',  -- Newznab cats supported (2000s movies / 5000s tv)
    last_rss_pull_at         timestamptz,
    polite_min_interval_secs int NOT NULL DEFAULT 900,     -- etiquette: don't poll faster than this
    created_at               timestamptz NOT NULL DEFAULT now(),
    updated_at               timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON indexers (enabled);
