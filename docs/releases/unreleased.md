# Open Model Gateway: unreleased

Notes for changes on `main` since v0.3.2. They become the next release's notes. Scale phases P0–P2 (measurement, lock-free reads, maintained rate counters) are summarized in the [changelog](../../CHANGELOG.md).

## Highlights

- **No installation-wide limits.** Limits now exist only on personal, team and project workspaces (workspace-type defaults, platform per-workspace overrides, tighten-only workspace caps) and on API keys. All of those stay hard limits, exactly as before. There is no installation budget (any period) and no installation requests per minute, tokens per minute, requests at once or jobs at once. Admission and settlement no longer read or write any installation-wide limit row, which removes the last global hot row from the request path's limit checks (scale plan, decision of 2026-10-09).
- **Scoped admission (no global lock on the request path).** Admission and settlement no longer serialize on the installation row. Requests of different workspaces run in parallel; requests of one workspace still serialize on its own totals rows, so budgets stay exact. Revocations, suspensions, membership, key, policy and catalog changes take scope locks that admission respects, so nothing is admitted after such a change commits. On the laptop load-test stack the ceiling rose from about 240 to about 1,050–1,080 successful requests/s, and three replicas now serve more than one (998 vs 814/s at 1000/s offered). One very hot workspace or key tops out around 210–250 requests/s.
- **Per-replica caches and one-replica background jobs.** Each replica now caches what it reads before admission (key metadata, candidate routes, routing configuration, immutable prices) and learns about configuration changes within milliseconds through PostgreSQL notifications (at most about a second without them). Admission still checks everything live, so a revoked key, removed member or tightened budget is refused immediately on every replica. Background jobs such as account cleanup, compaction and alert evaluation run on one replica at a time instead of on every replica. On the laptop load-test stack one replica now serves about 1,000 requests/s on a 10-connection pool (about 810 before) and three replicas about 1,340/s.
- **Monthly history partitions, hourly usage rollups and archival.** Request history (executions, reservations, ledger, audit events, storage usage hours) is partitioned by month, so old months no longer slow down or bloat current ones, and new months are created ahead of time automatically (an alert fires if they are not). Long usage ranges and the spend-spike alert read exact hourly rollups instead of scanning every request. Operators can archive a closed month (export with checksums, then detach) once it is past retention and has no pending or unknown cost. Measured: converting 2 M seeded attempts took 27.7 s with no read waiting longer than 5 ms and at most 0.68 s of exclusive locking; throughput without readers is unchanged from 2 M to 10 M attempts of history (three replicas 998.5/s at 1000/s offered on both); one hot workspace or key tops out lower than before (about 160–206/s, was 202–250/s).
- **History writes no longer lock their parents.** Every request used to take a shared lock on its workspace, key, route, price and cost-center rows through foreign keys, which under load created MultiXacts fast enough to force constant anti-wraparound vacuums (decision gate D3 failed in P6). The same relationships are now checked by triggers without locks, and those parent rows can never be deleted or re-keyed. In load tests the request path created no MultiXacts at all, and one busy workspace or key is back to about 240–255 requests/s (P6 had dropped it to 160–220).
- **See when one workspace or key nears its ceiling.** One workspace (or one API key) still serializes its own requests on its budget rows, which keeps its budgets exact but caps it at a few hundred requests/s on the reference hardware. New metrics show these waits without naming anyone (`gateway_scope_lock_wait_seconds`, `gateway_scope_lock_waiting`), and a new installation alert rule, **Admission ceiling**, names the Team or Project (or says "a personal workspace") whose request rate or lock wait crossed your threshold. Only Platform Admins see which workspace it is and get the email.
- **Faster cleanup of the reservation index admission reads.** Reservation partitions now vacuum after 10,000 dead rows (plus 1 %) instead of 20 % of the partition, and always clean their indexes, so the dead entries every settled request leaves behind no longer slow down admission while autovacuum waits. On the load-test stack this kept one replica at 1,032 instead of 989 requests/s at saturation and at 996 instead of 952/s afterwards.
- **No panics when the OS random number generator fails** (rand 0.10). Creating keys, invitations, sign-in sessions and encrypted files fails with an error instead (503, 500 or "unavailable"), never with a weaker generator.
- **Installation spend alert.** To keep an eye on total spend, Admin › Settings › Alerts has a new rule type, **Installation spend** (`spend_threshold`): it fires when installation-wide spend (settled plus on hold, all workspaces, personal ones as totals) in the current day, week, month or lifetime reaches up to five percentages of an amount you choose. It only notifies; it never blocks a request. Spend is exact integer micro-USD; requests with unknown cost are reported separately, never counted as zero.

