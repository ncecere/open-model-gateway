-- #102 slice B. Explicit additive upgrade AFTER the reserved 0058--0062
-- prefix. No email/outbox path; no historical replay or data backfill.
ALTER TABLE report_schedules ADD COLUMN next_period_end timestamptz;
CREATE FUNCTION report_calendar_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
 IF OLD.last_period_end IS NOT NULL AND (NEW.last_period_end IS NULL OR NEW.last_period_end<OLD.last_period_end) THEN
  RAISE EXCEPTION 'report cursor cannot move backwards' USING ERRCODE='23514';
 END IF;
 -- Runs after report_schedules_guard, which owns configuration_revision.
 IF NEW.configuration_revision<>OLD.configuration_revision THEN
  NEW.next_run_at=NULL; NEW.next_period_end=NULL;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER report_schedules_z_calendar_guard BEFORE UPDATE ON report_schedules FOR EACH ROW EXECUTE FUNCTION report_calendar_guard();

-- Separate short-lived ownership: no transaction spans aggregate generation.
CREATE TABLE report_generation_claims(
 schedule_id uuid PRIMARY KEY REFERENCES report_schedules(id),
 token uuid NOT NULL,
 configuration_revision bigint NOT NULL CHECK(configuration_revision>0),
 period_start timestamptz NOT NULL,
 period_end timestamptz NOT NULL CHECK(period_end>period_start),
 skipped_intervals bigint NOT NULL CHECK(skipped_intervals>=0),
 expires_at timestamptz NOT NULL
);
CREATE FUNCTION report_claim_identity_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.schedule_id<>OLD.schedule_id THEN RAISE EXCEPTION 'immutable report claim identity'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER report_claim_identity_guard BEFORE UPDATE ON report_generation_claims FOR EACH ROW EXECUTE FUNCTION report_claim_identity_guard();
CREATE TRIGGER report_claims_no_delete BEFORE DELETE ON report_generation_claims FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER report_claims_no_truncate BEFORE TRUNCATE ON report_generation_claims FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();

-- Small occurrence/deduplication metadata is retained independently of payloads.
CREATE TABLE report_runs(
 id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
 schedule_id uuid NOT NULL REFERENCES report_schedules(id),
 period_start timestamptz NOT NULL,
 period_end timestamptz NOT NULL CHECK(period_end>period_start),
 configuration_revision bigint NOT NULL CHECK(configuration_revision>0),
 configuration jsonb NOT NULL,
 claim_token uuid NOT NULL,
 lease_holder uuid NOT NULL,
 lease_epoch bigint NOT NULL,
 skipped_intervals bigint NOT NULL CHECK(skipped_intervals>=0),
 observed_at timestamptz NOT NULL,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 expires_at timestamptz NOT NULL,
 UNIQUE(schedule_id,period_start,period_end)
);
CREATE INDEX report_runs_schedule_recent ON report_runs(schedule_id,period_end DESC,id);
CREATE INDEX report_runs_expiry ON report_runs(expires_at,id);
CREATE FUNCTION report_run_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE s public.report_schedules; c public.report_generation_claims;
BEGIN
 PERFORM public.omg_lease_fence('report_scheduler',NEW.lease_holder,NEW.lease_epoch);
 SELECT * INTO STRICT s FROM public.report_schedules WHERE id=NEW.schedule_id FOR UPDATE;
 SELECT * INTO STRICT c FROM public.report_generation_claims WHERE schedule_id=s.id FOR UPDATE;
 IF s.state<>'active' OR s.deleted_at IS NOT NULL OR s.configuration_revision<>NEW.configuration_revision
  OR c.configuration_revision<>NEW.configuration_revision OR c.token<>NEW.claim_token OR c.expires_at<=clock_timestamp()
  OR c.period_start<>NEW.period_start OR c.period_end<>NEW.period_end OR c.skipped_intervals<>NEW.skipped_intervals
  OR NEW.period_end>clock_timestamp() OR NEW.observed_at>clock_timestamp() OR NEW.observed_at<NEW.period_end
  OR (s.cadence='weekly' AND (NEW.period_end<>date_trunc('week',NEW.period_end,'UTC') OR NEW.period_start<>(((NEW.period_end AT TIME ZONE 'UTC')-interval '7 days') AT TIME ZONE 'UTC')))
  OR (s.cadence='monthly' AND (NEW.period_end<>date_trunc('month',NEW.period_end,'UTC') OR NEW.period_start<>(((NEW.period_end AT TIME ZONE 'UTC')-interval '1 month') AT TIME ZONE 'UTC')))
  OR (s.last_period_end IS NOT NULL AND NEW.period_end<=s.last_period_end) THEN
  RAISE EXCEPTION 'stale report generation' USING ERRCODE='55006';
 END IF;
 NEW.configuration=jsonb_build_object('scope_kind',s.scope_kind,'workspace_id',s.workspace_id,'cost_center_id',s.cost_center_id,
  'created_by',s.created_by,'cadence',s.cadence,'timezone',s.timezone,'delivery_time',left(s.delivery_time::text,5),
  'recipient_user_ids',coalesce((SELECT jsonb_agg(user_id ORDER BY slot) FROM public.report_schedule_recipients WHERE schedule_id=s.id),'[]'::jsonb));
 NEW.created_at=clock_timestamp(); NEW.expires_at=NEW.created_at+interval '2160 hours';
 RETURN NEW;
