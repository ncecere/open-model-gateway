-- All governance writers first lock organizations.id FOR NO KEY UPDATE.
ALTER TABLE inference_executions ADD COLUMN root_request_id UUID;
UPDATE inference_executions SET root_request_id=id;
ALTER TABLE inference_executions ALTER COLUMN root_request_id SET NOT NULL;
ALTER TABLE inference_executions ADD COLUMN attempt_number INTEGER NOT NULL DEFAULT 1 CHECK (attempt_number BETWEEN 1 AND 3);
ALTER TABLE inference_executions ADD UNIQUE (root_request_id,attempt_number);
-- Key rotation preserves policy and usage lineage; creating a distinct key does not.
ALTER TABLE api_keys ADD COLUMN governance_key_id UUID;
UPDATE api_keys SET governance_key_id=id;
ALTER TABLE api_keys ALTER COLUMN governance_key_id SET NOT NULL;
ALTER TABLE api_keys ADD FOREIGN KEY (organization_id,workspace_id,governance_key_id) REFERENCES api_keys(organization_id,workspace_id,id);
CREATE INDEX api_key_governance_lineage ON api_keys(organization_id,workspace_id,governance_key_id);
CREATE FUNCTION initialize_key_lineage() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.governance_key_id := coalesce(NEW.governance_key_id,NEW.id);
    RETURN NEW;
END;
$$;
CREATE TRIGGER api_key_lineage_default BEFORE INSERT ON api_keys FOR EACH ROW EXECUTE FUNCTION initialize_key_lineage();

CREATE TABLE governance_policies (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES organizations(id),
    scope TEXT NOT NULL CHECK (scope IN ('organization','workspace','key')),
    workspace_id UUID,
    api_key_id UUID,
    requests_per_minute BIGINT CHECK (requests_per_minute > 0),
    tokens_per_minute BIGINT CHECK (tokens_per_minute > 0),
    concurrent_requests BIGINT CHECK (concurrent_requests > 0),
    monthly_budget_microusd BIGINT CHECK (monthly_budget_microusd > 0),
    CHECK ((scope='organization' AND workspace_id IS NULL AND api_key_id IS NULL)
        OR (scope='workspace' AND workspace_id IS NOT NULL AND api_key_id IS NULL)
        OR (scope='key' AND workspace_id IS NOT NULL AND api_key_id IS NOT NULL)),
    FOREIGN KEY (organization_id,workspace_id) REFERENCES workspaces(organization_id,id),
    FOREIGN KEY (organization_id,workspace_id,api_key_id) REFERENCES api_keys(organization_id,workspace_id,id)
);
CREATE UNIQUE INDEX governance_org_policy ON governance_policies(organization_id) WHERE scope='organization';
CREATE UNIQUE INDEX governance_workspace_policy ON governance_policies(organization_id,workspace_id) WHERE scope='workspace';
CREATE UNIQUE INDEX governance_key_policy ON governance_policies(organization_id,workspace_id,api_key_id) WHERE scope='key';

