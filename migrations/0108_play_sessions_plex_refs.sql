-- BSEED-2: persist the Plex identifying keys (rating keys + provider GUIDs)
-- a Tautulli history row resolved from, directly on the `play_sessions` row,
-- so the re-resolution pass (`tautulli::backfill::resolve_existing_unresolved`
-- / `POST /ops/library/resolve`) can re-match an unresolved session against
-- later-arriving `media_items` WITHOUT a Tautulli round-trip.
--
-- All nullable / defaulted so this is a pure additive migration: rows imported
-- before this migration simply carry NULL/`[]` here and are re-resolvable only
-- once their keys are (re)stamped (the ops route re-fetches them from Tautulli
-- for the pre-existing backfill; fresh imports stamp them inline).
ALTER TABLE play_sessions
    ADD COLUMN IF NOT EXISTS plex_rating_key             text,
    ADD COLUMN IF NOT EXISTS plex_grandparent_rating_key text,
    ADD COLUMN IF NOT EXISTS plex_grandparent_guid       text,
    ADD COLUMN IF NOT EXISTS plex_guids                  jsonb NOT NULL DEFAULT '[]'::jsonb;

-- Partial index for the re-resolution pass's `list_unresolved` scan (sessions
-- that never resolved to a library item/episode). Keeps the bounded pass cheap
-- even as the resolved-session count grows.
CREATE INDEX IF NOT EXISTS play_sessions_unresolved_idx
    ON play_sessions (account_id)
    WHERE media_item_id IS NULL AND episode_id IS NULL;
