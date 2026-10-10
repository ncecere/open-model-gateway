-- Scope-lock ceiling visibility (scale follow-up to P3; P3b per-scope shards
-- stay deferred). Explicit operator upgrade only (`migrate`).
--
-- Scoped admission serializes the admissions and settlements of one
-- workspace (and of one key lineage) on that scope's totals and counter rows
-- (0027), so a single workspace or key tops out at roughly 200-300
-- admissions/s on the reference hardware however many replicas run. This
-- migration adds what an operator needs to see a scope approach that
-- ceiling:
--   1. admission_lock_waits: per UTC minute and scope (workspace, key
--      lineage), how many interactive admissions waited at least 10, 25, 50,
--      100, 250, 500, 1000 and 2500 ms for their scope locks (authority plus
--      totals/counter rows). Each replica aggregates in memory, records only
--      admissions that waited at least 10 ms, and adds its counts every few
--      seconds (additive upsert, off the admission path). Rows older than an
--      hour are pruned by maintenance. The admission count of the same
--      minute and scope comes from rate_minute_counters (0024).
--   2. alert rule kind `admission_ceiling` (installation rules only): fires
--      per workspace or key lineage whose admissions stayed at or above
--      `ceiling_requests_per_second` in every complete minute of the window,
--      or whose lock-wait p95 over the window reached `ceiling_lock_wait_ms`
--      (one of the bucket bounds). The window is 5-10 minutes (minute
--      counters are retained 10 minutes). Email goes to Platform Admins only
--      (no listed addresses): the incident identifies the scope.
SET lock_timeout = '60s';

CREATE TABLE admission_lock_waits(
 minute_start timestamptz NOT NULL CHECK(minute_start=date_trunc('minute',minute_start,'UTC')),
 scope_kind text NOT NULL CHECK(scope_kind IN ('workspace','key')),
 scope_id uuid NOT NULL,
 -- waits[i]: admissions that waited at least 10/25/50/100/250/500/1000/2500 ms.
 waits bigint[] NOT NULL CHECK(cardinality(waits)=8 AND array_position(waits,NULL) IS NULL AND 0<=ALL(waits)),
 PRIMARY KEY(minute_start,scope_kind,scope_id)
) WITH (fillfactor=70,autovacuum_vacuum_scale_factor=0,autovacuum_vacuum_threshold=1000,
 autovacuum_vacuum_cost_limit=2000,autovacuum_vacuum_cost_delay=1);

ALTER TABLE alert_rules
 ADD COLUMN ceiling_requests_per_second integer CHECK(ceiling_requests_per_second BETWEEN 1 AND 100000),
 ADD COLUMN ceiling_lock_wait_ms integer CHECK(ceiling_lock_wait_ms IN (10,25,50,100,250,500,1000,2500));
ALTER TABLE alert_rules DROP CONSTRAINT alert_rules_kind_check;
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_kind_check
 CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing','batch_failed','batch_stalled','spend_threshold','admission_ceiling'));
ALTER TABLE alert_rules DROP CONSTRAINT alert_rules_window_kind;
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_window_kind
 CHECK((kind IN ('error_rate','provider_failing','batch_stalled','admission_ceiling')) = (window_minutes IS NOT NULL));
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_ceiling_shape
 CHECK((kind='admission_ceiling') = (ceiling_requests_per_second IS NOT NULL OR ceiling_lock_wait_ms IS NOT NULL));
-- Installation only, a window admission still reads, and no listed
-- addresses: the incident names the workspace (Platform Admins only).
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_ceiling_scope
 CHECK(kind<>'admission_ceiling' OR (scope='installation' AND window_minutes BETWEEN 5 AND 10
  AND cardinality(notify_emails)=0 AND NOT notify_workspace_admins AND thresholds IS NULL));

ALTER TABLE alert_events DROP CONSTRAINT alert_events_kind_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_kind_check CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing',
 'scim_last_admin','batch_failed','batch_stalled','spend_threshold','partitions_missing','history_orphans','admission_ceiling'));
RESET lock_timeout;
