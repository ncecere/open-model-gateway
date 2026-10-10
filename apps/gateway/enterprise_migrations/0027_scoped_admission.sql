-- Scoped admission (scale plan P3).
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- Admission and settlement stop serializing on the singleton installation row.
-- Every transaction acquires a subset of one canonical lock order, which makes
-- the protocol deadlock-free:
--   1. the catalog advisory lock 72419502 (unchanged: shared for admission and
--      ordinary management, exclusive for global catalog changes);
--   2. management only: the installation row (management-versus-management
--      serialization: SCIM, sign-in grants, last-admin checks). Never taken by
--      admission or settlement in the scoped mode;
--   3. authority advisory locks, transaction-scoped, in the two-int key space
--      (class, key): workspace type 72419510, workspace 72419511, user 72419512,
--      key lineage 72419513, each ascending by key. Admission takes them
--      shared; a management change that can affect admission takes the
--      matching ones exclusively;
--   4. an existing reservation row (settlement, reconciliation, realtime
--      windows, batch envelopes), FOR UPDATE;
--   5. the budget_totals rows, then rate_minute_counters rows, then
--      inflight_counters rows that the write will change, each in primary-key
--      order (FOR NO KEY UPDATE). The 0015/0024 triggers touch exactly these
--      rows, in the same order, so they never wait once the rows are held.
--   6. new rows (execution, reservation, ledger).
-- Advisory locks live in the lock manager: no tuple writes and no MultiXacts,
-- so the hot path no longer takes FOR SHARE row locks on authority rows.
--
-- Lock keys: a uuid scope uses its first 32 bits (omg_scope_key, also computed
-- by the gateway); a workspace type uses 1 personal, 2 team, 3 project. A key
-- collision only serializes two unrelated scopes; it never weakens exclusion.
--
-- Authority triggers (below) take the matching exclusive scope lock on every
-- write that can change an admission decision, so no code path (management,
-- SCIM, sign-in, lifecycle, CLI) can forget it. The gateway additionally takes
-- the same locks up front, in canonical order, before its first write, so the
-- lazy trigger acquisition never runs out of order. When the session setting
-- omg.scope_lock_audit is 'on' (tests only), a gateway transaction (one holding
-- the catalog lock shared) that reaches such a write without having taken the
-- lock up front, or that takes scope locks out of canonical order, fails.
-- Transactions holding the catalog lock exclusively (catalog_tx(write):
-- platform directory and catalog changes) already exclude every admission.
--
-- No table, column or history row changes.

CREATE FUNCTION omg_scope_key(id uuid) RETURNS integer LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
 SELECT ('x'||substr(id::text,1,8))::bit(32)::integer
$$;

CREATE FUNCTION omg_type_key(kind text) RETURNS integer LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE AS $$
BEGIN
 RETURN CASE kind WHEN 'personal' THEN 1 WHEN 'team' THEN 2 WHEN 'project' THEN 3 END;
END $$;

-- Test-only audit (omg.scope_lock_audit='on'): no scope lock of a later class
-- may already be held when locks of `first_class` are requested.
CREATE FUNCTION omg_scope_lock_audit_order(first_class integer) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
 IF coalesce(current_setting('omg.scope_lock_audit',true),'')='on' AND EXISTS(
  SELECT 1 FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid() AND objsubid=2
   AND classid::bigint BETWEEN first_class+1 AND 72419513) THEN
  RAISE EXCEPTION 'scope locks requested out of canonical order (class %)',first_class USING ERRCODE='XX000';
 END IF;
END $$;

