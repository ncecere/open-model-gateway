-- no-transaction
-- Monthly range partitioning of the audit trail and storage usage hours
-- (scale plan P6). Explicit operator upgrade only (`migrate`).
--
-- Same safe conversion as 0030: the existing table becomes the legacy
-- partition (rows before the first month start after its newest row) after
-- its new key is built as an index (SHARE lock) and CHECK (key < cutover)
-- NOT VALID is validated (SHARE UPDATE EXCLUSIVE); the swap itself is
-- metadata only. Each step is idempotent; a failed upgrade resumes on the
-- next `migrate`.
--   audit_events         by created_at; key (id, created_at). Immutable rows
--                        (row trigger on the parent, TRUNCATE guard on every
--                        partition).
--   storage_usage_hours  by hour_start; its key already includes it.
-- alert_events stays unpartitioned: one row per incident (not per request)
-- and its "one open incident per subject" uniqueness cannot include a time.
SET lock_timeout = '60s';
CREATE OR REPLACE FUNCTION pg_temp.p6_kind(rel text) RETURNS "char" LANGUAGE sql STABLE AS $$
 SELECT c.relkind FROM pg_class c WHERE c.relnamespace='public'::regnamespace AND c.relname=rel
$$;
CREATE OR REPLACE FUNCTION pg_temp.p6_has_constraint(rel text, con text) RETURNS boolean LANGUAGE sql STABLE AS $$
 SELECT EXISTS(SELECT 1 FROM pg_constraint k JOIN pg_class c ON c.oid=k.conrelid
  WHERE c.relnamespace='public'::regnamespace AND c.relname=rel AND k.conname=con)
$$;
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
CREATE OR REPLACE FUNCTION pg_temp.p6_cutover(rel text) RETURNS timestamptz LANGUAGE plpgsql STABLE AS $$
BEGIN
 RETURN (SELECT legacy_upper FROM public.history_partitions WHERE parent=rel);
END $$;

-- Step 1: registry rows. The storage guard's row trigger fires on
-- partitions (TG_TABLE_NAME is the partition): it passes its parent's name.
BEGIN;
CREATE OR REPLACE FUNCTION storage_usage_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF coalesce(TG_ARGV[0],TG_TABLE_NAME) = 'storage_usage_hours' THEN
  RAISE EXCEPTION 'storage usage history is append-only';
 END IF;
 IF TG_OP = 'DELETE' OR NEW.recorded_through < OLD.recorded_through THEN
  RAISE EXCEPTION 'storage usage progress only moves forward';
 END IF;
 RETURN NEW;
END $$;
INSERT INTO history_partitions(parent,position,partition_key,archive_group,legacy_upper)
SELECT 'audit_events',4,'created_at','audit',
 omg_next_month(greatest(clock_timestamp(),(SELECT max(created_at) FROM audit_events)))
ON CONFLICT DO NOTHING;
INSERT INTO history_partitions(parent,position,partition_key,archive_group,legacy_upper)
SELECT 'storage_usage_hours',5,'hour_start','storage',
 omg_next_month(greatest(clock_timestamp(),(SELECT max(hour_start) FROM storage_usage_hours)))
ON CONFLICT DO NOTHING;
COMMIT;

-- Step 2: keys and bounds on the existing tables.
BEGIN;
DO $$
BEGIN
 IF pg_temp.p6_kind('audit_events')='r' THEN
  PERFORM pg_temp.p6_index('audit_events_p_legacy_pkey','CREATE UNIQUE INDEX audit_events_p_legacy_pkey ON public.audit_events(id,created_at)');
  PERFORM pg_temp.p6_index('audit_installation_time_p_legacy','CREATE INDEX audit_installation_time_p_legacy ON public.audit_events(created_at,id) WHERE workspace_id IS NULL');
  IF pg_temp.p6_has_constraint('audit_events','audit_events_pkey') THEN
   ALTER TABLE audit_events DROP CONSTRAINT audit_events_pkey,
    ADD CONSTRAINT audit_events_p_legacy_pkey PRIMARY KEY USING INDEX audit_events_p_legacy_pkey;
  END IF;
  IF NOT pg_temp.p6_has_constraint('audit_events','audit_events_p_legacy_bound') THEN
   EXECUTE format('ALTER TABLE public.audit_events ADD CONSTRAINT audit_events_p_legacy_bound CHECK(created_at<%L) NOT VALID', pg_temp.p6_cutover('audit_events'));
  END IF;
 END IF;
 IF pg_temp.p6_kind('storage_usage_hours')='r' AND NOT pg_temp.p6_has_constraint('storage_usage_hours','storage_usage_hours_p_legacy_bound') THEN
  EXECUTE format('ALTER TABLE public.storage_usage_hours ADD CONSTRAINT storage_usage_hours_p_legacy_bound CHECK(hour_start<%L) NOT VALID', pg_temp.p6_cutover('storage_usage_hours'));
 END IF;
END $$;
COMMIT;

-- Step 3: prove the bounds (SHARE UPDATE EXCLUSIVE).
BEGIN;
DO $$
BEGIN
 IF EXISTS(SELECT 1 FROM pg_constraint WHERE conname='audit_events_p_legacy_bound' AND NOT convalidated) THEN
  ALTER TABLE audit_events VALIDATE CONSTRAINT audit_events_p_legacy_bound;
 END IF;
 IF EXISTS(SELECT 1 FROM pg_constraint WHERE conname='storage_usage_hours_p_legacy_bound' AND NOT convalidated) THEN
  ALTER TABLE storage_usage_hours VALIDATE CONSTRAINT storage_usage_hours_p_legacy_bound;
 END IF;
