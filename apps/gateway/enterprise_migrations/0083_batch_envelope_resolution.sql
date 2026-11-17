-- Explicit operator upgrade, while drained. No history is resolved here.
-- A gateway envelope pins no price and never carries line charges. Distinct
-- from 0082's free-price proof: NULL usage is retained on zero resolution.
ALTER TABLE governance_reservations ADD COLUMN batch_envelope_resolved boolean NOT NULL DEFAULT false;
ALTER TABLE governance_reservations DROP CONSTRAINT reservation_settled_usage;
ALTER TABLE governance_reservations
 ADD CONSTRAINT reservation_settled_usage CHECK (
  (state='settled' AND actual_microusd IS NOT NULL
   AND (free_price_resolved OR batch_envelope_resolved OR (input_tokens IS NOT NULL AND output_tokens IS NOT NULL)))
  OR (state<>'settled' AND actual_microusd IS NULL)),
 ADD CONSTRAINT reservation_batch_envelope_resolution CHECK (
  NOT batch_envelope_resolved OR (state='settled' AND actual_microusd=0
   AND held_microusd IS NOT NULL AND price_id IS NULL AND NOT unbounded_cost AND NOT free_price_resolved AND price_tier='standard'));
-- The NULL-usage exception is only an UNKNOWN terminal gateway envelope,
-- never an unpriced ordinary execution or a native provider attempt.
CREATE FUNCTION batch_envelope_resolution_guard() RETURNS trigger LANGUAGE plpgsql SET search_path=pg_catalog,public AS $$
BEGIN
 IF TG_OP='INSERT' THEN
  IF NEW.batch_envelope_resolved THEN
   RAISE EXCEPTION 'envelope resolution requires retained unknown history' USING ERRCODE='23514';
  END IF;
  RETURN NEW;
 END IF;
 IF OLD.batch_envelope_resolved AND NOT NEW.batch_envelope_resolved THEN
  RAISE EXCEPTION 'batch envelope resolution is permanent' USING ERRCODE='23514';
 END IF;
 IF NEW.batch_envelope_resolved AND NOT OLD.batch_envelope_resolved AND
  (OLD.state<>'unknown' OR NOT EXISTS(
   SELECT FROM async_jobs j JOIN inference_executions e ON e.id=j.execution_id AND e.started_at=NEW.admitted_at
   WHERE j.execution_id=NEW.execution_id AND j.workspace_id=NEW.workspace_id AND j.api_key_id=NEW.api_key_id AND j.deployment_id=NEW.deployment_id
    AND j.kind='batch' AND j.batch_mode='gateway' AND j.state IN('completed','failed','cancelled','expired') AND j.completed_at IS NOT NULL
    AND j.upstream_id IS NULL AND j.submit_started_at IS NULL
    AND e.workload_kind='batches' AND e.state<>'started' AND e.batch_job_id IS NULL)) THEN
  RAISE EXCEPTION 'resolution requires an unknown terminal gateway envelope' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER governance_reservations_batch_resolution BEFORE INSERT OR UPDATE OF batch_envelope_resolved ON governance_reservations FOR EACH ROW EXECUTE FUNCTION batch_envelope_resolution_guard();
