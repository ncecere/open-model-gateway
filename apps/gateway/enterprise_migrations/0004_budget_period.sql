-- Budget rotation periods. Every policy layer that carries a budget amount
-- (`monthly_budget_microusd`, kept for API/storage compatibility and now meaning
-- "budget amount per period") gains its own period. Existing rows keep their
-- calendar-month behavior through the default. Windows are UTC: day = UTC day,
-- week = ISO week starting Monday 00:00 UTC, month = UTC calendar month.
-- Changing a period changes only the window used for later admissions; no
-- reservation, ledger or settlement history is reset or rewritten.
-- Explicit operator upgrade only (`migrate`); never applied implicitly by serve.
ALTER TABLE installation_policy ADD COLUMN budget_period text NOT NULL DEFAULT 'month' CHECK (budget_period IN ('day','week','month'));
ALTER TABLE workspace_type_policies ADD COLUMN budget_period text NOT NULL DEFAULT 'month' CHECK (budget_period IN ('day','week','month'));
ALTER TABLE workspace_platform_policy_overrides ADD COLUMN budget_period text NOT NULL DEFAULT 'month' CHECK (budget_period IN ('day','week','month'));
ALTER TABLE workspace_local_policies ADD COLUMN budget_period text NOT NULL DEFAULT 'month' CHECK (budget_period IN ('day','week','month'));
ALTER TABLE key_policies ADD COLUMN budget_period text NOT NULL DEFAULT 'month' CHECK (budget_period IN ('day','week','month'));
-- Budget windows are evaluated on the immutable admission time, not the
-- month bucket, so day/week windows can span calendar months.
CREATE INDEX governance_admitted ON governance_reservations(admitted_at,workspace_id);
