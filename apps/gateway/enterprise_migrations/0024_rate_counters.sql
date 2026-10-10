-- Maintained rate and in-flight counters.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- Admission used to aggregate every reservation of the current UTC minute and
-- every pending lease (plus executions of the minute without a reservation)
-- once per rate policy layer while holding the installation lock, so its cost
-- grew with traffic. These tables keep exactly the quantities that scan
-- produced, per consumption scope:
--   scope_kind/scope_id: 'workspace' (workspace id) or 'key' (key lineage =
--                        api_keys.governance_key_id), the scopes of the
--                        workspace-type/override/local and key policy layers.
--                        There is deliberately no installation scope: one
--                        installation row would be a single hot row for every
--                        request. The installation policy layer (scheduled for
--                        removal) keeps the former scan unchanged.
-- rate_minute_counters, per scope and reservation minute_start (executions
-- without a reservation: the UTC minute of started_at):
--   requests:   interactive admissions (async jobs and gateway-run batch lines
--               excluded) plus executions without a reservation;
--   unreserved: the subset without a token reservation (reserved_tokens null,
--               or no reservation at all); any such row denies tokens/minute;
--   tokens:     sum over interactive reservations of
--               greatest(reserved, input+output, normalized input+output)
--               (rate_reserved_tokens): observed excess raises consumption,
--               nothing is refunded.
-- inflight_counters, per scope:
--   requests:   pending reservations not (yet) accepted as an async job
--               ("requests at once");
--   jobs:       pending async-job reservations (video/batch) whose job is not
--               terminal and not cancel-requested ("jobs at once").
-- Leases are not part of the counters: admission subtracts pending rows whose
-- lease already expired (lease_expires_at <= admission time, indexed by
-- governance_leases, bounded by reconciliation lag), so expiry releases
-- concurrency exactly when the former scan did.
--
-- Rows are maintained by triggers in the same transaction as every
-- reservation/execution/async-job write, so no code path can forget them.
-- Minute rows older than ten minutes are no longer read by admission and may
-- be pruned (rate_minute_counters_retained refuses removing newer rows).
-- History rows are never modified by this migration.
-- `open-model-gateway budget verify` compares the in-flight counters and the
-- retained minutes with a full scan.
-- Columns carry no >=0 CHECK: signed deltas are applied as upserts (see 0015).
-- Hot rows stay HOT-updatable: only the primary key is indexed and it holds
-- no counter column; fillfactor leaves room for in-page updates and
-- autovacuum is threshold-driven (not proportional to table size).
-- Per-replica approximate allowances (later) can be layered on these rows
-- (or replace the minute read) without changing the in-flight counters.
CREATE TABLE rate_minute_counters(
 minute_start timestamptz NOT NULL,
 scope_kind text NOT NULL CHECK(scope_kind IN ('workspace','key')),
 scope_id uuid NOT NULL,
 requests bigint NOT NULL DEFAULT 0,
 unreserved bigint NOT NULL DEFAULT 0,
 tokens numeric NOT NULL DEFAULT 0,
 PRIMARY KEY(minute_start,scope_kind,scope_id)
) WITH (fillfactor=70,autovacuum_vacuum_scale_factor=0,autovacuum_vacuum_threshold=1000,
 autovacuum_vacuum_cost_limit=2000,autovacuum_vacuum_cost_delay=1);

CREATE TABLE inflight_counters(
 scope_kind text NOT NULL CHECK(scope_kind IN ('workspace','key')),
 scope_id uuid NOT NULL,
 requests bigint NOT NULL DEFAULT 0,
 jobs bigint NOT NULL DEFAULT 0,
 PRIMARY KEY(scope_kind,scope_id)
) WITH (fillfactor=70,autovacuum_vacuum_scale_factor=0,autovacuum_vacuum_threshold=1000,
 autovacuum_vacuum_cost_limit=2000,autovacuum_vacuum_cost_delay=1);

-- Retained minutes are never removed (pruning old minutes is allowed).
CREATE FUNCTION rate_minute_counters_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.minute_start>=date_trunc('minute',clock_timestamp(),'UTC')-interval '10 minutes' THEN
  RAISE EXCEPTION 'rate counters of the retained window are never removed';
 END IF;
 RETURN OLD;
END $$;
CREATE TRIGGER rate_minute_counters_retained BEFORE DELETE ON rate_minute_counters
 FOR EACH ROW EXECUTE FUNCTION rate_minute_counters_guard();

