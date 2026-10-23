-- #37 PR1: admin API tokens, storage and lifecycle only (no request
-- authentication, API routes or UI; those are PR2 and PR3).
--
-- Format `omg_admin_<32-hex uuid>.<64-hex secret>`; the whole token's
-- SHA-256 is stored (apps/gateway/src/admin_tokens.rs mirrors auth.rs's
-- inference-key pattern: id-indexed lookup, then a constant-time compare of
-- the stored digest). Explicit operator upgrade only (`migrate`).
--
-- Immutable once created: identity (id, owner), digest, scopes, expiry
-- (1-90 days after creation) and the optional CIDR allowlist. No rename, no
-- renewal. Only revoked_at (final, like every other revocation:
-- omg_revocation_is_final(), migration 0039), last_used_at and
-- last_used_address ever change. No DELETE or TRUNCATE.
--
-- Owner-invalidation (final, never restored when access returns):
-- revoking a user's last active 'admin' platform_role_grants row (manual or
-- group-derived; whichever row loses it last, by the time every row of that
-- UPDATE has fired), suspending (users.disabled_at) or cleaning up
-- (users.cleaned_at) their account revokes every active token they own, in
-- the same transaction as whatever caused the loss. Regaining the Admin
-- role later never un-revokes them (the revocation-is-final trigger refuses
-- it, and nothing here ever clears revoked_at).
SET lock_timeout = '60s';

-- Every admin-token scope (route assignments are PR2's route_policy.rs); no
-- wildcard, no duplicates, non-empty.
CREATE FUNCTION valid_admin_token_scopes(scopes text[]) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
 SELECT cardinality(scopes) BETWEEN 1 AND 5 AND array_position(scopes,NULL) IS NULL
  AND scopes <@ ARRAY['platform:read','catalog:write','access:write','governance:write','settings:write']::text[]
  AND cardinality(scopes)=(SELECT count(DISTINCT s) FROM unnest(scopes) s)
$$;
-- Optional allowlist: omitted means unrestricted; supplied means 1-32 entries, no nulls.
-- Address normalization and malformed/mapped-network handling are PR2's
-- request-handling concern (identity.rs/throttle.rs); cidr's own input
-- parser already refuses a syntactically invalid network here.
CREATE FUNCTION valid_admin_token_allowlist(allowlist cidr[]) RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
 SELECT allowlist IS NULL OR (cardinality(allowlist) BETWEEN 1 AND 32 AND array_position(allowlist,NULL) IS NULL)
$$;

CREATE TABLE admin_api_tokens(
 id uuid PRIMARY KEY,
 owner_user_id uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
 name text NOT NULL CHECK(char_length(name) BETWEEN 1 AND 120 AND name !~ '[[:cntrl:]]'),
 secret_hash bytea NOT NULL UNIQUE CHECK(octet_length(secret_hash)=32),
 scopes text[] NOT NULL CHECK(valid_admin_token_scopes(scopes)),
 ip_allowlist cidr[] CHECK(valid_admin_token_allowlist(ip_allowlist)),
 created_at timestamptz NOT NULL DEFAULT now(),
 expires_at timestamptz NOT NULL,
 revoked_at timestamptz,
 last_used_at timestamptz,
 last_used_address inet,
 CHECK(expires_at>=created_at+interval '1 day' AND expires_at<=created_at+interval '90 days')
);
CREATE INDEX admin_api_tokens_owner_created ON admin_api_tokens(owner_user_id,created_at DESC,id);
-- Active-owner rows: the owner-invalidation triggers below and the "list my
-- live tokens" read (PR2) both filter on exactly this.
CREATE INDEX admin_api_tokens_active_owner ON admin_api_tokens(owner_user_id) WHERE revoked_at IS NULL;

CREATE FUNCTION admin_api_tokens_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NEW.id<>OLD.id OR NEW.owner_user_id<>OLD.owner_user_id OR NEW.name<>OLD.name
  OR NEW.secret_hash<>OLD.secret_hash OR NEW.scopes<>OLD.scopes
  OR NEW.ip_allowlist IS DISTINCT FROM OLD.ip_allowlist
  OR NEW.created_at<>OLD.created_at OR NEW.expires_at<>OLD.expires_at THEN
  RAISE EXCEPTION 'admin API token identity, digest, scopes, expiry and allowlist are immutable' USING ERRCODE='23514';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER admin_api_tokens_guard BEFORE UPDATE ON admin_api_tokens
 FOR EACH ROW EXECUTE FUNCTION admin_api_tokens_immutable();
-- Reuse the shared revocation-is-final trigger (0039): once revoked_at is
-- set, nobody, the schema owner included, can clear or move it.
CREATE TRIGGER admin_api_tokens_revocation_is_final BEFORE UPDATE OF revoked_at ON admin_api_tokens
 FOR EACH ROW EXECUTE FUNCTION omg_revocation_is_final();
CREATE TRIGGER admin_api_tokens_no_delete BEFORE DELETE ON admin_api_tokens
 FOR EACH ROW EXECUTE FUNCTION immutable_history();
CREATE TRIGGER admin_api_tokens_no_truncate BEFORE TRUNCATE ON admin_api_tokens
 FOR EACH STATEMENT EXECUTE FUNCTION immutable_history();

-- Idempotent: a transaction that revokes several of a user's grants (or
-- disables/cleans several users) in one statement fires this once per
-- affected row; coalesce leaves an already-revoked token's time untouched.
CREATE FUNCTION omg_admin_tokens_revoke_owner(target uuid) RETURNS void LANGUAGE sql AS $$
 UPDATE admin_api_tokens SET revoked_at=coalesce(revoked_at,now()) WHERE owner_user_id=target AND revoked_at IS NULL
