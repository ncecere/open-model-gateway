-- no-transaction
-- Monthly range partitioning of request history (scale plan P6).
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- inference_executions (by started_at), governance_reservations (by
-- admitted_at) and monetary_ledger (by admitted_at, a new column copied from
-- the reservation) become partitioned tables with one partition per UTC
-- month. Existing rows are not rewritten: each existing executions and
-- reservations table is attached as the "legacy" partition FOR VALUES FROM
-- (MINVALUE) TO (cutover), where cutover is the first month start after the
-- newest existing row (recorded in history_partitions.legacy_upper):
--   1. the new keys (which must include the partition key) are built as
--      unique indexes on the existing tables (SHARE lock: writes wait, reads
--      continue), plus CHECK (key < cutover) NOT VALID;
--   2. VALIDATE CONSTRAINT scans under SHARE UPDATE EXCLUSIVE (reads and
--      writes continue);
--   3. a short transaction renames the table to <table>_p_legacy, creates
--      the partitioned parent with the same columns, checks, indexes,
--      foreign keys and triggers, and ATTACHes the legacy table: the
--      validated CHECK proves the bound and the prebuilt indexes and foreign
--      keys are adopted, so nothing is scanned or rebuilt under the
--      exclusive lock.
-- The ledger gains admitted_at, so it is copied once into its partitioned
-- replacement while the old ledger is SHARE locked (writes wait, reads
-- continue); counts, sums and a per-row hash are compared before the old
-- table is dropped at the end of that transaction.
--
-- Each numbered step below is its own transaction and is idempotent, so a
-- failed or interrupted upgrade is resumed by running `migrate` again (SQLx
-- records this migration only after every step succeeded). Run it drained,
-- like every migration (readiness requires the exact lineage). Operators of
-- large installations can prebuild step 1's indexes online first with
-- `open-model-gateway partitions prepare` (CREATE INDEX CONCURRENTLY).
--
-- Keys: executions (id, started_at); reservations (execution_id,
-- admitted_at); ledger (id, admitted_at). Former single-column uniqueness
-- (execution id, reservation execution id, ledger id, (root request,
-- attempt)) now includes the partition key; ids are generated fresh (UUIDv7
-- for new rows) and attempts are numbered by the engine, so global
-- uniqueness holds by construction. The reservation -> execution foreign key
-- now also matches admitted_at = started_at, which admission always wrote.
-- The ledger -> reservation foreign key matches (execution_id, admitted_at).
-- async_jobs -> executions and realtime_responses -> reservations become
-- insert-time existence checks (triggers): those rows are never deleted, and
-- archived months stay referenced by id.
--
-- Future months: omg_ensure_partitions(ahead) creates the missing month
-- partitions (CREATE TABLE ... LIKE, then ATTACH: SHARE UPDATE EXCLUSIVE on
-- the parent, never ACCESS EXCLUSIVE) with a TRUNCATE guard each. It is
-- SECURITY DEFINER and creates nothing but canonical month partitions of
-- registered parents; the runtime calls the one-argument form from the
-- leased `partitions` job, operators `open-model-gateway partitions ensure`.
-- There is no default partition: a row outside every partition fails its
-- write (admission fails closed) and the partitions_missing alert fires long
-- before that (3 months ahead by default; alert below 2).
SET lock_timeout = '60s';

-- Step 0: session helpers (pg_temp: gone with the migration connection).
CREATE OR REPLACE FUNCTION pg_temp.p6_kind(rel text) RETURNS "char" LANGUAGE sql STABLE AS $$
 SELECT c.relkind FROM pg_class c WHERE c.relnamespace='public'::regnamespace AND c.relname=rel
$$;
CREATE OR REPLACE FUNCTION pg_temp.p6_has_constraint(rel text, con text) RETURNS boolean LANGUAGE sql STABLE AS $$
 SELECT EXISTS(SELECT 1 FROM pg_constraint k JOIN pg_class c ON c.oid=k.conrelid
  WHERE c.relnamespace='public'::regnamespace AND c.relname=rel AND k.conname=con)
