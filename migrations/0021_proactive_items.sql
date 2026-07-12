-- MUSE-03: proactive_items (spec §3.4) — the proactive-content outbox to
-- Lumina (dedup + cooldown so she isn't spammy). media_item_id uses ON
-- DELETE SET NULL for the same reason as play_sessions/taste_signals: a
-- past proactive item ("we told Lumina about this") should stay in the
-- outbox history even if the underlying library row is later removed.
CREATE TABLE proactive_items (
    id            bigserial PRIMARY KEY,
    account_id    bigint REFERENCES accounts(id) ON DELETE CASCADE,
    kind          text NOT NULL, -- 'new_season','finish_nudge','friday_pick','abandon_insight','deal','news'
    media_item_id bigint REFERENCES media_items(id) ON DELETE SET NULL,
    headline      text NOT NULL, -- the line Lumina says
    body          jsonb,         -- structured payload + rationale
    priority      int NOT NULL DEFAULT 5,
    earliest_at   timestamptz,   -- don't surface before (e.g. Friday 20:00)
    expires_at    timestamptz,
    delivered_at  timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON proactive_items (account_id, delivered_at);
CREATE INDEX ON proactive_items (earliest_at, expires_at);
