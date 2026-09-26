-- Infrastructure is platform-owned. Nullable organization_id columns below are
-- legacy provenance / trusted bootstrap input ONLY, never an authorization boundary.
-- Runtime catalog writers take pg_advisory_xact_lock(72419502); admissions and
-- entitlement writers take its SHARED variant BEFORE the consuming organization lock.
CREATE TABLE organization_model_grants (
    organization_id UUID NOT NULL REFERENCES organizations(id),
    model_id UUID NOT NULL,
    public_name TEXT NOT NULL CHECK (length(public_name) BETWEEN 1 AND 200),
    personal_enabled BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (organization_id, model_id),
    UNIQUE (organization_id, public_name),
    -- Deferred so the trusted BEFORE INSERT bootstrap trigger can preserve the
    -- original org alias before assigning a collision-free global canonical alias.
    FOREIGN KEY (model_id) REFERENCES models(id) DEFERRABLE INITIALLY DEFERRED
);
INSERT INTO organization_model_grants (organization_id,model_id,public_name,personal_enabled)
SELECT organization_id,id,public_name,personal_enabled FROM models;

-- Preserve IDs and org aliases; only duplicate global canonical names change.
DO $$
DECLARE item RECORD; candidate TEXT; suffix INTEGER;
BEGIN
    FOR item IN SELECT id,public_name FROM (
        SELECT id,public_name,row_number() OVER (PARTITION BY public_name ORDER BY id) AS n
        FROM models
    ) duplicates WHERE n>1 ORDER BY id LOOP
        suffix := 0;
        LOOP
            candidate := left(item.public_name,150)||'--'||item.id::text||
                         CASE WHEN suffix=0 THEN '' ELSE '-'||suffix::text END;
            EXIT WHEN NOT EXISTS (SELECT 1 FROM models WHERE public_name=candidate);
            suffix := suffix+1;
        END LOOP;
        UPDATE models SET public_name=candidate WHERE id=item.id;
    END LOOP;
END $$;
ALTER TABLE models ADD CONSTRAINT models_global_public_name_key UNIQUE (public_name);

ALTER TABLE deployments DROP CONSTRAINT deployments_organization_id_model_id_fkey;
ALTER TABLE deployments DROP CONSTRAINT deployments_organization_id_provider_connection_id_fkey;
ALTER TABLE deployments ADD FOREIGN KEY (model_id) REFERENCES models(id);
ALTER TABLE deployments ADD FOREIGN KEY (provider_connection_id) REFERENCES provider_connections(id);
ALTER TABLE workspace_model_grants DROP CONSTRAINT workspace_model_grants_organization_id_model_id_fkey;
ALTER TABLE workspace_model_grants ADD FOREIGN KEY (organization_id,model_id)
    REFERENCES organization_model_grants(organization_id,model_id);
CREATE TABLE user_model_grants (
    organization_id UUID NOT NULL,
    user_id UUID NOT NULL,
    model_id UUID NOT NULL,
    PRIMARY KEY (organization_id,user_id,model_id),
    FOREIGN KEY (organization_id,user_id) REFERENCES organization_memberships(organization_id,user_id),
    FOREIGN KEY (organization_id,model_id) REFERENCES organization_model_grants(organization_id,model_id)
);
ALTER TABLE inference_executions DROP CONSTRAINT inference_executions_organization_id_deployment_id_fkey;
ALTER TABLE inference_executions ADD FOREIGN KEY (deployment_id) REFERENCES deployments(id);
ALTER TABLE deployment_prices DROP CONSTRAINT deployment_prices_organization_id_deployment_id_fkey;
ALTER TABLE deployment_prices ADD FOREIGN KEY (deployment_id) REFERENCES deployments(id);
ALTER TABLE deployment_prices ADD UNIQUE (deployment_id,id);
-- PostgreSQL truncates generated constraint names; identify this FK by target.
DO $$ DECLARE fk RECORD; BEGIN
    FOR fk IN SELECT conname FROM pg_constraint
        WHERE conrelid='governance_reservations'::regclass AND confrelid='deployment_prices'::regclass AND contype='f'
    LOOP EXECUTE format('ALTER TABLE governance_reservations DROP CONSTRAINT %I',fk.conname); END LOOP;
END $$;
ALTER TABLE governance_reservations ADD FOREIGN KEY (deployment_id,price_id) REFERENCES deployment_prices(deployment_id,id);

