-- #45: platform-only destinations and a separate, retained webhook outbox.
-- Explicit upgrade only; no historical backfill and no worker in this slice.
CREATE TABLE alert_webhook_destinations(
 id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
 -- A partial unique slot enforces the cap even for stale RR snapshots.
 capacity_slot smallint NOT NULL CHECK(capacity_slot BETWEEN 1 AND 10),
 label text NOT NULL CHECK(char_length(label) BETWEEN 1 AND 120 AND label !~ '[[:cntrl:]]'),
 url text CHECK(octet_length(url) BETWEEN 1 AND 2048),
 current_secret_ref text CHECK(current_secret_ref ~ '^env:[A-Z_][A-Z0-9_]{0,127}$'),
 previous_secret_ref text CHECK(previous_secret_ref ~ '^env:[A-Z_][A-Z0-9_]{0,127}$'),
 previous_secret_until timestamptz,
 enabled boolean NOT NULL DEFAULT false,
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 deleted_at timestamptz,
 CHECK((previous_secret_ref IS NULL) = (previous_secret_until IS NULL)),
 CHECK(previous_secret_ref IS NULL OR previous_secret_ref<>current_secret_ref),
 CHECK((deleted_at IS NULL AND url IS NOT NULL AND current_secret_ref IS NOT NULL)
  OR (deleted_at IS NOT NULL AND NOT enabled AND url IS NULL AND current_secret_ref IS NULL
   AND previous_secret_ref IS NULL AND previous_secret_until IS NULL))
);
CREATE UNIQUE INDEX alert_webhook_destination_slots ON alert_webhook_destinations(capacity_slot) WHERE deleted_at IS NULL;
CREATE TABLE alert_webhook_outbox(
 id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
 event_id uuid REFERENCES alert_events(id),
 transition text CHECK(transition IN ('fired','resolved')),
 destination_id uuid NOT NULL REFERENCES alert_webhook_destinations(id),
 test boolean NOT NULL DEFAULT false,
 payload bytea NOT NULL CHECK(octet_length(payload) BETWEEN 1 AND 65536 AND convert_from(payload,'UTF8') IS NOT NULL),
 state text NOT NULL DEFAULT 'waiting' CHECK(state IN ('waiting','leased','sent','failed','exhausted','ineligible')),
 attempt_count integer NOT NULL DEFAULT 0 CONSTRAINT alert_webhook_outbox_attempt_count_check CHECK(attempt_count BETWEEN 0 AND 5 AND (NOT test OR attempt_count<=1)),
 created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 next_attempt_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 lease_owner uuid,
 lease_token uuid,
 lease_expires_at timestamptz,
 completed_at timestamptz,
 last_error text CHECK(last_error IN ('credential','connection','tls','rejected','timeout','interrupted','host_not_allowed','destination_policy')),
 UNIQUE(event_id,transition,destination_id),
 CHECK((test AND event_id IS NULL AND transition IS NULL) OR (NOT test AND event_id IS NOT NULL AND transition IS NOT NULL)),
 CHECK((state='leased') = (lease_owner IS NOT NULL AND lease_token IS NOT NULL AND lease_expires_at IS NOT NULL)),
 CHECK(state='leased' OR (lease_owner IS NULL AND lease_token IS NULL AND lease_expires_at IS NULL)),
 CHECK(state<>'leased' OR attempt_count>0),
 CHECK((state IN ('sent','failed','exhausted','ineligible')) = (completed_at IS NOT NULL))
);
CREATE INDEX alert_webhook_outbox_due ON alert_webhook_outbox(next_attempt_at,id) WHERE state='waiting';
CREATE INDEX alert_webhook_outbox_expired ON alert_webhook_outbox(lease_expires_at,id) WHERE state='leased';
CREATE INDEX alert_webhook_outbox_destination ON alert_webhook_outbox(destination_id,created_at DESC);
CREATE INDEX alert_webhook_outbox_waiting ON alert_webhook_outbox(destination_id) WHERE state='waiting';
CREATE INDEX alert_webhook_outbox_dead_letters ON alert_webhook_outbox(destination_id,state) WHERE state IN ('failed','exhausted');
CREATE INDEX alert_webhook_outbox_success ON alert_webhook_outbox(destination_id,completed_at DESC) WHERE state='sent';
CREATE INDEX alert_webhook_outbox_error ON alert_webhook_outbox(destination_id,created_at DESC,id DESC) WHERE last_error IS NOT NULL;

