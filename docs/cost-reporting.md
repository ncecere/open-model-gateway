# Usage and cost reporting

Reports answer “what configured-rate activity can I see for this UTC period?” They are live estimates, not provider invoices, complete billing guarantees or frozen financial statements. Unknown charges and conservative holds are separate from settled cost.

## Scope and date range

Browser-session endpoints:

- `/api/v1/platform/cost-report`: Platform Admin/Auditor totals across the installation, including personal financial totals. No foreign personal keys/request rows; platform service-account breakdown is empty.
- `/api/v1/workspaces/{ws}/cost-report`: owner-private personal scope; shared administrators see whole workspace, ordinary members their own human-key activity.
- `/api/v1/workspaces/{ws}/costs` and `/usage-export`: authorized workspace request details only, not a platform-wide detail/export API.

Authorization and own-actor predicates apply **before aggregation and dimension discovery**. Knowing a workspace/key ID is not access. An auditor's platform totals are not permission to browse another user's private requests.

Required `start_date,end_date` use strict YYYY-MM-DD, UTC **[start,end)**: start inclusive, **end exclusive**, **1–93 days**, end no later than tomorrow UTC. For all of January, use January 1 through February 1. `compare=previous_period` uses the equal preceding interval and identical filters; default `none`. Comparison date underflow fails.

Optional exact filters: `workspace_id`, `model` public alias, `provider`, `cost_center_id` UUID or `unallocated`, `actor_user_id`, `service_account_id`, `accounting_status=pending|unknown|settled|missing`, `key_id` (exact API key; never a personal-workspace key at platform scope) and `status` (comma-separated attempt states `succeeded|failed|cancelled|indeterminate|in_progress`). Workspace routes require a supplied workspace ID to match the path. Unknown/duplicate parameters and invalid values fail rather than broadening the report.

```http
GET /api/v1/workspaces/11111111-1111-4111-8111-111111111111/cost-report?start_date=2025-01-01&end_date=2025-02-01&compare=previous_period
```

## Read the aggregate

Every financial amount, aggregate count and billing counter is an exact decimal string. Unknown counters are null. Never convert them through floating-point JavaScript numbers to total money.

| Field | Meaning |
| --- | --- |
| `period` | Start/end, `timezone:"UTC"`, whether the period includes today/partial day. |
| `observed_at`, `basis`, `currency` | Snapshot time, `configured_rate_estimate`, `USD`. |
| `totals.known_cost_microusd` | Settled actual configured-rate cost only; unresolved estimates excluded. |
| `totals.held_microusd` | Active pending/unknown holds/floors; not another charge or full upper bound when unbounded. |
| `totals.attempts`, `root_requests` | Admitted upstream attempts versus distinct root requests. Fallbacks can increase attempts. |
| `totals.unresolved_attempts` | Attempts lacking actual valuation, including missing reservation. |
| `daily` | Every UTC day, including zero-activity days, with the same totals. |
| `breakdowns` | Models, providers, workspaces, cost centers, workspace service-account/human-key grouping. Max 100 entries each. |
| `breakdowns_truncated` | At least one dimension exceeds its displayed bound; totals remain full aggregates. |
| `comparison` | Null or previous period and totals; no implicit accuracy percentage. |

`health` counts pending, unknown, missing-reservation, unpriced, aged-hold and unbounded attempts. They **overlap**: do not sum them as a count of affected requests. Aged holds are unresolved pending/unknown leases past expiry; expiry does not refund money. Missing reservations are also unpriced/unbounded.

`coverage` counts priced, settled, total, complete-billing, legacy-pricing and incomplete-billing attempts. It measures observed accounting coverage, not an inferred invoice accuracy score.

`cost_components` aggregates the six disjoint **v2 settled** categories plus separate `legacy_microusd` for v1. Their sum equals known cost. V1 history is not silently classified as uncached/cache input. Aggregate `billing_usage` sums observed values; null means no observations for that category, not zero and not proof of complete coverage. Pair sums with coverage.

## Details and CSV

Details include execution/root/attempt IDs, public model/provider, state, workload kind, start time, raw `input_tokens,output_tokens`, normalized `billing_usage`, settled `cost_components`, pinned price ID/version, cost state and attribution. No prompts, responses or secrets are included.

