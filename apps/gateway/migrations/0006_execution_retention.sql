-- Retain immutable accounting/ownership references while compacting optional execution detail.
ALTER TABLE inference_executions ADD COLUMN details_redacted_at TIMESTAMPTZ;
CREATE INDEX inference_retention_candidates ON inference_executions(completed_at,id)
    WHERE completed_at IS NOT NULL AND details_redacted_at IS NULL;