CREATE FUNCTION alert_webhook_destination_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='INSERT' THEN
  -- This lock belongs only to destination management, never admission.
  PERFORM pg_advisory_xact_lock(72419555);
  SELECT slot INTO NEW.capacity_slot FROM generate_series(1,10) slot
   WHERE NOT EXISTS(SELECT FROM alert_webhook_destinations d WHERE d.deleted_at IS NULL AND d.capacity_slot=slot)
   ORDER BY slot LIMIT 1;
  IF NEW.capacity_slot IS NULL THEN
   RAISE EXCEPTION 'maximum ten nondeleted webhook destinations' USING ERRCODE='23514';
  END IF;
 ELSE
  IF NEW.id<>OLD.id OR NEW.capacity_slot<>OLD.capacity_slot OR NEW.created_at<>OLD.created_at OR OLD.deleted_at IS NOT NULL
   OR NEW.url IS DISTINCT FROM OLD.url THEN
   RAISE EXCEPTION 'immutable webhook destination identity or deleted destination';
  END IF;
  IF NEW.deleted_at IS NOT NULL THEN
   -- Only the final tombstone transition may scrub these columns. Callers
   -- cannot combine deletion with a label, signing or enablement rewrite.
   IF NEW.enabled OR NEW.label<>OLD.label OR NEW.current_secret_ref IS DISTINCT FROM OLD.current_secret_ref
    OR NEW.previous_secret_ref IS DISTINCT FROM OLD.previous_secret_ref OR NEW.previous_secret_until IS DISTINCT FROM OLD.previous_secret_until THEN
    RAISE EXCEPTION 'webhook deletion only permits the scrub transition';
   END IF;
   NEW.url=NULL;
   NEW.current_secret_ref=NULL;
   NEW.previous_secret_ref=NULL;
   NEW.previous_secret_until=NULL;
  END IF;
 END IF;
 IF NEW.previous_secret_until IS NOT NULL AND (TG_OP='INSERT' OR NEW.previous_secret_until IS DISTINCT FROM OLD.previous_secret_until OR NEW.previous_secret_ref IS DISTINCT FROM OLD.previous_secret_ref) THEN
  IF NEW.previous_secret_until<=clock_timestamp() OR NEW.previous_secret_until>clock_timestamp()+interval '24 hours' THEN
   RAISE EXCEPTION 'webhook signing overlap must be within 24 hours' USING ERRCODE='23514';
  END IF;
 END IF;
 NEW.updated_at=clock_timestamp();
 RETURN NEW;
END $$;
CREATE TRIGGER alert_webhook_destination_guard BEFORE INSERT OR UPDATE ON alert_webhook_destinations FOR EACH ROW EXECUTE FUNCTION alert_webhook_destination_guard();
CREATE TRIGGER alert_webhook_destinations_no_delete BEFORE DELETE ON alert_webhook_destinations FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER alert_webhook_destinations_no_truncate BEFORE TRUNCATE ON alert_webhook_destinations FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();

CREATE FUNCTION alert_webhook_outbox_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='INSERT' THEN
  -- A plain live read, never a destination row lock. Slice 2 must repeat
  -- eligibility at claim time; a later disable may race this snapshot.
  IF NOT EXISTS(SELECT FROM alert_webhook_destinations d WHERE d.id=NEW.destination_id AND d.enabled AND d.deleted_at IS NULL) THEN
   RAISE EXCEPTION 'webhook destination is not eligible' USING ERRCODE='23514';
  END IF;
  IF NEW.test AND convert_from(NEW.payload,'UTF8')::jsonb IS DISTINCT FROM jsonb_build_object(
   'schema','omg.alert.v1','installation_id',(SELECT id FROM installation WHERE singleton),'delivery_id',NEW.id,'test',true,'event',NULL) THEN
   RAISE EXCEPTION 'invalid webhook test envelope' USING ERRCODE='23514';
  END IF;
 ELSE
 IF NEW.id<>OLD.id OR NEW.event_id IS DISTINCT FROM OLD.event_id OR NEW.transition IS DISTINCT FROM OLD.transition
  OR NEW.destination_id<>OLD.destination_id OR NEW.test<>OLD.test OR NEW.payload<>OLD.payload OR NEW.created_at<>OLD.created_at
  OR OLD.state IN ('sent','failed','exhausted','ineligible')
  OR NEW.attempt_count<OLD.attempt_count OR NEW.attempt_count>OLD.attempt_count+1 THEN
  RAISE EXCEPTION 'immutable webhook payload, identity or terminal outcome';
 END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER alert_webhook_outbox_guard BEFORE INSERT OR UPDATE ON alert_webhook_outbox FOR EACH ROW EXECUTE FUNCTION alert_webhook_outbox_guard();
CREATE TRIGGER alert_webhook_outbox_no_delete BEFORE DELETE ON alert_webhook_outbox FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER alert_webhook_outbox_no_truncate BEFORE TRUNCATE ON alert_webhook_outbox FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();

CREATE FUNCTION alert_webhook_destination_ineligible() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NOT NEW.enabled OR NEW.deleted_at IS NOT NULL THEN
  -- An already authorized send may finish; unclaimed rows never replay.
  UPDATE alert_webhook_outbox SET state='ineligible',completed_at=clock_timestamp()
   WHERE destination_id=NEW.id AND state='waiting';
 END IF;
 RETURN NULL;
