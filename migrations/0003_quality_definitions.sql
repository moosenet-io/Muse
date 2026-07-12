-- MUSE-02: quality definitions — the fixed grid of source x resolution
-- quality tiers (ARR-BLUEPRINT §2 QualityDefinitions / §6 parsing model).
--
-- Divergence from spec §3.2 `quality_profiles.cutoff text`: the blueprint
-- shows *arr quality ids are historical/non-contiguous ints with no
-- semantic ordering, so Muse uses a semantic `quality_key` + explicit
-- `sort_order` instead of reusing *arr's raw integer ids.
CREATE TABLE quality_definitions (
    id                    bigserial PRIMARY KEY,
    quality_key           text NOT NULL UNIQUE,     -- e.g. 'bluray-2160p-remux', 'webdl-1080p'
    title                 text NOT NULL,             -- display title, e.g. 'Bluray-1080p Remux'
    source                text NOT NULL,             -- 'cam','telesync','telecine','workprint','dvd','dvdscr','sdtv','hdtv','webdl','webrip','bluray','remux'
    resolution            text,                      -- '480p','576p','720p','1080p','2160p'; NULL for source with no resolution concept
    modifier              text NOT NULL DEFAULT 'none', -- 'none','regional','screener','brdisk' (orthogonal sub-variant, blueprint §6)
    min_size_mb_per_min   real,
    max_size_mb_per_min   real,
    preferred_size_mb_per_min real,
    sort_order            int NOT NULL,              -- explicit preference ordering (ids are NOT semantically ordered)
    created_at            timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON quality_definitions (sort_order);
