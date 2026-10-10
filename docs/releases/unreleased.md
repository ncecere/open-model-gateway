# Open Model Gateway: unreleased

Notes for changes on `main` since v0.3.2. They become the next release's notes. Scale phases P0–P2 (measurement, lock-free reads, maintained rate counters) are summarized in the [changelog](../../CHANGELOG.md).

## Highlights

- **No installation-wide limits.** Limits now exist only on personal, team and project workspaces (workspace-type defaults, platform per-workspace overrides, tighten-only workspace caps) and on API keys. All of those stay hard limits, exactly as before. There is no installation budget (any period) and no installation requests per minute, tokens per minute, requests at once or jobs at once. Admission and settlement no longer read or write any installation-wide limit row, which removes the last global hot row from the request path's limit checks (scale plan, decision of 2026-10-09).
- **Installation spend alert.** To keep an eye on total spend, Admin › Settings › Alerts has a new rule type, **Installation spend** (`spend_threshold`): it fires when installation-wide spend (settled plus on hold, all workspaces, personal ones as totals) in the current day, week, month or lifetime reaches up to five percentages of an amount you choose. It only notifies; it never blocks a request. Spend is exact integer micro-USD; requests with unknown cost are reported separately, never counted as zero.

## Migration

`0026_remove_installation_limits.sql`:

- Records the removed installation configuration (rates and budgets) once in the audit log as `policy.installation_removed`.
- Converts installation budget alert rules to installation spend rules, keeping thresholds and recipients (details in [alerts](../alerts.md#upgrade-from-installation-budget-alerts-0026)). Each changed rule gets an `alert_rule.installation_layer_removed` audit event. A rule that watched only the installation budget while none was set could never fire and is soft-deleted.
- Deletes installation rows of `policy_budgets` and makes them unrepresentable; drops `installation_policy`.
- Stops maintaining the installation scope of `budget_totals` and deletes those rows. They were derived data (trigger-maintained sums that `budget verify` recomputes from history) and equal the sum of the workspace rows exactly. Reservations, executions, the ledger, prices, audit events and incidents are not modified (open installation-budget incidents that no rule continues are resolved as `superseded`, without email).
- Keeps the installation singleton row (settings, branding, the catalog lock and management serialization use it).

It briefly freezes history writes, like 0015 and 0025. On the 2 M-attempt load-test database it took 0.9 s. On the laptop load-test stack the overload ceiling rose 9–21 % with the change ([operations](../operations.md#combined-capacity-p1--p2--no-installation-limits)).

## Breaking and behaviour changes

- **`GET`/`PUT /api/v1/platform/installation/policy` return `410 Gone`** with `error.reason:"installation_limits_removed"`. Scripts that set installation limits must set workspace-type defaults (`/platform/workspace-types/{kind}/policy`), workspace overrides or key limits instead.
- **Alert rules can't watch an `installation` budget layer.** `budget_layers` containing `installation` is `400` with `reason:"installation_limits_removed"`; use `kind:"spend_threshold"` with `spend_period`, `spend_amount_microusd` and `thresholds`.
- **Removed response fields:** `installation_budgets` on `GET /platform/overview` and `GET /platform/usage/overview`; the `platform` (installation) row of `layers` in `GET …/access` (reasons may still name the `platform` layer for model-level facts such as a model turned off).
- **Denials:** `budget_exceeded`, `token_reservation_exceeds_limit` and `job_limit_exceeded` no longer have an installation scope; `gateway_admission_denials_total{scope="installation"}` no longer occurs.
- **Workspaces may now use more in total** than a former installation ceiling allowed: move any shared ceiling you relied on into type defaults, overrides or keys before upgrading.
- **Dashboard:** Admin › Settings › Defaults & limits has only the Personal, Team and Project tabs (an old `?tab=installation` link opens Personal); the Admin overview has no "Installation budgets" card or "Set an installation-wide budget" step; Usage & costs has no installation budget row; Effective access has no Installation row.

## Upgrade steps

1. Before upgrading, note any installation limits you still want and recreate them on type defaults, workspace overrides or keys after the upgrade (the migration records the old values in the audit log).
2. Back up and drain traffic (the migration freezes history writes briefly).
3. Run `open-model-gateway migrate` as the migrator.
4. Reapply `deploy/staging/runtime-grants.sql` (alert rules gain `spend_period`/`spend_amount_microusd` update grants; `installation_policy` grants are gone).
5. Run `open-model-gateway budget verify` and expect `mismatch_count: 0` and `rate_mismatch_count: 0`.
6. Review Admin › Settings › Alerts: converted rules are named after the original, with "(installation … spend)" for extra periods.