## Migration

`0026_remove_installation_limits.sql`:

- Records the removed installation configuration (rates and budgets) once in the audit log as `policy.installation_removed`.
- Converts installation budget alert rules to installation spend rules, keeping thresholds and recipients (details in [alerts](../alerts.md#upgrade-from-installation-budget-alerts-0026)). Each changed rule gets an `alert_rule.installation_layer_removed` audit event. A rule that watched only the installation budget while none was set could never fire and is soft-deleted.
- Deletes installation rows of `policy_budgets` and makes them unrepresentable; drops `installation_policy`.
- Stops maintaining the installation scope of `budget_totals` and deletes those rows. They were derived data (trigger-maintained sums that `budget verify` recomputes from history) and equal the sum of the workspace rows exactly. Reservations, executions, the ledger, prices, audit events and incidents are not modified (open installation-budget incidents that no rule continues are resolved as `superseded`, without email).
- Keeps the installation singleton row (settings, branding, the catalog lock and management serialization use it).

It briefly freezes history writes, like 0015 and 0025. On the 2 M-attempt load-test database it took 0.9 s. On the laptop load-test stack the overload ceiling rose 9–21 % with the change ([operations](../operations.md#combined-capacity-p1--p2--no-installation-limits)).

`0027_scoped_admission.sql` (functions and triggers only; no table, column or history change, no write freeze):

- Lock helpers for admission (`omg_admission_locks`), management (`omg_lock_scopes`) and totals/counter rows (`omg_lock_scope_rows`, which may create zero-valued `budget_totals`/counter rows; zeros equal missing rows for every reader and `budget verify`).
- Authority triggers on users, grants, memberships, workspaces, service accounts, keys, key restrictions, policies, budgets and catalog assignments take the matching exclusive scope lock; catalog tables take the exclusive catalog lock.

`0028_change_notifications.sql` (no write freeze; no existing table, column or history change):

- `config_versions`: one version per topic (`access`, `catalog`, `keys`, `policy`, `settings`), seeded, forward-only.
- Deferred constraint triggers (`omg_config_*`) on the catalog, routing, price, entitlement, membership, key, policy and settings tables bump the topic once per transaction at commit and `NOTIFY omg_config, '<topic>:<version>'`.

`0029_work_leases.sql` (no write freeze):

- `work_leases`: seeded singleton-job leases (`alerts`, `compaction`, `file_sweep`, `lifecycle`, `maintenance`, `metrics`) with forward-only fencing epochs, and the helpers `omg_lease_acquire`, `omg_lease_fence`, `omg_lease_complete`, `omg_lease_release`.

`0030_partition_history.sql` and `0031_partition_audit.sql` (`-- no-transaction`; run drained; every step resumes on the next `migrate`):

- Converts `inference_executions`, `governance_reservations`, `monetary_ledger`, `audit_events` and `storage_usage_hours` to monthly range partitions. Existing executions, reservations, audit events and storage hours are **attached** as `<table>_p_legacy` without a rewrite: the new keys are built as indexes first (`SHARE` lock; or beforehand with `open-model-gateway partitions prepare`), the legacy bound is proven with `CHECK … NOT VALID` then `VALIDATE`, and the swap is metadata only. The ledger gains `admitted_at` and is copied once into its partitioned replacement under a `SHARE` lock (reads continue), verified by count, amount sum and a per-row hash.
- Keys now include the month: executions `(id, started_at)`, reservations `(execution_id, admitted_at)`, ledger `(id, admitted_at)`, attempts `(root_request_id, attempt_number, started_at)`. The reservation → execution foreign key includes the time (`admitted_at = started_at`); the migration refuses, unchanged, if any row differs. The ledger → reservation key is `(execution_id, admitted_at)`.
- `async_jobs → inference_executions` and `realtime_responses → governance_reservations` foreign keys become insert-time existence checks.
- Registry `history_partitions`, helpers `omg_ensure_partitions` (`SECURITY DEFINER`), `omg_partition_coverage`, `omg_partition_bounds`; `partitions_missing` built-in alert kind; `partitions` work lease. No default partitions.
- Measured on 2 M seeded attempts (laptop, parallel workers off): 27.7 s in total; the longest exclusive lock on a history table 0.68 s; reads of recent history never waited more than 5 ms; ledger counts, sums and row hashes identical. Budget totals verify afterwards. `0033` also schema-qualifies the calls between CHECK validator functions: `pg_restore` loads data with an empty `search_path`, so before this a backup of any database with reservations could not be restored.

`0032_usage_rollups.sql`: `usage_rollups_hourly`, `usage_rollup_hours`, `usage_rollup_progress`, `usage_rollup_dirty` (change markers written by row triggers on executions and reservations, only for rows of hours that ended before the writing transaction began), `omg_usage_aggregate`/`omg_usage_rows`, and the `rollups` work lease. Nothing is backfilled in the migration; `serve` rolls 168 hours per minute, and `open-model-gateway rollups run --once` catches up faster.

`0033_partition_archive.sql`: schema `omg_archive` (detached months), `archived_partitions` and `archived_budget_contributions` (immutable).

`0034_history_parent_checks.sql` (run drained; metadata-only locks; no history row changes):

- Triggers refuse `DELETE`, `TRUNCATE` and `id` changes on `workspaces`, `api_keys` (also `workspace_id`), `deployments` and `cost_centers`, and `TRUNCATE` on `async_jobs`, for every role. (`deployment_prices` and `async_jobs` already refused deletion and identity changes.)
- `BEFORE INSERT OR UPDATE OF <references>` row triggers on `inference_executions` and `governance_reservations` check the scoped references with plain reads (SQLSTATE `23503` on violation).
- Drops `inference_executions_workspace_id_fkey`, `…_workspace_id_api_key_id_fkey`, `…_deployment_id_fkey`, `…_cost_center_id_fkey`, `inference_executions_batch_job` and `governance_reservations_deployment_id_price_id_fkey`. Reservation → execution and ledger → reservation keys remain.
- `budget_totals_maintain()` joins reservations to executions on the admission month (one partition).
- `history_orphans` built-in alert kind and the `history_verify` work lease.
- Measured: 35 ms on the 2 M-attempt and 43 ms on the 10 M-attempt load-test databases.

`0035_counter_autovacuum.sql` (metadata only; no write freeze; `SHARE UPDATE EXCLUSIVE` per partition, which waits for a running vacuum of that partition):

- Every `governance_reservations` partition: `autovacuum_vacuum_scale_factor=0.01`, `autovacuum_vacuum_threshold=10000`, `vacuum_index_cleanup=on`, `autovacuum_vacuum_cost_limit=2000`, `autovacuum_vacuum_cost_delay=2`.
- `omg_partition_storage(parent)` (owner only) and `omg_ensure_partitions` applying it to every new month partition. Other history tables keep the defaults.

`0036_admission_ceiling.sql` (run drained like every migration):

- `admission_lock_waits` (per minute and workspace/key-lineage scope: admissions that waited at least 10, 25, 50, 100, 250, 500, 1000, 2500 ms for their scope locks; threshold autovacuum like the counter tables).
- `alert_rules.ceiling_requests_per_second` and `ceiling_lock_wait_ms`; the `admission_ceiling` kind for rules and incidents (installation only, `window_minutes` 5–10, no listed email addresses or workspace admins, enforced by constraints).

## New environment variables

- `GATEWAY_PARTITIONS_AHEAD_MONTHS` (default 3, 2..12): future month partitions `serve` keeps (hourly, `partitions` lease).
- `GATEWAY_USAGE_ROLLUPS` (`auto` default, or `off`): `off` answers every usage read from raw history (identical results, slower).
- `GATEWAY_HISTORY_RETENTION_MONTHS` (default 25, 1..1200): months `archive partition` keeps hot (CLI `--retention-months` overrides).
- `GATEWAY_CONFIG_CACHE` (`on` default, or `off`): per-replica caches of pre-admission reads. `off` reads everything live (no LISTEN connection, no version poll).
- `GATEWAY_LISTEN_DATABASE_URL` (default `DATABASE_URL`): the one session connection per replica that LISTENs for configuration changes. Behind a transaction-mode PgBouncer, point it at PostgreSQL directly (or a session-mode pooler database). Count one extra connection per replica in `max_connections`.
- `GATEWAY_ADMISSION_MODE` (`scoped` default, or `global`): `global` is an operational rollback to the former installation-row protocol, kept for one release. It needs no schema change; invalid values fail startup.

## Breaking and behaviour changes

- **Manual SQL against history:** inserts into `monetary_ledger` must supply `admitted_at` (the reservation's); a reservation's `admitted_at` must equal its execution's `started_at`; updates that would move a row to another month are refused; partitions are reached only through their parents (the runtime has no partition privileges). New request, attempt, ledger and audit ids are UUIDv7 (time-ordered; `x-request-id` values change shape, not format).
- **New CLI commands:** `partitions ensure|status|prepare`, `rollups run --once`, `archive partition`, `archive verify`, `history verify [--since]`.
- **Parents of history can't be deleted (0034).** `DELETE`/`TRUNCATE` of workspaces, API keys, deployments and cost centers, and changing their ids or a key's workspace, now fail for every role including the schema owner (SQLSTATE `23503`). The product never did this (it disables, revokes and archives); operator scripts that removed unreferenced rows must disable them instead. History inserts that reference a missing or out-of-scope parent still fail with `23503`, but the error names the check (`execution key is not a key of its workspace`, …) instead of a foreign-key constraint.
- **New built-in incident** "Request history references missing or mismatched parents" (`history_orphans`, installation scope, critical, Platform Admins notified), opened by the hourly `history_verify` job or `history verify`; only a full clean `history verify` resolves it. New metric `gateway_history_verify_findings`; `gateway_background_runs_total{job}` and `gateway_work_leases_held{lease}` gain `history_verify`.
- **New metrics:** `gateway_history_partition_months_ahead{table}`, `gateway_usage_rollup_hours_total{reason}`; `gateway_background_runs_total{job}` gains `partitions` and `rollups`.
- **New built-in incident** "History tables are running out of monthly partitions" (`partitions_missing`, installation scope, Platform Admins notified).

- **Admission ceiling (0036).** New metrics `gateway_scope_lock_wait_seconds{lock,path}` and `gateway_scope_lock_waiting{path}`; `gateway_metrics_collection_errors_total{collector}` gains `scope_lock_waits`; `gateway_background_runs_total{job}` gains `lock_wait_prune`. New alert rule kind `admission_ceiling` (`/platform/alerts/rules`; workspace rules refuse it). Its incidents carry `workspace` and `details.workspace_name` for Platform Admins only; Auditors get them without. Each replica writes its per-scope wait counts every 5 s (one small upsert, off the request path).
- **rand 0.10: OS RNG failures fail closed.** Key creation and rotation, invitations: `503` with message "Secure random number generation unavailable" (nothing created). Sign-in and callback: `500` (no login attempt or session stored). Encrypted file writes and the file-store health probe: `unavailable`. `bootstrap-dev`: exits non-zero. Before, the process panicked. The PKCE verifier is now drawn by the gateway (32 bytes, base64url) instead of the OIDC library.
- **Routing:** with rand 0.10 the seeded weighted choice among equal-priority routes can pick a different route for the same request seed than before (same weights and distribution; seeds are per request).

- **Configuration changes reach other replicas' caches within milliseconds, not instantly.** Authorization is unaffected (admission re-checks live): during that window a revoked, disabled or expired key (or one whose owner lost access) used on another replica is refused by the live re-check with the same `401 authentication_error` as authentication (nothing is sent upstream), and that replica forgets the key at once. Files, batches, videos and realtime always authenticate live.
- **Singleton background jobs run on one replica** (the holder of its lease). With N replicas, alert rules are now evaluated once per interval instead of up to N times. `gateway_reservations_held` is exported only by the `metrics` lease holder (sum and max agree); `gateway_alert_evaluations_total{result="skipped"}` counts the other replicas' ticks.
- **New metrics:** `gateway_cache_lookups_total{cache,result}`, `gateway_config_changes_total{topic}`, `gateway_config_poll_failures_total`, `gateway_config_listener_up`, `gateway_work_leases_held{lease}`, `gateway_work_lease_terms_total{lease}`, `gateway_background_runs_total{job,result}`.
- **Manual SQL configuration changes** now also lock the `config_versions` rows at commit (after every other lock), so concurrent configuration transactions serialize briefly at commit.

- **Per-replica database connections now raise throughput.** Each replica admits on up to half of `GATEWAY_DATABASE_MAX_CONNECTIONS` and settles on up to three tenths (before: a quarter each, because the global lock serialized them anyway). Size PgBouncer and `max_connections` for the busier pools.
- **New metric** `gateway_lock_deadlock_retries_total{path}` and admission/settlement phase `lock_rows`; in scoped mode the admission `price` phase is folded into `read`.
- **Manual SQL changes** of users, grants, keys, policies or catalog assignments now wait for in-flight admissions of the affected scope (the triggers take the scope lock). Change several scopes in one transaction in the order type → workspace → user → key lineage to avoid deadlocks.

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
4. Reapply `deploy/staging/runtime-grants.sql` (alert rules gain `spend_period`/`spend_amount_microusd` update grants; `installation_policy` grants are gone; 0027 adds `EXECUTE` on the scope-lock helpers; 0028 adds `SELECT`/`UPDATE(version,changed_at)` on `config_versions` and `EXECUTE` on `omg_config_bump`; 0029 adds `SELECT`/`UPDATE` of the lease columns on `work_leases` and `EXECUTE` on the lease helpers).
4a. If replicas connect through a transaction-mode PgBouncer, set `GATEWAY_LISTEN_DATABASE_URL` to PostgreSQL directly and allow one more connection per replica.
4b. P6 (0030–0033): on large installations run `open-model-gateway partitions prepare` (migrator) before the drain; reapply runtime grants (adds `SELECT` on `history_partitions`, rollup tables and archive records, `EXECUTE` on `omg_ensure_partitions(integer)` and the coverage/rollup helpers; partitions get no grants). After starting, run `open-model-gateway partitions status` and `open-model-gateway rollups run --once`.
4c. 0034: no new grant; reapplying `runtime-grants.sql` is still required after every migration.
4d. 0035: no new grant. 0036: reapply runtime grants (`SELECT`/`INSERT`/`DELETE` and `UPDATE(waits)` on `admission_lock_waits`; `UPDATE` of the two ceiling columns on `alert_rules`). Review the [recommended autovacuum settings](../operations.md#postgresql-settings-for-hot-rows-and-group-commit) (`autovacuum_max_workers`, `autovacuum_naptime`, `autovacuum_work_mem`).
5. Run `open-model-gateway budget verify` and expect `mismatch_count: 0` and `rate_mismatch_count: 0`, and `open-model-gateway history verify` and expect `finding_count: 0`.
6. Review Admin › Settings › Alerts: converted rules are named after the original, with "(installation … spend)" for extra periods.