$$;
-- Build `name` with `ddl` unless a valid index of that name exists (an
-- invalid one, e.g. from an interrupted CREATE INDEX CONCURRENTLY, is rebuilt).
CREATE OR REPLACE FUNCTION pg_temp.p6_index(name text, ddl text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE valid boolean;
BEGIN
 SELECT i.indisvalid INTO valid FROM pg_class c JOIN pg_index i ON i.indexrelid=c.oid
  WHERE c.relnamespace='public'::regnamespace AND c.relname=name;
 IF valid THEN
  RETURN;
 ELSIF valid IS NOT NULL THEN
  EXECUTE format('DROP INDEX public.%I', name);
 END IF;
 EXECUTE ddl;
END $$;
CREATE OR REPLACE FUNCTION pg_temp.p6_rename_index(old text, new text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
 IF EXISTS(SELECT 1 FROM pg_class WHERE relnamespace='public'::regnamespace AND relname=old AND relkind='i') THEN
  EXECUTE format('ALTER INDEX public.%I RENAME TO %I', old, new);
 END IF;
END $$;
CREATE OR REPLACE FUNCTION pg_temp.p6_cutover() RETURNS timestamptz LANGUAGE plpgsql STABLE AS $$
BEGIN
 RETURN (SELECT legacy_upper FROM public.history_partitions WHERE parent='inference_executions');
END $$;

-- Step 1: registry, partition helpers, preconditions, reference checks.
BEGIN;
CREATE TABLE IF NOT EXISTS history_partitions(
 parent text PRIMARY KEY CHECK(parent ~ '^[a-z][a-z_]{0,40}$'),
 position smallint NOT NULL UNIQUE,
 partition_key text NOT NULL,
 -- Partitions archived together (`archive partition <group> <month>`).
 archive_group text NOT NULL CHECK(archive_group IN ('history','audit','storage')),
 -- Rows before this month start live in <parent>_p_legacy.
 legacy_upper timestamptz NOT NULL CHECK(legacy_upper=date_trunc('month',legacy_upper,'UTC')),
 converted_at timestamptz
);
CREATE OR REPLACE FUNCTION history_partitions_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP<>'UPDATE' OR NEW.parent<>OLD.parent OR NEW.legacy_upper<>OLD.legacy_upper
  OR NEW.partition_key<>OLD.partition_key OR NEW.archive_group<>OLD.archive_group THEN
  RAISE EXCEPTION 'history partition registry rows are fixed' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS history_partitions_fixed ON history_partitions;
CREATE TRIGGER history_partitions_fixed BEFORE UPDATE OR DELETE ON history_partitions
 FOR EACH ROW EXECUTE FUNCTION history_partitions_guard();
DO $$
DECLARE n bigint;
BEGIN
 IF pg_temp.p6_kind('governance_reservations')='r' AND pg_temp.p6_kind('inference_executions')='r' THEN
  SELECT count(*) INTO n FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id
   WHERE e.started_at<>r.admitted_at;
  IF n>0 THEN
   RAISE EXCEPTION '0030: % reservation(s) record an admission time different from their execution start; nothing was changed', n;
  END IF;
 END IF;
END $$;
INSERT INTO history_partitions(parent,position,partition_key,archive_group,legacy_upper)
SELECT v.parent,v.position,v.partition_key,'history',
 date_trunc('month',date_trunc('month',l.at,'UTC')+interval '960 hours','UTC')
FROM (VALUES('inference_executions',1,'started_at'),('governance_reservations',2,'admitted_at'),
  ('monetary_ledger',3,'admitted_at')) v(parent,position,partition_key)
CROSS JOIN (SELECT greatest(clock_timestamp(),(SELECT max(started_at) FROM inference_executions),
  (SELECT max(admitted_at) FROM governance_reservations)) at) l
ON CONFLICT DO NOTHING;

-- UTC month helpers (hour arithmetic is time-zone independent).
CREATE OR REPLACE FUNCTION omg_month_start(at timestamptz) RETURNS timestamptz LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
 SELECT date_trunc('month',at,'UTC')
$$;
CREATE OR REPLACE FUNCTION omg_next_month(at timestamptz) RETURNS timestamptz LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
 SELECT date_trunc('month',date_trunc('month',at,'UTC')+interval '960 hours','UTC')
$$;
-- Range of every partition of `parent` (lower '-infinity' for MINVALUE).
CREATE OR REPLACE FUNCTION omg_partition_bounds(parent text)
RETURNS TABLE(partition text, lower_bound timestamptz, upper_bound timestamptz) LANGUAGE plpgsql STABLE AS $$
DECLARE r record; m text[];
BEGIN
 FOR r IN SELECT c.relname,pg_get_expr(c.relpartbound,c.oid) b FROM pg_inherits h
   JOIN pg_class c ON c.oid=h.inhrelid JOIN pg_class p ON p.oid=h.inhparent
   WHERE p.relnamespace='public'::regnamespace AND p.relname=parent ORDER BY c.relname LOOP
  m := regexp_match(r.b,'^FOR VALUES FROM \((.+)\) TO \((.+)\)$');
  CONTINUE WHEN m IS NULL;
  partition := r.relname;
  lower_bound := CASE WHEN m[1]='MINVALUE' THEN '-infinity'::timestamptz ELSE btrim(m[1],'''')::timestamptz END;
  upper_bound := CASE WHEN m[2]='MAXVALUE' THEN 'infinity'::timestamptz ELSE btrim(m[2],'''')::timestamptz END;
  RETURN NEXT;
 END LOOP;
END $$;
-- Per registered partitioned parent: the end of the contiguous partition
-- coverage starting at the month of `at` (that month start = not covered).
CREATE OR REPLACE FUNCTION omg_partition_coverage(at timestamptz DEFAULT clock_timestamp())
RETURNS TABLE(parent text, covered_until timestamptz) LANGUAGE plpgsql STABLE AS $$
DECLARE h record; m timestamptz; up timestamptz;
BEGIN
 FOR h IN SELECT p.parent FROM public.history_partitions p ORDER BY p.position LOOP
  CONTINUE WHEN (SELECT c.relkind FROM pg_class c WHERE c.relnamespace='public'::regnamespace AND c.relname=h.parent) IS DISTINCT FROM 'p';
  m := omg_month_start(at);
  LOOP
   SELECT max(b.upper_bound) INTO up FROM omg_partition_bounds(h.parent) b
    WHERE b.lower_bound<=m AND b.upper_bound>m;
   EXIT WHEN up IS NULL OR up>='infinity';
   m := up;
   EXIT WHEN m>omg_month_start(at)+interval '87600 hours';
  END LOOP;
  parent := h.parent;
  covered_until := coalesce(up,m);
  RETURN NEXT;
 END LOOP;
END $$;
-- Create every missing month partition from the month of `at` through
-- `ahead` months later (0..12) for every registered partitioned parent:
-- CREATE TABLE ... (LIKE parent) then ATTACH, plus a TRUNCATE guard.
-- Returns the partitions created. Owner only (operators and tests); the
-- runtime calls the one-argument form, which always uses the database clock.
CREATE OR REPLACE FUNCTION omg_ensure_partitions(ahead integer, at timestamptz)
RETURNS TABLE(parent text, partition text) LANGUAGE plpgsql SECURITY DEFINER
SET search_path=pg_catalog,public,pg_temp SET lock_timeout='5s' AS $$
DECLARE h record; m timestamptz; last timestamptz; name text;
BEGIN
 IF ahead IS NULL OR ahead NOT BETWEEN 0 AND 12 OR at IS NULL THEN
  RAISE EXCEPTION 'partitions are created 0..12 months ahead' USING ERRCODE='22023';
 END IF;
 last := omg_month_start(at);
 FOR i IN 1..ahead LOOP
  last := omg_next_month(last);
 END LOOP;
 FOR h IN SELECT p.parent FROM public.history_partitions p ORDER BY p.position LOOP
  CONTINUE WHEN (SELECT c.relkind FROM pg_class c WHERE c.relnamespace='public'::regnamespace AND c.relname=h.parent) IS DISTINCT FROM 'p';
  m := omg_month_start(at);
  WHILE m<=last LOOP
   IF NOT EXISTS(SELECT 1 FROM omg_partition_bounds(h.parent) b WHERE b.lower_bound<=m AND b.upper_bound>=omg_next_month(m)) THEN
    IF EXISTS(SELECT 1 FROM omg_partition_bounds(h.parent) b WHERE b.lower_bound<omg_next_month(m) AND b.upper_bound>m) THEN
     RAISE EXCEPTION 'partition of % overlaps month % only partly; repair manually', h.parent, m USING ERRCODE='42P17';
    END IF;
    name := h.parent||'_p'||to_char(m AT TIME ZONE 'UTC','YYYY_MM');
    EXECUTE format('CREATE TABLE public.%I (LIKE public.%I INCLUDING DEFAULTS INCLUDING CONSTRAINTS INCLUDING GENERATED INCLUDING STORAGE)', name, h.parent);
    EXECUTE format('ALTER TABLE public.%I ATTACH PARTITION public.%I FOR VALUES FROM (%L) TO (%L)', h.parent, name, m, omg_next_month(m));
    EXECUTE format('CREATE TRIGGER %I BEFORE TRUNCATE ON public.%I FOR EACH STATEMENT EXECUTE FUNCTION public.immutable_history()', name||'_no_truncate', name);
    parent := h.parent;
    partition := name;
    RETURN NEXT;
   END IF;
   m := omg_next_month(m);
  END LOOP;
 END LOOP;
END $$;
CREATE OR REPLACE FUNCTION omg_ensure_partitions(ahead integer)
RETURNS TABLE(parent text, partition text) LANGUAGE sql SECURITY DEFINER
SET search_path=pg_catalog,public,pg_temp AS $$
 SELECT * FROM public.omg_ensure_partitions(ahead,clock_timestamp())
$$;
REVOKE ALL ON FUNCTION omg_ensure_partitions(integer,timestamptz) FROM PUBLIC;
REVOKE ALL ON FUNCTION omg_ensure_partitions(integer) FROM PUBLIC;

-- An UPDATE that changes a partition key so the row would move to another
-- month partition is refused: the move would run as DELETE + INSERT, which
-- the totals/counter UPDATE triggers never see. (The runtime cannot update
-- these columns at all; admission times are immutable snapshots.)
-- TG_ARGV: key column, parent table.
CREATE OR REPLACE FUNCTION omg_partition_key_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE k timestamptz := (to_jsonb(NEW)->>TG_ARGV[0])::timestamptz; lo timestamptz; hi timestamptz;
BEGIN
 SELECT b.lower_bound,b.upper_bound INTO lo,hi FROM omg_partition_bounds(TG_ARGV[1]) b WHERE b.partition=TG_TABLE_NAME;
 IF lo IS NULL OR k<lo OR k>=hi THEN
  RAISE EXCEPTION 'history rows never move between month partitions' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;

-- Existence checks replacing the async_jobs -> executions and
-- realtime_responses -> reservations foreign keys (parents are never
-- deleted; only an explicit archive detaches whole months).
CREATE OR REPLACE FUNCTION omg_history_reference() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_TABLE_NAME='async_jobs' THEN
  IF NOT EXISTS(SELECT 1 FROM inference_executions e WHERE e.id=NEW.execution_id AND e.workspace_id=NEW.workspace_id
   AND e.api_key_id=NEW.api_key_id AND e.deployment_id=NEW.deployment_id) THEN
   RAISE EXCEPTION 'async job execution does not exist' USING ERRCODE='23503';
  END IF;
 ELSIF NOT EXISTS(SELECT 1 FROM governance_reservations r WHERE r.execution_id=NEW.execution_id) THEN
  RAISE EXCEPTION 'realtime response reservation does not exist' USING ERRCODE='23503';
 END IF;
 RETURN NEW;
END $$;
ALTER TABLE async_jobs DROP CONSTRAINT IF EXISTS async_jobs_workspace_id_api_key_id_deployment_id_execution_fkey;
ALTER TABLE realtime_responses DROP CONSTRAINT IF EXISTS realtime_responses_execution_id_fkey;
DROP TRIGGER IF EXISTS async_jobs_execution_exists ON async_jobs;
CREATE TRIGGER async_jobs_execution_exists BEFORE INSERT OR UPDATE OF workspace_id,api_key_id,deployment_id,execution_id ON async_jobs
 FOR EACH ROW EXECUTE FUNCTION omg_history_reference();
DROP TRIGGER IF EXISTS realtime_responses_reservation_exists ON realtime_responses;
CREATE TRIGGER realtime_responses_reservation_exists BEFORE INSERT OR UPDATE OF execution_id ON realtime_responses
 FOR EACH ROW EXECUTE FUNCTION omg_history_reference();

-- Row triggers fire on partitions with the partition's TG_TABLE_NAME; the
-- execution row trigger passes its parent's name instead (otherwise
-- unchanged from 0024).
CREATE OR REPLACE FUNCTION rate_counters_maintain() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE d rate_counter_delta[] := '{}'; tbl text := coalesce(TG_ARGV[0],TG_TABLE_NAME);
BEGIN
 IF tbl='governance_reservations' THEN
  IF TG_OP IN ('INSERT','UPDATE') THEN
   d := d || ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,j.id IS NOT NULL,j.state,j.cancel_requested_at,1)
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at LEFT JOIN async_jobs j ON j.execution_id=r.execution_id);
  END IF;
  IF TG_OP IN ('UPDATE','DELETE') THEN
   d := d || ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,j.id IS NOT NULL,j.state,j.cancel_requested_at,-1)
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at LEFT JOIN async_jobs j ON j.execution_id=r.execution_id);
  END IF;
  -- A new reservation stops its execution counting as unreserved; a removed
  -- one makes it unreserved again. Unchanged execution links cancel out.
  IF TG_OP='INSERT' THEN
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),-1,-1,0,0,0)::rate_counter_delta
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at);
  ELSIF TG_OP='UPDATE' THEN
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),-1,-1,0,0,0)::rate_counter_delta
    FROM new_rows r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at
    WHERE NOT EXISTS(SELECT 1 FROM old_rows o WHERE o.execution_id=r.execution_id))
   || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),1,1,0,0,0)::rate_counter_delta
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at
    WHERE NOT EXISTS(SELECT 1 FROM new_rows n WHERE n.execution_id=r.execution_id));
  ELSE
   d := d || ARRAY(SELECT ROW(e.workspace_id,e.api_key_id,date_trunc('minute',e.started_at,'UTC'),1,1,0,0,0)::rate_counter_delta
    FROM old_rows r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at);
  END IF;
 ELSIF tbl='inference_executions' THEN
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
     FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at WHERE r.execution_id=NEW.execution_id)
    || ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,true,NEW.state,NEW.cancel_requested_at,1)
     FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at WHERE r.execution_id=NEW.execution_id);
  ELSE
   d := ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,true,OLD.state,OLD.cancel_requested_at,-1)
     FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at WHERE r.execution_id=OLD.execution_id)
    || ARRAY(SELECT rate_contribution(r,e.workload_kind,e.batch_job_id,true,NEW.state,NEW.cancel_requested_at,1)
     FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id AND e.started_at=r.admitted_at WHERE r.execution_id=NEW.execution_id);
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

