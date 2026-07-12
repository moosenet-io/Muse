-- MUSE-22: discovered Plex players / cast targets (Chromecast, AppleTV, TV
-- apps, Plex web/mobile) — the control targets for playback (spec §3.8).
--
-- High migration number (0090) is deliberate: this table has no FK
-- dependencies on other Muse schema, so it is safe to land ahead of/behind
-- parallel schema branches without ordering constraints.

CREATE TABLE IF NOT EXISTS plex_clients (
  id                 bigserial PRIMARY KEY,
  machine_identifier text UNIQUE NOT NULL,            -- Plex client id (the control target)
  name               text,
  product            text,
  device             text,
  platform           text,
  address            text,
  port               int,
  protocol_caps      text[],                          -- 'playback','timeline','navigation'
  is_cast_target     boolean NOT NULL DEFAULT false,  -- Chromecast/receiver
  last_seen_at       timestamptz NOT NULL DEFAULT now()
);
