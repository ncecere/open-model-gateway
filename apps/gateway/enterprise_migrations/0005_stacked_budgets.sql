-- Stacked budgets and reversible key disablement.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
--
-- Every policy scope (installation, workspace-type default, platform workspace
-- override, workspace local, key lineage) may hold at most one budget per
-- period: day (UTC day), week (ISO week from Monday 00:00 UTC), month (UTC
-- calendar month) or lifetime (all time since the scope was created). Every
-- applicable budget is enforced at admission. Existing single budget/period
-- pairs move here unchanged, then the legacy columns are dropped so there is
-- one source of truth. Consumption history is never touched.
CREATE TABLE policy_budgets(
 id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
 layer text NOT NULL CHECK(layer IN ('installation','type','override','local','key')),
 kind text CHECK(kind IN ('personal','team','project')),
 workspace_id uuid REFERENCES workspaces(id),
 governance_key_id uuid,
 period text NOT NULL CHECK(period IN ('day','week','month','lifetime')),
 amount_microusd bigint NOT NULL CHECK(amount_microusd>=0),
 created_at timestamptz NOT NULL DEFAULT now(),
 CHECK((layer='installation' AND kind IS NULL AND workspace_id IS NULL AND governance_key_id IS NULL)
  OR (layer='type' AND kind IS NOT NULL AND workspace_id IS NULL AND governance_key_id IS NULL)
  OR (layer IN ('override','local') AND kind IS NULL AND workspace_id IS NOT NULL AND governance_key_id IS NULL)
  OR (layer='key' AND kind IS NULL AND workspace_id IS NOT NULL AND governance_key_id IS NOT NULL)),
 FOREIGN KEY(workspace_id,governance_key_id) REFERENCES api_keys(workspace_id,id)
);
CREATE UNIQUE INDEX policy_budgets_scope_period ON policy_budgets(layer,coalesce(kind,''),coalesce(workspace_id,'00000000-0000-0000-0000-000000000000'::uuid),coalesce(governance_key_id,'00000000-0000-0000-0000-000000000000'::uuid),period);
CREATE INDEX policy_budgets_workspace ON policy_budgets(workspace_id) WHERE workspace_id IS NOT NULL;
INSERT INTO policy_budgets(layer,period,amount_microusd) SELECT 'installation',budget_period,monthly_budget_microusd FROM installation_policy WHERE monthly_budget_microusd IS NOT NULL;
INSERT INTO policy_budgets(layer,kind,period,amount_microusd) SELECT 'type',kind,budget_period,monthly_budget_microusd FROM workspace_type_policies WHERE monthly_budget_microusd IS NOT NULL;
INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) SELECT 'override',workspace_id,budget_period,monthly_budget_microusd FROM workspace_platform_policy_overrides WHERE monthly_budget_microusd IS NOT NULL;
INSERT INTO policy_budgets(layer,workspace_id,period,amount_microusd) SELECT 'local',workspace_id,budget_period,monthly_budget_microusd FROM workspace_local_policies WHERE monthly_budget_microusd IS NOT NULL;
INSERT INTO policy_budgets(layer,workspace_id,governance_key_id,period,amount_microusd) SELECT 'key',workspace_id,governance_key_id,budget_period,monthly_budget_microusd FROM key_policies WHERE monthly_budget_microusd IS NOT NULL;
ALTER TABLE installation_policy DROP COLUMN monthly_budget_microusd, DROP COLUMN budget_period;
ALTER TABLE workspace_type_policies DROP COLUMN monthly_budget_microusd, DROP COLUMN budget_period;
ALTER TABLE workspace_platform_policy_overrides DROP COLUMN monthly_budget_microusd, DROP COLUMN budget_period;
ALTER TABLE workspace_local_policies DROP COLUMN monthly_budget_microusd, DROP COLUMN budget_period;
ALTER TABLE key_policies DROP COLUMN monthly_budget_microusd, DROP COLUMN budget_period;
-- Disabled keys are refused at admission and can be re-enabled by authorized
-- people; revoked_at stays permanent and is never cleared.
ALTER TABLE api_keys ADD COLUMN disabled_at timestamptz;
-- Per-key last-use and key statistics lookups.
CREATE INDEX executions_key_time ON inference_executions(api_key_id,started_at DESC);
