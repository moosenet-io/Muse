-- MUSE-23: channel_runs — a composed (and optionally played) lineup
-- instance for an 'on_demand' channel: the actual ordered schedule Muse
-- built, the Plex play queue it was pushed into, and its playback status.
-- The composer that PRODUCES the `schedule` payload is MUSE-24; the
-- Terminus/Lumina tools that drive playback state transitions are MUSE-25.
-- This migration only owns the persisted run record.
CREATE TYPE channel_run_status AS ENUM (
    'composed',
    'playing',
    'paused',
    'stopped',
    'completed'
);

CREATE TABLE channel_runs (
    id                 bigserial PRIMARY KEY,
    channel_id         bigint REFERENCES channels(id) ON DELETE SET NULL,
    -- Seam: `accounts` not yet built in this repo (MUSE-03) — see
    -- 0092_channels.sql's account_id comment. No FK until then.
    account_id         bigint,
    target_client_id   bigint REFERENCES plex_clients(id) ON DELETE SET NULL,
    plex_play_queue_id text,                          -- the Plex playQueue this run was pushed into
    schedule           jsonb NOT NULL,                -- ordered [{type:episode|interstitial, ref, title, dur, rationale}]
    total_duration_ms  bigint,
    composed_at        timestamptz NOT NULL DEFAULT now(),
    started_at         timestamptz,
    ended_at           timestamptz,
    status             channel_run_status NOT NULL DEFAULT 'composed'
);
CREATE INDEX ON channel_runs (channel_id, composed_at DESC);
CREATE INDEX ON channel_runs (status);
