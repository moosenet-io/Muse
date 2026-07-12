-- MUSE-02: media_items — thin per-library instance state (Radarr Movies /
-- Sonarr Series parity), referencing shared media_metadata.
--
-- ARR-BLUEPRINT §2/§7.1/§7.9: this is the table that makes multi-library
-- cheap — the same media_metadata row can have N media_items rows (one per
-- library it's monitored/present in), each with its own path/monitored/
-- quality_profile/tags. library_id is first-class scoping per the blueprint's
-- 8-instance finding, not an afterthought.
CREATE TABLE media_items (
    id                   bigserial PRIMARY KEY,
    library_id           bigint NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    media_metadata_id    bigint NOT NULL REFERENCES media_metadata(id) ON DELETE CASCADE,
    path                 text NOT NULL,                 -- filesystem path within this library
    monitored            boolean NOT NULL DEFAULT false,
    in_library            boolean NOT NULL DEFAULT true, -- present in Plex/arr (vs metadata-only/wanted row)
    quality_profile_id    bigint REFERENCES quality_profiles(id),
    minimum_availability   text,                          -- movies: 'announced'|'in_cinemas'|'released'|'pre_db'
    plex_rating_key         text,
    added_at                 timestamptz,
    last_search_time          timestamptz,
    created_at                 timestamptz NOT NULL DEFAULT now(),
    updated_at                  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (library_id, media_metadata_id),
    UNIQUE (plex_rating_key)
);
CREATE INDEX ON media_items (media_metadata_id);
CREATE INDEX ON media_items (library_id);

-- Instance-level tags (arr Movies.Tags / Series.Tags parity — free-form
-- operator tags on the local copy, distinct from shared metadata).
CREATE TABLE tags (
    id     bigserial PRIMARY KEY,
    name   text NOT NULL UNIQUE,
    source text                        -- 'plex_label','muse_derived','arr_import'
);
CREATE TABLE media_item_tags (
    media_item_id bigint NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    tag_id        bigint NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (media_item_id, tag_id)
);