END $$;
CREATE TRIGGER alert_webhook_destination_ineligible AFTER UPDATE ON alert_webhook_destinations FOR EACH ROW EXECUTE FUNCTION alert_webhook_destination_ineligible();

-- Explicit kind-only projection. Future kinds remain ineligible until reviewed.
CREATE FUNCTION alert_webhook_summary(text) RETURNS text LANGUAGE sql IMMUTABLE STRICT AS $$
 SELECT CASE $1
  WHEN 'budget_threshold' THEN 'A shared workspace budget threshold was reached'
  WHEN 'spend_threshold' THEN 'An installation spend threshold was reached'
  WHEN 'spend_spike' THEN 'A spend spike was detected'
  WHEN 'error_rate' THEN 'An error rate threshold was reached'
  WHEN 'provider_failing' THEN 'An upstream connection is failing'
  WHEN 'batch_failed' THEN 'A batch failed'
  WHEN 'batch_stalled' THEN 'A batch stalled'
  WHEN 'scim_last_admin' THEN 'SCIM tried to remove the last Platform Admin'
  WHEN 'partitions_missing' THEN 'Future history partitions are missing'
  WHEN 'history_orphans' THEN 'History parent verification found orphan rows'
  ELSE NULL
 END
$$;

CREATE FUNCTION alert_webhook_enqueue() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE e alert_events; d record; delivery uuid; installation_id uuid; safe_summary text;
BEGIN
 -- Webhooks are best effort and cannot break email or built-in evaluation,
 -- including the explicit migrate-to-grants window. This subtransaction
 -- rolls back all webhook inserts on failure, without undoing the delivery.
 BEGIN
 IF NOT EXISTS(SELECT FROM alert_webhook_destinations WHERE enabled AND deleted_at IS NULL) THEN
  RETURN NULL;
 END IF;
 SELECT * INTO STRICT e FROM alert_events WHERE id=NEW.event_id;
 -- Deliberately narrower than platform history. Never copy summary, details
 -- or subject_key: even installation rules can refer to individual objects.
 IF e.kind IN ('key_expiring','admission_ceiling')
  OR (NEW.transition='resolved' AND e.resolution IS DISTINCT FROM 'cleared')
  OR (e.workspace_id IS NOT NULL AND NOT EXISTS(SELECT FROM workspaces w WHERE w.id=e.workspace_id AND w.kind IN ('team','project') AND w.disabled_at IS NULL))
  OR NOT (
   (e.rule_id IS NOT NULL AND EXISTS(SELECT FROM alert_rules r WHERE r.id=e.rule_id AND r.scope='installation' AND r.enabled AND r.deleted_at IS NULL AND r.notify_platform_admins))
   OR (e.rule_id IS NULL AND e.workspace_id IS NULL AND e.builtin IN ('scim_last_admin','partitions_missing','history_orphans'))
  ) THEN RETURN NULL; END IF;
 safe_summary=alert_webhook_summary(e.kind);
 -- Fail closed for future kinds until their projection is reviewed.
 IF safe_summary IS NULL THEN RETURN NULL; END IF;
 SELECT id INTO STRICT installation_id FROM installation WHERE singleton;
 -- READ COMMITTED snapshots do not lock destinations. A concurrent
 -- disable/delete is resolved by slice 2's mandatory claim-time recheck.
 -- If a claim locks both resources, destination precedes outbox row.
 FOR d IN SELECT id FROM alert_webhook_destinations WHERE enabled AND deleted_at IS NULL ORDER BY id LOOP
  delivery=gen_random_uuid();
  INSERT INTO alert_webhook_outbox(id,event_id,transition,destination_id,payload)
   VALUES(delivery,e.id,NEW.transition,d.id,convert_to(jsonb_build_object(
    'schema','omg.alert.v1','installation_id',installation_id,'delivery_id',delivery,'test',false,
    'event',jsonb_build_object('id',e.id,'transition',NEW.transition,'kind',e.kind,'severity',e.severity,
     'occurred_at',to_char((CASE WHEN NEW.transition='resolved' THEN e.resolved_at ELSE e.fired_at END) AT TIME ZONE 'UTC','YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
     'summary',safe_summary))::text,'UTF8')) ON CONFLICT(event_id,transition,destination_id) DO NOTHING;
 END LOOP;
 EXCEPTION WHEN OTHERS THEN
  -- Never interpolate error text: it may contain URLs, references or bodies.
  RAISE WARNING 'alert webhook enqueue failed; email delivery preserved';
 END;
 RETURN NULL;
END $$;
CREATE TRIGGER alert_deliveries_webhook_enqueue AFTER INSERT ON alert_deliveries FOR EACH ROW EXECUTE FUNCTION alert_webhook_enqueue();
