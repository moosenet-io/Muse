-- MUSE-23: interstitials — the bumper/commercial/music-video/ident pool that
-- fills the "between shows" glue on a composed pseudo-TV channel (spec
-- S96-muse-foundation §3.8/§4d-B). Schema-only in this item: the composer
-- (MUSE-24), the Terminus/Lumina tools (MUSE-25), the web guide (MUSE-27),
-- the linear tuner (MUSE-28), and the ffmpeg engine (MUSE-29) are separate,
-- later items — this migration only stands up the pool + its taxonomy so
-- those items have a stable seam to build against.
--
-- Migration block MUSE-23 owns: 0091-0099.
CREATE TYPE interstitial_kind AS ENUM (
    'bumper',
    'commercial',
    'music_video',
    'ident',
    'short',
    'trailer'
);

CREATE TABLE interstitials (
    id              bigserial PRIMARY KEY,
    plex_rating_key text UNIQUE,                 -- lives in a Plex "Bumpers"/"Commercials" library section
    kind            interstitial_kind NOT NULL,
    title           text,
    decade          int,                          -- 1980, 1990, 2000... (era-matching for the composer)
    theme           text,                          -- 'saturday_morning','horror','holiday','retro_tech'
    genre           text,
    mood            text,
    duration_ms     bigint,
    tags            text[] NOT NULL DEFAULT '{}',  -- free-form auto-tags (MUSE-23 local-LLM tagging pass) + user curation
    source          text,                          -- 'plex_library' | 'user'
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON interstitials (kind, decade, theme);
CREATE INDEX ON interstitials USING gin (tags);
