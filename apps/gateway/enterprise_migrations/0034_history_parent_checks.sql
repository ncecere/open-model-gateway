-- History parent checks without row locks (scale plan decision gate D3).
-- Explicit operator upgrade only (`migrate`), drained like every migration.
--
-- A foreign-key check on INSERT locks the referenced parent row FOR KEY
-- SHARE. Every admission inserts an execution referencing its workspace,
-- key, deployment and cost center and a reservation referencing its price
-- version, so concurrent admissions lock the same few hot parent rows
-- concurrently and every overlapping locker creates a new MultiXact
-- (measured in P6: median 1.3, up to 5.4 MultiXact members per request,
-- 2.6-10.9 % of the 2^32 member space per day at 1,000 requests/s; the gate
-- is about 1 %). Each lock also dirties the parent's heap page.
--
-- These parents are never removed by the product: workspaces are disabled,
-- keys revoked or disabled, deployments disabled, cost centers archived,
-- price versions are immutable and batch jobs are never deleted (the runtime
-- has no DELETE privilege on any of them). This migration makes that a
-- database rule and replaces the hot foreign keys with insert-time checks
-- that read the parent without locking it:
--   1. parents: DELETE and TRUNCATE are refused by trigger (for every role,
--      including the schema owner) and the referenced key columns are
--      immutable (ids; api_keys.workspace_id, the scope of a key). A row
--      that exists once therefore exists forever with the same scope, so a
--      plain read cannot race with a removal: the lock the foreign key took
--      only protected against removal, which can no longer happen;
--   2. history: BEFORE INSERT (and UPDATE OF the reference columns, which
--      the runtime cannot write) row triggers on the partitioned parents
--      check, in one statement, the scoped references the foreign keys
--      enforced:
--        executions: (workspace_id, api_key_id) is a key of that workspace
--          (keys reference their workspace, so the workspace exists too),
--          deployment_id exists, cost_center_id (if any) exists, and
--          (workspace_id, batch_job_id) (if any) is a job of that workspace;
--        reservations: (deployment_id, price_id) (if priced) is a price
--          version of that deployment.
--      A violation raises SQLSTATE 23503 (foreign_key_violation), like the
--      foreign key did. The reservation -> execution and ledger ->
--      reservation keys stay: they reference rows of the same transaction
--      (or a row the settlement already holds FOR UPDATE), so they create no
--      MultiXacts, and they guarantee admitted_at = started_at, which the
--      partition-pruned joins below rely on;
--   3. the foreign keys are dropped (metadata only: no scan, no rewrite;
--      every existing row was validated by them).
-- Why triggers and not admission code alone: executions and reservations
-- are written by several paths (live admission, batch lines and envelopes,
-- realtime, async jobs, operator tools); a trigger keeps the guarantee in
-- the database for all of them at the cost of 1-4 primary-key probes per
-- row, which admission's scope locks do not otherwise provide for
-- deployments, prices and cost centers. `history verify` (and the leased
-- `history_verify` job, which raises the `history_orphans` incident)
-- re-checks the same relations over stored history.
--
-- Also (P6 follow-up): the budget totals trigger joins a reservation to its
-- execution on (id, started_at = admitted_at), so the lookup touches one
-- month partition instead of probing every partition's index.
SET lock_timeout = '60s';

-- 1. Parents are never removed and never re-keyed.
CREATE FUNCTION omg_history_parent_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='UPDATE' THEN
  IF NEW.id IS DISTINCT FROM OLD.id THEN
   RAISE EXCEPTION '% ids are immutable (referenced by request history)', TG_TABLE_NAME USING ERRCODE='23503';
  END IF;
  IF TG_TABLE_NAME='api_keys' THEN
   IF NEW.workspace_id IS DISTINCT FROM OLD.workspace_id THEN
    RAISE EXCEPTION 'a key never moves to another workspace (referenced by request history)' USING ERRCODE='23503';
   END IF;
  END IF;
  RETURN NEW;
 END IF;
 RAISE EXCEPTION '% rows are referenced by request history and are never removed', TG_TABLE_NAME USING ERRCODE='23503';
END $$;
REVOKE ALL ON FUNCTION omg_history_parent_guard() FROM PUBLIC;

CREATE TRIGGER workspaces_history_parent BEFORE DELETE OR UPDATE OF id ON workspaces
 FOR EACH ROW EXECUTE FUNCTION omg_history_parent_guard();
CREATE TRIGGER workspaces_history_parent_truncate BEFORE TRUNCATE ON workspaces
 FOR EACH STATEMENT EXECUTE FUNCTION omg_history_parent_guard();
