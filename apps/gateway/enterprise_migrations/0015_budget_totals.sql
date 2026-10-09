-- Maintained budget consumption totals.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- Admission used to sum every reservation of a budget's scope admitted in the
-- budget's window while holding the installation lock, so its cost grew with
-- history. budget_totals keeps one row per consumption scope, period and
-- period start with exactly the quantities that scan produced:
--   scope_kind/scope_id: 'installation' (nil UUID), 'workspace' (workspace id)
--                        or 'key' (key lineage = api_keys.governance_key_id);
--   period/period_start: UTC day, ISO week (Monday), UTC month, or 'lifetime'
--                        starting at the epoch, bucketed by admission time;
--   settled_microusd:    sum of actual cost of settled reservations;
--   held_microusd:       sum of active pending/unknown holds (unknown cost
--                        retains its hold; nothing is refunded here);
--   unresolved:          unsettled reservations whose cost is unbounded or
--                        unpriced (a known floor is not an upper bound);
--   unreserved_executions: executions without a reservation (always unknown).
-- Budget-type layers (installation, type default, override, local, key) read
-- the row of their consumption scope and their own period, so every layer and
-- every period keeps applying; changing a budget or its period never resets,
-- moves or rewrites consumption. Amounts are exact integer micro-USD.
--
-- Rows are maintained by triggers in the same transaction as every
-- reservation/execution write (admission, settlement, expiry, reconciliation),
-- so no code path can forget them. Runtime writers already serialize on the
-- installation lock; multi-row updates touch rows in key order. History rows
-- (prices, ledger, reservations) are never modified by this migration.
-- `open-model-gateway budget verify` compares this table with a full scan.
-- Columns carry no >=0 CHECK: PostgreSQL checks the proposed INSERT row of an
-- upsert before conflict detection, and signed deltas are applied that way.
CREATE TABLE budget_totals(
 scope_kind text NOT NULL CHECK(scope_kind IN ('installation','workspace','key')),
 scope_id uuid NOT NULL,
 period text NOT NULL CHECK(period IN ('day','week','month','lifetime')),
 period_start timestamptz NOT NULL,
 settled_microusd numeric(38,0) NOT NULL DEFAULT 0,
 held_microusd numeric(38,0) NOT NULL DEFAULT 0,
 reservations bigint NOT NULL DEFAULT 0,
 pending bigint NOT NULL DEFAULT 0,
 unknown bigint NOT NULL DEFAULT 0,
 unresolved bigint NOT NULL DEFAULT 0,
 unreserved_executions bigint NOT NULL DEFAULT 0,
 PRIMARY KEY(scope_kind,scope_id,period,period_start),
 CHECK((scope_kind='installation')=(scope_id='00000000-0000-0000-0000-000000000000'::uuid)),
 CHECK((period='lifetime')=(period_start='epoch'::timestamptz))
) WITH (fillfactor=50);

-- One signed contribution of a reservation (or an unreserved execution).
CREATE TYPE budget_total_delta AS (
 workspace_id uuid, api_key_id uuid, at timestamptz,
 settled numeric, held numeric, reservations bigint, pending bigint,
 unknown bigint, unresolved bigint, unreserved bigint
);

