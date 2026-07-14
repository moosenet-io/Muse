-- MUSEX-15 (Plane TERM #391), part B: async "book-club" style discussion
-- threads for a premiere event/title. A thread is keyed to the title it
-- discusses (`media_metadata_id`); posts are keyed to the thread and to the
-- Discord identity that made them. Consent/allowlist enforcement (who may
-- post) happens in the `premiere::discussion` domain layer BEFORE a row is
-- ever inserted here — this table stores no consent state itself, mirroring
-- how `proactive_items`/`watch_stats` store data without re-deriving the
-- gating that produced it.
CREATE TABLE discussion_threads (
    id                 bigserial PRIMARY KEY,
    media_metadata_id  bigint NOT NULL REFERENCES media_metadata(id) ON DELETE CASCADE,
    title              text NOT NULL,
    created_at         timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX discussion_threads_media_metadata_id_idx
    ON discussion_threads (media_metadata_id);

CREATE TABLE discussion_posts (
    id                bigserial PRIMARY KEY,
    thread_id         bigint NOT NULL REFERENCES discussion_threads(id) ON DELETE CASCADE,
    -- The Discord user id of the (opted-in, allowlisted) friend who posted
    -- -- not a `muse_account_id` foreign key, since `FriendIdentity`'s own
    -- linkage is private (see `discord::identity`'s module doc); the
    -- domain layer resolves/authorizes the poster before this insert ever
    -- runs.
    discord_user_id  text NOT NULL,
    body              text NOT NULL,
    posted_at         timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX discussion_posts_thread_id_idx ON discussion_posts (thread_id);
