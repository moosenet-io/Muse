-- MUSE-16: releases — rolling grabbability snapshot fed by the Prowlarr
-- report-pull worker (§4b: RSS-first, ID-based targeted search sparingly).
-- One row per (indexer, release guid). `media_metadata_id`/`episode_id`
-- resolve a release to a known title/episode when the deterministic
-- release-name parser + title match succeeds — most releases will NOT
-- resolve on first sight and are still kept (negative-space discovery, per
-- spec). This is a *report*, never a grab: no download/execute path lives
-- here.
--
-- NOTE: the founding spec's reference DDL (§3.6) points this at
-- `media_items`/`media_children`; this build follows the MUSE-02 metadata/
-- instance split actually shipped (`media_metadata` is the shared,
-- library-independent title; `episodes` is the per-show episode leaf) since
-- grabbability is a title-level fact, not a per-library-instance one — same
-- divergence rationale documented in 0006_media_items.sql/0007_seasons.sql.
CREATE TABLE releases (
    id                bigserial PRIMARY KEY,
    media_metadata_id bigint REFERENCES media_metadata(id) ON DELETE SET NULL,
    episode_id        bigint REFERENCES episodes(id) ON DELETE SET NULL,
    indexer_id        bigint NOT NULL REFERENCES indexers(id) ON DELETE CASCADE,
    guid              text NOT NULL,                     -- indexer-unique release id
    title             text NOT NULL,                      -- raw release name (parsed below)
    info_url          text,
    download_url      text,
    info_hash         text,
    size_bytes        bigint,
    publish_date      timestamptz,                        -- release age (freshness signal)
    seeders           int,
    leechers          int,
    grabs             int,
    freeleech         boolean NOT NULL DEFAULT false,
    freeleech_pct     real,
    categories        int[] NOT NULL DEFAULT '{}',
    -- deterministic parse (the arr release-parsing brain, v0 — AI-augmented in Phase 1)
    parsed_title      text,
    parsed_year       int,
    quality           text,                               -- 'Bluray-2160p','WEB-DL-1080p'
    resolution        text,                                -- '2160p'
    source            text,                                -- 'BluRay','WEB','HDTV'
    video_codec       text,
    audio_codec       text,
    audio_channels    real,
    hdr               text[] NOT NULL DEFAULT '{}',        -- ['HDR10','DV']
    edition           text,
    release_group     text,
    proper_repack     boolean NOT NULL DEFAULT false,
    languages         text[] NOT NULL DEFAULT '{}',
    subtitles         text[] NOT NULL DEFAULT '{}',
    parse_confidence  real,
    first_seen_at     timestamptz NOT NULL DEFAULT now(),
    last_seen_at      timestamptz NOT NULL DEFAULT now(),
    expires_at        timestamptz,
    UNIQUE (indexer_id, guid)
);
CREATE INDEX ON releases (media_metadata_id);
CREATE INDEX ON releases (episode_id);
CREATE INDEX ON releases (publish_date DESC);
