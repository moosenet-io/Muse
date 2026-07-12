-- MUSE-02: episode_files — many-to-many join between episodes and
-- media_files (season-pack files satisfy multiple episodes).
--
-- ARR-BLUEPRINT §3/§7.3: Sonarr's EpisodeFiles.ReleaseType (single/multi/
-- season-pack) is what motivates this join rather than a 1:1
-- episodes.media_file_id column. A single-episode file still gets exactly
-- one row here; a season-pack media_files row gets one row per episode it
-- satisfies.
CREATE TABLE episode_files (
    episode_id     bigint NOT NULL REFERENCES episodes(id) ON DELETE CASCADE,
    media_file_id  bigint NOT NULL REFERENCES media_files(id) ON DELETE CASCADE,
    PRIMARY KEY (episode_id, media_file_id)
);
CREATE INDEX ON episode_files (media_file_id);
