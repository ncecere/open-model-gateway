# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.8.0](v0.8.0.md). They become the next release's notes.

## Development HTTP telemetry exports are loopback-only after DNS (#313)

**Behavior change for development installations.** Plain-HTTP OTLP traces, audit OTLP logs and audit webhooks (allowed only with `GATEWAY_ENV=development`) now connect only to canonical loopback addresses after name resolution, including IPv4-mapped loopback. If any resolved address is not loopback, the whole answer is refused, even when other answers are loopback or the address is covered by `GATEWAY_EGRESS_PRIVATE_ALLOW`. Literal non-loopback HTTP endpoints are refused before any connection. HTTPS exports keep the general egress policy, and production (non-development) installations are unaffected: they never allowed plain HTTP. To export to another host, use HTTPS. See [operations](../operations.md#outbound-connections).

## Alert webhook dashboard (#45, slice 3)

**Admin › Settings › Alerts › Webhooks** now manages saved signed receivers: create disabled, enable/disable, edit names and signing references with optional bounded previous-key overlap, confirm deletion, send fixed single-attempt tests, and inspect bounded delivery status/history. URLs remain immutable; signing secrets are never displayed. Auditors see redacted read-only status without mutating controls, URLs or references. Test throttling explains the limits without a countdown or automatic retry. Receiver guidance covers HMAC-SHA256, timestamp verification and durable delivery-ID deduplication. All webhook audit codes already have dashboard labels.

Adds Vitest contract/component coverage and a real-gateway Playwright journey with a disposable signed loopback receiver and 1440/390-width screenshots. No backend, migration, privilege or dependency changes. See [dashboard administration](../alerts.md#dashboard-administration-slice-3).

## Signed alert webhook delivery (#45, slice 2)

Saved enabled receivers now receive `omg.alert.v1` envelopes signed with HMAC-SHA256 over timestamp plus exact queued bytes, with optional dual signatures during the saved overlap (at most 24 hours). Signing variables are resolved only by the dedicated allowlist and require 32–4096 bytes. Short destination-before-outbox claims use generation checks and 90-second owner/token leases; fenced completion preserves terminal outcomes. Dispatch is bounded to 25 steps, a 60-second claiming budget and a 70-second hard timeout, with 1m/5m/30m/2h backoff and at most five attempts in 24 hours. Only connection failures, timeouts, 408/429 and 5xx retry. TLS, credentials, redirects and policy refusals are terminal. Egress checks include literals and checked DNS; proxies, redirects and implicit retries are disabled, requests time out after 10 seconds and response bodies are not read.

Adds `POST /platform/alerts/webhooks/{id}/test` (fixed saved-receiver single attempt, no body or `{}`, transactionally audited limits 2/user/minute, 2/destination/minute, 20/installation/hour) and `GET /platform/alerts/webhooks/{id}/deliveries` (bounded metadata/status, existing redaction). Waiting counts exclude stale generations. Fixed-label attempt/delivery metrics and oldest-waiting age expose failures without destination identifiers. Receiver guidance requires constant-time signatures, ±300-second timestamp checks, header/body delivery-id equality and durable deduplication. Crash-after-acceptance duplicates remain possible; there is no replay endpoint. No new migration or privilege is needed beyond 0057. The dashboard and browser acceptance journey are described in slice 3 above.

## Key dialog lifetime hardening (#276)

Each ask now gets a fresh dialog lifetime (explicit retries within one dialog keep it), and successful non-secret actions cannot be repeated through retained submit callbacks. Key creation and rotation suppress late disclosures after retirement; a creation that completes after retirement, or any 2xx without a token, still invalidates the keys list so the key can be found and revoked. Admitted create requests remain uncancelled on local retirement so a newly created key can appear in the list and be revoked. A successful response missing its token shows an error; retry remains explicit, never automatic.

## Fresh CNPG installs restrict database ACLs (#315)

Fresh, explicitly installed chart-managed CNPG clusters now revoke PUBLIC/runtime CONNECT
on `postgres` and `template1` and PUBLIC/runtime TEMPORARY on the application database.
Bootstrap seeds passwordless CNPG operational roles and preserves their maintenance CONNECT
explicitly; CNPG still supplies replication, monitoring and PgBouncer authentication privileges.
Application CONNECT is retained. This changes only one-time initdb hooks, not the verifier,
`grants apply`, migrations, external databases or existing clusters. Existing installations need
an explicitly reviewed [manual remediation](../postgresql-ha.md#existing-cnpg-cluster-acl-remediation-315),
including operational-role preconditions and strict tagged verification before restarting gateways.
See [bootstrap lifecycle evidence](../kubernetes.md#cnpg-bootstrap-database-acls-315) for CNPG 1.30.1.
The application TEMP revoke overlaps the separate #295 work; this change stays in the CNPG path.

## Runtime TEMP privilege revoked (#295)

`grants apply` now revokes `TEMPORARY` from `PUBLIC` and `gateway_runtime` on the connected gateway database, removing both PostgreSQL's default PUBLIC grant and any old explicit runtime grant. Runtime can no longer create temporary objects to shadow unqualified queries. Runtime code does not need TEMP; migrations that use temporary objects still run as the database/schema owner.

**Upgrade:** this change adds no schema migration or runtime privilege. Explicitly reapply the matching binary's grants as the database/schema/object owner (`gateway_migrator` in staging), then run `grants verify` with a role allowed to `SET ROLE gateway_runtime`; verification now attempts `CREATE TEMP TABLE` and requires its refusal. Use a **dedicated gateway database**: all roles relying on PUBLIC TEMP lose it there, not just runtime. The owner/migrator's privileges and all unrelated databases are unchanged. `serve` never migrates or applies grants automatically. See [database authority](../staging.md#database-authority-and-releases).

## Transaction isolation independent of PostgreSQL defaults (#281)

Normal gateway transactions now explicitly begin at **READ COMMITTED**, so database, login-role or connection defaults of REPEATABLE READ or SERIALIZABLE cannot leave lock-then-check queries using a stale snapshot. Session defaults are not rewritten, and session read-only policy is still inherited. Intentional reporting snapshots remain REPEATABLE READ and read-only; their bounded-staleness contract and the elevated-isolation refusal guards are unchanged. No migration or new database privilege is needed. See [transaction isolation](../governance.md#transaction-isolation).

## Local-upstream approvals in special-use ranges are now refused (#203)

**Breaking change for affected installations.** `GATEWAY_LOCAL_UPSTREAMS` approvals that pin an address in one of the ranges below are refused at startup. v0.8.0 only warned. An installation whose approvals are all public or RFC 1918 (private) addresses is not affected and needs no action.

| Range | Category |
|---|---|
| `198.18.0.0/15`, `2001:2::/48` | benchmarking |
| `192.0.0.0/24` | IETF protocol assignments |
| `192.88.99.0/24` | deprecated 6to4 relay |
| `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`, `3fff::/20` | documentation |
| `fd00:ec2::/32` (all of it, including what was accepted besides `fd00:ec2::254`) | AWS IPv6 metadata range |

These are the ranges the outbound egress policy already refused for every other client; approvals now follow the same table (`special_use.rs`), so the two cannot drift. The rules are over both schemes: the `fd00:ec2::/32` range was also accepted over plain HTTP like a private address, and is refused there too. An address that carries an IPv4 address (IPv4-mapped `::ffff:a.b.c.d`, compatible `::a.b.c.d`, 6to4, Teredo) is judged by the IPv4 address it carries, so `2002:c612:1::1` (6to4 of `198.18.0.1`) is refused as well. NAT64 (`64:ff9b::/96`) stays refused, as before. Every other address keeps its exact decision: public over HTTPS, RFC 1918 and unique-local over HTTP or HTTPS, loopback only with `GATEWAY_ENV=development`, and the same unconditional refusals as before (unspecified, link-local, multicast, broadcast, carrier-grade NAT, cloud metadata).

**How `serve` fails.** `open-model-gateway serve` stops before it connects to the database and exits with status 1, naming every affected approval (host, pinned address and category) and the setting, for example:

```
Error: GATEWAY_LOCAL_UPSTREAMS: refused approval(s), each pins an address in a range the gateway never connects to: host models.internal pins 198.18.0.1 (category: benchmarking). Change each approval's address to a public or RFC 1918 (private) address, or remove the approval (#203; see the release notes).
```

No credential can appear in it: the message contains only the URL's host, the pinned address and a category label (an approval URL cannot carry credentials; one that does is rejected as invalid). Only `serve` loads approvals; `migrate`, `grants` and the other commands are unaffected. A replica that fails this way never answers `/health/ready`, so a rolling deployment keeps the old replicas until you fix the setting.

**Check before you upgrade:**

1. Read `GATEWAY_LOCAL_UPSTREAMS` (every approval's `addresses`) against the table above.
2. If you run v0.8.0, look for the startup `WARN` it logs per affected approval (host and category), or `GET /health/ready`: `checks.local_upstream_approvals` is `"warning"` when any approval is affected. Stay on v0.8.0 until that is `"ok"`.

**What to change:** replace the approval's address with the real address of the model server, an ordinary public address or an RFC 1918 one (for example the `10.x` address of the host), or remove the approval. If you remove an approval, provider connections that use its endpoint can no longer be saved or used until they point at an approved endpoint; re-point or disable them first. Change `GATEWAY_LOCAL_UPSTREAMS` in the deployment's configuration before rolling out the new release.

**`/health/ready` is unchanged in shape.** `checks.local_upstream_approvals` stays in the response and is now always `"ok"`: the warning phase has ended, because an affected approval stops startup instead of being reported. It is kept so dashboards and alerts that read it keep working; do not rely on it for anything beyond "this replica started". It never affected the `ready`/`not_ready` decision and still carries no endpoint names.

Removed: the v0.8.0 startup warning (it can no longer apply). See [provider adapters](../provider-adapters.md#approve-local-endpoints-out-of-band), [operations](../operations.md#health-endpoints) and the threat model (EGR-3).

## Saved alert webhook authority and schema (#45, slice 1)

Adds migration **0057_alert_webhooks**, operator URL/signing-reference allowlists, a platform-only destination management API and a separate retained outbox. Destinations start disabled; live URLs and queued payloads are immutable, and final soft deletion scrubs URLs and signing references. Read-only tokens receive redacted status. The enqueue trigger takes no explicit destination locks (its outbox foreign key takes `FOR KEY SHARE`, which management's `FOR NO KEY UPDATE` does not block, so destination edits never stall alert evaluation), isolates webhook errors per destination from email/built-in evaluation, builds every envelope in one SQL function whose exact bytes the outbox guard requires (for direct runtime inserts too), and snapshots only approved installation transitions through an explicit privacy projection, excluding workspace rules, personal incidents, all key-expiry reminders and all admission-ceiling incidents. An enqueue failure or a racing destination emits a sanitized warning naming only the event id and skips just that destination, without backfill. Each destination has a trigger-owned enable generation, incremented on every `enabled` change; queued rows record the generation the enqueue read, the outbox guard refuses a stale one and re-enabling ends older waiting rows, so a late-committing enqueue can never be replayed after a disable and re-enable. Delivery in slice 2 must recheck eligibility before every claim (including deleted destinations, scrubbed NULL URLs and row generation equal to the destination's), locking destination before outbox row. A rule edited between fire and clear can drop the `resolved` webhook. Runtime column grants and refusal probes cover both tables. Apply `migrate` and `grants apply` explicitly; exact schema-lineage readiness remains required.

**Slice 1 alone sends no webhook:** the signed worker, test-send, delivery-status routes and metrics are added by slice 2 above; the UI is added in slice 3 above. Saving never contacts receivers or resolves signing bytes. See [alerts](../alerts.md#saved-platform-webhooks-45-slice-1), [API](../management-api.md#saved-alert-webhooks) and [configuration](../operations.md#configuration-reference).

## Bounded API-key issuance (#253, slice A)

Creating or rotating an API key now counts in a rolling 24 hours against two budgets: the holder's (a member, or a service account) and the workspace's. Defaults are **50 per holder and 500 per workspace**; both are editable on the workspace-type default, platform per-workspace override and (tighten-only) workspace limits, from 1 to 10,000. If neither the override nor the type default sets a value, the 10,000 ceiling applies; nothing is unlimited. Personal workspaces use the type default and any platform override. A spent budget refuses the request with `429`, `reason:"key_issuance_limit_exceeded"`, a `Retry-After` header and `scope`, `operation` and `retry_after_seconds`. Authorization and validation errors still come first, the body carries no amounts, and nothing is audited for a refusal (it is counted in `gateway_key_issuance_refused_total{scope,operation}`). Raising a limit admits at once, and lowering one never revokes keys.

New `GET /workspaces/{ws}/key-issuance` returns the caller's budget (and, for workspace admins, the workspace's). The limits editor gains two rows, and the API keys page shows "X of Y issued in the last 24 h" and explains a spent budget instead of failing at Create. Counting uses the existing workspace lock and the append-only `api_keys` table: no counter rows and no new lock.

**Override semantics:** on a platform override these two limits behave differently from the others: a field that is set replaces the type default, and an empty one **inherits** it (it is not "no limit"). So an override written by a replica that predates this release, during a rolling upgrade, never loosens a workspace; set an explicit value (up to 10,000) for more.

**Upgrade:** migration **0053_key_issuance_limits** adds the six policy columns (type defaults 50/500) and makes `api_keys.created_at` default to `clock_timestamp()`; its brief ALTER locks are the only locks it holds. **0054_key_issuance_index** builds `api_keys_workspace_created` with `CREATE INDEX CONCURRENTLY` outside a transaction, so neither key authentication nor key writes are blocked while it builds. If that build is interrupted, rerun `migrate`: before running migrations it drops the invalid leftover index with `DROP INDEX CONCURRENTLY` (a standalone statement, so nothing is blocked), and 0054 builds it again. `grants apply` now grants the runtime role `INSERT` on explicit `api_keys` columns that exclude `created_at`, so a key cannot be backdated out of the window. Run `migrate` and `grants apply` with migrator credentials first; `serve` never does either. Replicas still on the previous binary ignore the columns, so issuance is unenforced only during the rolling-upgrade overlap. Fair alert dispatch (slice B) is described below.

**Operating `migrate`:** `migrate` and `grants apply` now take their session lock by polling `pg_try_advisory_lock` (about every 100 ms) instead of blocking in `pg_advisory_lock`, so a second migrator (or a retried Job) waits without holding a transaction open; a blocked lock call would have deadlocked against the concurrent index build. `CREATE INDEX CONCURRENTLY` (and the recovery's `DROP INDEX CONCURRENTLY`) wait for every transaction older than themselves and there is no `lock_timeout`, so a session left idle in transaction stalls `migrate` until it ends. Find it with `SELECT pid, usename, state, xact_start, left(query, 80) FROM pg_stat_activity WHERE state IN ('idle in transaction', 'idle in transaction (aborted)') ORDER BY xact_start;` and end it with `SELECT pg_terminate_backend(<pid>);` (or fix the client); `migrate` then continues. `migrate` is safe to cancel and rerun.

See [governance](../governance.md#key-issuance), [governance API](../governance-api.md) and [management API](../management-api.md).

## Fair alert dispatch (#253, slice B)

Alert email slots are now shared fairly across workspace buckets, plus one bucket for installation-scope events. Each tick prefers the due bucket served least so far, then the least recently served across ticks (never-served first), then the oldest due head. The evaluator process keeps a capped 4,096-bucket last-served map, evicting the oldest; it may reset on restart or lease failover. A quiet workspace no longer waits behind another workspace's entire backlog; a lone workspace can still use all 25 slots. Multi-replica fairness is approximate. Live recipient authorization, fenced leases, retry backoff, the send budget and 24-hour interrupted cutoff are unchanged.

**Upgrade:** explicitly run **0055_alert_delivery_buckets** (column, insert trigger and pending-only backfill; its `ALTER TABLE` ACCESS EXCLUSIVE lock is held through the backfill until commit, so the locked interval grows with the pending backlog and event-join work—plan a maintenance window for large queues) and **0056_alert_delivery_bucket_index** (partial pending index built concurrently). 0055 resets its 60-second lock-acquisition timeout before the concurrent build; 0056 has no lock timeout. `migrate` recovers interrupted index builds as for 0054; `serve` never migrates. Older outbox-aware replicas continue FIFO during overlap, but their inserts receive the correct bucket from the trigger. Runtime grants need no new privilege; `bucket_id` cannot be updated by the runtime role. Terminal deliveries are not rewritten or replayed.

Claims use one captured database timestamp as an index range bound to skip expired backlog. A claim statement timeout logs a warning and ends dispatch for the tick; cleanup still runs. Label-free gauges `gateway_alert_delivery_pending_buckets` and `gateway_alert_delivery_oldest_pending_age_seconds` expose pending bucket count and oldest age, including overdue work awaiting cleanup. See [alerts](../alerts.md#recipient-outbox-46).

## Fixed

- **Bounded dashboard access refresh (#291).** A refused management GET now triggers at most one automatic session refresh per navigation. A repeated refusal retires private cached data and one-time credentials and shows a stable permission notice instead of remounting and retrying indefinitely. Navigating rearms automatic refresh; an explicit access refresh can recover the denied view, and a 401 still ends the session.

## Added

- **Aged unknown-hold alerts (#101, slice C).** Migration **0058** adds built-in 24-hour warning / 168-hour critical incidents, with one open incident per shared workspace and one identity-free personal aggregate. In-app visibility follows live platform-reader/shared-admin authority; members and personal owners retain queue access but get no role-only alert. Static incident content links to the Holds tab; silent retirement/escalation, rolling cooldowns and bounded fenced evaluation never mutate accounting or release money. Old replicas cannot see the new source-less kind. Email and dedicated metrics follow in slice D. Apply migrations explicitly; runtime grants need no new authority.

- **OTLP metrics groundwork (#99, slices 1–2).** Shared trace/audit HTTP validation, bounded trace acknowledgements and a scrape-derived metrics snapshot/encoder are implemented. Metrics settings are parsed and validated (`GATEWAY_METRICS_OTLP_URL`, `GATEWAY_METRICS_OTLP_HEADERS_ENV`, `GATEWAY_METRICS_OTLP_INTERVAL_SECONDS`, `GATEWAY_METRICS_OTLP_TRAFFIC`), but **a configured URL explicitly refuses startup in this release**: no metrics worker is wired yet. Leave all four unset. This avoids silently accepting an export that sends nothing. The future traffic opt-in defaults off and includes personal-workspace traffic as identity-free aggregates. The golden inventory pins privacy classes; System families take priority over Traffic under export caps. The encoder permits existing operational-enum `path` labels only on the scope-lock and lock-deadlock-retry families with `admission`, `settlement` or `reconciliation` values. Prometheus adds only fixed-enum exporter health metrics; no schema, dependency or listener change. See [OTLP metrics groundwork](../operations.md#otlp-metrics-groundwork-99-slice-2) for the Traffic data-class decisions and runtime requirements.

- **Aged unknown-hold review API (#101, slice A).** Read-only workspace and platform queues for reservations unknown for at least 24 hours in database time. Workspace activity remains owner/admin/own-human-key scoped; the platform queue exposes Team/Project rows (including retained holds after workspace disable) and one identity-free personal aggregate. Exact nullable retained floors are never spend or upper bounds. Keyset pagination uses strictly validated, stateless position cursors that work across replicas; every page reapplies the caller's full SQL visibility predicate, so cursor edits or cross-scope reuse cannot broaden access. Workspace routes refuse admin tokens; platform reads require `platform:read`. No migration, new privilege, alert, UI or accounting action in this slice. See [governance API](../governance-api.md#aged-unknown-hold-review-queue).
- **Holds tab on Usage & costs (#101, slice B).** `?tab=holds` on Admin (`/admin/costs`) and in each workspace (`/workspaces/{ws}/costs`) lists aged unknown holds oldest first, with the age, an exact "at least" floor (decimal strings, never rounded through JavaScript numbers), an "Unbounded" badge, the unresolved reason, and the model, provider and workload. The page states that a floor is not an upper bound and that nothing is refunded or released. Members see only their own keys' holds and personal owners their own; Admin sees Team/Project rows plus one combined personal total with no workspace, owner or request and no drill-down. "Load more" follows the API's keyset cursor. Rows link to the request only where the viewer can already open that request page, and Platform Admins can open the existing evidence-backed reconcile dialog from rows the API marks `can_reconcile`: no new write path, and no accept-as-zero, refund or release. A gateway without the queue (404) shows "not available" instead of an empty list. Notifications of the coming `aged_unknown_hold` kind get an "Open holds" action. No backend, migration or dependency change.