-- Token consumption of one reservation (the former scan's expression).
CREATE FUNCTION rate_reserved_tokens(reserved bigint, input bigint, output bigint, billing jsonb)
RETURNS numeric LANGUAGE sql IMMUTABLE AS $$
 SELECT greatest(coalesce(reserved,0)::numeric,coalesce(input,0)::numeric+coalesce(output,0)::numeric,
  coalesce((billing->>'total_input_tokens')::numeric,0)+coalesce(output,0)::numeric,
  coalesce((billing->>'uncached_input_tokens')::numeric,0)+coalesce((billing->>'cache_read_input_tokens')::numeric,0)
  +greatest(coalesce((billing->>'cache_write_input_tokens')::numeric,0),
   coalesce((billing->>'cache_write_default_input_tokens')::numeric,0)+coalesce((billing->>'cache_write_5m_input_tokens')::numeric,0)+coalesce((billing->>'cache_write_1h_input_tokens')::numeric,0))
  +coalesce(output,0)::numeric)
$$;

-- One signed contribution (minute part and in-flight part).
CREATE TYPE rate_counter_delta AS (
 workspace_id uuid, api_key_id uuid, minute_start timestamptz,
 requests bigint, unreserved bigint, tokens numeric, inflight bigint, jobs bigint
);

-- Contribution of reservation r given its execution's workload/batch link and
-- its async job (if any), exactly as the former scan classified it:
--   job      = video/batch workload or a gateway-run batch line;
--   accepted = an async job exists (or a batch line: its batch holds the slot);
--   job_done = the job is terminal or cancel-requested (batch lines: always).
CREATE FUNCTION rate_contribution(r governance_reservations, workload_kind text, batch_job_id uuid,
 has_job boolean, job_state text, job_cancel timestamptz, sign integer)
RETURNS rate_counter_delta LANGUAGE sql IMMUTABLE AS $$
 -- One expression without FROM, so the planner inlines it at every call site.
 SELECT ROW(r.workspace_id,r.api_key_id,r.minute_start,
  sign*(NOT (workload_kind IN('videos','batches') OR batch_job_id IS NOT NULL))::int,
  sign*(NOT (workload_kind IN('videos','batches') OR batch_job_id IS NOT NULL) AND r.reserved_tokens IS NULL)::int,
  sign*CASE WHEN workload_kind IN('videos','batches') OR batch_job_id IS NOT NULL THEN 0
   ELSE rate_reserved_tokens(r.reserved_tokens,r.input_tokens,r.output_tokens,r.billing_usage) END,
  sign*(r.state='pending' AND NOT (has_job OR batch_job_id IS NOT NULL))::int,
  sign*((workload_kind IN('videos','batches') OR batch_job_id IS NOT NULL) AND r.state='pending'
   AND NOT CASE WHEN has_job THEN job_state IN('completed','failed','cancelled','expired') OR job_cancel IS NOT NULL
    ELSE batch_job_id IS NOT NULL END)::int)::rate_counter_delta
$$;

CREATE FUNCTION rate_counters_maintain() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE d rate_counter_delta[] := '{}';
BEGIN
 IF TG_TABLE_NAME='governance_reservations' THEN
  IF TG_OP IN ('INSERT','UPDATE') THEN
   d := d || ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,j.id IS NOT NULL,j.state,j.cancel_requested_at,1)
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id LEFT JOIN async_jobs j ON j.execution_id=r.execution_id);
  END IF;
  IF TG_OP IN ('UPDATE','DELETE') THEN
   d := d || ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,j.id IS NOT NULL,j.state,j.cancel_requested_at,-1)
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id LEFT JOIN async_jobs j ON j.execution_id=r.execution_id);
  END IF;
  -- A new reservation stops its execution counting as unreserved; a removed
  -- one makes it unreserved again. Unchanged execution links cancel out.
  IF TG_OP='INSERT' THEN
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),-1,-1,0,0,0)::rate_counter_delta
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id);
  ELSIF TG_OP='UPDATE' THEN
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),-1,-1,0,0,0)::rate_counter_delta
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id
    WHERE NOT EXISTS(SELECT 1 FROM old_rows o WHERE o.execution_id=r.execution_id))
   || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),1,1,0,0,0)::rate_counter_delta
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id
    WHERE NOT EXISTS(SELECT 1 FROM new_rows n WHERE n.execution_id=r.execution_id));
  ELSE
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),1,1,0,0,0)::rate_counter_delta
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id);
  END IF;
 ELSIF TG_TABLE_NAME='inference_executions' THEN
  IF TG_LEVEL='ROW' THEN
   -- Attribution, time, workload or batch link changed (never by runtime grants).
   IF EXISTS(SELECT 1 FROM governance_reservations WHERE execution_id=NEW.id) THEN
    d := ARRAY(SELECT rate_contribution(r,OLD.workload_kind,OLD.batch_job_id,j.id IS NOT NULL,j.state,j.cancel_requested_at,-1)
      FROM governance_reservations r LEFT JOIN async_jobs j ON j.execution_id=r.execution_id WHERE r.execution_id=NEW.id)
     || ARRAY(SELECT rate_contribution(r,NEW.workload_kind,NEW.batch_job_id,j.id IS NOT NULL,j.state,j.cancel_requested_at,1)
      FROM governance_reservations r LEFT JOIN async_jobs j ON j.execution_id=r.execution_id WHERE r.execution_id=NEW.id);
   ELSE
    d := ARRAY[ROW(OLD.workspace_id,OLD.api_key_id,date_trunc('minute',OLD.started_at,'UTC'),-1,-1,0,0,0)::rate_counter_delta,
               ROW(NEW.workspace_id,NEW.api_key_id,date_trunc('minute',NEW.started_at,'UTC'),1,1,0,0,0)::rate_counter_delta];
   END IF;
  ELSIF TG_OP='INSERT' THEN
   -- An execution has no reservation until one is inserted (foreign key order).
   d := ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),1,1,0,0,0)::rate_counter_delta FROM new_rows e);
  ELSE
   -- Reserved executions cannot be deleted (foreign key), so these were unreserved.
   d := ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),-1,-1,0,0,0)::rate_counter_delta FROM old_rows e);
  END IF;
 ELSE
  -- async_jobs (row level): acceptance, then terminal state or cancel request.
  -- Jobs are never deleted and their execution link is immutable (0016).
  IF TG_OP='INSERT' THEN
   d := ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,false,NULL,NULL,-1)
     FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.execution_id=NEW.execution_id)
    || ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,true,NEW.state,NEW.cancel_requested_at,1)
     FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.execution_id=NEW.execution_id);
  ELSE
   d := ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,true,OLD.state,OLD.cancel_requested_at,-1)
     FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.execution_id=OLD.execution_id)
    || ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,true,NEW.state,NEW.cancel_requested_at,1)
     FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id WHERE r.execution_id=NEW.execution_id);
  END IF;
 END IF;
 IF cardinality(d)=0 THEN
  RETURN NULL;
 END IF;
 -- Rows are touched in primary-key order: minute rows, then in-flight rows.
 -- Either upsert is skipped when no delta touches its table.
 IF EXISTS(SELECT 1 FROM unnest(d) x WHERE x.requests<>0 OR x.unreserved<>0 OR x.tokens<>0) THEN
 INSERT INTO rate_minute_counters AS t(minute_start,scope_kind,scope_id,requests,unreserved,tokens)
 SELECT x.minute_start,s.kind,s.id,sum(x.requests),sum(x.unreserved),sum(x.tokens)
 FROM unnest(d) x JOIN api_keys k ON k.id=x.api_key_id
 CROSS JOIN LATERAL (VALUES('workspace',x.workspace_id),('key',k.governance_key_id)) s(kind,id)
 GROUP BY 1,2,3
 HAVING sum(x.requests)<>0 OR sum(x.unreserved)<>0 OR sum(x.tokens)<>0
 ORDER BY 1,2,3
 ON CONFLICT(minute_start,scope_kind,scope_id) DO UPDATE SET
  requests=t.requests+EXCLUDED.requests,
  unreserved=t.unreserved+EXCLUDED.unreserved,
  tokens=t.tokens+EXCLUDED.tokens;
 END IF;
 IF EXISTS(SELECT 1 FROM unnest(d) x WHERE x.inflight<>0 OR x.jobs<>0) THEN
 INSERT INTO inflight_counters AS t(scope_kind,scope_id,requests,jobs)
 SELECT s.kind,s.id,sum(x.inflight),sum(x.jobs)
 FROM unnest(d) x JOIN api_keys k ON k.id=x.api_key_id
 CROSS JOIN LATERAL (VALUES('workspace',x.workspace_id),('key',k.governance_key_id)) s(kind,id)
 GROUP BY 1,2
 HAVING sum(x.inflight)<>0 OR sum(x.jobs)<>0
 ORDER BY 1,2
 ON CONFLICT(scope_kind,scope_id) DO UPDATE SET
  requests=t.requests+EXCLUDED.requests,
  jobs=t.jobs+EXCLUDED.jobs;
 END IF;
 RETURN NULL;
