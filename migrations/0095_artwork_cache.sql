-- MUSE-27: artwork_cache — the local proxy/cache backing `/art/{kind}/{id}`
-- (spec §3.8/§4d-F). The whole point of this table: the browser never sees
-- the Plex token. Muse fetches artwork server-side (using the configured
-- `PLEX_TOKEN`) and caches the bytes here; the web guide only ever emits
-- same-origin `/art/{kind}/{id}` URLs.
--
-- Divergence from the spec's conceptual §3.8 DDL: the spec sketches
-- `local_path` (a disk-cache file reference). This migration instead caches
-- the image `bytes` directly in Postgres (`bytea`) — simpler operationally
-- for a stub (no cache-directory lifecycle to manage) and just as effective
-- for the poster/thumb-sized images this table holds. `source_url` is kept
-- (the upstream Plex path to (re)fetch from), plus `content_type`/`etag`/
-- `fetched_at` so the proxy handler can do a real fetch-once-cache-after
-- flow and set sane HTTP caching headers.
--
-- Migration block MUSE-27 owns: 0095-0096.
CREATE TABLE artwork_cache (
    id            bigserial PRIMARY KEY,
    entity_kind   text NOT NULL,                  -- 'media_item' | 'episode' | 'interstitial' | 'person'
    entity_id     bigint NOT NULL,
    variant       text NOT NULL DEFAULT 'poster', -- 'poster' | 'thumb' | 'art' | 'banner'
    source_url    text,                           -- upstream (Plex) path/URL to fetch from on a cache miss
    content_type  text,                            -- e.g. 'image/jpeg'; set once the bytes are cached
    bytes         bytea,                           -- cached image bytes (NULL until first successful fetch)
    etag          text,
    fetched_at    timestamptz,                     -- when `bytes` was last (re)fetched
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (entity_kind, entity_id, variant)
);
CREATE INDEX ON artwork_cache (entity_kind, entity_id);
