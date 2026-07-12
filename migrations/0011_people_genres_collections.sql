-- MUSE-02: people / genres / collections — taste dimensions hung off
-- SHARED media_metadata (spec §3.2 hangs these off media_items; moved here
-- per the metadata/instance split in 0005/0006 — cast/genre/collection are
-- provider-level facts, not per-library instance state, matching Radarr's
-- Credits/Collections keying off MovieMetadataId rather than MovieId).
-- (`tags` stayed on media_items — see 0006_media_items.sql — since arr Tags
-- are operator-set per local instance, not provider metadata.)
CREATE TABLE people (
    id                    bigserial PRIMARY KEY,
    tmdb_person_id        text UNIQUE,
    name                  text NOT NULL,
    known_for_department  text
);

CREATE TABLE media_metadata_credits (
    media_metadata_id  bigint NOT NULL REFERENCES media_metadata(id) ON DELETE CASCADE,
    person_id           bigint NOT NULL REFERENCES people(id) ON DELETE CASCADE,
    role                 text NOT NULL,             -- 'director','actor','writer',...
    character             text,
    cast_order              int,
    PRIMARY KEY (media_metadata_id, person_id, role)
);

CREATE TABLE genres (
    id    bigserial PRIMARY KEY,
    name  text NOT NULL UNIQUE
);
CREATE TABLE media_metadata_genres (
    media_metadata_id  bigint NOT NULL REFERENCES media_metadata(id) ON DELETE CASCADE,
    genre_id            bigint NOT NULL REFERENCES genres(id) ON DELETE CASCADE,
    PRIMARY KEY (media_metadata_id, genre_id)
);

CREATE TABLE collections (
    id               bigserial PRIMARY KEY,
    name             text NOT NULL,
    source           text,                          -- 'plex','tmdb','muse_curated'
    tmdb_collection_id text,
    plex_rating_key   text,
    description        text
);
CREATE TABLE media_metadata_collections (
    collection_id       bigint NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    media_metadata_id    bigint NOT NULL REFERENCES media_metadata(id) ON DELETE CASCADE,
    PRIMARY KEY (collection_id, media_metadata_id)
);