-- Missing future partitions raise one installation alert (alerts.rs).
ALTER TABLE alert_events DROP CONSTRAINT IF EXISTS alert_events_builtin_check,
 DROP CONSTRAINT IF EXISTS alert_events_builtin_shape, DROP CONSTRAINT IF EXISTS alert_events_kind_check;
ALTER TABLE alert_events
 ADD CONSTRAINT alert_events_builtin_check CHECK(builtin IN ('personal_budget','scim_last_admin','partitions_missing')),
 ADD CONSTRAINT alert_events_builtin_shape CHECK(builtin IS NULL
  OR (builtin='personal_budget' AND kind='budget_threshold' AND workspace_id IS NOT NULL)
  OR (builtin='scim_last_admin' AND kind='scim_last_admin' AND workspace_id IS NULL AND provider_connection_id IS NULL)
  OR (builtin='partitions_missing' AND kind='partitions_missing' AND workspace_id IS NULL AND provider_connection_id IS NULL)),
 ADD CONSTRAINT alert_events_kind_check CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing',
  'scim_last_admin','batch_failed','batch_stalled','spend_threshold','partitions_missing'));
ALTER TABLE alert_events DROP CONSTRAINT IF EXISTS alert_events_partitions_kind;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_partitions_kind CHECK(kind<>'partitions_missing' OR builtin IS NOT DISTINCT FROM 'partitions_missing');
INSERT INTO work_leases(name) VALUES('partitions') ON CONFLICT DO NOTHING;
COMMIT;

