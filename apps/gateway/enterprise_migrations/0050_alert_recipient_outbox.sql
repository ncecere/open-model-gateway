-- Explicit post-v0.7.0 upgrade. Never replay historical terminal deliveries.
-- Fingerprints are delivery-scoped pseudonymous personal data, NOT anonymous.
-- No addresses, bodies, replies or credentials belong in this outbox.
ALTER TABLE alert_deliveries
 ADD COLUMN initialized boolean NOT NULL DEFAULT false,
 ADD COLUMN attempt_count integer NOT NULL DEFAULT 0 CHECK(attempt_count>=0),
 ADD COLUMN retrying boolean NOT NULL DEFAULT false,
 ADD COLUMN next_attempt_at timestamptz,
 ADD COLUMN waiting integer NOT NULL DEFAULT 0 CHECK(waiting>=0),
 ADD COLUMN exhausted integer NOT NULL DEFAULT 0 CHECK(exhausted>=0);
CREATE INDEX alert_deliveries_recent ON alert_deliveries(created_at);
CREATE TABLE alert_recipient_outbox(
 delivery_id uuid NOT NULL REFERENCES alert_deliveries(id),
 fingerprint bytea NOT NULL CHECK(octet_length(fingerprint)=32),
 -- Preserve internal eligibility after address changes/account cleanup erase
 -- the old email. Never returned to the API; identity is immutable.
 internal_user_id uuid REFERENCES users(id),
 state text NOT NULL DEFAULT 'waiting' CHECK(state IN ('waiting','leased','sent','failed','ineligible','exhausted','not_configured')),
 attempt_count integer NOT NULL DEFAULT 0 CHECK(attempt_count BETWEEN 0 AND 5),
 next_attempt_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 lease_owner uuid,
 lease_token uuid,
 lease_expires_at timestamptz,
 error text CHECK(error IN ('credential','address','connection','tls','authentication','rejected','timeout','interrupted','host_not_allowed')),
 PRIMARY KEY(delivery_id,fingerprint),
 CHECK((state='leased') = (lease_owner IS NOT NULL AND lease_token IS NOT NULL AND lease_expires_at IS NOT NULL)),
 CHECK(state='leased' OR (lease_owner IS NULL AND lease_token IS NULL AND lease_expires_at IS NULL)),
 CHECK(state<>'leased' OR attempt_count>0)
);
CREATE INDEX alert_recipient_outbox_due ON alert_recipient_outbox(next_attempt_at,delivery_id) WHERE state='waiting';
CREATE INDEX alert_recipient_outbox_expired ON alert_recipient_outbox(lease_expires_at,delivery_id) WHERE state='leased';
CREATE FUNCTION alert_outbox_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE parent alert_deliveries;
BEGIN
 IF TG_OP='INSERT' THEN
  SELECT * INTO STRICT parent FROM alert_deliveries WHERE id=NEW.delivery_id FOR UPDATE;
  IF parent.status<>'pending' OR parent.initialized THEN
   RAISE EXCEPTION 'alert recipient snapshot already closed';
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
CREATE TRIGGER alert_recipient_outbox_guard BEFORE INSERT OR UPDATE ON alert_recipient_outbox FOR EACH ROW EXECUTE FUNCTION alert_outbox_guard();
CREATE TRIGGER alert_recipient_outbox_no_delete BEFORE DELETE ON alert_recipient_outbox FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER alert_recipient_outbox_no_truncate BEFORE TRUNCATE ON alert_recipient_outbox FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();
CREATE FUNCTION alert_delivery_outbox_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF (OLD.initialized AND NOT NEW.initialized) OR (OLD.status<>'pending' AND NEW IS DISTINCT FROM OLD) THEN
  RAISE EXCEPTION 'completed alert deliveries cannot be replayed';
 END IF;
 -- Rolling-upgrade overlap: a pre-0050 binary's legacy deliver_one finalizes
 -- a delivery straight to a terminal status with one UPDATE, unaware of the
 -- outbox. Refuse that loudly while recipients are still waiting or leased,
 -- so mail is never silently lost or duplicated and complete() never
 -- no-ops against a delivery it can no longer see as pending. New code
 -- never trips this: refresh() only leaves 'pending' once every recipient
 -- is terminal.
 IF OLD.initialized AND NEW.status<>'pending' AND EXISTS(SELECT 1 FROM alert_recipient_outbox q WHERE q.delivery_id=OLD.id AND q.state IN ('waiting','leased')) THEN
  RAISE EXCEPTION 'alert delivery outbox still has waiting or leased recipients' USING ERRCODE='55006';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER alert_delivery_outbox_guard BEFORE UPDATE ON alert_deliveries FOR EACH ROW EXECUTE FUNCTION alert_delivery_outbox_guard();
