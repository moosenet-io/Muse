-- MUSE-03: taste_signals (spec §3.4) — raw, auditable weighted taste
-- events; taste_profile (0019) is derived from these, never hand-edited.
-- media_item_id uses ON DELETE SET NULL (not CASCADE): a signal is a
-- historical fact about behavior ("this account rewatched this title 3
-- times") that stays meaningful even if the library item is later removed,
-- matching the play_sessions telemetry-preservation rationale (0015).
CREATE TABLE taste_signals (
    id            bigserial PRIMARY KEY,
    account_id    bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    media_item_id bigint REFERENCES media_items(id) ON DELETE SET NULL,
    signal_type   text NOT NULL, -- 'finished','abandoned','rewatched','rated','watchlisted','curation_note'
    weight        real NOT NULL, -- +1.0 finish, +2.5 rewatch, -1.5 abandon, explicit rating scaled
    context_key   text,
    note          text,          -- free-text curation ("loved the pacing")
    observed_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON taste_signals (account_id, observed_at DESC);
CREATE INDEX ON taste_signals (media_item_id);
