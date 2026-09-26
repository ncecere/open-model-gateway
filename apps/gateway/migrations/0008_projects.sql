-- Projects are sibling shared workspaces, never children of teams.
ALTER TABLE workspaces DROP CONSTRAINT workspaces_kind_check;
ALTER TABLE workspaces DROP CONSTRAINT workspaces_check;
ALTER TABLE workspaces ADD CONSTRAINT workspaces_kind_check
    CHECK (kind IN ('personal', 'team', 'project'));
ALTER TABLE workspaces ADD CONSTRAINT workspaces_check
    CHECK ((kind = 'personal' AND owner_user_id IS NOT NULL)
        OR (kind IN ('team', 'project') AND owner_user_id IS NULL));

-- A personal workspace cannot acquire another human member, including through
-- direct provisioning. The owner does not need a membership row, but if one is
-- stored it must describe that exact owner. Parent locking also serializes this
-- check against a concurrent kind/owner change.
CREATE FUNCTION enforce_private_workspace_membership() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    workspace_kind TEXT;
    workspace_owner UUID;
BEGIN
    SELECT kind, owner_user_id INTO workspace_kind, workspace_owner
        FROM workspaces WHERE organization_id=NEW.organization_id AND id=NEW.workspace_id
        FOR SHARE;
    IF workspace_kind = 'personal'
        AND (NEW.user_id IS DISTINCT FROM workspace_owner OR NEW.role <> 'owner') THEN
        RAISE EXCEPTION 'personal workspaces are owner-only' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE CONSTRAINT TRIGGER private_workspace_membership
    AFTER INSERT OR UPDATE ON workspace_memberships
    FOR EACH ROW EXECUTE FUNCTION enforce_private_workspace_membership();

CREATE FUNCTION enforce_shared_workspace_resource() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE workspace_kind TEXT;
BEGIN
    IF NEW.workspace_id IS NOT NULL THEN
        SELECT kind INTO workspace_kind FROM workspaces
            WHERE organization_id=NEW.organization_id AND id=NEW.workspace_id FOR SHARE;
        IF workspace_kind NOT IN ('team', 'project') THEN
            RAISE EXCEPTION 'resource requires a shared workspace' USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
CREATE CONSTRAINT TRIGGER service_account_shared_workspace
    AFTER INSERT OR UPDATE ON service_accounts
    FOR EACH ROW EXECUTE FUNCTION enforce_shared_workspace_resource();
CREATE CONSTRAINT TRIGGER invitation_shared_workspace
    AFTER INSERT OR UPDATE ON invitations
    FOR EACH ROW EXECUTE FUNCTION enforce_shared_workspace_resource();

-- Enforce the reverse direction too: a shared workspace cannot be converted to
-- a private one while retaining shared members, service accounts, or invitations.
CREATE FUNCTION enforce_private_workspace_owner() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.kind = 'personal' AND (
        EXISTS (SELECT 1 FROM workspace_memberships
            WHERE organization_id=NEW.organization_id AND workspace_id=NEW.id
              AND (user_id IS DISTINCT FROM NEW.owner_user_id OR role <> 'owner'))
        OR EXISTS (SELECT 1 FROM service_accounts
            WHERE organization_id=NEW.organization_id AND workspace_id=NEW.id)
        OR EXISTS (SELECT 1 FROM invitations
            WHERE organization_id=NEW.organization_id AND workspace_id=NEW.id)
    ) THEN
        RAISE EXCEPTION 'personal workspaces are owner-only' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;
CREATE CONSTRAINT TRIGGER private_workspace_owner
    AFTER INSERT OR UPDATE ON workspaces
    FOR EACH ROW EXECUTE FUNCTION enforce_private_workspace_owner();
