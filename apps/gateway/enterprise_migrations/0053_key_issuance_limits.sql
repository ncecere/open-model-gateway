-- #253 slice A: bound API-key issuance. Explicit operator upgrade only
-- (`migrate`); never applied implicitly by serve. See docs/governance.md
-- (#key-issuance) and docs/governance-api.md.
--
-- Each new `api_keys` row (a create or a rotation) counts in a rolling 24-hour
-- window against two budgets: the holder's (`issued_to_user_id` or
-- `service_account_id`, inside the workspace) and the workspace's (every row).
-- The count reads the append-only `api_keys` table under the workspace row lock
-- key issuance already takes, so there are no counter rows and no new lock.
--
-- The limits sit on the three workspace policy layers, like `storage_bytes`
-- (0020): the workspace-type default, the platform per-workspace override
-- (a field it sets replaces the type default; see below) and the tighten-only
-- workspace local layer.
-- There is no key layer (no key exists at creation) and no installation layer
-- (removed in 0026). Effective limit =
-- least(coalesce(platform,10000), coalesce(local,10000)); a NULL platform value
-- never means unlimited. 10000 is also the column ceiling.
--
-- Type defaults: 50 per holder, 500 per workspace per rolling 24 hours. Existing
-- rows take them through the constant DEFAULT (no table rewrite). Override and
-- local columns are NULL (inherit).
--
-- Override semantics differ from the other limits in one way. A platform
-- override's field that is SET replaces the type default; one that is NULL
-- INHERITS it (the effective platform value is
-- coalesce(override, type default, 10000)). That is fail-tight, and it makes an
-- override row written without these columns, by a replica that predates this
-- migration and is still running during the rolling upgrade, harmless: it keeps
-- the type default instead of being loosened to the 10000 ceiling. An admin who
-- wants more sets an explicit value (up to 10000). No backfill is needed.
--
-- `api_keys.created_at` is the window clock. It now defaults to
-- clock_timestamp() (the time of the INSERT, not of the transaction's start: a
-- key queued behind the installation lock counts from when it exists), and the
-- runtime role may no longer name it in an INSERT (runtime-grants.sql grants
-- INSERT on explicit columns), so a key cannot be backdated out of the window.
--
-- The window index is NOT built here. The ALTERs below hold ACCESS EXCLUSIVE
-- locks until this transaction ends, and key authentication reads api_keys on
-- every request, so a build in this transaction would block them for its whole
-- duration. 0054 builds it CONCURRENTLY outside a transaction (the migrator
-- recovers an invalid leftover of an interrupted build). These ALTERs are
-- metadata-only and brief (lock_timeout bounds the wait for the locks).
--
-- Rolling upgrade: run `migrate` first. Replicas still on the previous binary
-- ignore these columns and do not enforce the budget during the overlap; they
-- keep working on the new schema (additive only).
SET lock_timeout = '60s';

ALTER TABLE workspace_type_policies
 ADD COLUMN key_issuances_per_holder bigint DEFAULT 50 CHECK(key_issuances_per_holder BETWEEN 1 AND 10000),
 ADD COLUMN key_issuances_per_workspace bigint DEFAULT 500 CHECK(key_issuances_per_workspace BETWEEN 1 AND 10000);
-- Every kind has a row since 0018; keep that true on fresh installations.
INSERT INTO workspace_type_policies(kind) VALUES('personal'),('team'),('project') ON CONFLICT(kind) DO NOTHING;
ALTER TABLE workspace_platform_policy_overrides
 ADD COLUMN key_issuances_per_holder bigint CHECK(key_issuances_per_holder BETWEEN 1 AND 10000),
 ADD COLUMN key_issuances_per_workspace bigint CHECK(key_issuances_per_workspace BETWEEN 1 AND 10000);
ALTER TABLE workspace_local_policies
 ADD COLUMN key_issuances_per_holder bigint CHECK(key_issuances_per_holder BETWEEN 1 AND 10000),
 ADD COLUMN key_issuances_per_workspace bigint CHECK(key_issuances_per_workspace BETWEEN 1 AND 10000);

ALTER TABLE api_keys ALTER COLUMN created_at SET DEFAULT clock_timestamp();

RESET lock_timeout;