-- Step 2: new keys as indexes on the existing tables, and the legacy bound
-- (SHARE lock per index build: writes wait, reads continue).
BEGIN;
DO $$
BEGIN
 IF pg_temp.p6_kind('inference_executions')='r' THEN
  PERFORM pg_temp.p6_index('inference_executions_p_legacy_pkey','CREATE UNIQUE INDEX inference_executions_p_legacy_pkey ON public.inference_executions(id,started_at)');
  PERFORM pg_temp.p6_index('inference_executions_p_legacy_root_key','CREATE UNIQUE INDEX inference_executions_p_legacy_root_key ON public.inference_executions(root_request_id,attempt_number,started_at)');
  PERFORM pg_temp.p6_index('inference_executions_p_legacy_scope_key','CREATE UNIQUE INDEX inference_executions_p_legacy_scope_key ON public.inference_executions(workspace_id,api_key_id,deployment_id,id,started_at)');
  IF pg_temp.p6_has_constraint('inference_executions','inference_executions_pkey') THEN
   ALTER TABLE inference_executions DROP CONSTRAINT inference_executions_pkey,
    ADD CONSTRAINT inference_executions_p_legacy_pkey PRIMARY KEY USING INDEX inference_executions_p_legacy_pkey;
  END IF;
  IF NOT pg_temp.p6_has_constraint('inference_executions','inference_executions_p_legacy_root_key') THEN
   ALTER TABLE inference_executions ADD CONSTRAINT inference_executions_p_legacy_root_key UNIQUE USING INDEX inference_executions_p_legacy_root_key;
  END IF;
  IF NOT pg_temp.p6_has_constraint('inference_executions','inference_executions_p_legacy_scope_key') THEN
   ALTER TABLE inference_executions ADD CONSTRAINT inference_executions_p_legacy_scope_key UNIQUE USING INDEX inference_executions_p_legacy_scope_key;
  END IF;
  IF NOT pg_temp.p6_has_constraint('inference_executions','inference_executions_p_legacy_bound') THEN
   EXECUTE format('ALTER TABLE public.inference_executions ADD CONSTRAINT inference_executions_p_legacy_bound CHECK(started_at<%L) NOT VALID', pg_temp.p6_cutover());
  END IF;
 END IF;
 IF pg_temp.p6_kind('governance_reservations')='r' THEN
  PERFORM pg_temp.p6_index('governance_reservations_p_legacy_pkey','CREATE UNIQUE INDEX governance_reservations_p_legacy_pkey ON public.governance_reservations(execution_id,admitted_at)');
  PERFORM pg_temp.p6_index('governance_unknown_p_legacy','CREATE INDEX governance_unknown_p_legacy ON public.governance_reservations(admitted_at) WHERE state=''unknown''');
  IF NOT pg_temp.p6_has_constraint('governance_reservations','governance_reservations_p_legacy_bound') THEN
   EXECUTE format('ALTER TABLE public.governance_reservations ADD CONSTRAINT governance_reservations_p_legacy_bound CHECK(admitted_at<%L) NOT VALID', pg_temp.p6_cutover());
  END IF;
 END IF;
