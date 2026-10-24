-- #253 slice B: explicit operator upgrade only; serve never migrates.
-- Keep this BEFORE INSERT trigger independent of any AFTER INSERT delivery
-- consumers (including webhook enqueue). Old replicas may omit bucket_id.
SET lock_timeout='60s';
ALTER TABLE alert_deliveries
 ADD COLUMN bucket_id uuid NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000';

CREATE FUNCTION alert_delivery_set_bucket() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 SELECT coalesce(e.workspace_id,'00000000-0000-0000-0000-000000000000'::uuid)
 INTO STRICT NEW.bucket_id FROM alert_events e WHERE e.id=NEW.event_id;
 RETURN NEW;
END $$;
CREATE TRIGGER alert_delivery_set_bucket BEFORE INSERT ON alert_deliveries
 FOR EACH ROW EXECUTE FUNCTION alert_delivery_set_bucket();

-- Terminal deliveries are immutable history; their default nil bucket is
-- intentionally never corrected, and no delivery is reopened or replayed.
UPDATE alert_deliveries d
 SET bucket_id=coalesce(e.workspace_id,'00000000-0000-0000-0000-000000000000'::uuid)
 FROM alert_events e WHERE e.id=d.event_id AND d.status='pending';
