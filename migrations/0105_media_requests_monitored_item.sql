-- MUSEM-06 (Plane MUSE, S119 Sprint 1) follow-up (review: codex): links a
-- `media_requests` row back to the `monitored_items` row that produced it,
-- when applicable.
--
-- ## Why this is needed -- the gap `last_search_at.is_none()` couldn't see
-- The wanted worker's original "create-once" guard against spamming a new
-- `Requested` request every pass used `monitored_items.last_search_at IS
-- NULL` as a proxy for "has a worker request already been persisted for
-- this item". That proxy was wrong: `last_search_at` is ALSO set on a
-- FAILED search (Prowlarr unreachable that pass), which creates no
-- `media_requests` row at all. Sequence: pass 1's search fails
-- (`last_search_at` set, no request) -> pass 2 (after cooldown) searches
-- successfully and classifies `NeedsReview` -> the stale proxy says "not
-- first encounter" -> the request is silently never created. This column
-- lets the guard ask the real question directly: does an OPEN
-- (non-terminal) `media_requests` row already exist for this
-- `monitored_item_id`? See `repo::acquisition::has_open_worker_request_for_monitored_item`.
--
-- `NULLABLE` + `ON DELETE SET NULL`: `POST /requests`/`approve`/
-- `AcquisitionSink` (MUSEM-05) create requests with no monitored item at
-- all (a <media-service>-style ask for a title the operator doesn't otherwise
-- monitor) -- those keep passing `NULL` here, unaffected. A monitored item
-- being deleted must never cascade-delete the request rows it produced
-- (the request/history audit trail outlives the monitor), hence `SET NULL`
-- rather than `CASCADE`.
ALTER TABLE media_requests
    ADD COLUMN monitored_item_id bigint REFERENCES monitored_items(id) ON DELETE SET NULL;

CREATE INDEX ON media_requests (monitored_item_id);
