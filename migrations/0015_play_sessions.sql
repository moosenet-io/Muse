-- MUSE-03: play_sessions — reconstructed watch sessions (spec §3.3,
-- Tautulli session_history parity + extensions). A worker stitches
-- play_events (0014) + poll snapshots into these rows; this migration only
-- lays down the table the reconstruction/backfill workers write into.
--
-- Divergence from spec: the spec's flat media_items/media_children pair
-- doesn't match MUSE-02's actual schema (media_items = per-library instance
-- state; TV leaf content lives in episodes, see 0006/0008). Here
-- media_item_id points at the per-library instance (the movie itself, or
-- the owning show for TV context) and episode_id (spec's media_child_id)
-- points at the specific episode for TV plays — nullable, set only for
-- episode-level sessions. Both use ON DELETE SET NULL rather than CASCADE:
-- telemetry history is a first-class asset (Tautulli-replacement rationale)
-- that must survive a library item being removed/reorganized later.
CREATE TABLE play_sessions (
    id                bigserial PRIMARY KEY,
    account_id        bigint REFERENCES accounts(id) ON DELETE CASCADE,
    media_item_id     bigint REFERENCES media_items(id) ON DELETE SET NULL,
    episode_id        bigint REFERENCES episodes(id) ON DELETE SET NULL,
    session_key       text,                          -- Plex session key (null for backfill)
    tautulli_ref_id   bigint,                         -- provenance if imported from Tautulli
    started_at        timestamptz NOT NULL,
    stopped_at        timestamptz,
    duration_ms       bigint,                         -- item runtime
    watched_ms        bigint,                         -- actual watched (sum of playing intervals)
    view_offset_ms    bigint,                         -- final progress
    percent_complete  real,                           -- watched_ms/duration_ms (or offset-based)
    paused_counter    int NOT NULL DEFAULT 0,         -- # of pauses
    paused_ms         bigint NOT NULL DEFAULT 0,
    is_finished       boolean NOT NULL DEFAULT false, -- scrobble OR percent >= COMPLETE_THRESHOLD (0.90)
    is_abandoned      boolean NOT NULL DEFAULT false, -- stopped < ABANDON_THRESHOLD (0.15) -- strong NEGATIVE signal
    -- context (device/time -- taste is contextual)
    player            text,
    platform          text,
    product           text,
    device            text,
    ip_address        inet,
    started_hour      int,                            -- 0-23 local (time-of-day taste)
    started_dow       int,                            -- 0-6 (weekend vs weekday)
    is_cinema_context boolean,                        -- TV/large-screen vs phone/commute
    created_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, media_item_id, episode_id, started_at)
);
CREATE INDEX ON play_sessions (account_id, started_at DESC);
CREATE INDEX ON play_sessions (media_item_id);
CREATE INDEX ON play_sessions (episode_id);
CREATE INDEX ON play_sessions (session_key);
