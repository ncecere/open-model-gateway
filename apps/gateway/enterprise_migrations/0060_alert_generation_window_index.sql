-- no-transaction
-- #277: large retained delivery history must not block writes for an index
-- build. Invalid interrupted builds are recovered by CONCURRENT_INDEXES in
-- store.rs before migrate retries. 0059 already enforces the bound while this
-- index builds; never count suppressed/uncharged historical rows.
CREATE INDEX CONCURRENTLY IF NOT EXISTS alert_deliveries_generation_window ON alert_deliveries(bucket_id,created_at) WHERE quota_charged;
