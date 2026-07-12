-- MUSE-02: media_files — physical files, with COMPOUND quality and
-- many-to-many TV linkage.
--
-- ARR-BLUEPRINT §2/§3/§7.3/§7.4 (two structural divergences from spec §3.2):
--
-- 1. Quality is compound, not a flat string: {quality_tier_id (FK into
--    quality_definitions), revision:{version, real, is_repack}}. This is
--    needed for upgrade-eligibility comparison identical to *arr's
--    (PROPER/REPACK/REAL tags bump version/isRepack/real, used only to
--    decide "is this a better cut of the same source").
-- 2. Files are 1:1 for movies but MANY-TO-MANY for TV (a season-pack file
--    satisfies multiple episodes at once) — media_files itself stays
--    generic/unscoped-to-episode; the episode_files join table (next
--    migration) carries the many-to-many for TV, while movie files just
--    set media_item_id directly (1:1, matching Radarr's MovieFileId).
--
-- Like *arr, provider identity is never denormalized onto files — it always
-- resolves through media_item_id -> media_items -> media_metadata_id ->
-- media_metadata.tmdb_id/tvdb_id/imdb_id.
CREATE TABLE media_files (
    id                    bigserial PRIMARY KEY,
    media_item_id         bigint NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    relative_path          text NOT NULL,
    original_file_path      text,
    size_bytes                bigint,
    date_added                  timestamptz,
    scene_name                    text,               -- original scene-style release filename (parsing provenance)
    media_info                      jsonb,             -- codec/resolution/audio channels/HDR from file probing
    release_group                     text,             -- free text, no lookup table (blueprint §2: *arr re-parses, doesn't normalize)
    edition                             text,             -- 'Director''s Cut', free text
    languages                             text[] NOT NULL DEFAULT '{}',
    subtitles                               text[] NOT NULL DEFAULT '{}',
    indexer_flags                             int NOT NULL DEFAULT 0,   -- bitmask: freeleech/halfleech/scene/etc.
    release_type                                release_type_kind NOT NULL DEFAULT 'single',
    -- compound quality (blueprint §2/§7.4)
    quality_tier_id                               bigint REFERENCES quality_definitions(id),
    revision_version                                int NOT NULL DEFAULT 1,
    revision_real                                     int NOT NULL DEFAULT 0,
    revision_is_repack                                  boolean NOT NULL DEFAULT false,
    created_at                                            timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON media_files (media_item_id);