END $$;
COMMIT;

-- Step 3: prove the legacy bounds (SHARE UPDATE EXCLUSIVE).
BEGIN;
DO $$
BEGIN
 IF EXISTS(SELECT 1 FROM pg_constraint WHERE conname='inference_executions_p_legacy_bound' AND NOT convalidated) THEN
  ALTER TABLE inference_executions VALIDATE CONSTRAINT inference_executions_p_legacy_bound;
 END IF;
 IF EXISTS(SELECT 1 FROM pg_constraint WHERE conname='governance_reservations_p_legacy_bound' AND NOT convalidated) THEN
  ALTER TABLE governance_reservations VALIDATE CONSTRAINT governance_reservations_p_legacy_bound;
 END IF;
END $$;
COMMIT;

-- Step 4: executions become partitioned (metadata only).
BEGIN;
DO $$
BEGIN
 IF pg_temp.p6_kind('inference_executions')<>'r' THEN
  RETURN;
 END IF;
 DROP TRIGGER budget_totals_execution_insert ON inference_executions;
 DROP TRIGGER budget_totals_execution_delete ON inference_executions;
 DROP TRIGGER budget_totals_execution_move ON inference_executions;
 DROP TRIGGER rate_counters_execution_insert ON inference_executions;
 DROP TRIGGER rate_counters_execution_delete ON inference_executions;
 DROP TRIGGER rate_counters_execution_move ON inference_executions;
 PERFORM pg_temp.p6_rename_index('executions_workspace_time','executions_workspace_time_p_legacy');
 PERFORM pg_temp.p6_rename_index('executions_batch_job','executions_batch_job_p_legacy');
 PERFORM pg_temp.p6_rename_index('executions_key_time','executions_key_time_p_legacy');
 PERFORM pg_temp.p6_rename_index('executions_time','executions_time_p_legacy');
 PERFORM pg_temp.p6_rename_index('executions_workspace_session','executions_workspace_session_p_legacy');
 ALTER TABLE inference_executions RENAME TO inference_executions_p_legacy;
 CREATE TABLE inference_executions (LIKE inference_executions_p_legacy INCLUDING DEFAULTS INCLUDING CONSTRAINTS
  INCLUDING GENERATED INCLUDING STORAGE INCLUDING COMMENTS) PARTITION BY RANGE (started_at);
 ALTER TABLE inference_executions DROP CONSTRAINT inference_executions_p_legacy_bound;
 ALTER TABLE inference_executions ADD CONSTRAINT inference_executions_pkey PRIMARY KEY(id,started_at),
  ADD CONSTRAINT inference_executions_root_attempt_key UNIQUE(root_request_id,attempt_number,started_at),
  ADD CONSTRAINT inference_executions_scope_key UNIQUE(workspace_id,api_key_id,deployment_id,id,started_at);
 CREATE INDEX executions_workspace_time ON inference_executions(workspace_id,started_at,id);
 CREATE INDEX executions_batch_job ON inference_executions(batch_job_id) WHERE batch_job_id IS NOT NULL;
 CREATE INDEX executions_key_time ON inference_executions(api_key_id,started_at DESC);
 CREATE INDEX executions_time ON inference_executions(started_at,id);
 CREATE INDEX executions_workspace_session ON inference_executions(workspace_id,client_session_id,started_at) WHERE client_session_id IS NOT NULL;
 EXECUTE format('ALTER TABLE public.inference_executions ATTACH PARTITION public.inference_executions_p_legacy FOR VALUES FROM (MINVALUE) TO (%L)', pg_temp.p6_cutover());
 -- Same foreign keys as the legacy table, which are adopted unscanned.
 ALTER TABLE inference_executions
  ADD CONSTRAINT inference_executions_workspace_id_fkey FOREIGN KEY(workspace_id) REFERENCES workspaces(id),
  ADD CONSTRAINT inference_executions_workspace_id_api_key_id_fkey FOREIGN KEY(workspace_id,api_key_id) REFERENCES api_keys(workspace_id,id),
  ADD CONSTRAINT inference_executions_deployment_id_fkey FOREIGN KEY(deployment_id) REFERENCES deployments(id),
  ADD CONSTRAINT inference_executions_cost_center_id_fkey FOREIGN KEY(cost_center_id) REFERENCES cost_centers(id),
  ADD CONSTRAINT inference_executions_batch_job FOREIGN KEY(workspace_id,batch_job_id) REFERENCES async_jobs(workspace_id,id);
 CREATE TRIGGER budget_totals_execution_insert AFTER INSERT ON inference_executions
  REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION budget_totals_maintain();
 CREATE TRIGGER budget_totals_execution_delete AFTER DELETE ON inference_executions
  REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION budget_totals_maintain();
 CREATE TRIGGER budget_totals_execution_move AFTER UPDATE OF workspace_id,api_key_id,started_at ON inference_executions
  FOR EACH ROW WHEN (OLD.workspace_id IS DISTINCT FROM NEW.workspace_id OR OLD.api_key_id IS DISTINCT FROM NEW.api_key_id OR OLD.started_at IS DISTINCT FROM NEW.started_at)
  EXECUTE FUNCTION budget_totals_maintain();
 CREATE TRIGGER rate_counters_execution_insert AFTER INSERT ON inference_executions
  REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION rate_counters_maintain();
 CREATE TRIGGER rate_counters_execution_delete AFTER DELETE ON inference_executions
  REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION rate_counters_maintain();
 CREATE TRIGGER rate_counters_execution_move AFTER UPDATE OF workspace_id,api_key_id,started_at,workload_kind,batch_job_id ON inference_executions
  FOR EACH ROW WHEN (OLD.workspace_id IS DISTINCT FROM NEW.workspace_id OR OLD.api_key_id IS DISTINCT FROM NEW.api_key_id OR OLD.started_at IS DISTINCT FROM NEW.started_at OR OLD.workload_kind IS DISTINCT FROM NEW.workload_kind OR OLD.batch_job_id IS DISTINCT FROM NEW.batch_job_id)
  EXECUTE FUNCTION rate_counters_maintain('inference_executions');
 CREATE TRIGGER inference_executions_stay_in_partition BEFORE UPDATE OF started_at ON inference_executions
  FOR EACH ROW WHEN (OLD.started_at IS DISTINCT FROM NEW.started_at)
  EXECUTE FUNCTION omg_partition_key_guard('started_at','inference_executions');
 CREATE TRIGGER inference_executions_no_truncate BEFORE TRUNCATE ON inference_executions
  FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
 CREATE TRIGGER inference_executions_p_legacy_no_truncate BEFORE TRUNCATE ON inference_executions_p_legacy
  FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
 UPDATE history_partitions SET converted_at=clock_timestamp() WHERE parent='inference_executions';
 PERFORM omg_ensure_partitions(3,clock_timestamp());
