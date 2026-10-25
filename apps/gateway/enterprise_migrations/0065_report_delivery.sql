-- #102C explicit additive upgrade, AFTER the reserved 0058--0064 prefix.
-- Reuse the recipient outbox and fair buckets, not alert incidents or a second
-- queue. report_pending is intentionally invisible to pre-0065 dispatchers.
ALTER TABLE alert_deliveries ALTER COLUMN event_id DROP NOT NULL;
ALTER TABLE alert_deliveries ADD COLUMN report_run_id uuid UNIQUE REFERENCES report_runs(id);
ALTER TABLE alert_deliveries DROP CONSTRAINT alert_deliveries_status_check;
ALTER TABLE alert_deliveries DROP CONSTRAINT alert_deliveries_check;
-- Extend 0059's status set; keep suppression_check and its immutable admission
-- metadata trigger intact. As in 0059, do not rescan retained alert history.
ALTER TABLE alert_deliveries ADD CONSTRAINT alert_deliveries_status_check CHECK(status IN ('pending','report_pending','sent','partial','failed','not_configured','no_recipients','suppressed')) NOT VALID;
ALTER TABLE alert_deliveries ADD CONSTRAINT alert_deliveries_completion CHECK((status IN ('pending','report_pending'))=(completed_at IS NULL));
ALTER TABLE alert_deliveries ADD CONSTRAINT alert_deliveries_source CHECK((event_id IS NULL)<>(report_run_id IS NULL));
ALTER TABLE alert_deliveries ADD CONSTRAINT alert_deliveries_source_state CHECK((report_run_id IS NULL OR (status NOT IN ('pending','suppressed') AND transition='fired' AND NOT quota_charged)) AND (event_id IS NULL OR status<>'report_pending'));
-- Existing 0056 index remains available to old workers during rollout.
CREATE INDEX alert_deliveries_shared_pending_bucket ON alert_deliveries(bucket_id,created_at,id) WHERE status IN ('pending','report_pending');
CREATE INDEX alert_deliveries_report_recent ON alert_deliveries(created_at DESC,id) WHERE report_run_id IS NOT NULL;

-- Keep 0059's alert_delivery_set_bucket() VERBATIM: its READ COMMITTED
-- refusal, canonical prelock/fail-fast inversion, rolling count, server-owned
-- metadata and suppression all remain the alert path. Reports use a separate
-- BEFORE INSERT branch, without acquiring or consuming the alert quota.
DROP TRIGGER alert_delivery_set_bucket ON alert_deliveries;
CREATE TRIGGER alert_delivery_set_bucket BEFORE INSERT ON alert_deliveries
 FOR EACH ROW WHEN (NEW.event_id IS NOT NULL) EXECUTE FUNCTION alert_delivery_set_bucket();
CREATE FUNCTION report_delivery_set_bucket() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
 IF current_setting('transaction_isolation')<>'read committed' THEN
  RAISE EXCEPTION 'report delivery generation requires READ COMMITTED' USING ERRCODE='25001';
 END IF;
 IF NEW.event_id IS NOT NULL OR NEW.status NOT IN ('pending','report_pending') OR NEW.initialized OR NEW.attempt_count<>0 OR NEW.completed_at IS NOT NULL THEN
  RAISE EXCEPTION 'invalid report delivery envelope' USING ERRCODE='23514';
 END IF;
 SELECT coalesce(s.workspace_id,'00000000-0000-0000-0000-000000000000'::uuid)
  INTO STRICT NEW.bucket_id FROM public.report_runs r JOIN public.report_schedules s ON s.id=r.schedule_id
  JOIN public.report_snapshots p ON p.run_id=r.id WHERE r.id=NEW.report_run_id AND p.expires_at>clock_timestamp();
 NEW.status='report_pending'; NEW.created_at=clock_timestamp();
 NEW.quota_charged=false; NEW.suppression_reason=NULL;
 NEW.completed_at=NULL; NEW.initialized=false; NEW.recipients=0; NEW.sent=0; NEW.failed=0;
 NEW.error=NULL; NEW.attempt_count=0; NEW.retrying=false;
 NEW.next_attempt_at=NULL; NEW.waiting=0; NEW.exhausted=0;
 RETURN NEW;
