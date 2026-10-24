-- no-transaction
-- #253 slice B: distinct-bucket walk and per-bucket due head lookups. Build
-- outside a transaction so a large delivery table does not block reads/writes.
-- An interrupted INVALID index is dropped CONCURRENTLY before the next migrate
-- (CONCURRENT_INDEXES in store.rs), then this statement builds it again.
CREATE INDEX CONCURRENTLY IF NOT EXISTS alert_deliveries_pending_bucket ON alert_deliveries(bucket_id,created_at,id) WHERE status='pending';
