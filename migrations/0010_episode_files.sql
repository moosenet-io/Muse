-- MUSE-02: episode_files — many-to-many join between episodes and
-- media_files (season-pack files satisfy multiple episodes).
--
-- ARR-BLUEPRINT §3/§7.3: Sonarr's EpisodeFiles.ReleaseType (single/multi/
-- season-pack) is what motivates this join rather than a 1:1
-- episodes.media_file_id column. A single-episode file still gets exactly
-- one row here; a season-pack media_files row gets one row per episode it
-- satisfies.
-- media_item_id is carried on the join purely to anchor two composite FKs
-- (to episodes and to media_files) that structurally guarantee the episode and
-- the file belong to the SAME show — a season-pack file can never be linked to
-- an episode of a different media_item. It is derived from the episode at
-- insert time (see repo::media_file::attach_to_episode), never supplied by the
-- caller.
CREATE TABLE episode_files (
    episode_id     bigint NOT NULL,
    media_file_id  bigint NOT NULL,
    media_item_id  bigint NOT NULL,
    PRIMARY KEY (episode_id, media_file_id),
    FOREIGN KEY (episode_id, media_item_id) REFERENCES episodes (id, media_item_id) ON DELETE CASCADE,
    FOREIGN KEY (media_file_id, media_item_id) REFERENCES media_files (id, media_item_id) ON DELETE CASCADE
);
CREATE INDEX ON episode_files (media_file_id);