CREATE TRIGGER api_keys_history_parent BEFORE DELETE OR UPDATE OF id,workspace_id ON api_keys
 FOR EACH ROW EXECUTE FUNCTION omg_history_parent_guard();
CREATE TRIGGER api_keys_history_parent_truncate BEFORE TRUNCATE ON api_keys
 FOR EACH STATEMENT EXECUTE FUNCTION omg_history_parent_guard();
CREATE TRIGGER deployments_history_parent BEFORE DELETE OR UPDATE OF id ON deployments
 FOR EACH ROW EXECUTE FUNCTION omg_history_parent_guard();
CREATE TRIGGER deployments_history_parent_truncate BEFORE TRUNCATE ON deployments
 FOR EACH STATEMENT EXECUTE FUNCTION omg_history_parent_guard();
CREATE TRIGGER cost_centers_history_parent BEFORE DELETE OR UPDATE OF id ON cost_centers
 FOR EACH ROW EXECUTE FUNCTION omg_history_parent_guard();
CREATE TRIGGER cost_centers_history_parent_truncate BEFORE TRUNCATE ON cost_centers
 FOR EACH STATEMENT EXECUTE FUNCTION omg_history_parent_guard();
-- deployment_prices already refuse UPDATE, DELETE and TRUNCATE (0001,
-- immutable_history); async_jobs refuse DELETE (0016) and identity changes
-- (async_job_guard: id, workspace_id). Jobs also refuse TRUNCATE now.
CREATE TRIGGER async_jobs_history_parent_truncate BEFORE TRUNCATE ON async_jobs
 FOR EACH STATEMENT EXECUTE FUNCTION omg_history_parent_guard();

-- 2. Insert-time reference checks (plain reads: no row lock on the parent).
-- Row triggers fire on partitions with the partition's TG_TABLE_NAME, so the
-- parent table is passed as the argument.
CREATE FUNCTION omg_history_parent_check() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE key_ok boolean; deployment_ok boolean; cost_center_ok boolean; job_ok boolean;
BEGIN
 IF TG_ARGV[0]='inference_executions' THEN
  SELECT EXISTS(SELECT 1 FROM public.api_keys k WHERE k.workspace_id=NEW.workspace_id AND k.id=NEW.api_key_id),
   EXISTS(SELECT 1 FROM public.deployments d WHERE d.id=NEW.deployment_id),
   NEW.cost_center_id IS NULL OR EXISTS(SELECT 1 FROM public.cost_centers c WHERE c.id=NEW.cost_center_id),
   NEW.batch_job_id IS NULL OR EXISTS(SELECT 1 FROM public.async_jobs j WHERE j.workspace_id=NEW.workspace_id AND j.id=NEW.batch_job_id)
  INTO key_ok,deployment_ok,cost_center_ok,job_ok;
  IF NOT key_ok THEN
   RAISE EXCEPTION 'execution key is not a key of its workspace' USING ERRCODE='23503',
    DETAIL=format('workspace_id=%s api_key_id=%s', NEW.workspace_id, NEW.api_key_id);
  ELSIF NOT deployment_ok THEN
   RAISE EXCEPTION 'execution deployment does not exist' USING ERRCODE='23503',
    DETAIL=format('deployment_id=%s', NEW.deployment_id);
  ELSIF NOT cost_center_ok THEN
   RAISE EXCEPTION 'execution cost center does not exist' USING ERRCODE='23503',
    DETAIL=format('cost_center_id=%s', NEW.cost_center_id);
  ELSIF NOT job_ok THEN
   RAISE EXCEPTION 'execution batch job is not a job of its workspace' USING ERRCODE='23503',
    DETAIL=format('workspace_id=%s batch_job_id=%s', NEW.workspace_id, NEW.batch_job_id);
  END IF;
 ELSIF TG_ARGV[0]='governance_reservations' THEN
  IF NEW.price_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM public.deployment_prices p
    WHERE p.deployment_id=NEW.deployment_id AND p.id=NEW.price_id) THEN
   RAISE EXCEPTION 'reservation price is not a price version of its deployment' USING ERRCODE='23503',
    DETAIL=format('deployment_id=%s price_id=%s', NEW.deployment_id, NEW.price_id);
  END IF;
 ELSE
  RAISE EXCEPTION 'omg_history_parent_check: unknown table %', TG_ARGV[0];
 END IF;
 RETURN NEW;
END $$;
REVOKE ALL ON FUNCTION omg_history_parent_check() FROM PUBLIC;

CREATE TRIGGER inference_executions_parent_check
 BEFORE INSERT OR UPDATE OF workspace_id,api_key_id,deployment_id,cost_center_id,batch_job_id ON inference_executions
 FOR EACH ROW EXECUTE FUNCTION omg_history_parent_check('inference_executions');
