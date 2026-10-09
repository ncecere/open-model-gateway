# Alerts

Alerts tell people when budgets fill up, spend jumps, requests fail or a connection keeps failing. They notify; they never block a request. Admission enforces budgets on its own. Schema: `enterprise_migrations/0011_alerts.sql`. API: [management API](management-api.md#alerts).

## Where

- **Admin › Settings › Alerts** (`/admin/settings/alerts`): installation-wide rules (Rules tab) and their incidents (History tab). Platform Admins edit; Auditors read. Rules open on their own page (`/admin/alerts/{id}`, `/admin/alerts/new`) with Cancel and Save in the header.
- **Workspace › Settings › Alerts**: Team/Project rules, for that workspace's admins (actual membership). Platform readers may read them through the API; members don't see the tab.
- **Personal workspaces** have built-in budget alerts for their owner only (80% and 100%). Nothing is configurable and nobody else sees them, Platform Admins included.
- **Notifications** (`/notifications`, the bell in the top bar with the unread count): each person's alerts, with per-user read state.

## Rule kinds

All money is exact integer micro-USD. Spend is never estimated from floats, and unknown cost is never counted as zero.

| Kind | Fires when | Scopes |
|---|---|---|
| `budget_threshold` | Spend in a budget's current window reaches one of up to five percentages (default 50/80/100) of an applicable stacked budget. Layers: `installation`, `type` (type default, only while no platform override exists), `override`, `local` (workspace) and `key` (each key lineage with an active key). | Installation (all layers; Team/Project workspaces, never personal ones); workspace (all but `installation`) |
| `spend_spike` | Settled spend admitted in the last hour ≥ factor × the trailing 7-day hourly average (the 168 hours before the last hour), and ≥ a minimum amount. The factor is an integer percent (`300` = 3×, 110–100000). | Installation (all workspaces, as totals); workspace |
| `error_rate` | Attempts started in the window (5–1440 min): `failed / finished ≥ rate%`, with at least `min_requests` finished. | Installation; workspace |
| `provider_failing` | For each enabled connection (or one chosen connection): the last N relevant attempts in the window all failed upstream, or the upstream error rate over the window crosses its threshold with enough volume. Upstream failures are `upstream_unavailable`, `timeout_error`, `invalid_upstream_response` and `provider_configuration_error`. Successes count; request-specific rejections and cancellations neither count nor reset. | Installation only |

Budget spend is settled actual cost plus in-flight pending holds, as admission counts them. Reservations in state `unknown`, and pending holds with no bounded amount, are not added to spend: they are reported as `unknown_cost_requests`, and the UI says the spend may be higher. Spend spikes use settled cost only and flag unknown cost the same way. Zero (deny-all) budgets never alert.

A budget alert fires at the highest threshold reached. When spend later reaches a higher one, the open incident is marked `superseded` and a new one fires (one email). When the window rolls over, the budget is raised or the key is revoked, the incident resolves (`cleared`).

## Evaluation

`serve` runs a background evaluator every `GATEWAY_ALERT_INTERVAL_SECONDS` (default `60`; `0` disables it; at most `86400`; anything else fails startup). One-shot: `open-model-gateway alerts evaluate --once` evaluates every rule, sends pending email and prints counts.

- **Idempotent.** At most one open incident per rule (or built-in alert) and subject, enforced by a partial unique index. Repeated evaluation never fires twice; an incident resolves once (a trigger refuses re-resolution, edits and deletion).
- **Bounded.** One replica evaluates at a time (a transaction advisory lock; others skip that tick). Statements time out after 10 seconds and the whole pass after 60. Each rule runs in its own savepoint: a failing rule neither resolves its incidents nor affects other rules. At most 2000 rules and 5000 budget conditions per rule are considered.
- **No admission impact.** The evaluator reads with the runtime role and never takes the installation lock.
- Turning a rule off, deleting it (soft delete; its history stays) or disabling its workspace closes its open incidents as `rule_disabled`, without email.

## Delivery

- **In-app.** Visibility follows live authority, not a stored recipient list: installation incidents for Platform Admins and Auditors; workspace incidents for that workspace's current owners and admins (and for platform readers when the rule notifies Platform Admins); built-in incidents for the personal owner. Losing a role removes the incidents from that person's feed. The feed covers the last 90 days.
- **Email** through the [Admin › Settings › Email](settings.md#email) relay when one is set up, when an incident fires and when it clears: to the rule's recipients (workspace admins, Platform Admins and up to 10 validated addresses) or, for built-in alerts, the owner. Disabled users get nothing. Messages contain the summary, a scope label (Installation, the Team/Project or connection name, "Your personal workspace"), the rule name and time, and a link to Notifications when `GATEWAY_PUBLIC_URL` is set.
- Outcomes are recorded per incident transition: `sent`, `partial`, `failed` (with a category: `credential`, `address`, `connection`, `tls`, `authentication`, `rejected`, `timeout`), `not_configured` or `no_recipients`. Sending holds the delivery row's lock, so replicas never send the same email twice; up to 25 deliveries per tick, sequentially, with the relay's own timeouts. A delivery left pending for a day is marked `failed`/`interrupted`. Email failures never stop evaluation.

## Privacy

Incidents store a server-generated summary and typed facts (layer, period, amounts as strings, counts, window). They never contain prompt or response data, key names or ids, owner identities or request details. Platform rules never evaluate personal workspaces one by one (installation totals include them, as reports already do). Email bodies, recipients and relay replies are never logged.

## Storage and grants

`alert_rules` (soft-deleted; scope, workspace and kind fixed at creation), `alert_events` (incidents; resolve once), `alert_deliveries` (counts and a category per transition) and `alert_reads` (insert-only read marks). The runtime role has no DELETE or TRUNCATE on any of them; see `deploy/staging/runtime-grants.sql` and the probes in `verify-privileges.sql`. Migration 0011 also adds the `governance_reservations(workspace_id, admitted_at)` index used by budget alerts.
