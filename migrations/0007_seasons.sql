-- MUSE-02: seasons — the middle level of the 3-level TV hierarchy
-- (series -> season -> episode).
--
-- ARR-BLUEPRINT §3/§7.2: the deployed Sonarr does NOT have a Seasons table
-- (season state is a JSON array embedded in Series.Seasons, and per-season
-- statistics are computed at API-serve time, not stored) — but the blueprint
-- explicitly recommends Muse store season as a first-class row instead,
-- since Radarr movies have no intermediate level and the spec's flat
-- media_children (season+episode as siblings distinguished by `kind`) can't
-- cleanly represent season-level state (monitored) separately from
-- episode-level state. Seasons hang off media_items (the per-library
-- instance), not media_metadata — `monitored` is instance state, same as
-- Sonarr's Series.Seasons.
CREATE TABLE seasons (
    id              bigserial PRIMARY KEY,
    media_item_id   bigint NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    season_number   int NOT NULL,
    title           text,
    overview        text,
    monitored       boolean NOT NULL DEFAULT false,
    air_date        date,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (media_item_id, season_number),
    -- superkey enabling a composite FK from episodes(season_id, media_item_id),
    -- so an episode's denormalized media_item_id is structurally forced to match
    -- its season's owning media_item (prevents cross-show episode drift).
    UNIQUE (id, media_item_id)
);
CREATE INDEX ON seasons (media_item_id, season_number);