CREATE TRIGGER governance_reservations_parent_check
 BEFORE INSERT OR UPDATE OF deployment_id,price_id ON governance_reservations
 FOR EACH ROW EXECUTE FUNCTION omg_history_parent_check('governance_reservations');

-- 3. The hot foreign keys (dropped on the parents; partitions follow).
ALTER TABLE inference_executions
 DROP CONSTRAINT IF EXISTS inference_executions_workspace_id_fkey,
 DROP CONSTRAINT IF EXISTS inference_executions_workspace_id_api_key_id_fkey,
 DROP CONSTRAINT IF EXISTS inference_executions_deployment_id_fkey,
 DROP CONSTRAINT IF EXISTS inference_executions_cost_center_id_fkey,
 DROP CONSTRAINT IF EXISTS inference_executions_batch_job;
ALTER TABLE governance_reservations
 DROP CONSTRAINT IF EXISTS governance_reservations_deployment_id_price_id_fkey;

-- 4. Budget totals: reservation -> execution joins pruned to the admission
-- month (started_at = admitted_at is enforced by the retained key).
-- Otherwise unchanged from 0025/0030.
CREATE OR REPLACE FUNCTION budget_totals_maintain() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE d budget_total_delta[] := '{}';
BEGIN
 IF TG_TABLE_NAME='governance_reservations' THEN
  IF TG_OP IN ('INSERT','UPDATE') THEN
   d := d || ARRAY(
    SELECT ROW(r.workspace_id,r.api_key_id,r.admitted_at,
     CASE WHEN r.state='settled' THEN coalesce(r.actual_microusd,0) ELSE 0 END::numeric,
     CASE WHEN r.state='settled' THEN 0 ELSE coalesce(r.held_microusd,0) END::numeric,
     1::bigint,(r.state='pending')::int::bigint,(r.state='unknown')::int::bigint,
     (r.state<>'settled' AND (r.unbounded_cost OR r.held_microusd IS NULL))::int::bigint,
     0::bigint,
     CASE WHEN r.state='unknown' THEN coalesce(r.held_microusd,0) ELSE 0 END::numeric,
     (r.state='unknown' AND (r.unbounded_cost OR r.held_microusd IS NULL))::int::bigint)::budget_total_delta FROM new_rows r);
  END IF;
  IF TG_OP IN ('UPDATE','DELETE') THEN
   d := d || ARRAY(
    SELECT ROW(r.workspace_id,r.api_key_id,r.admitted_at,
     -(CASE WHEN r.state='settled' THEN coalesce(r.actual_microusd,0) ELSE 0 END)::numeric,
     -(CASE WHEN r.state='settled' THEN 0 ELSE coalesce(r.held_microusd,0) END)::numeric,
     -1::bigint,-(r.state='pending')::int::bigint,-(r.state='unknown')::int::bigint,
     -(r.state<>'settled' AND (r.unbounded_cost OR r.held_microusd IS NULL))::int::bigint,
     0::bigint,
     -(CASE WHEN r.state='unknown' THEN coalesce(r.held_microusd,0) ELSE 0 END)::numeric,
     -(r.state='unknown' AND (r.unbounded_cost OR r.held_microusd IS NULL))::int::bigint)::budget_total_delta FROM old_rows r);
  END IF;
  -- A new reservation stops its execution counting as unreserved; a removed
  -- one makes it unreserved again. Unchanged execution links cancel out.
  IF TG_OP='INSERT' THEN
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,-1,0,0)::budget_total_delta
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at);
  ELSIF TG_OP='UPDATE' THEN
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,-1,0,0)::budget_total_delta
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at
    WHERE NOT EXISTS(SELECT 1 FROM old_rows o WHERE o.execution_id=r.execution_id))
   || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,1,0,0)::budget_total_delta
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at
    WHERE NOT EXISTS(SELECT 1 FROM new_rows n WHERE n.execution_id=r.execution_id));
  ELSE
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,1,0,0)::budget_total_delta
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at);
  END IF;
 ELSIF TG_LEVEL='ROW' THEN
  -- inference_executions attribution/time changed (never by runtime grants).
  IF NOT EXISTS(SELECT 1 FROM governance_reservations WHERE execution_id=NEW.id) THEN
   d := ARRAY[ROW(OLD.workspace_id,OLD.api_key_id,OLD.started_at,0,0,0,0,0,0,-1,0,0)::budget_total_delta,
              ROW(NEW.workspace_id,NEW.api_key_id,NEW.started_at,0,0,0,0,0,0,1,0,0)::budget_total_delta];
  END IF;
 ELSIF TG_OP='INSERT' THEN
  -- An execution has no reservation until one is inserted (foreign key order).
  d := ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,1,0,0)::budget_total_delta FROM new_rows e);
 ELSE
  -- Reserved executions cannot be deleted (foreign key), so these were unreserved.
  d := ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,-1,0,0)::budget_total_delta FROM old_rows e);
 END IF;
 IF cardinality(d)=0 THEN
  RETURN NULL;
 END IF;
 -- Key order: 'key' rows, then 'workspace' rows (no installation scope).
 INSERT INTO budget_totals AS t(scope_kind,scope_id,period,period_start,settled_microusd,held_microusd,reservations,pending,unknown,unresolved,unreserved_executions,held_unknown_microusd,unresolved_unknown)
 SELECT s.kind,s.id,p.period,
  CASE WHEN p.period='lifetime' THEN 'epoch'::timestamptz ELSE date_trunc(p.period,x.at,'UTC') END,
  sum(x.settled),sum(x.held),sum(x.reservations),sum(x.pending),sum(x.unknown),sum(x.unresolved),sum(x.unreserved),
  sum(x.held_unknown),sum(x.unresolved_unknown)
 FROM unnest(d) x JOIN api_keys k ON k.id=x.api_key_id
 CROSS JOIN LATERAL (VALUES('workspace',x.workspace_id),('key',k.governance_key_id)) s(kind,id)
 CROSS JOIN (VALUES('day'),('week'),('month'),('lifetime')) p(period)
 GROUP BY 1,2,3,4
 HAVING sum(x.settled)<>0 OR sum(x.held)<>0 OR sum(x.reservations)<>0 OR sum(x.pending)<>0
  OR sum(x.unknown)<>0 OR sum(x.unresolved)<>0 OR sum(x.unreserved)<>0
  OR sum(x.held_unknown)<>0 OR sum(x.unresolved_unknown)<>0
 ORDER BY 1,2,3,4
 ON CONFLICT(scope_kind,scope_id,period,period_start) DO UPDATE SET
  settled_microusd=t.settled_microusd+EXCLUDED.settled_microusd,
  held_microusd=t.held_microusd+EXCLUDED.held_microusd,
  reservations=t.reservations+EXCLUDED.reservations,
  pending=t.pending+EXCLUDED.pending,
  unknown=t.unknown+EXCLUDED.unknown,
  unresolved=t.unresolved+EXCLUDED.unresolved,
  unreserved_executions=t.unreserved_executions+EXCLUDED.unreserved_executions,
  held_unknown_microusd=t.held_unknown_microusd+EXCLUDED.held_unknown_microusd,
  unresolved_unknown=t.unresolved_unknown+EXCLUDED.unresolved_unknown;
 RETURN NULL;