$$;

-- A revoked platform_role_grants row that leaves its user with no remaining
-- active 'admin' grant (checked fresh, after this row's own revocation is
-- applied) revokes every token that user owns. A multi-row UPDATE (e.g.
-- revoking a manual and a bootstrap 'admin' grant together) fires this once
-- per row; by the time the last 'admin' row of that statement has fired,
-- the EXISTS check sees every row's final state, so correctness holds even
-- though an earlier row's firing may still see a sibling row not yet
-- updated. Provenance (manual vs. group) is irrelevant: either source
-- losing the user's last 'admin' grant revokes the same way.
CREATE FUNCTION omg_admin_tokens_role_lost() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF NOT EXISTS(SELECT 1 FROM platform_role_grants WHERE user_id=NEW.user_id AND role='admin' AND revoked_at IS NULL) THEN
  PERFORM omg_admin_tokens_revoke_owner(NEW.user_id);
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER admin_api_tokens_role_lost AFTER UPDATE OF revoked_at ON platform_role_grants
 FOR EACH ROW WHEN (NEW.role='admin' AND NEW.revoked_at IS NOT NULL AND OLD.revoked_at IS NULL)
 EXECUTE FUNCTION omg_admin_tokens_role_lost();

-- Suspension (users.disabled_at) and cleanup (users.cleaned_at) each revoke
-- every token the user owns outright: a disabled or cleaned-up user cannot
-- be an effective Admin regardless of their platform_role_grants rows
-- (effective_platform_roles already excludes them), so no further EXISTS
-- check is needed here.
CREATE FUNCTION omg_admin_tokens_owner_lost() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 PERFORM omg_admin_tokens_revoke_owner(NEW.id);
 RETURN NEW;
END $$;
CREATE TRIGGER admin_api_tokens_user_disabled AFTER UPDATE OF disabled_at ON users
 FOR EACH ROW WHEN (NEW.disabled_at IS NOT NULL AND OLD.disabled_at IS NULL)
 EXECUTE FUNCTION omg_admin_tokens_owner_lost();
CREATE TRIGGER admin_api_tokens_user_cleaned AFTER UPDATE OF cleaned_at ON users
 FOR EACH ROW WHEN (NEW.cleaned_at IS NOT NULL AND OLD.cleaned_at IS NULL)
 EXECUTE FUNCTION omg_admin_tokens_owner_lost();

RESET lock_timeout;
