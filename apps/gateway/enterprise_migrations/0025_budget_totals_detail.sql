-- Budget totals detail: split unknown-cost holds out of held_microusd.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- held_microusd (0015) is every active pending/unknown hold and stays what
-- admission enforces. Two subsets let alerts, the metrics gauge and reports
-- read maintained totals instead of scanning history:
--   held_unknown_microusd: holds of reservations whose cost is unknown (state
--                          'unknown'); unknown cost keeps its hold, so this
--                          never shrinks except through evidence-backed
--                          reconciliation;
--   unresolved_unknown:    unknown reservations that are also unresolved
--                          (unbounded or unpriced), so pending unresolved
--                          = unresolved - unresolved_unknown.
-- Amounts are exact integer micro-USD. The migration backfills from the
-- existing reservations and modifies no history rows.
ALTER TABLE budget_totals
 ADD COLUMN held_unknown_microusd numeric(38,0) NOT NULL DEFAULT 0,
 ADD COLUMN unresolved_unknown bigint NOT NULL DEFAULT 0;

-- Hot rows: threshold-driven autovacuum (the table's fillfactor=50 from 0015
-- keeps counter updates HOT; only the primary key is indexed).
ALTER TABLE budget_totals SET (autovacuum_vacuum_scale_factor=0,autovacuum_vacuum_threshold=1000,
 autovacuum_vacuum_cost_limit=2000,autovacuum_vacuum_cost_delay=1);

ALTER TYPE budget_total_delta ADD ATTRIBUTE held_unknown numeric, ADD ATTRIBUTE unresolved_unknown bigint;

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
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id);
  ELSIF TG_OP='UPDATE' THEN
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,-1,0,0)::budget_total_delta
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id
    WHERE NOT EXISTS(SELECT 1 FROM old_rows o WHERE o.execution_id=r.execution_id))
   || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,1,0,0)::budget_total_delta
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id
    WHERE NOT EXISTS(SELECT 1 FROM new_rows n WHERE n.execution_id=r.execution_id));
  ELSE
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,e.started_at,0,0,0,0,0,0,1,0,0)::budget_total_delta
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id);
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
 INSERT INTO budget_totals AS t(scope_kind,scope_id,period,period_start,settled_microusd,held_microusd,reservations,pending,unknown,unresolved,unreserved_executions,held_unknown_microusd,unresolved_unknown)
 SELECT s.kind,s.id,p.period,
  CASE WHEN p.period='lifetime' THEN 'epoch'::timestamptz ELSE date_trunc(p.period,x.at,'UTC') END,
  sum(x.settled),sum(x.held),sum(x.reservations),sum(x.pending),sum(x.unknown),sum(x.unresolved),sum(x.unreserved),
  sum(x.held_unknown),sum(x.unresolved_unknown)
 FROM unnest(d) x JOIN api_keys k ON k.id=x.api_key_id
 CROSS JOIN LATERAL (VALUES('installation','00000000-0000-0000-0000-000000000000'::uuid),('workspace',x.workspace_id),('key',k.governance_key_id)) s(kind,id)
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

-- Block concurrent writers while the backfill and the new trigger body are
-- installed together; readers are unaffected.
LOCK TABLE inference_executions,governance_reservations IN SHARE MODE;

-- Exact backfill of the new columns from unknown reservations (every bucket
-- they contribute to already exists: 0015 counts them in `unknown`).
UPDATE budget_totals t SET held_unknown_microusd=u.held_unknown,unresolved_unknown=u.unresolved_unknown
FROM (
 SELECT s.kind,s.id,p.period,
  CASE WHEN p.period='lifetime' THEN 'epoch'::timestamptz ELSE date_trunc(p.period,r.admitted_at,'UTC') END period_start,
  sum(coalesce(r.held_microusd,0)) held_unknown,
  sum((r.unbounded_cost OR r.held_microusd IS NULL)::int) unresolved_unknown
 FROM governance_reservations r JOIN api_keys k ON k.id=r.api_key_id
 CROSS JOIN LATERAL (VALUES('installation','00000000-0000-0000-0000-000000000000'::uuid),('workspace',r.workspace_id),('key',k.governance_key_id)) s(kind,id)
 CROSS JOIN (VALUES('day'),('week'),('month'),('lifetime')) p(period)
 WHERE r.state='unknown'
 GROUP BY 1,2,3,4
) u WHERE t.scope_kind=u.kind AND t.scope_id=u.id AND t.period=u.period AND t.period_start=u.period_start;
