-- MUSE-23: channel_programs — the LINEAR EPG grid: a time-anchored
-- programming schedule for a `mode='linear'` channel. This is the data the
-- future XMLTV/HDHomeRun-emulation guide (MUSE-28) renders and the ffmpeg
-- streaming engine (MUSE-29) concatenates against; MUSE-23 only owns the
-- table + row shape.
--
-- Divergence from the spec's conceptual §3.8 DDL: the spec's reference
-- schema points `media_item_id` at a flat `media_children` table for both
-- movies and episodes. This repo's real MUSE-02 schema (see
-- `migrations/0006_media_items.sql` / `0008_episodes.sql`) already split
-- that into `media_items` (movies + show-level rows) and a first-class
-- `episodes` table, so `channel_programs` follows suit with two separate
-- nullable content FKs (`media_item_id` for a movie, `episode_id` for a TV
-- episode) instead of one.
CREATE TYPE channel_program_item_type AS ENUM (
    'episode',
    'movie',
    'interstitial'
);

CREATE TABLE channel_programs (
    id              bigserial PRIMARY KEY,
    channel_id      bigint NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    item_type       channel_program_item_type NOT NULL,
    media_item_id   bigint REFERENCES media_items(id) ON DELETE SET NULL,    -- movie (item_type = 'movie')
    episode_id      bigint REFERENCES episodes(id) ON DELETE SET NULL,       -- TV episode (item_type = 'episode')
    interstitial_id bigint REFERENCES interstitials(id) ON DELETE SET NULL,  -- bumper/commercial/etc (item_type = 'interstitial')
    title           text NOT NULL,
    subtitle        text,                          -- episode title / "S2E4"
    description     text,
    artwork_url     text,                          -- poster/cover (Muse-proxied, MUSE-27 artwork_cache)
    start_at        timestamptz NOT NULL,           -- guide start (linear timeline)
    end_at          timestamptz NOT NULL,
    duration_ms     bigint NOT NULL,
    rationale       text,                          -- why the director scheduled it here (MUSE-24)
    -- Seam: once a program actually plays, MUSE-25's taste loop logs it back
    -- into `play_events` (telemetry, MUSE-03 — in flight, not yet built in
    -- this repo). No FK until that table exists; referenced here by name
    -- only, left nullable.
    play_event_id   bigint,
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (channel_id, start_at),
    CHECK (media_item_id IS NOT NULL OR episode_id IS NOT NULL OR interstitial_id IS NOT NULL),
    CHECK (end_at > start_at)
);
CREATE INDEX ON channel_programs (channel_id, start_at);
CREATE INDEX ON channel_programs (episode_id);
CREATE INDEX ON channel_programs (media_item_id);