CREATE FUNCTION budget_totals_maintain() RETURNS trigger LANGUAGE plpgsql AS $$
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
     0::bigint)::budget_total_delta FROM new_rows r);
  END IF;
  IF TG_OP IN ('UPDATE','DELETE') THEN
   d := d || ARRAY(
    SELECT ROW(r.workspace_id,r.api_key_id,r.admitted_at,
     -(CASE WHEN r.state='settled' THEN coalesce(r.actual_microusd,0) ELSE 0 END)::numeric,
     -(CASE WHEN r.state='settled' THEN 0 ELSE coalesce(r.held_microusd,0) END)::numeric,
     -1::bigint,-(r.state='pending')::int::bigint,-(r.state='unknown')::int::bigint,
     -(r.state<>'settled' AND (r.unbounded_cost OR r.held_microusd IS NULL))::int::bigint,
     0::bigint)::budget_total_delta FROM old_rows r);
  END IF;
  -- A new reservation stops its execution counting as unreserved; a removed
  -- one makes it unreserved again. Unchanged execution links cancel out.
  IF TG_OP='INSERT' THEN
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,-1)::budget_total_delta
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id);
  ELSIF TG_OP='UPDATE' THEN
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,-1)::budget_total_delta
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id
    WHERE NOT EXISTS(SELECT 1 FROM old_rows o WHERE o.execution_id=r.execution_id))
   || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,1)::budget_total_delta
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id
    WHERE NOT EXISTS(SELECT 1 FROM new_rows n WHERE n.execution_id=r.execution_id));
  ELSE
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,1)::budget_total_delta
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id);
  END IF;
 ELSIF TG_LEVEL='ROW' THEN
  -- inference_executions attribution/time changed (never by runtime grants).
  IF NOT EXISTS(SELECT 1 FROM governance_reservations WHERE execution_id=NEW.id) THEN
   d := ARRAY[ROW(OLD.workspace_id,OLD.api_key_id,OLD.started_at,0,0,0,0,0,0,-1)::budget_total_delta,
              ROW(NEW.workspace_id,NEW.api_key_id,NEW.started_at,0,0,0,0,0,0,1)::budget_total_delta];
  END IF;
 ELSIF TG_OP='INSERT' THEN
  -- An execution has no reservation until one is inserted (foreign key order).
  d := ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,1)::budget_total_delta FROM new_rows e);
 ELSE
  -- Reserved executions cannot be deleted (foreign key), so these were unreserved.
  d := ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,-1)::budget_total_delta FROM old_rows e);
 END IF;
 IF cardinality(d)=0 THEN
  RETURN NULL;
 END IF;
 INSERT INTO budget_totals AS t(scope_kind,scope_id,period,period_start,settled_microusd,held_microusd,reservations,pending,unknown,unresolved,unreserved_executions)
 SELECT s.kind,s.id,p.period,
  CASE WHEN p.period='lifetime' THEN 'epoch'::timestamptz ELSE date_trunc(p.period,x.at,'UTC') END,
  sum(x.settled),sum(x.held),sum(x.reservations),sum(x.pending),sum(x.unknown),sum(x.unresolved),sum(x.unreserved)
 FROM unnest(d) x JOIN api_keys k ON k.id=x.api_key_id
 CROSS JOIN LATERAL (VALUES('installation','00000000-0000-0000-0000-000000000000'::uuid),('workspace',x.workspace_id),('key',k.governance_key_id)) s(kind,id)
 CROSS JOIN (VALUES('day'),('week'),('month'),('lifetime')) p(period)
 GROUP BY 1,2,3,4
 HAVING sum(x.settled)<>0 OR sum(x.held)<>0 OR sum(x.reservations)<>0 OR sum(x.pending)<>0
  OR sum(x.unknown)<>0 OR sum(x.unresolved)<>0 OR sum(x.unreserved)<>0
 ORDER BY 1,2,3,4
 ON CONFLICT(scope_kind,scope_id,period,period_start) DO UPDATE SET
  settled_microusd=t.settled_microusd+EXCLUDED.settled_microusd,
  held_microusd=t.held_microusd+EXCLUDED.held_microusd,
  reservations=t.reservations+EXCLUDED.reservations,
  pending=t.pending+EXCLUDED.pending,
  unknown=t.unknown+EXCLUDED.unknown,
  unresolved=t.unresolved+EXCLUDED.unresolved,
  unreserved_executions=t.unreserved_executions+EXCLUDED.unreserved_executions;
 RETURN NULL;
END $$;

-- Block concurrent writers while the backfill and triggers are installed
-- together; readers are unaffected.
LOCK TABLE inference_executions,governance_reservations IN SHARE MODE;

-- Exact backfill from the existing reservation projection and executions
-- (the same rules the admission scan applied).
INSERT INTO budget_totals(scope_kind,scope_id,period,period_start,settled_microusd,held_microusd,reservations,pending,unknown,unresolved,unreserved_executions)
SELECT s.kind,s.id,p.period,
 CASE WHEN p.period='lifetime' THEN 'epoch'::timestamptz ELSE date_trunc(p.period,c.at,'UTC') END,
 sum(c.settled),sum(c.held),sum(c.reservations),sum(c.pending),sum(c.unknown),sum(c.unresolved),sum(c.unreserved)
FROM (
 SELECT r.workspace_id,r.api_key_id,r.admitted_at at,
  CASE WHEN r.state='settled' THEN coalesce(r.actual_microusd,0) ELSE 0 END::numeric settled,
  CASE WHEN r.state='settled' THEN 0 ELSE coalesce(r.held_microusd,0) END::numeric held,
  1::bigint reservations,(r.state='pending')::int::bigint pending,(r.state='unknown')::int::bigint unknown,
  (r.state<>'settled' AND (r.unbounded_cost OR r.held_microusd IS NULL))::int::bigint unresolved,
  0::bigint unreserved
 FROM governance_reservations r
 UNION ALL
 SELECT e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,1 FROM inference_executions e
 WHERE NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=e.id)
) c JOIN api_keys k ON k.id=c.api_key_id
CROSS JOIN LATERAL (VALUES('installation','00000000-0000-0000-0000-000000000000'::uuid),('workspace',c.workspace_id),('key',k.governance_key_id)) s(kind,id)
CROSS JOIN (VALUES('day'),('week'),('month'),('lifetime')) p(period)
GROUP BY 1,2,3,4;

CREATE TRIGGER budget_totals_reservation_insert AFTER INSERT ON governance_reservations
 REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION budget_totals_maintain();
CREATE TRIGGER budget_totals_reservation_update AFTER UPDATE ON governance_reservations
 REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION budget_totals_maintain();
CREATE TRIGGER budget_totals_reservation_delete AFTER DELETE ON governance_reservations
 REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION budget_totals_maintain();
CREATE TRIGGER budget_totals_execution_insert AFTER INSERT ON inference_executions
 REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION budget_totals_maintain();
CREATE TRIGGER budget_totals_execution_delete AFTER DELETE ON inference_executions
 REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION budget_totals_maintain();
CREATE TRIGGER budget_totals_execution_move AFTER UPDATE OF workspace_id,api_key_id,started_at ON inference_executions
 FOR EACH ROW WHEN (OLD.workspace_id IS DISTINCT FROM NEW.workspace_id OR OLD.api_key_id IS DISTINCT FROM NEW.api_key_id OR OLD.started_at IS DISTINCT FROM NEW.started_at)
 EXECUTE FUNCTION budget_totals_maintain();
