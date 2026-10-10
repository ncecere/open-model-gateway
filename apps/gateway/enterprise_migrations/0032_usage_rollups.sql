-- Hourly usage rollups (scale plan P6).
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- usage_rollups_hourly holds, per UTC hour of execution start and per
-- (workspace, key, deployment, model alias, provider, cost-center snapshot,
-- execution state, accounting state), exact integer sums of attempts,
-- tokens and micro-USD. Unknown stays separate: accounting_state NULL is an
-- attempt without a reservation, 'unknown' keeps its hold in held_microusd
-- and every attempt without a known cost is counted in unresolved_attempts;
-- nothing unknown is summed as zero. Request counts (distinct root requests)
-- are not additive across groups and are never answered from rollups.
--
-- Exactness protocol (src/rollups.rs):
--   * the leased `rollups` job recomputes an hour H from raw rows in one
--     REPEATABLE READ transaction (delete + insert + usage_rollup_hours row)
--     only once H ended at or before every running gateway transaction
--     started (and at least an hour ago);
--   * every later write to a row of an ended hour (a transaction that began
--     at or after the end of the row's hour) appends a usage_rollup_dirty
--     marker in the same transaction (row triggers below, WHEN-filtered so
--     the admission path evaluates one comparison);
--   * readers use an hour's rollup rows only if usage_rollup_hours has the
--     hour and no marker of the hour is visible in their snapshot, and
--     aggregate raw rows for every other hour (omg_usage_rows); the job
--     deletes only the markers its snapshot saw.
-- Rollups are derived data (the runtime may delete and rebuild them); raw
-- history is never modified. Archived months keep their rollups.

CREATE TABLE usage_rollups_hourly(
 hour_start timestamptz NOT NULL CHECK(hour_start=date_trunc('hour',hour_start,'UTC')),
 workspace_id uuid NOT NULL,
 api_key_id uuid NOT NULL,
 deployment_id uuid NOT NULL,
 public_model text NOT NULL,
 provider text NOT NULL,
 cost_center_id uuid,
 cost_center_name text,
 execution_state text NOT NULL,
 accounting_state text CHECK(accounting_state IN ('pending','unknown','settled')),
 attempts bigint NOT NULL CHECK(attempts>0),
 input_tokens numeric(38,0),
 output_tokens numeric(38,0),
 tokens numeric(38,0) NOT NULL,
 unknown_token_attempts bigint NOT NULL,
 known_cost_microusd numeric(38,0),
 held_microusd numeric(38,0),
 unresolved_attempts bigint NOT NULL,
 cache_input_tokens numeric(38,0),
 cache_read_tokens numeric(38,0),
 priced_cost_microusd numeric(38,0),
 priced_tokens numeric(38,0),
 CONSTRAINT usage_rollups_hourly_group UNIQUE NULLS NOT DISTINCT(hour_start,workspace_id,api_key_id,deployment_id,
  public_model,provider,cost_center_id,cost_center_name,execution_state,accounting_state)
);
CREATE INDEX usage_rollups_workspace_hour ON usage_rollups_hourly(workspace_id,hour_start);

-- Hours whose rollup rows are complete as of the job's snapshot, with the
-- fencing epoch of the `rollups` lease term that wrote them.
CREATE TABLE usage_rollup_hours(
 hour_start timestamptz PRIMARY KEY CHECK(hour_start=date_trunc('hour',hour_start,'UTC')),
 rolled_at timestamptz NOT NULL,
 groups bigint NOT NULL CHECK(groups>=0),
 lease_epoch bigint
);
-- Every hour before rolled_through has been rolled at least once.
CREATE TABLE usage_rollup_progress(
 singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton),
 rolled_through timestamptz NOT NULL CHECK(rolled_through=date_trunc('hour',rolled_through,'UTC'))
);
INSERT INTO usage_rollup_progress(rolled_through)
 SELECT date_trunc('hour',coalesce(min(started_at),clock_timestamp()),'UTC') FROM inference_executions;