-- Scope locks in the given order (the gateway passes canonical order:
-- ascending class, then key); shared for admission, exclusive for changes.
CREATE FUNCTION omg_lock_scopes(classes integer[], keys integer[], exclusive boolean) RETURNS void LANGUAGE plpgsql AS $$
DECLARE i integer;
BEGIN
 IF cardinality(classes) IS DISTINCT FROM cardinality(keys) THEN
  RAISE EXCEPTION 'scope lock arrays differ';
 END IF;
 IF cardinality(classes)>0 THEN
  PERFORM omg_scope_lock_audit_order(classes[1]);
 END IF;
 FOR i IN 1..coalesce(cardinality(classes),0) LOOP
  IF classes[i] NOT BETWEEN 72419510 AND 72419513 THEN
   RAISE EXCEPTION 'unknown scope lock class %',classes[i];
  END IF;
  IF i>1 AND (classes[i],keys[i])<(classes[i-1],keys[i-1]) THEN
   RAISE EXCEPTION 'scope locks out of canonical order';
  END IF;
  IF exclusive THEN
   PERFORM pg_advisory_xact_lock(classes[i],keys[i]);
  ELSE
   PERFORM pg_advisory_xact_lock_shared(classes[i],keys[i]);
  END IF;
 END LOOP;
END $$;

-- Admission's lock prefix in one round trip: the shared catalog lock, then the
-- shared authority locks of the key's workspace type, workspace, issuing user
-- (human keys) and key lineage. Kind and lineage are immutable columns, read
-- without row locks. Unknown workspace/key: catalog lock only, null result
-- (live revalidation then refuses the key).
CREATE FUNCTION omg_admission_locks(ws uuid, usr uuid, api_key uuid, OUT lineage uuid, OUT kind text)
LANGUAGE plpgsql AS $$
BEGIN
 PERFORM pg_advisory_xact_lock_shared(72419502);
 SELECT w.kind INTO kind FROM workspaces w WHERE w.id=ws;
 SELECT k.governance_key_id INTO lineage FROM api_keys k WHERE k.id=api_key AND k.workspace_id=ws;
 IF kind IS NULL OR lineage IS NULL THEN
  lineage := NULL;
  kind := NULL;
  RETURN;
 END IF;
 PERFORM omg_scope_lock_audit_order(72419510);
 PERFORM pg_advisory_xact_lock_shared(72419510,omg_type_key(kind));
 PERFORM pg_advisory_xact_lock_shared(72419511,omg_scope_key(ws));
 IF usr IS NOT NULL THEN
  PERFORM pg_advisory_xact_lock_shared(72419512,omg_scope_key(usr));
 END IF;
 PERFORM pg_advisory_xact_lock_shared(72419513,omg_scope_key(lineage));
END $$;

-- Lock (creating zero rows where missing) every budget_totals row, and with
-- `counters` every rate_minute_counters and inflight_counters row, that a
-- write of reservations/executions of (workspace, api key, admission time)
-- will change: the workspace and key-lineage scopes x 4 periods, their minute
-- rows and in-flight rows. Tables in canonical order, rows in primary-key
-- order. Zero rows equal missing ones for every reader and `budget verify`.
-- Each statement takes a fresh snapshot, so a caller's next statement reads
-- the latest committed values of rows it now holds.
CREATE FUNCTION omg_lock_scope_rows(workspace_ids uuid[], api_key_ids uuid[], ats timestamptz[], counters boolean)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
 scope_kinds text[];
 scope_ids uuid[];
 periods text[];
 starts timestamptz[];
 minutes timestamptz[];
