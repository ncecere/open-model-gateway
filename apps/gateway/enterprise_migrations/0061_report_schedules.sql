-- #102 slice A: schedule configuration only. No generation, deliveries or replay.
-- Explicit additive upgrade. 0058/0059/0060 are reserved by independent work.
CREATE TABLE report_schedules(
 id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
 scope_kind text NOT NULL CHECK(scope_kind IN ('installation','cost_center','workspace','personal')),
 workspace_id uuid REFERENCES workspaces(id),
 cost_center_id uuid REFERENCES cost_centers(id),
 created_by uuid NOT NULL REFERENCES users(id),
 cadence text NOT NULL CHECK(cadence IN ('weekly','monthly')),
 timezone text NOT NULL CHECK(char_length(timezone) BETWEEN 1 AND 64),
 delivery_time time NOT NULL CHECK(EXTRACT(SECOND FROM delivery_time)=0 AND delivery_time<'24:00'),
 state text NOT NULL DEFAULT 'paused' CHECK(state IN ('active','paused')),
 configuration_revision bigint NOT NULL DEFAULT 1 CHECK(configuration_revision>0),
 -- Internal configuration marker for the normalized recipient set. Management
 -- advances it once only when that set changes, never for order-only edits.
 recipient_revision bigint NOT NULL DEFAULT 1 CHECK(recipient_revision>0),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 deleted_at timestamptz,
 -- Reserved for the calendar/generation slice. Runtime cannot write these yet.
 -- NULL is explicitly "not scheduled by a worker", never a fabricated next run.
 next_run_at timestamptz,
 last_period_end timestamptz,
 CHECK((scope_kind='installation' AND workspace_id IS NULL AND cost_center_id IS NULL)
    OR (scope_kind='cost_center' AND workspace_id IS NULL AND cost_center_id IS NOT NULL)
    OR (scope_kind='workspace' AND workspace_id IS NOT NULL)
    OR (scope_kind='personal' AND workspace_id IS NOT NULL AND cost_center_id IS NULL)),
 CHECK(deleted_at IS NULL OR state='paused')
);
-- Scope is supplied by the parent, never independently chosen by a recipient.
-- Fifty unique slots enforce the cap under concurrent/stale snapshots too.
CREATE TABLE report_schedule_recipients(
 schedule_id uuid NOT NULL REFERENCES report_schedules(id),
 slot smallint NOT NULL CHECK(slot BETWEEN 1 AND 50),
 user_id uuid NOT NULL REFERENCES users(id),
 PRIMARY KEY(schedule_id,user_id),
 UNIQUE(schedule_id,slot)
);
CREATE INDEX report_schedules_platform ON report_schedules(id) WHERE workspace_id IS NULL AND deleted_at IS NULL;
CREATE INDEX report_schedules_workspace ON report_schedules(workspace_id,id) WHERE deleted_at IS NULL;
CREATE INDEX report_schedules_due ON report_schedules(next_run_at,id) WHERE state='active' AND deleted_at IS NULL;
CREATE INDEX report_schedule_recipient_user ON report_schedule_recipients(user_id,schedule_id);
CREATE FUNCTION report_schedules_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
 IF TG_OP='INSERT' THEN
  IF NEW.workspace_id IS NOT NULL AND NOT EXISTS(SELECT FROM public.workspaces w WHERE w.id=NEW.workspace_id
     AND ((NEW.scope_kind='workspace' AND w.kind IN ('team','project'))
       OR (NEW.scope_kind='personal' AND w.kind='personal' AND w.owner_user_id=NEW.created_by))) THEN
   RAISE EXCEPTION 'report schedule workspace scope mismatch' USING ERRCODE='23514';
  END IF;
  NEW.configuration_revision=1;
  NEW.recipient_revision=1;
 ELSE
  IF OLD.deleted_at IS NOT NULL OR NEW.id<>OLD.id OR NEW.scope_kind<>OLD.scope_kind
   OR NEW.workspace_id IS DISTINCT FROM OLD.workspace_id OR NEW.cost_center_id IS DISTINCT FROM OLD.cost_center_id
   OR NEW.created_by<>OLD.created_by OR NEW.created_at<>OLD.created_at
   OR NEW.configuration_revision<>OLD.configuration_revision THEN
   RAISE EXCEPTION 'immutable report schedule identity or deleted schedule';
  END IF;
  IF NEW.recipient_revision NOT IN (OLD.recipient_revision,OLD.recipient_revision+1) THEN
   RAISE EXCEPTION 'report recipient revision must advance by one';
  END IF;
  -- Cursor/observation/tombstone-only updates never invalidate a generation's
  -- pinned configuration. Identity/scope narrowing remains immutable above.
  IF ROW(NEW.cadence,NEW.timezone,NEW.delivery_time,NEW.state,NEW.recipient_revision)
     IS DISTINCT FROM ROW(OLD.cadence,OLD.timezone,OLD.delivery_time,OLD.state,OLD.recipient_revision) THEN
   NEW.configuration_revision=OLD.configuration_revision+1;
  END IF;
 END IF;
 NEW.updated_at=clock_timestamp();
 RETURN NEW;
END $$;
CREATE TRIGGER report_schedules_guard BEFORE INSERT OR UPDATE ON report_schedules FOR EACH ROW EXECUTE FUNCTION report_schedules_guard();
CREATE TRIGGER report_schedules_no_delete BEFORE DELETE ON report_schedules FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER report_schedules_no_truncate BEFORE TRUNCATE ON report_schedules FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
CREATE TRIGGER report_schedule_recipients_no_update BEFORE UPDATE ON report_schedule_recipients FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER report_schedule_recipients_no_truncate BEFORE TRUNCATE ON report_schedule_recipients FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
-- Seed only: old binaries have no job or claim path for this lease.
INSERT INTO work_leases(name) VALUES('report_scheduler');
