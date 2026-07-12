-- MUSE-02: episodes — the leaf level of the 3-level TV hierarchy.
--
-- ARR-BLUEPRINT §3 (Sonarr Episodes table parity), including scene-numbering
-- divergence tracking (some release groups number episodes differently than
-- TVDB) and anime absolute numbering. media_item_id is denormalized onto
-- episodes (in addition to season_id) purely to make "all episodes for this
-- show/library" queries a single-table scan without joining through seasons.
CREATE TABLE episodes (
    id                          bigserial PRIMARY KEY,
    season_id                   bigint NOT NULL REFERENCES seasons(id) ON DELETE CASCADE,
    media_item_id                bigint NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    episode_number                int NOT NULL,
    absolute_episode_number         int,               -- anime-style continuous numbering, nullable
    scene_absolute_episode_number     int,
    scene_season_number                int,
    scene_episode_number                 int,           -- scene-vs-TVDB numbering mismatch handling (blueprint §3)
    unverified_scene_numbering            boolean NOT NULL DEFAULT false,
    title                                  text,
    overview                                text,
    air_date                                 date,
    air_date_utc                              timestamptz,
    runtime_minutes                            int,
    monitored                                   boolean NOT NULL DEFAULT false,
    has_file                                     boolean NOT NULL DEFAULT false,
    tvdb_id                                       text,
    plex_rating_key                                text,
    last_search_time                                timestamptz,
    created_at                                       timestamptz NOT NULL DEFAULT now(),
    updated_at                                        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (season_id, episode_number),
    UNIQUE (plex_rating_key)
);
CREATE INDEX ON episodes (media_item_id);
CREATE INDEX ON episodes (season_id, episode_number);
