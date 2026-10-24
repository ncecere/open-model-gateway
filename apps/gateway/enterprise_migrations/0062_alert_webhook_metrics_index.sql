-- no-transaction
-- #310: exact recent-terminal metrics without scanning retained outbox history.
-- Time leads for one range across all terminal states; state is an INCLUDE
-- column so the aggregation stays index-only, but other queries (for example
-- per-destination dead letters) cannot use state as an index condition here
-- and keep their own matching partial indexes.
-- Active rows stay in the existing waiting/leased partial indexes, not this one.
-- An interrupted INVALID build is dropped CONCURRENTLY before the next migrate
-- (CONCURRENT_INDEXES in store.rs), then this standalone statement rebuilds it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS alert_webhook_outbox_completed ON alert_webhook_outbox(completed_at) INCLUDE (state) WHERE state IN ('sent','failed','exhausted','ineligible');