- `cost_microusd`: null until actual cost is known.
- `reserved_microusd`: retained reservation hold projection (may have grown with known overspend), **not current exposure after settlement** and not necessarily the original hold event.
- `active_held_microusd`: `"0"` once actual cost is known; otherwise nullable active hold. Use this for unresolved exposure.
- `unbounded_cost`: remains true when a positive known floor exists but no finite upper bound is proven.
- `unresolved_reason`: missing reservation, unpriced, unbounded/unknown rates or incomplete usage/cache allocation.
- `cost_center_id,name,code`: exact admission snapshot; null means Unallocated.
- `details_redacted_at`: settled error/latency compaction marker, not financial deletion.

`/costs` is `{data,has_more}`, default 100/max 200 rows, offset max 100000. CSV is one explicit bounded page (default/max 1000); cells are quoted/formula-safe, unknown values are empty, normalized usage/components serialize as JSON. It sends no-store and export limit/offset/row-count headers. Paging separate live exports is not a stable full statement.

Aggregates use one full SQL statement snapshot, not sampled detail rows. A ten-second handler deadline includes lock acquisition. Installation serialization can contend with admission; row/output caps do not demonstrate bounded scan cost. Measure under realistic traffic.

## Usage analytics

`GET /api/v1/workspaces/{ws}/usage/overview` and `GET /api/v1/platform/usage/overview` (and `…/usage/explore`) use the same authorization as the cost reports: personal owners and shared administrators see the whole workspace, ordinary members their own human keys, Platform Admins/Auditors installation totals including personal **totals**. At platform scope, personal-workspace keys collapse into one row (`id:null`, `name:"Personal workspace keys"`), so no personal key identity is shown. Personal workspaces themselves stay separate totals named `Personal · <owner>` (the owner's display name, else email; `former user` after cleanup), so a platform reader can tell them apart; the owner's identity is already visible with their totals on the user's page and in the Member grouping. Parameters are strict: `start_date,end_date` (UTC `[start,end)`, 1–93 days, end no later than tomorrow) and, at platform scope, optional `workspace_id`. Unknown or repeated parameters fail. Both endpoints also accept optional narrowing filters: `model_id`, `key_id` (exact API key, the same ID as `top.keys`; at platform scope a personal-workspace key never matches), `member_user_id` (that user's human keys; requires workspace-wide visibility, otherwise `403` `reason:"workspace_wide_visibility_required"`), `status` (comma-separated attempt states), `cost_center_id` (UUID or `unallocated`) and `service_account_id`. Filters only narrow what the caller may already see.

Overview returns `tiles` for `spend` (settled known estimate; with `held_microusd` and `unresolved_attempts`), `requests` (distinct root requests; with `attempts`), `tokens` (observed input+output; with `input_tokens`, `output_tokens`, `unknown_token_attempts`), `cache_hit_rate` (cache-read ÷ total input over attempts that report both) and `blended_microusd_per_million` (settled cost × 1,000,000 ÷ tokens of settled attempts). Each tile has `value`, `previous` (the equal preceding period), `delta`, `change_ratio` (4 decimal places, null when the previous value is zero) and `daily` (every UTC day). All values are decimal strings; ratios and rates are exact decimals rounded to 4 places, not floats, and null when undefined. `top` lists the top 10 `models`, `keys` and `members` (null for ordinary members) by spend with `share` of total spend and the row's own `blended_microusd_per_million`; model rows add `model_id` (null when the alias mapped to several models in the period). At platform scope, `members` rows are per-user totals only (user ID, email, spend, requests, tokens); personal keys and request details never appear. Platform overview also returns `installation_budgets` (each installation budget's `amount_microusd`, `used_microusd` = `settled_microusd` + `held_microusd`, `unresolved_usage`, `exhausted` and window), unaffected by filters; workspace overview returns null.

Explore pivots one `metric` (`spend|requests|tokens|cache_hit_rate`) by `group_by` and optional `then_by` (`model|key|member|workspace|cost_center|provider|day`, distinct) with `top` 1–25 (default 10). Explore accepts the same filters as overview. It returns `total`, `rows` (`group`, `then`, `value`, `share` of the total for additive metrics, `held_microusd`, `unresolved_attempts`; at most 5 `then` values per group), `other` (the remainder outside the top groups), `truncated` and a daily `series` for the top groups; series values are decimal strings like every other count and amount (`null` for an undefined cache hit rate). `group_by=day` lists days with activity chronologically (no series). Grouping by `member` requires workspace-wide visibility (`403`, `reason:"workspace_wide_visibility_required"`). Held and unknown amounts are always reported separately and are never added to spend.

Allocation and interpretation: [enterprise costs](enterprise-costs.md). Exact payloads/reconciliation: [governance API](governance-api.md). Cache partition rules: [cache pricing](cache-pricing.md). This page makes no new validation or production-readiness claim.
