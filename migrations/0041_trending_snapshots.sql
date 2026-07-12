-- MUSE-19: "what's trending on streaming" feed — spec §3.7/§4c of
-- specs/S96-muse-foundation.md. Rolling population-level trending/popular
-- snapshots ("the masses"), day-one sourced from TMDb `/trending` and
-- `/*/popular`; Trakt/FlixPatrol/JustWatch are documented optional seams
-- (see `src/trending/mod.rs::OptionalSource`), not built here.
--
-- Divergence from the spec's reference DDL: the spec points its
-- `media_item_id` FK at a flat `media_items(tmdb_id)` row. The real MUSE-02
-- schema (0005/0006/0011) split that into shared `media_metadata` (carries
-- tmdb_id/tvdb_id/imdb_id) + per-library `media_items` (carries only
-- library-local instance state e.g. path/monitored). tmdb_id resolution
-- therefore targets `media_metadata`, not `media_items` — same divergence
-- already documented in 0011_people_genres_collections.sql for
-- credits/genres/collections. Column is named `media_metadata_id`
-- accordingly (most trending entries won't resolve to anything in a home
-- library and will stay NULL, carrying `external_ref` instead).
CREATE TABLE trending_snapshots (
    id                bigserial PRIMARY KEY,
    source            text NOT NULL,                 -- 'tmdb' (day-one); 'trakt'|'flixpatrol'|'justwatch'|'netflix_top10' reserved for optional sources
    scope             text NOT NULL,                 -- 'trending' | 'popular' | 'most_watched' | 'most_played' | 'top10'
    platform          text,                          -- 'netflix','prime','disney','hbo',… or NULL = aggregate
    region            text NOT NULL DEFAULT 'US',
    "window"          text NOT NULL,                 -- 'day' | 'week'
    rank              int,
    media_metadata_id bigint REFERENCES media_metadata(id) ON DELETE SET NULL,
    external_ref      jsonb,                         -- {tmdb_id,imdb_id,title,year} when not resolved to our catalog
    popularity        real,                          -- source popularity/score
    captured_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (source, scope, platform, region, "window", rank, captured_at)
);
CREATE INDEX ON trending_snapshots (captured_at DESC);
CREATE INDEX ON trending_snapshots (media_metadata_id);