END $$;
COMMIT;

-- Step 5: the reservation -> execution key including the partition key,
-- added unvalidated to the (still plain) reservations table.
BEGIN;
DO $$
BEGIN
 IF pg_temp.p6_kind('governance_reservations')='r' AND NOT pg_temp.p6_has_constraint('governance_reservations','governance_reservations_execution_fkey') THEN
  ALTER TABLE governance_reservations ADD CONSTRAINT governance_reservations_execution_fkey
   FOREIGN KEY(workspace_id,api_key_id,deployment_id,execution_id,admitted_at)
   REFERENCES inference_executions(workspace_id,api_key_id,deployment_id,id,started_at) NOT VALID;
 END IF;
END $$;
COMMIT;

-- Step 6: validate it (SHARE UPDATE EXCLUSIVE; reads and writes continue).
BEGIN;
DO $$
BEGIN
 IF EXISTS(SELECT 1 FROM pg_constraint WHERE conname='governance_reservations_execution_fkey' AND NOT convalidated) THEN
  ALTER TABLE governance_reservations VALIDATE CONSTRAINT governance_reservations_execution_fkey;
 END IF;
END $$;
COMMIT;

-- Step 7: reservations become partitioned (metadata only).
BEGIN;
DO $$
BEGIN
 IF pg_temp.p6_kind('governance_reservations')<>'r' THEN
  RETURN;
 END IF;
 ALTER TABLE governance_reservations DROP CONSTRAINT governance_reservations_workspace_id_api_key_id_deployment_fkey;
 -- The ledger is rebuilt in step 8 with a key including admitted_at.
 ALTER TABLE monetary_ledger DROP CONSTRAINT IF EXISTS monetary_ledger_execution_id_fkey;
 DROP TRIGGER budget_totals_reservation_insert ON governance_reservations;
 DROP TRIGGER budget_totals_reservation_update ON governance_reservations;
 DROP TRIGGER budget_totals_reservation_delete ON governance_reservations;
 DROP TRIGGER rate_counters_reservation_insert ON governance_reservations;
 DROP TRIGGER rate_counters_reservation_update ON governance_reservations;
 DROP TRIGGER rate_counters_reservation_delete ON governance_reservations;
 ALTER TABLE governance_reservations DROP CONSTRAINT governance_reservations_pkey,
  ADD CONSTRAINT governance_reservations_p_legacy_pkey PRIMARY KEY USING INDEX governance_reservations_p_legacy_pkey;
 PERFORM pg_temp.p6_rename_index('governance_minute','governance_minute_p_legacy');
 PERFORM pg_temp.p6_rename_index('governance_month','governance_month_p_legacy');
 PERFORM pg_temp.p6_rename_index('governance_leases','governance_leases_p_legacy');
 PERFORM pg_temp.p6_rename_index('governance_admitted','governance_admitted_p_legacy');
 PERFORM pg_temp.p6_rename_index('governance_workspace_admitted','governance_workspace_admitted_p_legacy');
 ALTER TABLE governance_reservations RENAME TO governance_reservations_p_legacy;
 -- rate_contribution takes the reservation row type: recreate it for the parent.
 DROP FUNCTION rate_contribution(governance_reservations_p_legacy,text,uuid,boolean,text,timestamptz,integer);
 CREATE TABLE governance_reservations (LIKE governance_reservations_p_legacy INCLUDING DEFAULTS INCLUDING CONSTRAINTS
  INCLUDING GENERATED INCLUDING STORAGE INCLUDING COMMENTS) PARTITION BY RANGE (admitted_at);
 ALTER TABLE governance_reservations DROP CONSTRAINT governance_reservations_p_legacy_bound;
 ALTER TABLE governance_reservations ADD CONSTRAINT governance_reservations_pkey PRIMARY KEY(execution_id,admitted_at);
 CREATE INDEX governance_minute ON governance_reservations(minute_start,workspace_id);
 CREATE INDEX governance_month ON governance_reservations(month_start,workspace_id);
 CREATE INDEX governance_leases ON governance_reservations(lease_expires_at) WHERE state='pending';
 CREATE INDEX governance_admitted ON governance_reservations(admitted_at,workspace_id);
 CREATE INDEX governance_workspace_admitted ON governance_reservations(workspace_id,admitted_at);
 CREATE INDEX governance_unknown ON governance_reservations(admitted_at) WHERE state='unknown';
 EXECUTE format('ALTER TABLE public.governance_reservations ATTACH PARTITION public.governance_reservations_p_legacy FOR VALUES FROM (MINVALUE) TO (%L)', pg_temp.p6_cutover());
 ALTER TABLE governance_reservations
  ADD CONSTRAINT governance_reservations_deployment_id_price_id_fkey FOREIGN KEY(deployment_id,price_id) REFERENCES deployment_prices(deployment_id,id),
  ADD CONSTRAINT governance_reservations_execution_fkey FOREIGN KEY(workspace_id,api_key_id,deployment_id,execution_id,admitted_at)
   REFERENCES inference_executions(workspace_id,api_key_id,deployment_id,id,started_at);
 CREATE FUNCTION rate_contribution(r governance_reservations, workload_kind text, batch_job_id uuid, has_job boolean, job_state text, job_cancel timestamptz, sign integer)
 RETURNS rate_counter_delta LANGUAGE sql IMMUTABLE AS $f$
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
 $f$;
 REVOKE ALL ON FUNCTION rate_contribution(governance_reservations,text,uuid,boolean,text,timestamptz,integer) FROM PUBLIC;
 CREATE TRIGGER budget_totals_reservation_insert AFTER INSERT ON governance_reservations
  REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION budget_totals_maintain();
 CREATE TRIGGER budget_totals_reservation_update AFTER UPDATE ON governance_reservations
  REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION budget_totals_maintain();
 CREATE TRIGGER budget_totals_reservation_delete AFTER DELETE ON governance_reservations
  REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION budget_totals_maintain();
 CREATE TRIGGER rate_counters_reservation_insert AFTER INSERT ON governance_reservations
  REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION rate_counters_maintain();
 CREATE TRIGGER rate_counters_reservation_update AFTER UPDATE ON governance_reservations
  REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION rate_counters_maintain();
 CREATE TRIGGER rate_counters_reservation_delete AFTER DELETE ON governance_reservations
  REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION rate_counters_maintain();
 CREATE TRIGGER governance_reservations_stay_in_partition BEFORE UPDATE OF admitted_at ON governance_reservations
  FOR EACH ROW WHEN (OLD.admitted_at IS DISTINCT FROM NEW.admitted_at)
  EXECUTE FUNCTION omg_partition_key_guard('admitted_at','governance_reservations');
 CREATE TRIGGER governance_reservations_no_truncate BEFORE TRUNCATE ON governance_reservations
  FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
 CREATE TRIGGER governance_reservations_p_legacy_no_truncate BEFORE TRUNCATE ON governance_reservations_p_legacy
  FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
 UPDATE history_partitions SET converted_at=clock_timestamp() WHERE parent='governance_reservations';
 PERFORM omg_ensure_partitions(3,clock_timestamp());
