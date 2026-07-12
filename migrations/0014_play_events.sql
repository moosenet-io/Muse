-- MUSE-03: play_events — raw Plex webhook/poll event stream (spec §3.3),
-- the append-only forensic log that play_sessions (0015) is reconstructed
-- from. Immutable: workers only INSERT here, never UPDATE/DELETE.
--
-- Divergence from spec: the spec's UNIQUE key is
-- `(source, event_type, session_key, view_offset_ms, received_at)`.
-- `received_at` defaults to `now()` and is never supplied by the caller, so
-- literally including it in the dedup key means two calls for the *same*
-- logical delivery never collide (their timestamps differ by however many
-- microseconds elapsed between them) -- defeating the stated purpose ("a
-- duplicate delivery... is a no-op"). Dropping `received_at` from the key
-- makes the intended behavior real: a webhook retry or an overlapping poll
-- tick for the same (source, event_type, session_key, view_offset_ms)
-- dedups via ON CONFLICT DO NOTHING (see repo::play_event::insert).
CREATE TABLE play_events (
    id             bigserial PRIMARY KEY,
    received_at    timestamptz NOT NULL DEFAULT now(),
    source         text NOT NULL,   -- 'plex_webhook' | 'plex_poll' | 'tautulli_backfill'
    event_type     text NOT NULL,   -- 'media.play','media.pause','media.resume','media.stop','media.scrobble','media.rate'
    account_ref    text,            -- Plex accountID (raw; resolved to accounts.id downstream by the reconstruction worker)
    session_key    text,            -- Plex Session.key (stitches events into a session)
    rating_key     text,            -- Plex ratingKey of what's playing
    view_offset_ms bigint,          -- progress at event time
    player         text,
    platform       text,
    product        text,
    device         text,
    ip_address     inet,
    raw            jsonb NOT NULL,  -- full payload for forensic replay
    UNIQUE (source, event_type, session_key, view_offset_ms)
);
CREATE INDEX ON play_events (session_key, received_at);
CREATE INDEX ON play_events (account_ref);
