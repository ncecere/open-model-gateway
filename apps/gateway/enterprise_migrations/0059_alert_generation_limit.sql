-- #277. Explicit operator upgrade; 0058 is reserved by #312.
-- Existing statuses satisfy the stricter old check and existing rows get a
-- NULL suppression reason. NOT VALID avoids scanning retained history under
-- the ALTER lock; both constraints immediately check all new/updated rows.
SET lock_timeout='60s';
ALTER TABLE alert_deliveries
 ADD COLUMN quota_charged boolean NOT NULL DEFAULT false,
 ADD COLUMN suppression_reason text,
 DROP CONSTRAINT alert_deliveries_status_check,
 ADD CONSTRAINT alert_deliveries_status_check CHECK(status IN ('pending','sent','partial','failed','not_configured','no_recipients','suppressed')) NOT VALID,
 ADD CONSTRAINT alert_deliveries_suppression_check CHECK(
  (status='suppressed' AND suppression_reason IS NOT NULL AND suppression_reason='workspace_generation_limit'
   AND NOT quota_charged AND NOT initialized AND recipients=0 AND sent=0 AND failed=0
   AND attempt_count=0 AND NOT retrying AND next_attempt_at IS NULL AND waiting=0 AND exhausted=0 AND error IS NULL)
  OR (status<>'suppressed' AND suppression_reason IS NULL)) NOT VALID;

-- Dedicated TWO-int advisory namespace 72419556, separate from governance,
-- evaluation (72419507, one bigint) and webhook management (72419555, bigint).
-- Sort the actual signed lock keys, not UUIDs: hash collisions only serialize
-- unrelated buckets, never combine their allowances. Producers prelock their
-- entire bucket union before incident/delivery writes. The nonblocking fallback
-- also protects old rolling-upgrade replicas and direct SQL from lock inversions:
-- an inverted acquisition never waits (40001 rolls back the producer).
CREATE FUNCTION alert_delivery_lock_buckets(buckets uuid[]) RETURNS void LANGUAGE plpgsql
 SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE k integer; inverted boolean;
BEGIN
 IF current_setting('transaction_isolation')<>'read committed' THEN
  RAISE EXCEPTION 'alert delivery generation requires READ COMMITTED' USING ERRCODE='25001';
 END IF;
 FOR k IN SELECT DISTINCT public.omg_scope_key(b) FROM unnest(buckets) b WHERE b IS NOT NULL ORDER BY 1 LOOP
  SELECT EXISTS(SELECT FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid()
   AND classid=72419556 AND objsubid=2 AND granted
   AND (objid::bigint+2147483648)%4294967296-2147483648>k) INTO inverted;
  IF inverted THEN
   IF NOT pg_try_advisory_xact_lock(72419556,k) THEN
    RAISE EXCEPTION 'alert delivery bucket lock order; retry the transaction' USING ERRCODE='40001';
   END IF;
  ELSE
   PERFORM pg_advisory_xact_lock(72419556,k);
  END IF;
 END LOOP;
END $$;

-- VOLATILE is deliberate: statements after a lock wait must see newly
-- committed deliveries at READ COMMITTED, as well as earlier rows of a
-- multi-row INSERT. Bounded to 100 index entries by 0060's partial index.
CREATE FUNCTION alert_delivery_recent_count(bucket uuid, at_time timestamptz) RETURNS bigint
 LANGUAGE sql VOLATILE SET search_path = pg_catalog, public, pg_temp AS $$
 SELECT count(*) FROM (SELECT 1 FROM public.alert_deliveries
  WHERE bucket_id=bucket AND quota_charged
   AND created_at>at_time-interval '24 hours' AND created_at<=at_time LIMIT 100) recent
$$;

CREATE OR REPLACE FUNCTION alert_delivery_set_bucket() RETURNS trigger LANGUAGE plpgsql
 SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE at_time timestamptz;