END $$;

-- Block concurrent writers while the backfill and triggers are installed
-- together; readers are unaffected.
LOCK TABLE inference_executions,governance_reservations,async_jobs IN SHARE MODE;

-- Exact backfill: every pending reservation (in-flight) and the retained
-- minutes (the last ten, plus any later ones).
INSERT INTO rate_minute_counters(minute_start,scope_kind,scope_id,requests,unreserved,tokens)
SELECT x.minute_start,s.kind,s.id,sum(x.requests),sum(x.unreserved),sum(x.tokens)
FROM (
 SELECT c.* FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id
  LEFT JOIN async_jobs j ON j.execution_id=r.execution_id
  CROSS JOIN LATERAL rate_contribution(r,e.workload_kind,e.batch_job_id,j.id IS NOT NULL,j.state,j.cancel_requested_at,1) c
  WHERE r.minute_start>=date_trunc('minute',clock_timestamp(),'UTC')-interval '10 minutes'
 UNION ALL
 SELECT e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),1,1,0,0,0 FROM inference_executions e
  WHERE e.started_at>=date_trunc('minute',clock_timestamp(),'UTC')-interval '10 minutes'
  AND NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id)
) x JOIN api_keys k ON k.id=x.api_key_id
CROSS JOIN LATERAL (VALUES('workspace',x.workspace_id),('key',k.governance_key_id)) s(kind,id)
GROUP BY 1,2,3
HAVING sum(x.requests)<>0 OR sum(x.unreserved)<>0 OR sum(x.tokens)<>0;

