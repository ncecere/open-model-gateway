-- Identity and operator bootstrap. No automatic linking of existing email accounts.
-- Organization-wide policy for personal model access, without exposing personal workspaces.
ALTER TABLE models ADD COLUMN personal_enabled BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE users ADD COLUMN platform_admin BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE users ADD COLUMN oidc_link_allowed BOOLEAN NOT NULL DEFAULT false;
CREATE UNIQUE INDEX users_email_case_insensitive ON users(lower(email));
CREATE TABLE oidc_identities (
    issuer TEXT NOT NULL,
    subject TEXT NOT NULL,
    user_id UUID NOT NULL REFERENCES users(id),
    PRIMARY KEY (issuer, subject),
    UNIQUE (user_id)
);
CREATE TABLE login_attempts (
    state_hash BYTEA PRIMARY KEY,
    browser_hash BYTEA NOT NULL,
    nonce TEXT NOT NULL,
    pkce_verifier TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE browser_sessions (
    token_hash BYTEA PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id),
    csrf_hash BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ
);
CREATE INDEX sessions_user ON browser_sessions(user_id);

CREATE TABLE service_accounts (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
    disabled_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (organization_id, workspace_id, id),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces(organization_id,id)
);
ALTER TABLE api_keys ALTER COLUMN issued_to_user_id DROP NOT NULL;
ALTER TABLE api_keys ADD COLUMN service_account_id UUID;
ALTER TABLE api_keys ADD FOREIGN KEY (organization_id,workspace_id,service_account_id)
    REFERENCES service_accounts(organization_id,workspace_id,id);
ALTER TABLE api_keys ADD CHECK ((issued_to_user_id IS NOT NULL) <> (service_account_id IS NOT NULL));

CREATE TABLE invitations (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES organizations(id),
    workspace_id UUID,
    email TEXT NOT NULL,
    organization_role TEXT NOT NULL CHECK (organization_role IN ('admin','member')),
    workspace_role TEXT NOT NULL CHECK (workspace_role IN ('admin','member')),
    token_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash)=32),
    invited_by UUID NOT NULL REFERENCES users(id),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    accepted_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (organization_id,workspace_id) REFERENCES workspaces(organization_id,id)
);
CREATE INDEX invitations_org ON invitations(organization_id,created_at);
CREATE TABLE audit_events (
    id UUID PRIMARY KEY,
    organization_id UUID REFERENCES organizations(id),
    workspace_id UUID,
    actor_user_id UUID NOT NULL REFERENCES users(id),
    action TEXT NOT NULL,
    target_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (organization_id,workspace_id) REFERENCES workspaces(organization_id,id)
);
CREATE INDEX audit_org_time ON audit_events(organization_id,created_at);