END $$;
-- Runs before the alert branch, so a mixed-source envelope cannot enter it.
CREATE TRIGGER alert_delivery_report_bucket BEFORE INSERT ON alert_deliveries
 FOR EACH ROW WHEN (NEW.report_run_id IS NOT NULL) EXECUTE FUNCTION report_delivery_set_bucket();
CREATE OR REPLACE FUNCTION alert_delivery_outbox_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog,public,pg_temp AS $$
BEGIN
 IF NEW.id<>OLD.id OR NEW.event_id IS DISTINCT FROM OLD.event_id OR NEW.report_run_id IS DISTINCT FROM OLD.report_run_id OR NEW.transition<>OLD.transition OR NEW.bucket_id<>OLD.bucket_id
  OR (OLD.initialized AND NOT NEW.initialized) OR (OLD.status NOT IN ('pending','report_pending') AND NEW IS DISTINCT FROM OLD)
  OR (NEW.status IN ('pending','report_pending') AND NEW.status<>OLD.status) THEN
  RAISE EXCEPTION 'immutable delivery identity or completed outcome';
 END IF;
 IF OLD.initialized AND NEW.status NOT IN ('pending','report_pending') AND EXISTS(SELECT FROM alert_recipient_outbox q WHERE q.delivery_id=OLD.id AND q.state IN ('waiting','leased')) THEN
  RAISE EXCEPTION 'delivery outbox still has waiting or leased recipients' USING ERRCODE='55006';
 END IF;
 RETURN NEW;
END $$;
-- Unavoidable replacement to admit report_pending parents: carry 0059's
-- recipient guard forward verbatim apart from the expanded pending status,
-- then add report lineage validation AFTER the shared isolation/50-row guard.
CREATE OR REPLACE FUNCTION alert_outbox_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog,public,pg_temp AS $$
DECLARE parent alert_deliveries;
BEGIN
 IF TG_OP='INSERT' THEN
  -- The count below must see recipients committed by earlier writers: a
  -- REPEATABLE READ/SERIALIZABLE snapshot could count a stale total.
  IF current_setting('transaction_isolation')<>'read committed' THEN
   RAISE EXCEPTION 'alert recipient snapshots require READ COMMITTED' USING ERRCODE='25001';
  END IF;
  SELECT * INTO STRICT parent FROM alert_deliveries WHERE id=NEW.delivery_id FOR UPDATE;
  IF parent.status NOT IN ('pending','report_pending') OR parent.initialized THEN
   RAISE EXCEPTION 'alert recipient snapshot already closed';
  END IF;
  IF (SELECT count(*) FROM (SELECT 1 FROM alert_recipient_outbox WHERE delivery_id=NEW.delivery_id LIMIT 50) q)>=50 THEN
   RAISE EXCEPTION 'maximum fifty alert snapshot recipients' USING ERRCODE='23514';
  END IF;
  IF parent.report_run_id IS NOT NULL AND NOT EXISTS(
   SELECT FROM public.report_runs r JOIN public.users u ON r.configuration->'recipient_user_ids' @> jsonb_build_array(u.id)
   WHERE r.id=parent.report_run_id AND u.id=NEW.internal_user_id AND u.email IS NOT NULL
    AND NEW.fingerprint=sha256(uuid_send(parent.id)||convert_to(':omg.alert.recipient.v1:','UTF8')||convert_to(lower(btrim(u.email)),'UTF8'))
  ) THEN RAISE EXCEPTION 'invalid report recipient lineage' USING ERRCODE='23514'; END IF;
 ELSE
  IF NEW.delivery_id<>OLD.delivery_id OR NEW.fingerprint<>OLD.fingerprint OR NEW.internal_user_id IS DISTINCT FROM OLD.internal_user_id
   OR OLD.state IN ('sent','failed','ineligible','exhausted','not_configured')
   OR NEW.attempt_count<OLD.attempt_count OR NEW.attempt_count>OLD.attempt_count+1 THEN
   RAISE EXCEPTION 'immutable alert recipient identity or completed outcome';
  END IF;
 END IF;
 RETURN NEW;
END $$;
-- Report envelopes never create webhook payloads, even when destinations exist.
DROP TRIGGER alert_deliveries_webhook_enqueue ON alert_deliveries;
CREATE TRIGGER alert_deliveries_webhook_enqueue AFTER INSERT ON alert_deliveries FOR EACH ROW WHEN (NEW.event_id IS NOT NULL) EXECUTE FUNCTION alert_webhook_enqueue();
