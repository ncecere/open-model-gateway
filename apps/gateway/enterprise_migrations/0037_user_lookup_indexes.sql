-- R06 (#65): per-user lookups (directory lists, user detail, session context)
-- used to scan these tables for every row: ~20 ms per row at 200k grants.
CREATE INDEX workspace_membership_grants_user ON workspace_membership_grants(user_id, workspace_id);
CREATE INDEX platform_role_grants_user ON platform_role_grants(user_id);