BEGIN
 SELECT array_agg(q.kind ORDER BY q.kind,q.id,q.period,q.bucket),array_agg(q.id ORDER BY q.kind,q.id,q.period,q.bucket),
  array_agg(q.period ORDER BY q.kind,q.id,q.period,q.bucket),array_agg(q.bucket ORDER BY q.kind,q.id,q.period,q.bucket)
 INTO scope_kinds,scope_ids,periods,starts
 FROM (SELECT DISTINCT s.kind,s.id,p.period,
   CASE WHEN p.period='lifetime' THEN 'epoch'::timestamptz ELSE date_trunc(p.period,x.at,'UTC') END AS bucket
  FROM unnest(workspace_ids,api_key_ids,ats) x(ws,key,at) JOIN api_keys k ON k.id=x.key AND k.workspace_id=x.ws
  CROSS JOIN LATERAL (VALUES('workspace',x.ws),('key',k.governance_key_id)) s(kind,id)
  CROSS JOIN (VALUES('day'),('week'),('month'),('lifetime')) p(period)) q;
 IF scope_kinds IS NULL THEN
  RETURN;
 END IF;
 INSERT INTO budget_totals(scope_kind,scope_id,period,period_start)
 SELECT * FROM unnest(scope_kinds,scope_ids,periods,starts) u ORDER BY 1,2,3,4
 ON CONFLICT DO NOTHING;
 PERFORM 1 FROM budget_totals t JOIN unnest(scope_kinds,scope_ids,periods,starts) u(kind,id,period,bucket)
  ON t.scope_kind=u.kind AND t.scope_id=u.id AND t.period=u.period AND t.period_start=u.bucket
 ORDER BY t.scope_kind,t.scope_id,t.period,t.period_start FOR NO KEY UPDATE OF t;
 IF NOT counters THEN
  RETURN;
 END IF;
 SELECT array_agg(q.mstart ORDER BY q.mstart,q.kind,q.id),array_agg(q.kind ORDER BY q.mstart,q.kind,q.id),
  array_agg(q.id ORDER BY q.mstart,q.kind,q.id)
 INTO minutes,scope_kinds,scope_ids
 FROM (SELECT DISTINCT date_trunc('minute',x.at,'UTC') AS mstart,s.kind,s.id
  FROM unnest(workspace_ids,api_key_ids,ats) x(ws,key,at) JOIN api_keys k ON k.id=x.key AND k.workspace_id=x.ws
  CROSS JOIN LATERAL (VALUES('workspace',x.ws),('key',k.governance_key_id)) s(kind,id)) q;
 INSERT INTO rate_minute_counters(minute_start,scope_kind,scope_id)
 SELECT * FROM unnest(minutes,scope_kinds,scope_ids) u ORDER BY 1,2,3
 ON CONFLICT DO NOTHING;
 PERFORM 1 FROM rate_minute_counters t JOIN unnest(minutes,scope_kinds,scope_ids) u(mstart,kind,id)
  ON t.minute_start=u.mstart AND t.scope_kind=u.kind AND t.scope_id=u.id
 ORDER BY t.minute_start,t.scope_kind,t.scope_id FOR NO KEY UPDATE OF t;
 SELECT array_agg(q.kind ORDER BY q.kind,q.id),array_agg(q.id ORDER BY q.kind,q.id)
 INTO scope_kinds,scope_ids
 FROM (SELECT DISTINCT s.kind,s.id
  FROM unnest(workspace_ids,api_key_ids) x(ws,key) JOIN api_keys k ON k.id=x.key AND k.workspace_id=x.ws
  CROSS JOIN LATERAL (VALUES('workspace',x.ws),('key',k.governance_key_id)) s(kind,id)) q;
 INSERT INTO inflight_counters(scope_kind,scope_id)
 SELECT * FROM unnest(scope_kinds,scope_ids) u ORDER BY 1,2
 ON CONFLICT DO NOTHING;
 PERFORM 1 FROM inflight_counters t JOIN unnest(scope_kinds,scope_ids) u(kind,id)
  ON t.scope_kind=u.kind AND t.scope_id=u.id
 ORDER BY t.scope_kind,t.scope_id FOR NO KEY UPDATE OF t;
END $$;

-- This transaction's catalog lock: 'exclusive', 'shared' or 'none'.
CREATE FUNCTION omg_catalog_lock_mode() RETURNS text LANGUAGE sql STABLE AS $$
 SELECT coalesce((SELECT CASE WHEN bool_or(mode='ExclusiveLock') THEN 'exclusive' ELSE 'shared' END
  FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid() AND granted
   AND classid=0::oid AND objid=72419502::oid AND objsubid=1 HAVING count(*)>0),'none')
$$;