END $$;
CREATE TRIGGER report_run_guard BEFORE INSERT ON report_runs FOR EACH ROW EXECUTE FUNCTION report_run_guard();
CREATE TRIGGER report_runs_immutable BEFORE UPDATE OR DELETE ON report_runs FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER report_runs_no_truncate BEFORE TRUNCATE ON report_runs FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
CREATE TABLE report_snapshots(
 run_id uuid PRIMARY KEY REFERENCES report_runs(id),
 body jsonb NOT NULL CHECK(jsonb_typeof(body)='object' AND octet_length(body::text)<=1048576)
);
CREATE FUNCTION report_snapshot_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE r public.report_runs;
BEGIN
 IF TG_OP='INSERT' THEN
  SELECT * INTO STRICT r FROM public.report_runs WHERE id=NEW.run_id;
  PERFORM public.omg_lease_fence('report_scheduler',r.lease_holder,r.lease_epoch);
  IF r.expires_at<=clock_timestamp() OR NOT EXISTS(SELECT FROM public.report_generation_claims c WHERE c.schedule_id=r.schedule_id AND c.token=r.claim_token AND c.configuration_revision=r.configuration_revision AND c.expires_at>clock_timestamp()) THEN
   RAISE EXCEPTION 'stale report snapshot' USING ERRCODE='55006';
  END IF;
  RETURN NEW;
 END IF;
 IF TG_OP='DELETE' AND EXISTS(SELECT FROM public.report_runs WHERE id=OLD.run_id AND expires_at<=clock_timestamp()) THEN RETURN OLD; END IF;
 RAISE EXCEPTION 'immutable report snapshot';
END $$;
CREATE TRIGGER report_snapshots_immutable BEFORE INSERT OR UPDATE OR DELETE ON report_snapshots FOR EACH ROW EXECUTE FUNCTION report_snapshot_guard();
CREATE TRIGGER report_snapshots_no_truncate BEFORE TRUNCATE ON report_snapshots FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
-- Runtime has INSERT/SELECT only, never DELETE. This fixed, bounded maintenance
-- function is the only runtime removal path; occurrence metadata is untouched.
CREATE FUNCTION omg_report_prune_snapshots(cap integer) RETURNS bigint LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE n bigint;
BEGIN
 WITH expired AS (SELECT p.run_id FROM public.report_snapshots p JOIN public.report_runs r ON r.id=p.run_id
   WHERE r.expires_at<=clock_timestamp() ORDER BY r.expires_at,r.id LIMIT greatest(0,least(cap,100)) FOR UPDATE OF p SKIP LOCKED),
 removed AS (DELETE FROM public.report_snapshots p USING expired e WHERE p.run_id=e.run_id RETURNING p.run_id)
 SELECT count(*) INTO n FROM removed;
 RETURN n;
END $$;
REVOKE ALL ON FUNCTION omg_report_prune_snapshots(integer) FROM PUBLIC;
