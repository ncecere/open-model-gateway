-- #100: authorized key-expiry reminders (7-day and 1-day, built in, no new
-- rule/configuration surface). Explicit operator upgrade only (`migrate`).
--
-- Rolling-upgrade safety (condition of approval): an older gateway binary's
-- generic built-in visibility/delivery branches key off `builtin IS NOT
-- NULL` (feed/summary/read: `apps/gateway/src/management/alerts.rs`
-- `VISIBLE`) or off `rule_id`/`builtin` membership in fixed lists
-- (`workspace_events`, `platform_events`, `deliver_one`'s recipient SQL).
-- `key_expiring` events keep BOTH `rule_id` and `builtin` NULL (a shape no
-- pre-0049 query can match: the old `(rule_id IS NULL) <> (builtin IS
-- NULL)` CHECK below is relaxed only for this kind), so every old-binary
-- read path returns nothing for them and the old delivery worker's
-- recipient query (keyed off `alert_rules.id=$1` or
-- `alert_events.builtin=ANY(installation_builtins)`) resolves zero
-- recipients and records `no_recipients` (or `not_configured` without
-- SMTP) instead of guessing. An old worker can consume a pending email;
-- email remains best effort during overlap, never replayed. A new binary
-- recognizes `kind='key_expiring'` directly and applies the key-specific
-- visibility/recipient logic documented in docs/alerts.md. Old replicas
-- serving traffic for the few minutes a rolling upgrade takes therefore
-- never display or deliver these events to anyone, intended recipient or
-- not; they simply do not exist yet from an old replica's point of view.
SET lock_timeout = '60s';

ALTER TABLE alert_events
 ADD COLUMN api_key_id uuid REFERENCES api_keys(id),
 ADD COLUMN key_expires_at timestamptz,
 ADD COLUMN threshold_days integer;

ALTER TABLE alert_events DROP CONSTRAINT alert_events_kind_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_kind_check CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing',
 'scim_last_admin','batch_failed','batch_stalled','spend_threshold','partitions_missing','history_orphans','admission_ceiling','key_expiring'));

-- `key_expiring` has neither a rule nor a `builtin` label (the safety
-- mechanism above); every other kind keeps the original either/or shape.
ALTER TABLE alert_events DROP CONSTRAINT alert_events_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_check
 CHECK((kind='key_expiring' AND rule_id IS NULL AND builtin IS NULL) OR (kind<>'key_expiring' AND (rule_id IS NULL) <> (builtin IS NULL)));

-- Kind-specific shape: the three new columns travel together and only with
-- `key_expiring`; thresholds are fixed at 7 and 1 days.
ALTER TABLE alert_events ADD CONSTRAINT alert_events_key_expiring_shape
 CHECK((kind='key_expiring' AND api_key_id IS NOT NULL AND key_expires_at IS NOT NULL AND threshold_days IS NOT NULL AND level=threshold_days AND provider_connection_id IS NULL)
    OR (kind<>'key_expiring' AND api_key_id IS NULL AND key_expires_at IS NULL AND threshold_days IS NULL));
ALTER TABLE alert_events ADD CONSTRAINT alert_events_key_expiring_threshold
 CHECK(threshold_days IS NULL OR threshold_days IN (1,7));

-- Permanent deduplication (docs/alerts.md): once (credential, expiry,
-- threshold) has been recorded, it is never recorded again, including after
-- resolution (rotation or expiry change gets a new credential id or a new
-- `key_expires_at`, so a new tuple; restoring an identical expiry does not
-- replay it). This index is NOT partial on `resolved_at`, unlike
-- `alert_events_open` (which still gives the usual "one open incident"
-- behavior for `key_expiring`, since `rule_id`/`builtin` collapse to the
-- same sentinel for every row of this kind and `subject_key` carries the
-- key id).
CREATE UNIQUE INDEX alert_events_key_expiry_dedup ON alert_events(api_key_id,key_expires_at,threshold_days) WHERE kind='key_expiring';
-- Bounds the per-tick retirement scan of open key-expiry incidents.
CREATE INDEX alert_events_key_expiring_open ON alert_events(fired_at) WHERE kind='key_expiring' AND resolved_at IS NULL;

-- Attribution (api_key_id/key_expires_at/threshold_days) is immutable, like
-- every other identifying column on an incident.
CREATE OR REPLACE FUNCTION alert_event_resolve_once() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.resolved_at IS NOT NULL OR NEW.resolved_at IS NULL OR NEW.id<>OLD.id OR NEW.rule_id IS DISTINCT FROM OLD.rule_id
  OR NEW.builtin IS DISTINCT FROM OLD.builtin OR NEW.subject_key<>OLD.subject_key OR NEW.level<>OLD.level
  OR NEW.kind<>OLD.kind OR NEW.severity<>OLD.severity OR NEW.workspace_id IS DISTINCT FROM OLD.workspace_id
  OR NEW.provider_connection_id IS DISTINCT FROM OLD.provider_connection_id
  OR NEW.summary<>OLD.summary OR NEW.details<>OLD.details OR NEW.fired_at<>OLD.fired_at
  OR NEW.api_key_id IS DISTINCT FROM OLD.api_key_id OR NEW.key_expires_at IS DISTINCT FROM OLD.key_expires_at
  OR NEW.threshold_days IS DISTINCT FROM OLD.threshold_days THEN
  RAISE EXCEPTION 'alert events resolve once and are otherwise immutable';
 END IF;
 RETURN NEW;
END $$;

-- Scoped key/workspace relationship: a key-expiring incident's
-- `workspace_id` must be exactly the credential's own workspace (never a
-- different workspace, and never left unset).
CREATE FUNCTION alert_event_key_scope() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE v_ws uuid;
BEGIN
 IF NEW.kind='key_expiring' THEN
  SELECT workspace_id INTO v_ws FROM api_keys WHERE id=NEW.api_key_id;
  IF v_ws IS NULL OR NEW.workspace_id IS DISTINCT FROM v_ws THEN
   RAISE EXCEPTION 'key-expiring incident workspace must match the credential''s own workspace' USING ERRCODE='23514';
  END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER alert_events_key_scope BEFORE INSERT ON alert_events FOR EACH ROW EXECUTE FUNCTION alert_event_key_scope();

-- Candidate selection (evaluator): active, expiring keys ordered by expiry
-- so a bounded tick makes progress on the most urgent keys first and never
-- starves later candidates behind already-recorded ones.
CREATE INDEX api_keys_active_expiry ON api_keys(expires_at,id) WHERE revoked_at IS NULL AND disabled_at IS NULL AND expires_at IS NOT NULL;

-- Pre-existing gap (#168 predates this migration): `host_not_allowed` is a
-- real `DeliveryError` (`apps/gateway/src/email.rs`) but was never added to
-- the delivery outcome CHECK, so recording it rolled back the whole
-- delivery transaction instead of recording the failure.
ALTER TABLE alert_deliveries DROP CONSTRAINT alert_deliveries_error_check;
ALTER TABLE alert_deliveries ADD CONSTRAINT alert_deliveries_error_check
 CHECK(error IN ('credential','address','connection','tls','authentication','rejected','timeout','interrupted','host_not_allowed'));

RESET lock_timeout;
