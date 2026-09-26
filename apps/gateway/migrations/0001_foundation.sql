-- Global identities; OIDC subjects will be linked in a later migration.
CREATE TABLE users (
    id UUID PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    disabled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE organizations (
    id UUID PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    disabled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE organization_memberships (
    organization_id UUID NOT NULL REFERENCES organizations(id),
    user_id UUID NOT NULL REFERENCES users(id),
    role TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    disabled_at TIMESTAMPTZ,
    PRIMARY KEY (organization_id, user_id)
);

-- A team is a shared workspace in the initial product, not a second ownership hierarchy.
CREATE TABLE workspaces (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES organizations(id),
    name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('personal', 'team')),
    owner_user_id UUID,
    disabled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (organization_id, id),
    CHECK ((kind = 'personal' AND owner_user_id IS NOT NULL)
        OR (kind = 'team' AND owner_user_id IS NULL)),
    FOREIGN KEY (organization_id, owner_user_id)
        REFERENCES organization_memberships(organization_id, user_id)
);
CREATE UNIQUE INDEX one_personal_workspace_per_user
    ON workspaces(organization_id, owner_user_id) WHERE kind = 'personal';

CREATE TABLE workspace_memberships (
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    user_id UUID NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    disabled_at TIMESTAMPTZ,
    PRIMARY KEY (organization_id, workspace_id, user_id),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces(organization_id, id),
    FOREIGN KEY (organization_id, user_id) REFERENCES organization_memberships(organization_id, user_id)
);

CREATE TABLE api_keys (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    issued_to_user_id UUID NOT NULL,
    name TEXT NOT NULL,
    secret_hash BYTEA NOT NULL CHECK (octet_length(secret_hash) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces(organization_id, id),
    FOREIGN KEY (organization_id, issued_to_user_id) REFERENCES organization_memberships(organization_id, user_id)
);
CREATE INDEX api_keys_workspace ON api_keys(organization_id, workspace_id);

CREATE TABLE provider_connections (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES organizations(id),
    name TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('openai', 'anthropic', 'bedrock', 'azure_openai', 'vertex', 'openai_compatible')),
    -- References only, e.g. env:OPENAI_API_KEY or a secret-manager identifier.
    -- Resolution, URL validation and provider execution are not implemented yet.
    credential_ref TEXT NOT NULL CHECK (length(credential_ref) > 0),
    endpoint TEXT,
    region TEXT,
    enabled BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, name)
);

CREATE TABLE models (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES organizations(id),
    public_name TEXT NOT NULL CHECK (length(public_name) BETWEEN 1 AND 200),
    display_name TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, public_name)
);

CREATE TABLE deployments (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    model_id UUID NOT NULL,
    provider_connection_id UUID NOT NULL,
    upstream_model TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (organization_id, id),
    FOREIGN KEY (organization_id, model_id) REFERENCES models(organization_id, id),
    FOREIGN KEY (organization_id, provider_connection_id) REFERENCES provider_connections(organization_id, id)
);
CREATE INDEX deployments_model ON deployments(organization_id, model_id);

CREATE TABLE workspace_model_grants (
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    model_id UUID NOT NULL,
    PRIMARY KEY (organization_id, workspace_id, model_id),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces(organization_id, id),
    FOREIGN KEY (organization_id, model_id) REFERENCES models(organization_id, id)
);
