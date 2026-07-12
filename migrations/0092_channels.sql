-- MUSE-23: channels — a defined pseudo-TV channel: personal/theme/genre/era
-- preset, either composed on-demand (cast a play-queue, MUSE-22/25) or
-- persistent 'linear' (a tuner channel in the Plex guide, MUSE-28/29). The
-- composer that actually fills `rules`/`directive` into a schedule is
-- MUSE-24 — this migration only owns the channel DEFINITION.
--
-- `target_client_id` casts to `plex_clients` (MUSE-22, §3.8). If
-- `0090_plex_clients.sql` has not yet merged to main when this migration is
-- authored, this FK still targets it by name/number ordering: MUSE-23's
-- block starts at 0091, strictly after MUSE-22's 0090, so plex_clients is
-- guaranteed to exist by the time this migration actually runs against a
-- database that has migrated in order.
CREATE TYPE channel_kind AS ENUM (
    'personal',
    'theme',
    'genre',
    'era',
    'preset'
);

CREATE TYPE channel_mode AS ENUM (
    'on_demand',
    'linear'
);

CREATE TABLE channels (
    id              bigserial PRIMARY KEY,
    -- Seam: `accounts` (spec §3.2) is not yet built in this repo (it ships
    -- with the MUSE-03 telemetry/taste migrations). No FK until then —
    -- referenced here by name/column only, left nullable.
    account_id      bigint,
    name            text NOT NULL,                -- 'Saturday Morning','90s Chaos','Comfort Rewatch','Discover'
    kind            channel_kind NOT NULL,
    mode            channel_mode NOT NULL DEFAULT 'on_demand',
    channel_number  real,                          -- guide channel number for linear mode (e.g. 101.1)
    target_client_id bigint REFERENCES plex_clients(id) ON DELETE SET NULL,
    directive       text,                          -- the NL brief ("an ep of each sitcom + retro ads, 2 hrs")
    rules           jsonb NOT NULL DEFAULT '{}',   -- episode-selection policy, interstitial cadence/ratio,
                                                    --   era/theme constraints, session length, shuffle vs order
    is_preset       boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (channel_number)
);
CREATE INDEX ON channels (account_id);
CREATE INDEX ON channels (mode);