-- Exclusive scope lock taken by the authority triggers. Audit mode (tests):
-- a gateway transaction must already hold it (taken up front in canonical
-- order), so lazy acquisition can never run out of order.
CREATE FUNCTION omg_scope_lock_exclusive(class integer, key integer) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
 IF key IS NULL THEN
  RETURN;
 END IF;
 -- Audited: a gateway transaction holding the catalog lock shared. Under the
 -- exclusive catalog lock no admission (nor any other holder of scope locks,
 -- all of which take the catalog lock shared first) can run, so lazy
 -- acquisition cannot run out of order there.
 IF coalesce(current_setting('omg.scope_lock_audit',true),'')='on'
  AND omg_catalog_lock_mode()='shared'
  AND NOT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid()
   AND classid=class::oid AND objid=key::oid AND objsubid=2 AND mode='ExclusiveLock' AND granted) THEN
  RAISE EXCEPTION 'authority change without its scope lock (class %, key %) taken up front',class,key USING ERRCODE='XX000';
 END IF;
 PERFORM pg_advisory_xact_lock(class,key);
END $$;

-- Exclusive catalog lock for global catalog writes (catalog_tx(write) already
-- holds it; audit mode refuses a gateway transaction that does not).
CREATE FUNCTION omg_catalog_lock_exclusive() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF coalesce(current_setting('omg.scope_lock_audit',true),'')='on' AND omg_catalog_lock_mode()='shared' THEN
  RAISE EXCEPTION 'catalog change without the exclusive catalog lock' USING ERRCODE='XX000';
 END IF;
 PERFORM pg_advisory_xact_lock(72419502);
 RETURN NULL;
END $$;

-- One authority trigger function; TG_ARGV[0] names the scope:
--   'user' (column user_id, or id on users), 'workspace' (workspace_id, or id
--   on workspaces), 'lineage' (governance_key_id), 'type' (kind), or 'budget'
--   (policy_budgets: by layer). Old and new rows are both locked.
CREATE FUNCTION omg_authority_lock() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
 r jsonb;
 rows jsonb[] := '{}';
 col text := coalesce(TG_ARGV[1],'');
BEGIN
 IF TG_OP IN ('UPDATE','DELETE') THEN
  rows := rows || to_jsonb(OLD);
 END IF;
 IF TG_OP IN ('INSERT','UPDATE') THEN
  rows := rows || to_jsonb(NEW);
 END IF;
 FOREACH r IN ARRAY rows LOOP
  IF TG_ARGV[0]='type' THEN
   PERFORM omg_scope_lock_exclusive(72419510,omg_type_key(r->>'kind'));
  ELSIF TG_ARGV[0]='workspace' THEN
   PERFORM omg_scope_lock_exclusive(72419511,omg_scope_key((r->>col)::uuid));
  ELSIF TG_ARGV[0]='user' THEN
   PERFORM omg_scope_lock_exclusive(72419512,omg_scope_key((r->>col)::uuid));
  ELSIF TG_ARGV[0]='lineage' THEN
   PERFORM omg_scope_lock_exclusive(72419513,omg_scope_key((r->>col)::uuid));
  ELSIF TG_ARGV[0]='budget' THEN
   IF r->>'layer'='type' THEN
    PERFORM omg_scope_lock_exclusive(72419510,omg_type_key(r->>'kind'));
   ELSIF r->>'layer' IN ('override','local') THEN
    PERFORM omg_scope_lock_exclusive(72419511,omg_scope_key((r->>'workspace_id')::uuid));
   ELSIF r->>'layer'='key' THEN
    PERFORM omg_scope_lock_exclusive(72419513,omg_scope_key((r->>'governance_key_id')::uuid));
   END IF;
  ELSE
   RAISE EXCEPTION 'unknown authority scope %',TG_ARGV[0];
  END IF;
 END LOOP;
 IF TG_OP='DELETE' THEN
  RETURN OLD;
 END IF;
 RETURN NEW;
END $$;

