-- MUSEX-WIRE-05 (Plane TERM #398, slice 5): the persisted opt-in store that
-- makes the consent-gated personalized paths wired in WIRE-01..04 actually
-- reachable in production.
--
-- `crate::settings::DiscordBotSettings::trusted_friends` (MUSEX-18) is
-- ALLOWLIST-ONLY -- it can never itself grant `taste_opt_in`, since
-- `crate::discord::identity::FriendIdentity`'s consent field stays private
-- and settable only through `FriendIdentity::opt_in` (see that module's
-- doc). This table is the missing persisted signal: one row per Discord
-- friend who has actually consented, which
-- `crate::discord::roster::resolve_trusted_friends` (this slice) reads to
-- decide whether to call `FriendIdentity::opt_in` when building the
-- production roster. It stores no consent LOGIC of its own -- it is a
-- plain fact table; `FriendIdentity::opt_in` remains the sole place
-- `taste_opt_in` is ever set to `true` in a `FriendIdentity`, mirroring how
-- `discussion_threads`/`discussion_posts` (migrations/0101) store data
-- without re-deriving the gating that produced it.
CREATE TABLE friend_opt_in (
    discord_user_id  text PRIMARY KEY,
    opted_in         boolean NOT NULL DEFAULT false,
    -- The linked Muse account whose taste/watch-data this friend has
    -- consented to have used. `ON DELETE SET NULL` (not CASCADE): if the
    -- linked account is deleted, the opt-in row survives as an orphaned,
    -- account-less record -- `resolve_trusted_friends` treats
    -- `opted_in = true` with `muse_account_id = NULL` as NOT opted in
    -- (defensively; `FriendIdentity::opt_in` requires an account id, so
    -- this shape can never come from a normal opt-in write, only from this
    -- FK degrade), never a dangling/garbage account id.
    muse_account_id  bigint REFERENCES accounts(id) ON DELETE SET NULL,
    opted_in_at      timestamptz
);
