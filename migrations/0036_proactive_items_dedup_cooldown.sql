-- MUSE-12: proactive content generator — cooldown/dedup support for
-- proactive_items (MUSE-03's 0021_proactive_items.sql already carries the
-- account/kind/media_item/headline/body/priority/earliest/expires/delivered
-- shape; this adds what the generator needs on top of it):
--   * dedup_key    — "same nudge" identity within a kind (e.g. a
--                    media_metadata_id, or a synthetic key for a
--                    non-title-scoped nudge like a Friday-evening pick).
--                    The generator's cooldown check is
--                    (account_id, kind, dedup_key, created_at within window).
--   * status       — explicit pending/sent/dismissed tri-state so
--                    `POST /proactive/{id}/ack` can distinguish "Lumina
--                    delivered it" from "the account dismissed it" without
--                    overloading delivered_at for both. Existing callers
--                    that only look at delivered_at (MUSE-03's
--                    list_pending_for_account/mark_delivered) keep working
--                    unmodified — status is additive, default 'pending'.
--   * dismissed_at — when status transitions to 'dismissed'.
ALTER TABLE proactive_items ADD COLUMN dedup_key text;
ALTER TABLE proactive_items ADD COLUMN status text NOT NULL DEFAULT 'pending';
ALTER TABLE proactive_items ADD COLUMN dismissed_at timestamptz;

-- The cooldown-check query's access path: "any item for this
-- (account, kind, dedup_key) created since <cooldown window start>?".
CREATE INDEX ON proactive_items (account_id, kind, dedup_key, created_at);
