-- MUSE-03: watch_stats / ratings / watchlist (spec §3.3) — per-(account,
-- item) derived aggregates and explicit signals. All three are small,
-- always-per-account-and-media_item tables so they're grouped in one
-- migration (matches the MUSE-02 0011 grouping precedent).
--
-- All key off media_items(id) at the per-library-instance level (see
-- 0015_play_sessions.sql divergence note) with ON DELETE CASCADE, matching
-- the spec exactly: these rows have no meaning independent of the account
-- or the item, unlike raw telemetry which we chose to preserve.
CREATE TABLE watch_stats (
    account_id       bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    media_item_id    bigint NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    play_count       int NOT NULL DEFAULT 0,
    finished_count   int NOT NULL DEFAULT 0,
    rewatch_count    int NOT NULL DEFAULT 0,   -- finishes beyond the first -- VERY strong +
    total_watched_ms bigint NOT NULL DEFAULT 0,
    avg_percent      real,
    last_watched_at  timestamptz,
    abandoned        boolean NOT NULL DEFAULT false, -- ever abandoned early w/o later finish -- NEGATIVE
    first_watched_at timestamptz,
    PRIMARY KEY (account_id, media_item_id)
);

CREATE TABLE ratings (
    account_id    bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    media_item_id bigint NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    rating        real,      -- Plex user rating (0-10) / thumbs
    rated_at      timestamptz,
    PRIMARY KEY (account_id, media_item_id)
);

CREATE TABLE watchlist (
    account_id    bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    media_item_id bigint NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    added_at      timestamptz,
    removed_at    timestamptz,
    fulfilled     boolean NOT NULL DEFAULT false, -- later watched -- intent to action signal
    PRIMARY KEY (account_id, media_item_id)
);
