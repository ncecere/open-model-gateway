-- no-transaction
-- #310: exact recent-terminal metrics without scanning retained outbox history.
-- Time leads for one range across all terminal states; state covers aggregation.
-- Active rows stay in the existing waiting/leased partial indexes, not this one.
-- An interrupted INVALID build is dropped CONCURRENTLY before the next migrate
-- (CONCURRENT_INDEXES in store.rs), then this standalone statement rebuilds it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS alert_webhook_outbox_completed ON alert_webhook_outbox(completed_at,state) WHERE state IN ('sent','failed','exhausted','ineligible');