END $$;
COMMIT;

-- Step 4: swap (metadata only).
BEGIN;
DO $$
BEGIN
 IF pg_temp.p6_kind('audit_events')='r' THEN
  DROP TRIGGER audit_events_immutable ON audit_events;
  DROP TRIGGER audit_events_no_truncate ON audit_events;
  PERFORM pg_temp.p6_rename_index('audit_workspace_time','audit_workspace_time_p_legacy');
  ALTER TABLE audit_events RENAME TO audit_events_p_legacy;
  CREATE TABLE audit_events (LIKE audit_events_p_legacy INCLUDING DEFAULTS INCLUDING CONSTRAINTS
   INCLUDING GENERATED INCLUDING STORAGE INCLUDING COMMENTS) PARTITION BY RANGE (created_at);
  ALTER TABLE audit_events DROP CONSTRAINT audit_events_p_legacy_bound;
  ALTER TABLE audit_events ADD CONSTRAINT audit_events_pkey PRIMARY KEY(id,created_at);
  CREATE INDEX audit_workspace_time ON audit_events(workspace_id,created_at,id);
  CREATE INDEX audit_installation_time ON audit_events(created_at,id) WHERE workspace_id IS NULL;
  EXECUTE format('ALTER TABLE public.audit_events ATTACH PARTITION public.audit_events_p_legacy FOR VALUES FROM (MINVALUE) TO (%L)', pg_temp.p6_cutover('audit_events'));
  ALTER TABLE audit_events
   ADD CONSTRAINT audit_events_actor_user_id_fkey FOREIGN KEY(actor_user_id) REFERENCES users(id),
   ADD CONSTRAINT audit_events_workspace_id_fkey FOREIGN KEY(workspace_id) REFERENCES workspaces(id);
  CREATE TRIGGER audit_events_immutable BEFORE UPDATE OR DELETE ON audit_events
   FOR EACH ROW EXECUTE FUNCTION immutable_history();
  CREATE TRIGGER audit_events_no_truncate BEFORE TRUNCATE ON audit_events
   FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
  CREATE TRIGGER audit_events_p_legacy_no_truncate BEFORE TRUNCATE ON audit_events_p_legacy
   FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
  ALTER TABLE audit_events_p_legacy DROP CONSTRAINT audit_events_p_legacy_bound;
  UPDATE history_partitions SET converted_at=clock_timestamp() WHERE parent='audit_events';
 END IF;
 IF pg_temp.p6_kind('storage_usage_hours')='r' THEN
  DROP TRIGGER storage_usage_hours_guard ON storage_usage_hours;
  ALTER TABLE storage_usage_hours RENAME CONSTRAINT storage_usage_hours_pkey TO storage_usage_hours_p_legacy_pkey;
  PERFORM pg_temp.p6_rename_index('storage_usage_hours_time','storage_usage_hours_time_p_legacy');
  ALTER TABLE storage_usage_hours RENAME TO storage_usage_hours_p_legacy;
  CREATE TABLE storage_usage_hours (LIKE storage_usage_hours_p_legacy INCLUDING DEFAULTS INCLUDING CONSTRAINTS
   INCLUDING GENERATED INCLUDING STORAGE INCLUDING COMMENTS) PARTITION BY RANGE (hour_start);
  ALTER TABLE storage_usage_hours DROP CONSTRAINT storage_usage_hours_p_legacy_bound;
  ALTER TABLE storage_usage_hours ADD CONSTRAINT storage_usage_hours_pkey PRIMARY KEY(workspace_id,purpose,hour_start);
  CREATE INDEX storage_usage_hours_time ON storage_usage_hours(hour_start);
  EXECUTE format('ALTER TABLE public.storage_usage_hours ATTACH PARTITION public.storage_usage_hours_p_legacy FOR VALUES FROM (MINVALUE) TO (%L)', pg_temp.p6_cutover('storage_usage_hours'));
  ALTER TABLE storage_usage_hours
   ADD CONSTRAINT storage_usage_hours_workspace_id_fkey FOREIGN KEY(workspace_id) REFERENCES workspaces(id);
  CREATE TRIGGER storage_usage_hours_guard BEFORE UPDATE OR DELETE ON storage_usage_hours
   FOR EACH ROW EXECUTE FUNCTION storage_usage_guard('storage_usage_hours');
  CREATE TRIGGER storage_usage_hours_no_truncate BEFORE TRUNCATE ON storage_usage_hours
   FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
  CREATE TRIGGER storage_usage_hours_p_legacy_no_truncate BEFORE TRUNCATE ON storage_usage_hours_p_legacy
   FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
  ALTER TABLE storage_usage_hours_p_legacy DROP CONSTRAINT storage_usage_hours_p_legacy_bound;
  UPDATE history_partitions SET converted_at=clock_timestamp() WHERE parent='storage_usage_hours';
 END IF;
 PERFORM omg_ensure_partitions(3,clock_timestamp());
END $$;
COMMIT;
ANALYZE audit_events;
ANALYZE storage_usage_hours;
RESET lock_timeout;
