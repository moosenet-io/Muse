-- MUSE-03: enable pgvector — deferred from 0000_extensions.sql (MUSE-02
-- comment: "the embeddings/taste tables are MUSE-03/08 scope"). This is the
-- first migration in this repo to touch vector columns; 0018_embeddings.sql
-- and 0019_taste_profile.sql depend on it.
CREATE EXTENSION IF NOT EXISTS vector;

-- Spec §3.1 also carries `decision_kind` (video/audio/transcode decision
-- enum for the Tautulli-parity media-info table, 0016). `media_kind` and
-- `play_state` from the spec's §3.1 enum block are NOT redeclared here:
-- `media_kind` already exists from MUSE-02 (0001_enums.sql); `play_state`
-- isn't referenced by any column in this block (play_events.event_type and
-- play_sessions use plain text/booleans instead — see 0014/0015) so it's
-- left out rather than added unused.
CREATE TYPE decision_kind AS ENUM ('direct_play', 'direct_stream', 'transcode', 'copy');
