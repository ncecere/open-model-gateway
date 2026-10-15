-- #174: gap detection for audit export receivers.
--
-- The leased exporter numbers queued events (seq) before their first delivery
-- attempt, from a counter that only grows: numbers are never reused or
-- rewritten, so a receiver that sees a gap knows events were lost (archived
-- before delivery, no longer platform-visible, or removed from the queue).
-- Numbers are assigned by the exporter, not the enqueue trigger, so audit
-- writes never wait on one counter row. An idle export sends a signed
-- heartbeat (last_heartbeat_at) while export is enabled.
--
-- Turning export off or on writes its audit marker in the database, for every
-- role: `audit_export.disabled` is queued before the change, `enabled` after,
-- so even a direct UPDATE of the state shows in the export.

-- Every audit insert reads audit_export_state (enqueue trigger): don't queue
-- behind a long transaction while altering it.
SET lock_timeout = '10s';

ALTER TABLE audit_export_state
 -- Below 2^53: exact in every JSON receiver.
 ADD COLUMN next_seq bigint NOT NULL DEFAULT 1 CHECK(next_seq BETWEEN 1 AND 9007199254740992),
 ADD COLUMN last_heartbeat_at timestamptz;
ALTER TABLE audit_export_queue ADD COLUMN seq bigint UNIQUE CHECK(seq IS NULL OR seq BETWEEN 1 AND 9007199254740991);
-- Numbering reads only the unnumbered rows, oldest first.
CREATE INDEX audit_export_queue_unnumbered ON audit_export_queue(event_created_at, event_id) WHERE seq IS NULL;

CREATE FUNCTION omg_audit_export_sequence_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_TABLE_NAME = 'audit_export_state' THEN
  IF NEW.next_seq < OLD.next_seq THEN
   RAISE EXCEPTION 'audit export sequence numbers are never reused';
  END IF;
 ELSIF TG_OP = 'INSERT' THEN
  IF NEW.seq IS NOT NULL THEN
   RAISE EXCEPTION 'audit export sequence numbers are assigned by the exporter';
  END IF;
 ELSIF NEW.seq IS DISTINCT FROM OLD.seq THEN
  -- Set once, and only from the unassigned range: a removed event's number
  -- can't be given to another one to hide the gap.
  IF OLD.seq IS NOT NULL OR NEW.seq < (SELECT next_seq FROM audit_export_state) THEN
   RAISE EXCEPTION 'audit export sequence numbers are written once';
  END IF;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER audit_export_state_sequence BEFORE UPDATE ON audit_export_state
 FOR EACH ROW EXECUTE FUNCTION omg_audit_export_sequence_guard();
CREATE TRIGGER audit_export_queue_sequence BEFORE INSERT OR UPDATE ON audit_export_queue
 FOR EACH ROW EXECUTE FUNCTION omg_audit_export_sequence_guard();

-- The on/off markers (previously written by the gateway at startup).
-- BEFORE for off (the state still says on, so the marker is queued), AFTER
-- for on (the state already says on).
CREATE FUNCTION omg_audit_export_marker_before() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.enabled AND NOT NEW.enabled THEN
  INSERT INTO audit_events(id,action,resource_type,metadata)
  VALUES(gen_random_uuid(), 'audit_export.disabled', 'installation', '{}');
 END IF;
 RETURN NEW;
END $$;
CREATE FUNCTION omg_audit_export_marker_after() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.enabled AND NOT OLD.enabled THEN
  INSERT INTO audit_events(id,action,resource_type,metadata)
  VALUES(gen_random_uuid(), 'audit_export.enabled', 'installation', '{}');
 END IF;
 RETURN NULL;
END $$;
CREATE TRIGGER audit_export_state_off BEFORE UPDATE OF enabled ON audit_export_state
 FOR EACH ROW EXECUTE FUNCTION omg_audit_export_marker_before();
CREATE TRIGGER audit_export_state_on AFTER UPDATE OF enabled ON audit_export_state
 FOR EACH ROW EXECUTE FUNCTION omg_audit_export_marker_after();

RESET lock_timeout;
