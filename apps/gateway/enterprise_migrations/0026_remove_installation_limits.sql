-- Remove the installation-wide policy layer (decision of 2026-10-09).
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- There are no installation-wide limits any more: no installation budget (any
-- period) and no installation requests/tokens per minute, requests at once or
-- jobs at once. Limits exist only on personal/team/project workspaces
-- (workspace-type defaults, platform per-workspace overrides, tighten-only
-- workspace local) and key lineages, all still hard limits. Installation-wide
-- spend stays visible through a non-blocking alert rule kind,
-- `spend_threshold` (Admin > Settings > Alerts).
--
-- What this migration changes:
--  1. The removed configuration (installation rate limits and budgets) is
--     recorded once in the audit log (`policy.installation_removed`), so the
--     values an operator had set stay readable.
--  2. Alert rules: every live installation `budget_threshold` rule that watched
--     the `installation` layer keeps its thresholds and recipients and becomes
--     an installation spend rule per former installation budget (period,
--     amount = the reference amount the percentages apply to). A rule that
--     watched only that layer is converted in place (same id and history) for
--     its shortest budget period; further periods, and the installation part of
--     rules that also watch other layers, become new `spend_threshold` rules
--     with deterministic ids (re-running inserts nothing twice). A rule that
--     watched only the installation layer while no installation budget existed
--     could never fire: it is retired (soft-deleted, history kept). Each change
--     is written to the audit log (`alert_rule.installation_layer_removed`).
--     Open installation-budget incidents that no rule continues are resolved
--     as `superseded` (no "resolved" email); an in-place rule keeps its open
--     incident (same subject), so nothing fires twice.
--  3. `policy_budgets` installation rows are deleted and the layer becomes
--     unrepresentable; the `installation_policy` table (rate/concurrency/job
--     limits only) is dropped. Policy rows are configuration, not history.
--  4. `budget_totals` stops maintaining the installation scope: its rows were
--     the single hot row every admission and settlement updated. They are
--     derived data (trigger-maintained aggregates of reservations and
--     executions that `budget verify` recomputes from a full scan), and each
--     installation row equals the sum of the workspace rows of the same period
--     and period start exactly (every reservation/execution has exactly one
--     workspace), so they are deleted rather than kept stale. Installation
--     spend is now that sum (index below). Reservations, executions, the
--     monetary ledger, prices, audit events and alert incidents are not
--     modified (immutable history), except that open incidents are resolved
--     as described in 2.
-- The installation singleton row stays (settings, branding, catalog lock and
-- management serialization use it).

-- Freeze writers: history writes (trigger replacement and totals cleanup are
-- atomic with them) and policy/alert configuration writes. Reads continue.
LOCK TABLE inference_executions,governance_reservations,budget_totals IN SHARE MODE;
LOCK TABLE installation_policy,policy_budgets,alert_rules,alert_events IN EXCLUSIVE MODE;

-- 1. Audit record of the removed installation configuration (only if any was set).
INSERT INTO audit_events(id,action,resource_type,resource_id,metadata)
SELECT gen_random_uuid(),'policy.installation_removed','installation',i.id,
 jsonb_build_object('migration','0026',
  'requests_per_minute',p.requests_per_minute,'tokens_per_minute',p.tokens_per_minute,
  'concurrent_requests',p.concurrent_requests,'concurrent_jobs',p.concurrent_jobs,
  'budgets',coalesce((SELECT jsonb_agg(jsonb_build_object('period',b.period,'amount_microusd',b.amount_microusd::text)
    ORDER BY array_position(ARRAY['day','week','month','lifetime'],b.period))
   FROM policy_budgets b WHERE b.layer='installation'),'[]'::jsonb))
FROM installation i LEFT JOIN installation_policy p ON p.singleton
WHERE i.singleton AND (EXISTS(SELECT 1 FROM policy_budgets WHERE layer='installation')
 OR p.requests_per_minute IS NOT NULL OR p.tokens_per_minute IS NOT NULL
 OR p.concurrent_requests IS NOT NULL OR p.concurrent_jobs IS NOT NULL);

-- 2. Alerts: the non-blocking installation spend threshold rule kind.
ALTER TABLE alert_rules
 ADD COLUMN spend_period text CHECK(spend_period IN ('day','week','month','lifetime')),
 ADD COLUMN spend_amount_microusd bigint CHECK(spend_amount_microusd>=1);
ALTER TABLE alert_rules DROP CONSTRAINT alert_rules_kind_check;
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_kind_check
 CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing','batch_failed','batch_stalled','spend_threshold'));
ALTER TABLE alert_events DROP CONSTRAINT alert_events_kind_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_kind_check
 CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing','scim_last_admin','batch_failed','batch_stalled','spend_threshold'));

-- Installation budgets that can serve as a reference amount (a zero budget
-- denied everything and never alerted).
CREATE TEMP TABLE m0026_budgets ON COMMIT DROP AS
 SELECT period,amount_microusd,array_position(ARRAY['day','week','month','lifetime'],period) ord,
  CASE period WHEN 'day' THEN 'daily' WHEN 'week' THEN 'weekly' WHEN 'month' THEN 'monthly' ELSE 'lifetime' END word
 FROM policy_budgets WHERE layer='installation' AND amount_microusd>0;
