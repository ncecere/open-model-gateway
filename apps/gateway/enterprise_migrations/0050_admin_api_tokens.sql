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
--
-- The role-loss check and the creation guard below both take the repo's
-- canonical user-scope authority lock (AGENTS.md: "authority advisory
-- locks ... user ... exclusive in any management change of that scope")
-- before their own live check (write-skew guard: two admin grants of
-- different provenance revoked by two otherwise-unsynchronized concurrent
-- transactions must still both be seen gone; a concurrent demotion and a
-- concurrent creation must serialize). An earlier revision used a plain
-- `users ... FOR UPDATE` row lock here instead, which deadlocked (40P01)
-- against the manual-demotion path in management/directory.rs: that path
-- locks the users row first, then updates the grant, which takes 0027's
-- advisory lock; group-mapping deletion updates the grant first (taking
-- the advisory lock first) and this trigger then wanted the row. The
-- advisory lock avoids that: it is the same lock 0027's own
-- omg_authority_platform_grants/omg_authority_users triggers already take
-- for any platform_role_grants or users.disabled_at/cleaned_at change, and
-- it is re-entrant within a transaction, so a transaction that already
-- holds it (the common case: Rust callers take it up front, per
-- AGENTS.md) pays nothing to ask for it again here.
SET lock_timeout = '60s';

-- Project-specific SQLSTATE codes (PostgreSQL Appendix A: codes whose
-- class starts with a digit 5-9 or a letter I-Z are available for
-- implementation-defined conditions and never collide with a standard or
-- future PostgreSQL code). This file's custom codes:
--   OM001 - admin API token creation refused: the owner is not, at the
--           instant of insertion, a currently effective Platform Admin.
--   OM002 - an admin-token authority guard (creation or role-loss) was
--           run under an isolation level other than READ COMMITTED.
-- Every other RAISE in this file reuses an existing standard SQLSTATE
-- (23514 check_violation and friends), matching the rest of this
-- repository's migrations; these two are deliberately distinct so Rust
-- can map on the code alone, with no message-text matching (#37 PR1,
-- coordinator review issue 5a).

-- Both admin-token authority guards below (role-loss and creation) read
-- `effective_platform_roles`/`platform_role_grants` with a plain `SELECT
-- ... EXISTS` after taking the user-scope advisory lock. Under READ
-- COMMITTED (the gateway's own isolation level; see crate::db::begin) that
-- statement always takes a fresh snapshot, so once the lock is granted the
-- read is guaranteed to see every write already committed by whoever held
-- the lock before. Under REPEATABLE READ or SERIALIZABLE, the whole
-- transaction shares one snapshot taken at its first statement; the
-- advisory lock still serializes with a concurrent demotion, but the
-- EXISTS check can still read a snapshot that predates a commit it waited
-- on, so a transaction that observed the owner as admin before a
-- concurrent, now-committed demotion can still create a token for them (or
-- still see an admin grant that was already revoked out from under it).
-- PostgreSQL's Serializable Snapshot Isolation does not reliably abort
-- this either (verified by hand): the anomaly is a stale read behind a
-- lock wait, not the read-write conflict pattern SSI detects. Refuse
-- outright instead of trying to reason about it further.
CREATE FUNCTION omg_admin_tokens_require_read_committed() RETURNS void LANGUAGE plpgsql AS $$
BEGIN
 IF current_setting('transaction_isolation')<>'read committed' THEN
  RAISE EXCEPTION 'admin API token authority checks require READ COMMITTED (a higher isolation level''s snapshot can read a stale admin grant behind this function''s own lock wait)' USING ERRCODE='OM002';
 END IF;
END $$;

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
-- active 'admin' grant (checked fresh, after every statement of the
-- transaction has run; see the DEFERRABLE INITIALLY DEFERRED trigger
-- below) revokes every token that user owns. Provenance (manual vs.
-- group) is irrelevant: either source losing the user's last 'admin'
-- grant revokes the same way.
--
-- Deferred to commit (coordinator review of #37 PR1, issue 3: a routine
-- group re-sync, like an IdP group mapping getting recreated with a new
-- id but the same group value/role, revokes the grant tied to the old
-- mapping id in one statement and inserts its replacement, tied to the
-- new mapping id, in the very next statement -- both in the same
-- transaction, as `lifecycle::apply_group_grants` does. An immediate
-- `AFTER ROW` trigger fires at the end of the first statement, before the
-- replacement exists, and would revoke every token the user owns even
-- though they never actually lost Admin. `DEFERRABLE INITIALLY DEFERRED`
-- instead queues this trigger's firing for each affected row and runs
-- them all at commit, against the transaction's *final* state: by then
-- the replacement (if any) already exists, so the EXISTS check below sees
-- it. A real loss with no replacement still revokes, just at commit
-- instead of mid-transaction (observably identical to every caller: no
-- code reads a token's revocation state from inside the same transaction
-- that revoked its owner's last grant). Each row's firing is independently
-- idempotent (coalesce in omg_admin_tokens_revoke_owner), so firing order
-- between rows, and between this and the suspension/cleanup triggers
-- below (left unconditional and immediate: a suspension or cleanup is
-- always a real loss, never a same-transaction replacement), does not
-- matter.
CREATE FUNCTION omg_admin_tokens_role_lost() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 -- Snapshot-bound guard: refuse outright under anything but READ
 -- COMMITTED (see the function's own comment above); only the gateway's
 -- own isolation level may rely on the lock-then-EXISTS pattern below.
 PERFORM omg_admin_tokens_require_read_committed();
 -- Write-skew guard: two admin grants of different provenance (manual and
 -- group) can be revoked by two concurrent transactions that otherwise
 -- share no lock here. Take the same user-scope authority lock 0027's
 -- omg_authority_platform_grants/omg_authority_users triggers already
 -- take for this exact user (re-entrant: a transaction that holds it
 -- already, the common case, pays nothing to ask again). The EXISTS below
 -- then runs as a fresh statement: under READ COMMITTED every plain SQL
 -- statement (including one run from inside a PL/pgSQL function) takes a
 -- new snapshot, so once this lock is granted the EXISTS is guaranteed to
 -- see whatever the other transaction already committed, instead of each
 -- transaction's stale view of the sibling grant.
 PERFORM omg_scope_lock_exclusive(72419512,omg_scope_key(NEW.user_id));
 IF NOT EXISTS(SELECT 1 FROM platform_role_grants WHERE user_id=NEW.user_id AND role='admin' AND revoked_at IS NULL) THEN
  PERFORM omg_admin_tokens_revoke_owner(NEW.user_id);
 END IF;
 RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER admin_api_tokens_role_lost AFTER UPDATE OF revoked_at ON platform_role_grants
 DEFERRABLE INITIALLY DEFERRED
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

-- Creation is bound to the owner's live Admin status at the instant of
-- insertion, not only to a caller's earlier check (PR2's handler still
-- enforces "create only for oneself"; this is the storage-level backstop).
-- Take the same user-scope authority lock the role-loss check above takes
-- (and 0027's omg_authority_platform_grants/omg_authority_users triggers
-- already take for any platform_role_grants or users.disabled_at/
-- cleaned_at change), so a concurrent demotion/suspension/cleanup and a
-- concurrent creation serialize on it: a demotion already in flight blocks
-- the create until it commits, and the create then correctly refuses; a
-- create already in flight is evaluated while the owner was still
-- genuinely admin and blocks the demotion until the insert's implicit
-- transaction ends. effective_platform_roles already excludes disabled
-- and cleaned-up users, so this one EXISTS also covers suspension/cleanup.
-- An earlier revision used a plain `users ... FOR UPDATE` row lock here,
-- which deadlocked (40P01) against the manual-demotion path's own
-- row-then-advisory-lock order; the advisory lock does not, and is free
-- (re-entrant) for the common case where the caller already holds it.
CREATE FUNCTION omg_admin_tokens_owner_must_be_admin() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 -- Snapshot-bound guard: see omg_admin_tokens_require_read_committed's
 -- own comment above for why this refuses anything but READ COMMITTED.
 PERFORM omg_admin_tokens_require_read_committed();
 PERFORM omg_scope_lock_exclusive(72419512,omg_scope_key(NEW.owner_user_id));
 IF NOT EXISTS(SELECT 1 FROM effective_platform_roles WHERE user_id=NEW.owner_user_id AND role='admin') THEN
  RAISE EXCEPTION 'admin API token owner must be a currently effective Platform Admin' USING ERRCODE='OM001';
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER admin_api_tokens_owner_is_admin BEFORE INSERT ON admin_api_tokens
 FOR EACH ROW EXECUTE FUNCTION omg_admin_tokens_owner_must_be_admin();

RESET lock_timeout;