-- Authorization (live revalidation inputs). Grants and insertions only widen
-- access, so a race resolves to "admitted before the grant"; revocations,
-- disables and owner changes lock.
CREATE TRIGGER omg_authority_users BEFORE UPDATE OF disabled_at,cleaned_at ON users
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('user','id');
CREATE TRIGGER omg_authority_platform_grants BEFORE UPDATE OF revoked_at,user_id,role OR DELETE ON platform_role_grants
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('user','user_id');
CREATE TRIGGER omg_authority_memberships BEFORE UPDATE OF revoked_at,user_id,workspace_id OR DELETE ON workspace_membership_grants
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('user','user_id');
CREATE TRIGGER omg_authority_workspaces BEFORE UPDATE OF disabled_at,owner_user_id,cost_center_id,kind ON workspaces
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('workspace','id');
CREATE TRIGGER omg_authority_service_accounts BEFORE UPDATE OF disabled_at,workspace_id OR DELETE ON service_accounts
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('workspace','workspace_id');
CREATE TRIGGER omg_authority_api_keys BEFORE UPDATE OF revoked_at,disabled_at,expires_at,governance_key_id,workspace_id,issued_to_user_id,service_account_id OR DELETE ON api_keys
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('lineage','governance_key_id');
-- Entitlement (workspace_model_allowed, key restrictions) and limits
-- (policies, budgets): every change locks, in either direction.
CREATE TRIGGER omg_authority_key_restrictions BEFORE INSERT OR UPDATE OR DELETE ON key_model_restrictions
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('lineage','governance_key_id');
CREATE TRIGGER omg_authority_key_selections BEFORE INSERT OR UPDATE OR DELETE ON key_model_selections
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('lineage','governance_key_id');
CREATE TRIGGER omg_authority_key_policies BEFORE INSERT OR UPDATE OR DELETE ON key_policies
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('lineage','governance_key_id');
CREATE TRIGGER omg_authority_policy_budgets BEFORE INSERT OR UPDATE OR DELETE ON policy_budgets
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('budget');
CREATE TRIGGER omg_authority_overrides BEFORE INSERT OR UPDATE OR DELETE ON workspace_platform_policy_overrides
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('workspace','workspace_id');
CREATE TRIGGER omg_authority_local_policies BEFORE INSERT OR UPDATE OR DELETE ON workspace_local_policies
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('workspace','workspace_id');
CREATE TRIGGER omg_authority_type_policies BEFORE INSERT OR DELETE OR UPDATE OF kind,requests_per_minute,tokens_per_minute,concurrent_requests,concurrent_jobs ON workspace_type_policies
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('type');
CREATE TRIGGER omg_authority_type_catalogs BEFORE INSERT OR UPDATE OR DELETE ON workspace_type_catalogs
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('type');
CREATE TRIGGER omg_authority_catalog_overrides BEFORE INSERT OR UPDATE OR DELETE ON workspace_catalog_overrides
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('workspace','workspace_id');
CREATE TRIGGER omg_authority_catalog_override_items BEFORE INSERT OR UPDATE OR DELETE ON workspace_catalog_override_items
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('workspace','workspace_id');
CREATE TRIGGER omg_authority_model_grants BEFORE INSERT OR UPDATE OR DELETE ON workspace_model_grants
 FOR EACH ROW EXECUTE FUNCTION omg_authority_lock('workspace','workspace_id');
-- Global catalog: the exclusive catalog lock (statement level).
CREATE TRIGGER omg_catalog_catalogs BEFORE INSERT OR UPDATE OR DELETE ON catalogs
 FOR EACH STATEMENT EXECUTE FUNCTION omg_catalog_lock_exclusive();
CREATE TRIGGER omg_catalog_catalog_models BEFORE INSERT OR UPDATE OR DELETE ON catalog_models
 FOR EACH STATEMENT EXECUTE FUNCTION omg_catalog_lock_exclusive();
CREATE TRIGGER omg_catalog_models BEFORE INSERT OR UPDATE OR DELETE ON models
 FOR EACH STATEMENT EXECUTE FUNCTION omg_catalog_lock_exclusive();
CREATE TRIGGER omg_catalog_deployments BEFORE INSERT OR UPDATE OR DELETE ON deployments
 FOR EACH STATEMENT EXECUTE FUNCTION omg_catalog_lock_exclusive();
CREATE TRIGGER omg_catalog_provider_connections BEFORE INSERT OR UPDATE OR DELETE ON provider_connections
 FOR EACH STATEMENT EXECUTE FUNCTION omg_catalog_lock_exclusive();
CREATE TRIGGER omg_catalog_deployment_prices BEFORE INSERT ON deployment_prices
 FOR EACH STATEMENT EXECUTE FUNCTION omg_catalog_lock_exclusive();