CREATE TABLE deployment_prices (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    deployment_id UUID NOT NULL,
    input_microusd_per_million BIGINT NOT NULL CHECK (input_microusd_per_million >= 0),
    output_microusd_per_million BIGINT NOT NULL CHECK (output_microusd_per_million >= 0),
    input_token_limit BIGINT NOT NULL CHECK (input_token_limit > 0),
    output_token_limit BIGINT NOT NULL CHECK (output_token_limit > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (organization_id,deployment_id,id),
    FOREIGN KEY (organization_id,deployment_id) REFERENCES deployments(organization_id,id)
);
CREATE INDEX deployment_price_latest ON deployment_prices(organization_id,deployment_id,created_at DESC,id DESC);
ALTER TABLE inference_executions ADD UNIQUE (organization_id,workspace_id,api_key_id,deployment_id,id);
CREATE TABLE governance_reservations (
    execution_id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    api_key_id UUID NOT NULL,
    deployment_id UUID NOT NULL,
    price_id UUID,
    admitted_at TIMESTAMPTZ NOT NULL,
    minute_start TIMESTAMPTZ NOT NULL,
    month_start TIMESTAMPTZ NOT NULL,
    lease_expires_at TIMESTAMPTZ NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending','unknown','settled')),
    reserved_tokens BIGINT CHECK (reserved_tokens >= 0),
    held_microusd BIGINT CHECK (held_microusd >= 0),
    actual_microusd BIGINT CHECK (actual_microusd >= 0),
    input_tokens BIGINT CHECK (input_tokens >= 0),
    output_tokens BIGINT CHECK (output_tokens >= 0),
    UNIQUE (organization_id,execution_id),
    CHECK ((price_id IS NULL AND held_microusd IS NULL) OR (price_id IS NOT NULL AND held_microusd IS NOT NULL)),
    CHECK ((state='settled' AND actual_microusd IS NOT NULL AND input_tokens IS NOT NULL AND output_tokens IS NOT NULL)
        OR (state<>'settled' AND actual_microusd IS NULL)),
    FOREIGN KEY (organization_id,workspace_id,api_key_id,deployment_id,execution_id)
        REFERENCES inference_executions(organization_id,workspace_id,api_key_id,deployment_id,id),
    FOREIGN KEY (organization_id,deployment_id,price_id) REFERENCES deployment_prices(organization_id,deployment_id,id)
);
CREATE INDEX governance_minute ON governance_reservations(organization_id,minute_start);
CREATE INDEX governance_month ON governance_reservations(organization_id,month_start);
CREATE INDEX governance_leases ON governance_reservations(lease_expires_at) WHERE state='pending';

-- Amounts are USD micro-units, never floating point. These are events, NOT
-- additive account balances: settlement/reconciliation replaces the earlier hold.
CREATE TABLE monetary_ledger (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    execution_id UUID NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('hold','unknown','settlement','reconciliation')),
    amount_microusd BIGINT CHECK (amount_microusd >= 0),
    input_tokens BIGINT CHECK (input_tokens >= 0),
    output_tokens BIGINT CHECK (output_tokens >= 0),
    evidence TEXT CHECK (length(evidence) BETWEEN 1 AND 200),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CHECK ((kind IN ('hold','settlement','reconciliation') AND amount_microusd IS NOT NULL) OR kind='unknown'),
    CHECK (kind <> 'reconciliation' OR (evidence IS NOT NULL AND input_tokens IS NOT NULL AND output_tokens IS NOT NULL)),
    UNIQUE (organization_id,execution_id,kind),
    FOREIGN KEY (organization_id,execution_id) REFERENCES governance_reservations(organization_id,execution_id)
);
CREATE INDEX monetary_ledger_org_time ON monetary_ledger(organization_id,created_at,id);

-- Preserve pre-governance traffic as UNKNOWN, never free. Existing pending
-- attempts get the maximum supported engine deadline plus teardown margin.
INSERT INTO governance_reservations
    (execution_id,organization_id,workspace_id,api_key_id,deployment_id,
     admitted_at,minute_start,month_start,lease_expires_at,state,input_tokens,output_tokens)
SELECT id,organization_id,workspace_id,api_key_id,deployment_id,
       started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),
       started_at+interval '3605 seconds',CASE WHEN state='started' THEN 'pending' ELSE 'unknown' END,
       input_tokens,output_tokens
FROM inference_executions;
INSERT INTO monetary_ledger (id,organization_id,execution_id,kind,input_tokens,output_tokens)
SELECT gen_random_uuid(),organization_id,execution_id,'unknown',input_tokens,output_tokens
FROM governance_reservations;

CREATE FUNCTION governance_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'immutable governance history';
END;
$$;
CREATE TRIGGER deployment_prices_immutable BEFORE UPDATE OR DELETE ON deployment_prices
    FOR EACH ROW EXECUTE FUNCTION governance_immutable();
CREATE TRIGGER deployment_prices_no_truncate BEFORE TRUNCATE ON deployment_prices
    FOR EACH STATEMENT EXECUTE FUNCTION governance_immutable();
CREATE TRIGGER monetary_ledger_immutable BEFORE UPDATE OR DELETE ON monetary_ledger
    FOR EACH ROW EXECUTE FUNCTION governance_immutable();
CREATE TRIGGER monetary_ledger_no_truncate BEFORE TRUNCATE ON monetary_ledger
    FOR EACH STATEMENT EXECUTE FUNCTION governance_immutable();
