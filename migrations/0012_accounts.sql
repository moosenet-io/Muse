-- MUSE-03: accounts — Plex managed/home users.
--
-- Spec S96 §3.2 defines `accounts` alongside the arr-shaped core, but
-- MUSE-02 didn't need it (no per-user data yet). MUSE-03 is the first item
-- that needs per-user separation (telemetry/taste are NEVER blended across
-- users), so it's introduced here as the first table of this block.
CREATE TABLE accounts (
    id              bigserial PRIMARY KEY,
    plex_account_id text UNIQUE,               -- Plex accountID
    username        text,
    friendly_name   text,
    is_home_user    boolean NOT NULL DEFAULT false,
    is_primary      boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now()
);
