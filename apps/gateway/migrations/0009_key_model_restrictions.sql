-- Absence of a header inherits live grants; an empty header denies every model.
-- Restrictions belong to governance lineage, not to the rotating credential.
CREATE TABLE key_model_restrictions (
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    governance_key_id UUID NOT NULL,
    PRIMARY KEY (organization_id, workspace_id, governance_key_id),
    FOREIGN KEY (organization_id, workspace_id, governance_key_id)
        REFERENCES api_keys(organization_id, workspace_id, id)
);
CREATE TABLE key_model_selections (
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    governance_key_id UUID NOT NULL,
    model_id UUID NOT NULL,
    PRIMARY KEY (organization_id, workspace_id, governance_key_id, model_id),
    FOREIGN KEY (organization_id, workspace_id, governance_key_id)
        REFERENCES key_model_restrictions(organization_id, workspace_id, governance_key_id)
        ON DELETE CASCADE,
    FOREIGN KEY (organization_id, model_id)
        REFERENCES organization_model_grants(organization_id, model_id)
        ON DELETE CASCADE
);
CREATE INDEX key_model_selections_entitlement ON key_model_selections(organization_id, model_id);
-- Entitlement removal deletes selections only, NEVER the header. Reassignment
-- cannot revive removed selections or convert a restricted key to inheritance.
-- Creation and entitlement mutation serialize on catalog then organization locks;
-- rotation/revocation serialize on the organization lock. Admission holds both.
-- Thus even a missing-header read is protected without a row/gap lock. Discovery
-- uses a statement snapshot and admission always rechecks before reserving.