ALTER TABLE model_routing_policies DROP CONSTRAINT model_routing_policies_organization_id_model_id_fkey;
ALTER TABLE model_routing_policies DROP CONSTRAINT model_routing_policies_pkey;
ALTER TABLE model_routing_policies ADD PRIMARY KEY (model_id);
ALTER TABLE model_routing_policies ADD FOREIGN KEY (model_id) REFERENCES models(id) ON DELETE CASCADE;
ALTER TABLE deployment_routing DROP CONSTRAINT deployment_routing_organization_id_deployment_id_fkey;
ALTER TABLE deployment_routing DROP CONSTRAINT deployment_routing_pkey;
ALTER TABLE deployment_routing ADD PRIMARY KEY (deployment_id);
ALTER TABLE deployment_routing ADD FOREIGN KEY (deployment_id) REFERENCES deployments(id) ON DELETE CASCADE;
ALTER TABLE deployment_route_health DROP CONSTRAINT deployment_route_health_organization_id_deployment_id_fkey;
ALTER TABLE deployment_route_health DROP CONSTRAINT deployment_route_health_pkey;
ALTER TABLE deployment_route_health ADD PRIMARY KEY (deployment_id);
ALTER TABLE deployment_route_health ADD FOREIGN KEY (deployment_id) REFERENCES deployments(id) ON DELETE CASCADE;

ALTER TABLE provider_connections ALTER COLUMN organization_id DROP NOT NULL;
ALTER TABLE models ALTER COLUMN organization_id DROP NOT NULL;
ALTER TABLE deployments ALTER COLUMN organization_id DROP NOT NULL;
ALTER TABLE deployment_prices ALTER COLUMN organization_id DROP NOT NULL;
ALTER TABLE model_routing_policies ALTER COLUMN organization_id DROP NOT NULL;
ALTER TABLE deployment_routing ALTER COLUMN organization_id DROP NOT NULL;
ALTER TABLE deployment_route_health ALTER COLUMN organization_id DROP NOT NULL;
COMMENT ON COLUMN models.organization_id IS 'Legacy provenance / trusted INSERT bootstrap input only; never authorization';
COMMENT ON COLUMN models.personal_enabled IS 'Deprecated trusted INSERT bootstrap input only; organization_model_grants is authoritative';
COMMENT ON COLUMN provider_connections.organization_id IS 'Legacy provenance only; never authorization';
COMMENT ON COLUMN deployments.organization_id IS 'Legacy provenance only; never authorization';
COMMENT ON COLUMN deployment_prices.organization_id IS 'Immutable legacy provenance only; never authorization';
COMMENT ON COLUMN model_routing_policies.organization_id IS 'Legacy provenance only; never authorization';
COMMENT ON COLUMN deployment_routing.organization_id IS 'Legacy provenance only; never authorization';
COMMENT ON COLUMN deployment_route_health.organization_id IS 'Legacy provenance only; never authorization';
CREATE INDEX deployments_global_model ON deployments(model_id);
CREATE INDEX deployment_prices_global_latest ON deployment_prices(deployment_id,created_at DESC,id DESC);

CREATE FUNCTION bootstrap_organization_model_grant() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE original_name TEXT; suffix INTEGER := 0;
BEGIN
    IF NEW.organization_id IS NOT NULL THEN
        -- Trusted SQL/bootstrap provisioning compatibility, NOT a request fallback.
        -- Fresh platform API INSERTs omit organization_id and receive no grant.
        PERFORM pg_advisory_xact_lock(72419502);
        original_name := NEW.public_name;
        INSERT INTO organization_model_grants (organization_id,model_id,public_name,personal_enabled)
        VALUES (NEW.organization_id,NEW.id,original_name,NEW.personal_enabled);
        WHILE EXISTS (SELECT 1 FROM models WHERE public_name=NEW.public_name) LOOP
            NEW.public_name := left(original_name,150)||'--'||NEW.id::text||
                CASE WHEN suffix=0 THEN '' ELSE '-'||suffix::text END;
            suffix := suffix+1;
        END LOOP;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER model_bootstrap_entitlement BEFORE INSERT ON models
    FOR EACH ROW EXECUTE FUNCTION bootstrap_organization_model_grant();

CREATE TABLE platform_organization_policies (
    organization_id UUID PRIMARY KEY REFERENCES organizations(id),
    requests_per_minute BIGINT CHECK (requests_per_minute > 0),
    tokens_per_minute BIGINT CHECK (tokens_per_minute > 0),
    concurrent_requests BIGINT CHECK (concurrent_requests > 0),
    monthly_budget_microusd BIGINT CHECK (monthly_budget_microusd > 0)
);
INSERT INTO platform_organization_policies
    (organization_id,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd)
SELECT organization_id,requests_per_minute,tokens_per_minute,concurrent_requests,monthly_budget_microusd
FROM governance_policies WHERE scope='organization';
-- No UPDATE/DELETE of deployment_prices or monetary_ledger. Existing immutable
-- history triggers and all consuming tenant/workspace/key/execution FKs remain.
