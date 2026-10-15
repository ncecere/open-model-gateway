-- #106: durable audit export to a SIEM.
--
-- Every exported audit event is queued in the same transaction that writes
-- it (a trigger on audit_events), so the exporter never skips an event that
-- committed late. (An event archived with its partition before delivery is
-- skipped and counted by the exporter.) One
-- leased exporter delivers the queue in order and deletes rows only after the
-- receiver accepted them (at-least-once; consumers dedup on the event id).
--
-- Exported events are the platform audit view's: no workspace, or a team or
-- project workspace. Personal-workspace events are never queued. Nothing is
-- queued while export is disabled (no backfill).

CREATE TABLE audit_export_state(
 singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton),
 -- Set by each gateway at startup from its configuration.
 enabled boolean NOT NULL DEFAULT false,
 enabled_at timestamptz,
 -- Destination host only, for the status page (never the URL's path or query).
 destination_host text CHECK(destination_host IS NULL OR length(destination_host) <= 253),
 attempts integer NOT NULL DEFAULT 0 CHECK(attempts >= 0),
 next_attempt_at timestamptz NOT NULL DEFAULT '-infinity',
 last_success_at timestamptz,
 last_error text CHECK(last_error IS NULL OR last_error ~ '^[a-z_0-9]{1,40}$'),
 last_error_at timestamptz,
 delivered_total bigint NOT NULL DEFAULT 0 CHECK(delivered_total >= 0)
);
INSERT INTO audit_export_state DEFAULT VALUES;

CREATE TABLE audit_export_queue(
 event_id uuid PRIMARY KEY,
 event_created_at timestamptz NOT NULL,
 enqueued_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audit_export_queue_order ON audit_export_queue(event_created_at, event_id);

CREATE FUNCTION omg_audit_export_enqueue() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF (SELECT enabled FROM audit_export_state) AND (
      NEW.workspace_id IS NULL
      OR EXISTS(SELECT 1 FROM workspaces w WHERE w.id=NEW.workspace_id AND w.kind IN ('team','project'))
    ) THEN
  INSERT INTO audit_export_queue(event_id,event_created_at) VALUES(NEW.id,NEW.created_at)
  ON CONFLICT DO NOTHING;
 END IF;
 RETURN NULL;
END $$;
CREATE TRIGGER audit_events_export AFTER INSERT ON audit_events
 FOR EACH ROW EXECUTE FUNCTION omg_audit_export_enqueue();

INSERT INTO work_leases(name) VALUES('audit_export') ON CONFLICT DO NOTHING;