END $$;

-- 5. The leased `history_verify` job and its built-in incident.
INSERT INTO work_leases(name) VALUES('history_verify') ON CONFLICT DO NOTHING;
ALTER TABLE alert_events DROP CONSTRAINT IF EXISTS alert_events_builtin_check,
 DROP CONSTRAINT IF EXISTS alert_events_builtin_shape, DROP CONSTRAINT IF EXISTS alert_events_kind_check,
 DROP CONSTRAINT IF EXISTS alert_events_partitions_kind;
ALTER TABLE alert_events
 ADD CONSTRAINT alert_events_builtin_check CHECK(builtin IN ('personal_budget','scim_last_admin','partitions_missing','history_orphans')),
 ADD CONSTRAINT alert_events_builtin_shape CHECK(builtin IS NULL
  OR (builtin='personal_budget' AND kind='budget_threshold' AND workspace_id IS NOT NULL)
  OR (builtin='scim_last_admin' AND kind='scim_last_admin' AND workspace_id IS NULL AND provider_connection_id IS NULL)
  OR (builtin='partitions_missing' AND kind='partitions_missing' AND workspace_id IS NULL AND provider_connection_id IS NULL)
  OR (builtin='history_orphans' AND kind='history_orphans' AND workspace_id IS NULL AND provider_connection_id IS NULL)),
 ADD CONSTRAINT alert_events_kind_check CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing',
  'scim_last_admin','batch_failed','batch_stalled','spend_threshold','partitions_missing','history_orphans')),
 ADD CONSTRAINT alert_events_partitions_kind CHECK(kind<>'partitions_missing' OR builtin IS NOT DISTINCT FROM 'partitions_missing'),
 ADD CONSTRAINT alert_events_history_kind CHECK(kind<>'history_orphans' OR builtin IS NOT DISTINCT FROM 'history_orphans');
RESET lock_timeout;
