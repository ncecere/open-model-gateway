-- Provider IDs are registry keys, not a database enum. Adding an adapter must not
-- require changing the engine or applying a provider-specific migration.
ALTER TABLE provider_connections DROP CONSTRAINT provider_connections_provider_check;
ALTER TABLE provider_connections ADD CONSTRAINT provider_identifier
    CHECK (provider ~ '^[a-z0-9_]{1,64}$');
ALTER TABLE api_keys ADD UNIQUE (organization_id, workspace_id, id);

-- Execution accounting, NOT financial billing. A durable 'started' row precedes
-- every outbound attempt. Crashed processes may leave started rows for reconciliation.
CREATE TABLE inference_executions (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    api_key_id UUID NOT NULL,
    deployment_id UUID NOT NULL,
    public_model TEXT NOT NULL,
    provider TEXT NOT NULL,
    streamed BOOLEAN NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('started', 'succeeded', 'failed', 'cancelled')),
    error_code TEXT,
    input_tokens BIGINT CHECK (input_tokens >= 0),
    output_tokens BIGINT CHECK (output_tokens >= 0),
    elapsed_ms BIGINT CHECK (elapsed_ms >= 0),
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    FOREIGN KEY (organization_id, workspace_id, api_key_id) REFERENCES api_keys(organization_id, workspace_id, id),
    FOREIGN KEY (organization_id, deployment_id) REFERENCES deployments(organization_id, id),
    CHECK ((state = 'started' AND completed_at IS NULL) OR (state <> 'started' AND completed_at IS NOT NULL))
);
CREATE INDEX inference_workspace_time ON inference_executions(organization_id, workspace_id, started_at);
CREATE INDEX inference_unfinished ON inference_executions(started_at) WHERE state = 'started';
