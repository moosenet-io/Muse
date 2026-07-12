-- MUSE-02: media_metadata — shared, provider-keyed descriptive metadata.
--
-- ARR-BLUEPRINT §2/§7.1/§7.7 (the central divergence from spec §3.2): Radarr
-- splits "what TMDb says about this movie" (MovieMetadata) from "my local
-- copy of it" (Movies), joined by MovieMetadataId; Sonarr in this deployed
-- version does NOT (Series is one denormalized table) but the blueprint
-- explicitly recommends Muse follow Radarr's more modern normalized
-- pattern for BOTH movies and shows. One media_metadata row per real-world
-- title, shared across every library that carries it (radarr vs
-- radarr_uhd vs radarr_anime all pointing at the same metadata row).
--
-- provider_ids jsonb (blueprint §7.7): anime titles alone can carry 5+
-- provider ids (TVDB/TMDb/IMDb/TVMaze/MAL/AniList) simultaneously, and
-- provider-id precedence differs by media type (Radarr: TMDb primary;
-- Sonarr: TVDB primary). tmdb_id/tvdb_id/imdb_id stay as first-class
-- columns (they're the two join points instance rows actually key off and
-- are worth indexing), with the long tail in provider_ids.
CREATE TABLE media_metadata (
    id                    bigserial PRIMARY KEY,
    kind                  media_kind NOT NULL,           -- 'movie' | 'show'
    tmdb_id               text,                          -- Radarr-primary provider key
    tvdb_id               text,                          -- Sonarr-primary provider key
    imdb_id               text,
    provider_ids          jsonb NOT NULL DEFAULT '{}',   -- {"tvrage": "...", "tvmaze": "...", "mal": "...", "anilist": "..."}
    title                 text NOT NULL,
    sort_title            text,
    clean_title            text,
    original_title        text,
    clean_original_title  text,
    original_language     text,
    status                text,                          -- 'tba','announced','in_cinemas','released','continuing','ended','upcoming','deleted'
    overview              text,
    tagline               text,
    studio                text,
    network               text,                          -- TV only
    website                text,
    youtube_trailer_id     text,
    certification          text,
    runtime_minutes         int,
    year                    int,
    secondary_year          int,
    in_cinemas              timestamptz,
    physical_release        timestamptz,
    digital_release         timestamptz,
    first_aired              timestamptz,
    last_aired                timestamptz,
    next_airing                timestamptz,
    images                  jsonb NOT NULL DEFAULT '[]', -- [{coverType,url,remoteUrl}]
    keywords                 jsonb NOT NULL DEFAULT '[]', -- TMDb keywords
    ratings                  jsonb NOT NULL DEFAULT '{}', -- {imdb:{votes,value}, tmdb:{...}, trakt:{...}, rotten_tomatoes:{...}}
    recommendations          jsonb NOT NULL DEFAULT '[]', -- provider-recommended related ids
    popularity                real,
    collection_tmdb_id         text,                      -- movie-collection (franchise) grouping, denormalized like *arr
    collection_title           text,
    last_info_sync              timestamptz,
    created_at                   timestamptz NOT NULL DEFAULT now(),
    updated_at                    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (kind, tmdb_id),
    UNIQUE (kind, tvdb_id)
);
CREATE INDEX ON media_metadata USING gin (title gin_trgm_ops);
CREATE INDEX ON media_metadata (imdb_id);
CREATE INDEX ON media_metadata USING gin (provider_ids);
