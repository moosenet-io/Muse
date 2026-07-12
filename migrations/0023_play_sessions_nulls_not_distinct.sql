-- MUSE-07: make the play_sessions natural-key UNIQUE a true idempotency key
-- even when media_item_id / episode_id are NULL.
--
-- MUSE-03's `UNIQUE (account_id, media_item_id, episode_id, started_at)` uses
-- Postgres's default NULL semantics, where every NULL is distinct. That means
-- a movie play (episode_id NULL) or a not-yet-resolved play (both NULL) never
-- conflicts with itself, so the session-reconstruction upsert
-- (`repo::play_session::upsert`, `ON CONFLICT (account_id, media_item_id,
-- episode_id, started_at)`) would INSERT a duplicate row on every re-run
-- instead of UPDATING the existing session — breaking the idempotency the
-- native tracker (and the Tautulli backfill) depend on.
--
-- `NULLS NOT DISTINCT` (PG15+) makes those NULLs compare equal, so the tuple
-- is a real dedup key regardless of media resolution. This strictly tightens
-- dedup and is safe for the Tautulli backfill too: distinct real watches have
-- distinct `started_at`, and that path already de-duplicates on
-- `tautulli_ref_id` / ±120s overlap before inserting.
--
-- The original constraint's auto-generated name is dropped by lookup (it may
-- be server-truncated), then re-added with the same columns so the existing
-- `ON CONFLICT (account_id, media_item_id, episode_id, started_at)` (matched
-- by column list, not name) resolves to the new index.
DO $$
DECLARE
    cname text;
BEGIN
    SELECT conname INTO cname
    FROM pg_constraint
    WHERE conrelid = 'play_sessions'::regclass
      AND contype = 'u'
      AND array_length(conkey, 1) = 4;
    IF cname IS NOT NULL THEN
        EXECUTE format('ALTER TABLE play_sessions DROP CONSTRAINT %I', cname);
    END IF;
END $$;

ALTER TABLE play_sessions
    ADD CONSTRAINT play_sessions_natural_key_uniq
    UNIQUE NULLS NOT DISTINCT (account_id, media_item_id, episode_id, started_at);