-- Live rules watching the installation layer, and how each is handled:
-- 'in_place' (installation only, a budget exists), 'retire' (installation
-- only, no budget) or 'strip' (other layers remain).
CREATE TEMP TABLE m0026_rules ON COMMIT DROP AS
 SELECT r.id,CASE WHEN array_remove(r.budget_layers,'installation')<>'{}' THEN 'strip'
   WHEN EXISTS(SELECT 1 FROM m0026_budgets) THEN 'in_place' ELSE 'retire' END mode,
  (SELECT b.period FROM m0026_budgets b ORDER BY b.ord LIMIT 1) kept_period
 FROM alert_rules r
 WHERE r.deleted_at IS NULL AND r.kind='budget_threshold' AND 'installation'=ANY(r.budget_layers);

-- New spend rules: every reference budget of 'strip' rules, and the periods
-- after the first for 'in_place' rules.
INSERT INTO alert_rules(id,scope,kind,name,enabled,thresholds,spend_period,spend_amount_microusd,
 notify_workspace_admins,notify_platform_admins,notify_emails,created_by,created_at,updated_by,updated_at)
SELECT md5('0026:'||r.id::text||':'||b.period)::uuid,'installation','spend_threshold',
 left(r.name,90)||' (installation '||b.word||' spend)',r.enabled,r.thresholds,b.period,b.amount_microusd,
 r.notify_workspace_admins,r.notify_platform_admins,r.notify_emails,r.created_by,now(),r.updated_by,now()
FROM m0026_rules m JOIN alert_rules r ON r.id=m.id CROSS JOIN m0026_budgets b
WHERE m.mode='strip' OR (m.mode='in_place' AND b.period<>m.kept_period)
ON CONFLICT(id) DO NOTHING;

-- Open installation-budget incidents that no rule continues.
UPDATE alert_events e SET resolved_at=clock_timestamp(),resolution='superseded'
FROM m0026_rules m
WHERE e.rule_id=m.id AND e.resolved_at IS NULL AND e.subject_key LIKE 'installation:%'
 AND NOT (m.mode='in_place' AND e.subject_key='installation:'||m.kept_period);
UPDATE alert_events e SET resolved_at=clock_timestamp(),resolution='rule_disabled'
FROM m0026_rules m WHERE e.rule_id=m.id AND e.resolved_at IS NULL AND m.mode='retire';

-- Audit every changed rule (before the rules themselves change).
INSERT INTO audit_events(id,action,resource_type,resource_id,metadata)
SELECT gen_random_uuid(),'alert_rule.installation_layer_removed','alert_rule',m.id,
 jsonb_build_object('migration','0026','handling',m.mode,
  'spend_periods',coalesce((SELECT jsonb_agg(b.period ORDER BY b.ord) FROM m0026_budgets b WHERE m.mode<>'retire'),'[]'::jsonb),
  'spend_rule_ids',coalesce((SELECT jsonb_agg(md5('0026:'||m.id::text||':'||b.period)::uuid ORDER BY b.ord)
    FROM m0026_budgets b WHERE m.mode='strip' OR (m.mode='in_place' AND b.period<>m.kept_period)),'[]'::jsonb))
FROM m0026_rules m;

UPDATE alert_rules r SET budget_layers=array_remove(r.budget_layers,'installation'),updated_at=now()
FROM m0026_rules m WHERE r.id=m.id AND m.mode='strip';
UPDATE alert_rules r SET kind='spend_threshold',budget_layers=NULL,spend_period=b.period,
 spend_amount_microusd=b.amount_microusd,updated_at=now()
FROM m0026_rules m JOIN m0026_budgets b ON b.period=m.kept_period WHERE r.id=m.id AND m.mode='in_place';
UPDATE alert_rules r SET enabled=false,deleted_at=now(),updated_at=now()
FROM m0026_rules m WHERE r.id=m.id AND m.mode='retire';

-- Shapes: spend rules are installation-only, percent thresholds of a
-- reference amount per period. Live rules can no longer watch the
-- installation layer (soft-deleted rules keep their stored configuration).
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_spend_shape
 CHECK((kind='spend_threshold') = (spend_period IS NOT NULL AND spend_amount_microusd IS NOT NULL));
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_spend_scope
 CHECK(kind<>'spend_threshold' OR (scope='installation' AND thresholds IS NOT NULL AND budget_layers IS NULL));
ALTER TABLE alert_rules ADD CONSTRAINT alert_rules_no_installation_layer
 CHECK(deleted_at IS NOT NULL OR NOT coalesce('installation'=ANY(budget_layers),false));

-- 3. Policy layers: no installation budgets or limits.
DELETE FROM policy_budgets WHERE layer='installation';
ALTER TABLE policy_budgets DROP CONSTRAINT policy_budgets_layer_check;
ALTER TABLE policy_budgets ADD CONSTRAINT policy_budgets_layer_check CHECK(layer IN ('type','override','local','key'));
DROP TABLE installation_policy;

-- 4. Budget totals: workspace and key lineage scopes only.
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

DELETE FROM budget_totals WHERE scope_kind='installation';
ALTER TABLE budget_totals DROP CONSTRAINT budget_totals_check;
ALTER TABLE budget_totals DROP CONSTRAINT budget_totals_scope_kind_check;
ALTER TABLE budget_totals ADD CONSTRAINT budget_totals_scope_kind_check CHECK(scope_kind IN ('workspace','key'));
-- Installation spend (alerts, the reservation gauge) sums the workspace rows
-- of one period window. Only the bucket key is indexed (never updated), so
-- counter updates stay HOT.
CREATE INDEX budget_totals_installation_spend ON budget_totals(period,period_start) WHERE scope_kind='workspace';