BEGIN
 SELECT coalesce(e.workspace_id,'00000000-0000-0000-0000-000000000000'::uuid)
 INTO STRICT NEW.bucket_id FROM public.alert_events e WHERE e.id=NEW.event_id;
 PERFORM public.alert_delivery_lock_buckets(ARRAY[NEW.bucket_id]);
 at_time=clock_timestamp();
 -- All admission metadata is server-owned, even for old INSERT shapes or
 -- clients explicitly supplying forged timestamps, buckets or outcomes.
 NEW.created_at=at_time;
 NEW.quota_charged=false;
 NEW.suppression_reason=NULL;
 NEW.status='pending'; NEW.completed_at=NULL;
 NEW.initialized=false; NEW.recipients=0; NEW.sent=0; NEW.failed=0;
 NEW.error=NULL; NEW.attempt_count=0; NEW.retrying=false;
 NEW.next_attempt_at=NULL; NEW.waiting=0; NEW.exhausted=0;
 IF public.alert_delivery_recent_count(NEW.bucket_id,at_time)>=100 THEN
  NEW.status='suppressed';
  NEW.suppression_reason='workspace_generation_limit';
  NEW.completed_at=at_time;
 ELSE
  NEW.quota_charged=true;
 END IF;
 RETURN NEW;
END $$;

-- Keep the existing worker/rolling-upgrade guards unchanged; metadata is
-- immutable independently of terminal status and of the runtime column ACLs.
CREATE FUNCTION alert_delivery_generation_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.quota_charged IS DISTINCT FROM OLD.quota_charged
  OR NEW.suppression_reason IS DISTINCT FROM OLD.suppression_reason
  OR NEW.bucket_id<>OLD.bucket_id OR NEW.created_at<>OLD.created_at
  OR NEW.event_id<>OLD.event_id OR NEW.transition<>OLD.transition OR NEW.id<>OLD.id THEN
  RAISE EXCEPTION 'immutable alert generation metadata';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER alert_delivery_generation_immutable BEFORE UPDATE ON alert_deliveries
 FOR EACH ROW EXECUTE FUNCTION alert_delivery_generation_immutable();

CREATE OR REPLACE FUNCTION alert_outbox_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE parent alert_deliveries;
BEGIN
 IF TG_OP='INSERT' THEN
  -- The count below must see recipients committed by earlier writers: a
  -- REPEATABLE READ/SERIALIZABLE snapshot could count a stale total.
  IF current_setting('transaction_isolation')<>'read committed' THEN
   RAISE EXCEPTION 'alert recipient snapshots require READ COMMITTED' USING ERRCODE='25001';
  END IF;
  SELECT * INTO STRICT parent FROM alert_deliveries WHERE id=NEW.delivery_id FOR UPDATE;
  IF parent.status<>'pending' OR parent.initialized THEN
   RAISE EXCEPTION 'alert recipient snapshot already closed';
  END IF;
  IF (SELECT count(*) FROM (SELECT 1 FROM alert_recipient_outbox WHERE delivery_id=NEW.delivery_id LIMIT 50) q)>=50 THEN
   RAISE EXCEPTION 'maximum fifty alert snapshot recipients' USING ERRCODE='23514';
  END IF;
 ELSE
  IF NEW.delivery_id<>OLD.delivery_id OR NEW.fingerprint<>OLD.fingerprint OR NEW.internal_user_id IS DISTINCT FROM OLD.internal_user_id
   OR OLD.state IN ('sent','failed','ineligible','exhausted','not_configured')
   OR NEW.attempt_count<OLD.attempt_count OR NEW.attempt_count>OLD.attempt_count+1 THEN
   RAISE EXCEPTION 'immutable alert recipient identity or completed outcome';
  END IF;
 END IF;
 RETURN NEW;
END $$;
-- Historical rows (including pending) remain uncharged: never replay or refund.
RESET lock_timeout;