INSERT INTO inflight_counters(scope_kind,scope_id,requests,jobs)
SELECT s.kind,s.id,sum(c.inflight),sum(c.jobs)
FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id
LEFT JOIN async_jobs j ON j.execution_id=r.execution_id
CROSS JOIN LATERAL rate_contribution(r,e.workload_kind,e.batch_job_id,j.id IS NOT NULL,j.state,j.cancel_requested_at,1) c
JOIN api_keys k ON k.id=r.api_key_id
CROSS JOIN LATERAL (VALUES('workspace',r.workspace_id),('key',k.governance_key_id)) s(kind,id)
WHERE r.state='pending'
GROUP BY 1,2
HAVING sum(c.inflight)<>0 OR sum(c.jobs)<>0;

CREATE TRIGGER rate_counters_reservation_insert AFTER INSERT ON governance_reservations
 REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION rate_counters_maintain();
CREATE TRIGGER rate_counters_reservation_update AFTER UPDATE ON governance_reservations
 REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION rate_counters_maintain();
CREATE TRIGGER rate_counters_reservation_delete AFTER DELETE ON governance_reservations
 REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION rate_counters_maintain();
CREATE TRIGGER rate_counters_execution_insert AFTER INSERT ON inference_executions
 REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION rate_counters_maintain();
CREATE TRIGGER rate_counters_execution_delete AFTER DELETE ON inference_executions
 REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION rate_counters_maintain();
CREATE TRIGGER rate_counters_execution_move AFTER UPDATE OF workspace_id,api_key_id,started_at,workload_kind,batch_job_id ON inference_executions
 FOR EACH ROW WHEN (OLD.workspace_id IS DISTINCT FROM NEW.workspace_id OR OLD.api_key_id IS DISTINCT FROM NEW.api_key_id
  OR OLD.started_at IS DISTINCT FROM NEW.started_at OR OLD.workload_kind IS DISTINCT FROM NEW.workload_kind
  OR OLD.batch_job_id IS DISTINCT FROM NEW.batch_job_id)
 EXECUTE FUNCTION rate_counters_maintain();
CREATE TRIGGER rate_counters_job_insert AFTER INSERT ON async_jobs
 FOR EACH ROW EXECUTE FUNCTION rate_counters_maintain();
CREATE TRIGGER rate_counters_job_update AFTER UPDATE OF state,cancel_requested_at ON async_jobs
 FOR EACH ROW WHEN (OLD.state IS DISTINCT FROM NEW.state OR OLD.cancel_requested_at IS DISTINCT FROM NEW.cancel_requested_at)
 EXECUTE FUNCTION rate_counters_maintain();