END $$;
COMMIT;

-- Step 8: the ledger is copied once into its partitioned replacement (old
-- ledger SHARE locked: writes wait, reads continue), verified, and swapped.
BEGIN;
DO $$
DECLARE cols text; old_stats text; new_stats text; r record;
BEGIN
 IF pg_temp.p6_kind('monetary_ledger')<>'r' THEN
  RETURN;
 END IF;
 LOCK TABLE monetary_ledger IN SHARE MODE;
 SELECT string_agg(format('%I',a.attname),',' ORDER BY a.attnum) INTO cols FROM pg_attribute a
  WHERE a.attrelid='public.monetary_ledger'::regclass AND a.attnum>0 AND NOT a.attisdropped;
 CREATE TABLE monetary_ledger_p6 (LIKE monetary_ledger INCLUDING DEFAULTS INCLUDING CONSTRAINTS
  INCLUDING GENERATED INCLUDING STORAGE INCLUDING COMMENTS, admitted_at timestamptz NOT NULL)
  PARTITION BY RANGE (admitted_at);
 EXECUTE format('CREATE TABLE public.monetary_ledger_p6_legacy PARTITION OF public.monetary_ledger_p6 FOR VALUES FROM (MINVALUE) TO (%L)', pg_temp.p6_cutover());
 EXECUTE format('INSERT INTO public.monetary_ledger_p6(%1$s,admitted_at) SELECT %2$s,r.admitted_at FROM public.monetary_ledger l JOIN public.governance_reservations r ON r.execution_id=l.execution_id',
  cols,(SELECT string_agg('l.'||c,',') FROM unnest(string_to_array(cols,',')) c));
 -- Same rows: count, amount sum and a per-row hash sum over the old columns.
 EXECUTE format('SELECT concat_ws('','',count(*),coalesce(sum(amount_microusd),0),coalesce(sum((''x''||substr(md5(ROW(%1$s)::text),1,15))::bit(60)::bigint::numeric),0)) FROM public.monetary_ledger',cols) INTO old_stats;
 EXECUTE format('SELECT concat_ws('','',count(*),coalesce(sum(amount_microusd),0),coalesce(sum((''x''||substr(md5(ROW(%1$s)::text),1,15))::bit(60)::bigint::numeric),0)) FROM public.monetary_ledger_p6',cols) INTO new_stats;
 IF old_stats IS DISTINCT FROM new_stats THEN
  RAISE EXCEPTION '0030: ledger copy differs (% vs %); nothing was changed', old_stats, new_stats;
 END IF;
 ALTER TABLE monetary_ledger_p6 ADD CONSTRAINT monetary_ledger_p6_pkey PRIMARY KEY(id,admitted_at),
  ADD CONSTRAINT monetary_ledger_p6_execution_kind_key UNIQUE(execution_id,kind,admitted_at);
 ALTER TABLE monetary_ledger_p6 ADD CONSTRAINT monetary_ledger_execution_fkey FOREIGN KEY(execution_id,admitted_at)
  REFERENCES governance_reservations(execution_id,admitted_at);
 DROP TABLE monetary_ledger;
 ALTER TABLE monetary_ledger_p6 RENAME TO monetary_ledger;
 ALTER TABLE monetary_ledger RENAME CONSTRAINT monetary_ledger_p6_pkey TO monetary_ledger_pkey;
 ALTER TABLE monetary_ledger RENAME CONSTRAINT monetary_ledger_p6_execution_kind_key TO monetary_ledger_execution_kind_key;
 ALTER TABLE monetary_ledger_p6_legacy RENAME TO monetary_ledger_p_legacy;
 FOR r IN SELECT c.relname FROM pg_class c WHERE c.relnamespace='public'::regnamespace AND c.relkind='i' AND c.relname LIKE 'monetary\_ledger\_p6\_legacy%' LOOP
  EXECUTE format('ALTER INDEX public.%I RENAME TO %I', r.relname, replace(r.relname,'monetary_ledger_p6_legacy','monetary_ledger_p_legacy'));
 END LOOP;
 CREATE TRIGGER monetary_ledger_immutable BEFORE UPDATE OR DELETE ON monetary_ledger
  FOR EACH ROW EXECUTE FUNCTION immutable_history();
 CREATE TRIGGER monetary_ledger_no_truncate BEFORE TRUNCATE ON monetary_ledger
  FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
 CREATE TRIGGER monetary_ledger_p_legacy_no_truncate BEFORE TRUNCATE ON monetary_ledger_p_legacy
  FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
 UPDATE history_partitions SET converted_at=clock_timestamp() WHERE parent='monetary_ledger';
 PERFORM omg_ensure_partitions(3,clock_timestamp());
END $$;
COMMIT;

-- Step 9: drop the superseded single-column keys and bounds of the legacy
-- partitions (their replacements are the partitioned keys) and analyze the
-- parents (autovacuum analyzes partitions, never partitioned parents).
BEGIN;
ALTER TABLE inference_executions_p_legacy DROP CONSTRAINT IF EXISTS inference_executions_root_request_id_attempt_number_key,
 DROP CONSTRAINT IF EXISTS inference_executions_workspace_id_api_key_id_deployment_id__key,
 DROP CONSTRAINT IF EXISTS inference_executions_p_legacy_bound;
ALTER TABLE governance_reservations_p_legacy DROP CONSTRAINT IF EXISTS governance_reservations_p_legacy_bound;
COMMIT;
ANALYZE inference_executions;
ANALYZE governance_reservations;
ANALYZE monetary_ledger;
RESET lock_timeout;
