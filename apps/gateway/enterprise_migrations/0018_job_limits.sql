-- Job limits and the SCIM last-admin alert (0018). Explicit operator upgrade
-- only (`migrate`); never applied implicitly by serve.
--
-- "Jobs at once" (`concurrent_jobs`): how many async jobs (video + batch) a
-- scope may have active at the same time. It lives on the same stacked policy
-- layers as the other limits (installation, workspace-type default, platform
-- override, workspace local, key lineage). NULL inherits; local/key layers
-- are tighten-only (enforced by the management API). An accepted job holds a
-- job slot, not a "requests at once" slot, until it reaches a terminal state,
-- is cancelled or its lease expires. See docs/governance.md and
-- docs/async-jobs.md.
ALTER TABLE installation_policy ADD COLUMN concurrent_jobs bigint CHECK(concurrent_jobs>0);
-- Type defaults start at 2 jobs at once per workspace. Existing rows get the
-- default too, and every kind gets a row so the default applies on fresh
-- installations (a row with only NULL limits is the same as no row).
ALTER TABLE workspace_type_policies ADD COLUMN concurrent_jobs bigint DEFAULT 2 CHECK(concurrent_jobs>0);
INSERT INTO workspace_type_policies(kind) VALUES('personal'),('team'),('project') ON CONFLICT(kind) DO NOTHING;
ALTER TABLE workspace_platform_policy_overrides ADD COLUMN concurrent_jobs bigint CHECK(concurrent_jobs>0);
ALTER TABLE workspace_local_policies ADD COLUMN concurrent_jobs bigint CHECK(concurrent_jobs>0);
ALTER TABLE key_policies ADD COLUMN concurrent_jobs bigint CHECK(concurrent_jobs>0);

-- SCIM last-admin protection: a built-in installation incident when SCIM
-- tried to remove platform access from the last active Platform Admin (the
-- change itself is refused). One open incident at a time; the alert evaluator
-- resolves it once another active Platform Admin exists. History rules of
-- 0011 (resolve once, never deleted) are unchanged.
ALTER TABLE alert_events DROP CONSTRAINT alert_events_builtin_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_builtin_check CHECK(builtin IN ('personal_budget','scim_last_admin'));
ALTER TABLE alert_events DROP CONSTRAINT alert_events_kind_check;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_kind_check CHECK(kind IN ('budget_threshold','spend_spike','error_rate','provider_failing','scim_last_admin'));
ALTER TABLE alert_events DROP CONSTRAINT alert_events_check1;
ALTER TABLE alert_events ADD CONSTRAINT alert_events_builtin_shape CHECK(builtin IS NULL
 OR (builtin='personal_budget' AND kind='budget_threshold' AND workspace_id IS NOT NULL)
 OR (builtin='scim_last_admin' AND kind='scim_last_admin' AND workspace_id IS NULL AND provider_connection_id IS NULL));
-- Only built-in incidents may use the SCIM kind.
ALTER TABLE alert_events ADD CONSTRAINT alert_events_scim_kind CHECK(kind<>'scim_last_admin' OR builtin IS NOT DISTINCT FROM 'scim_last_admin');
