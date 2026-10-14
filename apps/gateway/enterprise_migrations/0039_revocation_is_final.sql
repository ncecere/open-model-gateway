-- #145: a revocation is final (AGENTS.md: revoked keys never reactivate).
-- Once revoked_at is set on a key, grant, session or invitation, no role,
-- the schema owner included, can clear it or move it. Re-revoking with
-- coalesce(revoked_at, now()) leaves the value unchanged and stays allowed.
CREATE FUNCTION omg_revocation_is_final() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF OLD.revoked_at IS NOT NULL AND NEW.revoked_at IS DISTINCT FROM OLD.revoked_at THEN
  RAISE EXCEPTION 'revocation is final: % cannot change revoked_at once set', TG_TABLE_NAME
   USING ERRCODE = 'check_violation';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER api_keys_revocation_is_final BEFORE UPDATE OF revoked_at ON api_keys
 FOR EACH ROW EXECUTE FUNCTION omg_revocation_is_final();
CREATE TRIGGER workspace_membership_grants_revocation_is_final BEFORE UPDATE OF revoked_at ON workspace_membership_grants
 FOR EACH ROW EXECUTE FUNCTION omg_revocation_is_final();
CREATE TRIGGER platform_role_grants_revocation_is_final BEFORE UPDATE OF revoked_at ON platform_role_grants
 FOR EACH ROW EXECUTE FUNCTION omg_revocation_is_final();
CREATE TRIGGER browser_sessions_revocation_is_final BEFORE UPDATE OF revoked_at ON browser_sessions
 FOR EACH ROW EXECUTE FUNCTION omg_revocation_is_final();
CREATE TRIGGER workspace_invitations_revocation_is_final BEFORE UPDATE OF revoked_at ON workspace_invitations
 FOR EACH ROW EXECUTE FUNCTION omg_revocation_is_final();