CREATE FUNCTION usage_rollup_progress_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP<>'UPDATE' OR NEW.rolled_through<OLD.rolled_through THEN
  RAISE EXCEPTION 'rollup progress only moves forward' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER usage_rollup_progress_forward BEFORE UPDATE OR DELETE ON usage_rollup_progress
 FOR EACH ROW EXECUTE FUNCTION usage_rollup_progress_guard();
CREATE TRIGGER usage_rollup_progress_no_truncate BEFORE TRUNCATE ON usage_rollup_progress
 FOR EACH STATEMENT EXECUTE FUNCTION usage_rollup_progress_guard();

-- Change markers of ended hours (append-only for writers; the job deletes).
CREATE TABLE usage_rollup_dirty(
 id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
 hour_start timestamptz NOT NULL,
 marked_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX usage_rollup_dirty_hour ON usage_rollup_dirty(hour_start);

-- TG_ARGV[0]: the row's time column (started_at or admitted_at).
CREATE FUNCTION usage_rollup_mark() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE o timestamptz; n timestamptz;
BEGIN
 IF TG_OP IN ('UPDATE','DELETE') THEN
  o := (to_jsonb(OLD)->>TG_ARGV[0])::timestamptz;
 END IF;
 IF TG_OP IN ('INSERT','UPDATE') THEN
  n := (to_jsonb(NEW)->>TG_ARGV[0])::timestamptz;
 END IF;
 INSERT INTO usage_rollup_dirty(hour_start)
  SELECT DISTINCT date_trunc('hour',t,'UTC') FROM unnest(ARRAY[o,n]) t
  WHERE t IS NOT NULL AND date_trunc('hour',t,'UTC')+interval '1 hour'<=transaction_timestamp();
 RETURN NULL;
END $$;
CREATE TRIGGER usage_rollup_execution_insert AFTER INSERT ON inference_executions FOR EACH ROW
 WHEN (date_trunc('hour',NEW.started_at,'UTC')+interval '1 hour'<=transaction_timestamp())
 EXECUTE FUNCTION usage_rollup_mark('started_at');
CREATE TRIGGER usage_rollup_execution_update AFTER UPDATE OF state,input_tokens,output_tokens,billing_usage,public_model,
  provider,workspace_id,api_key_id,deployment_id,cost_center_id,cost_center_name,started_at ON inference_executions FOR EACH ROW
 WHEN (date_trunc('hour',OLD.started_at,'UTC')+interval '1 hour'<=transaction_timestamp()
  OR date_trunc('hour',NEW.started_at,'UTC')+interval '1 hour'<=transaction_timestamp())
 EXECUTE FUNCTION usage_rollup_mark('started_at');
CREATE TRIGGER usage_rollup_execution_delete AFTER DELETE ON inference_executions FOR EACH ROW
 WHEN (date_trunc('hour',OLD.started_at,'UTC')+interval '1 hour'<=transaction_timestamp())
 EXECUTE FUNCTION usage_rollup_mark('started_at');
CREATE TRIGGER usage_rollup_reservation_insert AFTER INSERT ON governance_reservations FOR EACH ROW
 WHEN (date_trunc('hour',NEW.admitted_at,'UTC')+interval '1 hour'<=transaction_timestamp())
 EXECUTE FUNCTION usage_rollup_mark('admitted_at');
CREATE TRIGGER usage_rollup_reservation_update AFTER UPDATE OF state,actual_microusd,held_microusd,admitted_at ON governance_reservations FOR EACH ROW
 WHEN (date_trunc('hour',OLD.admitted_at,'UTC')+interval '1 hour'<=transaction_timestamp()
  OR date_trunc('hour',NEW.admitted_at,'UTC')+interval '1 hour'<=transaction_timestamp())
 EXECUTE FUNCTION usage_rollup_mark('admitted_at');
CREATE TRIGGER usage_rollup_reservation_delete AFTER DELETE ON governance_reservations FOR EACH ROW
 WHEN (date_trunc('hour',OLD.admitted_at,'UTC')+interval '1 hour'<=transaction_timestamp())
 EXECUTE FUNCTION usage_rollup_mark('admitted_at');

-- Raw aggregation of [lo, hi) in the rollup shape (the job writes exactly
-- this for one hour; readers use it for hours without a usable rollup).
CREATE FUNCTION omg_usage_aggregate(lo timestamptz, hi timestamptz)
RETURNS SETOF usage_rollups_hourly LANGUAGE sql STABLE AS $$
 SELECT date_trunc('hour',e.started_at,'UTC'),e.workspace_id,e.api_key_id,e.deployment_id,e.public_model,e.provider,
  e.cost_center_id,e.cost_center_name,e.state,r.state,
  count(*),sum(e.input_tokens),sum(e.output_tokens),sum(coalesce(e.input_tokens,0)+coalesce(e.output_tokens,0)),
  count(*) FILTER(WHERE e.input_tokens IS NULL OR e.output_tokens IS NULL),
  sum(r.actual_microusd),
  sum(r.held_microusd) FILTER(WHERE r.actual_microusd IS NULL AND r.state IN('pending','unknown')),
  count(*) FILTER(WHERE r.actual_microusd IS NULL),
  sum((e.billing_usage->>'total_input_tokens')::numeric) FILTER(WHERE e.billing_usage->>'total_input_tokens' IS NOT NULL AND e.billing_usage->>'cache_read_input_tokens' IS NOT NULL),
  sum((e.billing_usage->>'cache_read_input_tokens')::numeric) FILTER(WHERE e.billing_usage->>'total_input_tokens' IS NOT NULL AND e.billing_usage->>'cache_read_input_tokens' IS NOT NULL),
  sum(r.actual_microusd) FILTER(WHERE r.actual_microusd IS NOT NULL AND e.input_tokens IS NOT NULL AND e.output_tokens IS NOT NULL),
  sum(e.input_tokens+e.output_tokens) FILTER(WHERE r.actual_microusd IS NOT NULL)
 FROM inference_executions e
 LEFT JOIN governance_reservations r ON r.execution_id=e.id AND r.admitted_at=e.started_at
 WHERE e.started_at>=lo AND e.started_at<hi
 GROUP BY 1,2,3,4,5,6,7,8,9,10
$$;

-- Usage of [lo, hi) in the rollup shape: rollup rows of whole hours inside
-- the range that are rolled without a visible change marker, raw
-- aggregation of every other (part of an) hour. Exact in the caller's
-- snapshot (see the protocol above).
CREATE FUNCTION omg_usage_rows(lo timestamptz, hi timestamptz)
RETURNS SETOF usage_rollups_hourly LANGUAGE sql STABLE AS $$
 WITH hours AS (
  SELECT h.h,h.h>=lo AND h.h+interval '1 hour'<=hi
   AND EXISTS(SELECT 1 FROM usage_rollup_hours u WHERE u.hour_start=h.h)
   AND NOT EXISTS(SELECT 1 FROM usage_rollup_dirty d WHERE d.hour_start=h.h) rolled
  FROM generate_series(date_trunc('hour',lo,'UTC'),hi-interval '1 microsecond',interval '1 hour') h(h))
 SELECT u.* FROM hours JOIN usage_rollups_hourly u ON u.hour_start=hours.h WHERE hours.rolled
 UNION ALL
 SELECT a.* FROM hours CROSS JOIN LATERAL omg_usage_aggregate(greatest(hours.h,lo),least(hours.h+interval '1 hour',hi)) a
 WHERE NOT hours.rolled
$$;
INSERT INTO work_leases(name) VALUES('rollups') ON CONFLICT DO NOTHING;
