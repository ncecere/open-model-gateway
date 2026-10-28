# HA PostgreSQL operations

For operators running two or more gateway replicas on highly available PostgreSQL (a primary with at least one standby, managed by CloudNativePG (CNPG), Patroni or a cloud service). It covers what the gateway does when the primary fails over, replication, connection sizing, backups and monitoring. Details live in the [operations runbook](operations.md) and the [Kubernetes guide](kubernetes.md).

Measurements come from the bitop-talos homelab on 2026-10-14 (UTC, #25): CloudNativePG 1.30.1, PostgreSQL 17.6, a primary plus one synchronous standby (`ANY 1`, quorum) on `local-ssd`, PgBouncer 1.25.2 in transaction mode through the CNPG Pooler, and 1, 3 and 6 gateway replicas. Gaps are listed in [Not yet measured](#not-yet-measured).

## Checklist

- Use the explicit required/quorum policy with three CNPG instances, not legacy min/max sync settings ([replication](#synchronous-or-asynchronous-replication)).
- With chart-managed CNPG, keep `cnpg.cluster.smartShutdownTimeout` short (chart default 15 s, [failover timing](kubernetes.md#failover-timing)).
- Point `GATEWAY_LISTEN_DATABASE_URL` at the primary directly, never at a transaction-mode pooler ([PgBouncer caveats](kubernetes.md#pgbouncer-caveats-read-before-enabling-the-pooler)).
- Size PgBouncer and `max_connections` for every consumer in [Connection sizing](#connection-sizing).
- Take base backups with WAL archiving for point-in-time recovery, and encrypted logical backups with `scripts/backup.py` ([backups](#backups)).
- Run the [restore drill](operations.md#restore-drill-quarterly-and-after-schema-changes) quarterly and after schema changes.

## Existing CNPG cluster ACL remediation (#315)

Older chart-created CNPG clusters inherited PUBLIC CONNECT on `postgres`/`template1`
and PUBLIC TEMPORARY on `gateway`. With every gateway stopped, the strict tagged
`open-model-gateway grants verify` can therefore fail with `maintenance database access`
(and application TEMP is independently forbidden). A chart upgrade does **not** rerun
[initdb ACL hooks](kubernetes.md#cnpg-bootstrap-database-acls-315). Neither `serve` nor the
migration Job repairs maintenance database ACLs; do not weaken the verifier.

This is an **explicit operator maintenance procedure**, following the production v0.8.0
repair on CNPG/PostgreSQL 17. Review the SQL, role inventory and operator version with
another operator before execution. It is not a general-purpose migration: the example
requires database `gateway`, owner `gateway_migrator`, runtime `gateway_runtime`, and the
three named CNPG roles. For different names or additional consumers, stop and prepare a
separately reviewed grant plan; do not skip failed preconditions or create missing roles
as part of this existing-cluster repair.

1. Select the exact namespace/Cluster/primary and database explicitly. Take a current
   backup and record database ACLs, ownership, role attributes/memberships and consumers
   (pooler, metrics, replicas, backups, other applications). Keep a superuser maintenance
   connection available. A shared cluster may have other roles depending on PUBLIC
   CONNECT: preserve their previously approved access explicitly or stop.
2. Drain and stop **every** gateway replica, suspend autoscaling/reconciliation that could
   restart it, and stop runtime jobs/CLI sessions. Revoking CONNECT does not disconnect
   existing sessions. Confirm no runtime sessions remain.
3. Connect directly to that primary's `postgres` database **as a PostgreSQL superuser**, not
   through PgBouncer or with migrator/runtime credentials. Run the following as one script
   with `psql -X -v ON_ERROR_STOP=1` using your approved connection method. Any failed
   assertion must abort/roll back; investigate rather than broadening privileges.

```sql
BEGIN;
DO $preconditions$
BEGIN
  IF current_database() <> 'postgres'
     OR NOT (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user) THEN
    RAISE EXCEPTION 'select the primary postgres database as a superuser';
  END IF;
  IF pg_catalog.pg_is_in_recovery() THEN
    RAISE EXCEPTION 'must run on the selected primary';
  END IF;
  IF NOT EXISTS (
    SELECT 1 FROM pg_catalog.pg_database d JOIN pg_catalog.pg_roles r ON r.oid = d.datdba
    WHERE d.datname = 'gateway' AND r.rolname = 'gateway_migrator'
  ) OR NOT EXISTS (
    SELECT 1 FROM pg_catalog.pg_roles
    WHERE rolname = 'gateway_runtime' AND NOT rolsuper AND NOT rolcreatedb
      AND NOT rolcreaterole AND NOT rolreplication AND NOT rolbypassrls
  ) THEN
    RAISE EXCEPTION 'unexpected application owner or runtime role; review inventory';
  END IF;
  IF EXISTS (SELECT 1 FROM pg_catalog.pg_stat_activity WHERE usename = 'gateway_runtime') THEN
    RAISE EXCEPTION 'stop all runtime sessions first';
  END IF;
  IF (SELECT count(*) FROM pg_catalog.pg_roles WHERE rolname IN
      ('cnpg_pooler_pgbouncer', 'cnpg_metrics_exporter', 'streaming_replica')) <> 3 THEN
    RAISE EXCEPTION 'missing CNPG operational role; do not create it in this repair';
  END IF;
  IF EXISTS (
    SELECT 1 FROM pg_catalog.pg_roles r
    CROSS JOIN (VALUES ('postgres'), ('template1')) AS d(name)
    WHERE r.rolname IN ('cnpg_pooler_pgbouncer', 'cnpg_metrics_exporter', 'streaming_replica')
      AND NOT pg_catalog.has_database_privilege(r.oid, d.name, 'CONNECT')
  ) THEN
    RAISE EXCEPTION 'CNPG roles must already have effective maintenance CONNECT';
  END IF;
  IF NOT pg_catalog.has_database_privilege('gateway_runtime', 'gateway', 'CONNECT') THEN
    RAISE EXCEPTION 'runtime must already have application CONNECT';
  END IF;
END;
$preconditions$;

-- Preserve existing effective access explicitly BEFORE removing PUBLIC access.
GRANT CONNECT ON DATABASE postgres, template1
  TO cnpg_pooler_pgbouncer, cnpg_metrics_exporter, streaming_replica;
REVOKE CONNECT ON DATABASE postgres, template1 FROM PUBLIC, gateway_runtime;
REVOKE TEMPORARY ON DATABASE gateway FROM PUBLIC, gateway_runtime;
-- Deliberately leave PUBLIC CONNECT on gateway unchanged.

DO $postconditions$
BEGIN
  IF pg_catalog.has_database_privilege('gateway_runtime', 'postgres', 'CONNECT')
     OR pg_catalog.has_database_privilege('gateway_runtime', 'template1', 'CONNECT')
     OR pg_catalog.has_database_privilege('gateway_runtime', 'gateway', 'TEMPORARY')
     OR NOT pg_catalog.has_database_privilege('gateway_runtime', 'gateway', 'CONNECT') THEN
    RAISE EXCEPTION 'runtime ACL boundary failed (check inherited memberships); roll back';
  END IF;
  IF EXISTS (
    SELECT 1 FROM pg_catalog.pg_roles r
    CROSS JOIN (VALUES ('postgres'), ('template1')) AS d(name)
    WHERE r.rolname IN ('cnpg_pooler_pgbouncer', 'cnpg_metrics_exporter', 'streaming_replica')
      AND NOT pg_catalog.has_database_privilege(r.oid, d.name, 'CONNECT')
  ) THEN
    RAISE EXCEPTION 'CNPG operational CONNECT lost; roll back';
  END IF;
END;
$postconditions$;
COMMIT;
```

4. While gateways remain stopped, run the **same tagged** `open-model-gateway grants verify`
   directly against `gateway`, with the approved verifier credential that can SET ROLE to
   runtime. It must pass without exceptions/whitelists. Also use actual runtime credentials:
   direct connections to `postgres` and `template1` must fail with permission denied;
   connection to `gateway` must succeed, and `CREATE TEMP TABLE acl_probe (id integer)`
   must fail. Run that TEMP probe in a disposable session (no application writes).
5. Check new pooled authentication/query connections (not merely already-open pooler
   sessions), metrics collection without database permission errors, and streaming/synchronous
   standby health. Restart gateways only after verification passes, restore the previous
   scaling configuration, then check every replica's readiness and LISTEN reconnection.
   Recheck operational health after reconciliation and a planned failover. If any check fails,
   keep gateways stopped and investigate; do not restore PUBLIC maintenance access as a
   workaround or reset/rebootstrap the database.

This procedure does not change application table/column grants or schema. Issue #295's
separate `grants apply` TEMP hardening overlaps the application TEMP revoke only; the CNPG
bootstrap and this explicit existing-cluster maintenance procedure also cover maintenance
CONNECT and operational-role access. Neither requires changing the privilege verifier.

## What happens during a primary failover

The gateway has no failover logic of its own. It reaches the primary through a host name (the CNPG `-rw` service, the Pooler service, or a managed writer endpoint) and relies on that name moving to the new primary. While no primary is reachable, it refuses new work instead of serving it unaccounted.

### Database connections

- The main pool (`GATEWAY_DATABASE_MAX_CONNECTIONS` per replica) waits at most 3 s for a connection, then the operation fails.
- `/health/ready` fails its `database` check when a pooled connection does not answer within 2 s, so replicas become unready while the primary is unreachable ([health endpoints](operations.md#health-endpoints)). `/health/live` does not touch the database, so Kubernetes does not restart gateway pods because the database is down.

### New requests

Admission must commit a durable reservation on the primary before anything is sent upstream. If no database connection is available within 3 s (the pool's acquire timeout), the request fails with 503 `accounting_unavailable`. If the primary accepts the connection but the statement or commit hangs (for example, waiting for a synchronous standby that is being promoted), admission has no statement timeout of its own: it waits until the request deadline and fails with 504 `timeout_error`. Either way, no upstream work starts. Time spent waiting for admission counts toward `GATEWAY_REQUEST_TIMEOUT_SECONDS`. The gateway does not retry a failed admission.

### In-flight requests and settlement

An attempt admitted before the failover keeps running upstream. When it ends, the gateway settles it with a 3 s timeout (`ExecutionGuard::finish` in `apps/gateway/src/inference/mod.rs`). If the write fails:

1. The client gets an error: a 503 `accounting_unavailable` for a complete response, or an error event instead of a normal end for a stream.
2. The execution stays `started` and its reservation `pending`, with the hold in place. `gateway_settlements_total{outcome="held"}` counts it, whether the write failed or timed out; the error log also records `inference finalization failed; reconciliation required`.
3. A reservation's lease ends `GATEWAY_REQUEST_TIMEOUT_SECONDS` + 5 s after admission. Every 5 s, each replica reconciles expired leases (`reconcile_expired` in `apps/gateway/src/governance.rs`, with `FOR UPDATE SKIP LOCKED` so replicas never collide): the reservation becomes `unknown`, the execution `cancelled` with `lease_expired`, and an `unknown` ledger row is appended.
4. The hold is kept. Unknown cost is never treated as zero and never refunded automatically ([cancellation and reconciliation](governance.md#cancellation-reconciliation-and-retention)).

A cancelled request (client disconnect) is finalized in the background with the same 3 s limit and follows the same path when that write fails.

### Configuration listener and caches

Each replica holds one LISTEN session (`GATEWAY_LISTEN_DATABASE_URL`) and also polls configuration versions through its pool every second ([caches and change notifications](governance.md#caches-and-change-notifications)).

- The LISTEN session stays open as long as the old primary keeps it. While it is open, `gateway_config_listener_up` stays 1 even though no notifications arrive from the new primary.
- When the session ends, the replica reconnects with backoff (250 ms, doubling up to 10 s) and flushes every cache on reconnect. `gateway_config_listener_up` is 0 until then.
- If no version poll has succeeded for 3 s, every cache is bypassed and reads go to the database live. Admission never uses the caches, so a stale cache cannot admit a revoked key.

### Background work leases

Singleton jobs run under fenced work leases (30 s term, renewed every 10 s), so a replica that lost contact cannot commit after its term ended; the leases are renewed or taken over once the database is back ([background work leases](governance.md#background-work-leases)). Reservation reconciliation is not leased: every replica runs it.

### Measured failover

- With CNPG's default `smartShutdownTimeout` of 180 s, deleting the primary under load kept writes down for about 3 minutes, because pooled and LISTEN sessions never disconnect on their own.
- The chart now sets 15 s (`cnpg.cluster.smartShutdownTimeout`). Under load (60 requests/s, 3 gateway replicas, 2 database instances) the standby was promoted **in 18 s**, with about 15 s of fast failures (`503 accounting_unavailable` and 500 `api_error` within milliseconds).
- About 22 s after recovering, writes stalled again: requests took about 30 s and most failed, for at least 19 s. The cause of that earlier stall was not established by #25.
- In the #25 runs, interrupted attempts with surviving reservations reconciled as `unknown` and the internal verifiers reported no findings. Those checks do not prove that all acknowledged records survived.
- A later #149 fault run on CNPG 1.30.1 with two instances and legacy min/max `1/1` lost **16 acknowledged requests' accounting rows**. Primary deletion caused the legacy synchronous requirement to drop to zero while the old primary was still committing. The [explicit required/quorum policy](kubernetes.md#production-durability-policy-391) addresses that downgrade; three legacy instances alone are not a durability guarantee. No new fault-run result is claimed here.

### Pooler restart and a dropped LISTEN connection

Measured on the same setup at 60 requests/s (#25):

- **Restarting the PgBouncer Pooler** (`kubectl rollout restart`) failed 2 of 5,400 requests with `503 accounting_unavailable`. Every other request succeeded, and the accounting checks were clean.
- **Terminating every replica's LISTEN session** (`pg_terminate_backend`) failed no requests. Each replica reconnected (`gateway_config_listener_up` back to 1 within 20 s), with the 1 s version poll covering the gap.

### After a failover

1. Confirm the new primary accepts writes and `/health/ready` returns 200 on every replica.
2. Check that `sum(gateway_config_listener_up)` equals the replica count.
3. Watch `gateway_reservations_held{state="pending"}` return to its usual level once the leases expire. Use `max` across replicas ([metrics](operations.md#metrics)).
4. Run `open-model-gateway budget verify` and `open-model-gateway history verify`. Both must report no mismatch or finding. Run full scans off-peak on large installations.
5. Review the new `unknown` reservations and resolve them with evidence. They keep their holds and can block budgeted admission (`unresolved_usage`) until resolved. Never delete reservations.
6. Confirm that a new standby is streaming and synchronous again (see [Monitoring](#monitoring)).

## Gateway replica restarts

Measured on bitop-talos (#25):

- **Graceful pod delete:** the replica finished its in-flight work before exiting (3,600 of 3,600 requests succeeded). The chart allows `gateway.terminationGracePeriodSeconds: 150`, which is longer than the default 120 s request timeout plus the 10 s `preStop` sleep.
- **Hard kill mid-stream:** 28 streams were cut. All 28 reservations were reconciled as `unknown` with their holds kept.

## Synchronous or asynchronous replication

For production on CNPG 1.30.1, explicitly select three instances with this chart values fragment
(merge it with your existing Secret/storage/backup settings):

```yaml
cnpg:
  cluster:
    instances: 3
    synchronous:
      method: any
      number: 1
      dataDurability: required
      failoverQuorum: true
    postgresql:
      parameters:
        synchronous_commit: "on"
```

See [production durability policy](kubernetes.md#production-durability-policy-391) for source
citations, upgrade instructions and rehearsal requirements. Chart defaults deliberately remain
two instances with legacy min/max `1/1` for compatibility; legacy CNPG may disable sync even
below the configured minimum, as observed in #149. Upgrades must explicitly remove the legacy
values and opt in to `cnpg.cluster.synchronous` (#391).

**Required synchronous replication.** With application sessions using `synchronous_commit=on`,
commits wait for the required standby WAL flush. `dataDurability: required` keeps that requirement
during outages: writes block rather than acknowledge without a copy. `failoverQuorum: true`
adds a safe-intersection check before automatic promotion; `required` alone does not ensure
the available promotion candidate contains every acknowledged commit. With three instances and
`number: 1`, the primary tolerates one standby outage; if the primary and a standby are lost,
quorum can refuse automatic failover. With **two instances and `required`, writes cannot resume
after promotion until a standby returns**. `preferred` can relax the requirement to preserve
availability; it is not the recommended accounting durability policy.

This is not protection against loss of all copies, unsafe manual promotion, or sessions overriding
synchronous commit. It adds commit latency and failover downtime. On bitop-talos, above about
150 req/s the limit was primary CPU plus WAL writes and synchronous-replication commit waits.
On slow-flush storage, the runbook suggests trying `commit_delay` (see
[group commit](operations.md#postgresql-settings-for-hot-rows-and-group-commit)).

**Asynchronous.** Commits do not wait for the standby, so a failover can lose the most recent committed transactions. For the gateway that means:

- A lost admission: the upstream attempt was dispatched, but its reservation does not exist on the new primary. Its cost is never held or recorded, and its settlement fails.
- A lost settlement: the reservation is `pending` again on the new primary and is reconciled as `unknown`, with its hold kept.
- A lost management change, such as a key revocation or a budget reduction, is undone.

What asynchronous replication saves in throughput or latency has not been measured. The runbook does recommend an asynchronous hot standby for the [reporting replica](operations.md#read-snapshots-and-the-reporting-replica), which never takes writes.

## Connection sizing

Every replica uses these connections:

| Consumer | Connections | Target |
|---|---|---|
| Main pool | up to `GATEWAY_DATABASE_MAX_CONNECTIONS` (default 10, chart 32, range 2–500) | PgBouncer or the primary |
| LISTEN session | 1, when `GATEWAY_CONFIG_CACHE=on` | the primary directly |
| Reporting pool (optional) | up to `GATEWAY_DATABASE_MAX_CONNECTIONS`, opened lazily | the standby set in `GATEWAY_REPORTING_DATABASE_URL` |

Inside the main pool, scoped admissions use at most half the connections and settlements three tenths. The rest is left for authentication and routing.

Outside the replicas:

- `migrate` and `grants apply` hold one session with a session-level advisory lock, so they must connect to the primary directly, never through PgBouncer.
- `scripts/backup.py backup` runs one `pg_dump` session.
- Every CLI command (`budget verify`, `history verify`, `partitions`, `archive`, `wait-for-database` and others) opens its own connections; budget at least one per running command.
- Leave room for your platform's own sessions (the Pooler's `auth_query`, monitoring, `superuser_reserved_connections`).

Worked example with the chart defaults (`replicaCount: 3`, pool 32, Pooler `instances: 2`, `max_client_conn: 200`, `default_pool_size: 25`):

- **Gateway to PgBouncer:** up to 3 × 32 = 96 client connections. Each PgBouncer instance enforces its own `max_client_conn`, and the Service may not spread connections evenly, so size every instance for the total plus headroom. At 6 replicas the total is 192, close to the default 200.
- **PgBouncer to PostgreSQL:** up to 2 × 25 = 50 server connections for the runtime role. Size `default_pool_size` for the connections that are actually busy, not the maximum ([PgBouncer](operations.md#pgbouncer)).
- **Direct to the primary:** 3 LISTEN sessions, plus the migrator, backups and tools while they run.
- **Standby:** 3 × 32 = 96 reporting connections if the reporting replica is enabled. PostgreSQL requires a hot standby's `max_connections` to be at least the primary's, so set both to the larger need.

The chart does not set `max_connections`; check it with `SHOW max_connections`. In CNPG mode the reporting URL targets the `-ro` service, which with two instances is the synchronous standby.

### Database sizing

Measured on bitop-talos (#25):

- The chart's default database (2 CPU and 2 GiB, `shared_buffers` 256 MB, Pooler limited to 500m CPU) collapsed at 50 req/s.
- 6 CPU and 8 GiB with `shared_buffers` 2 GB handled 100–150 req/s cleanly, with p50 latency of 70–90 ms.
- Above about 150 req/s, primary CPU plus WAL writes and synchronous-replication commit waits were the limit.
- Some runs stalled for 1–5 s on row-lock queues behind slow commits. #132 tracks this. A fix for the health-row amplifier is in PR #133, and these capacity numbers will be measured again after it.

Override `cnpg.cluster.resources` and `cnpg.cluster.postgresql.parameters` for production; the chart defaults are not a capacity claim.

## Backups

Use both kinds of backup.

| | CNPG base backups and WAL archive | `scripts/backup.py` |
|---|---|---|
| What it holds | A physical copy of the cluster plus continuous WAL | A logical `pg_dump` of the gateway database, age-encrypted, with a checksummed manifest and the migration lineage |
| Recovery point | Any time covered by a base backup and the WAL archived after it | The moment the dump was taken; the RPO is the backup interval |
| Restores into | A new cluster | A new, empty database, checked against the release that will serve it |
| Guide | [Point-in-time recovery](operations.md#point-in-time-recovery) | [Backups and restore](operations.md#backups-and-restore), [encrypted backups](operations.md#encrypted-backups) |

Point-in-time recovery needs both parts: a base backup alone restores only to the end of that backup, and archived WAL is useless without a base backup to replay it onto.

**CNPG.** `cnpg.cluster.backup.enabled=true` renders `backup.barmanObjectStore` on the `Cluster` from `cnpg.cluster.backup.barmanObjectStore`, plus a `ScheduledBackup` (`cnpg.cluster.backup.scheduledBackup.schedule`, default `"0 0 3 * * *"`, daily). Put the destination path and credentials inside `barmanObjectStore`. The template renders only that map, not the separate `cnpg.cluster.backup.destinationPath` value. CloudNativePG archives WAL through this setting, and its documentation marks the native `barmanObjectStore` interface as deprecated in favour of the Barman Cloud Plugin, which the chart does not configure. The chart has no recovery bootstrap: create the recovery cluster yourself, then run steps 3–5 of the [restore drill](operations.md#restore-drill-quarterly-and-after-schema-changes) against it before cutting over. See also [Kubernetes backups](kubernetes.md#backups).

**Logical.** The clean-host [restore drill](operations.md#restore-drill-quarterly-and-after-schema-changes) (#105) measured an RPO of 48 s (the time since the last backup) and an RTO of 92 s on the homelab, including creating the environment. A large installation restores more slowly; time your own.

**After any restore:**

- Reservations still `pending` at the recovery point are reconciled as `unknown` once their leases expire and a gateway runs.
- Requests served after the recovery point are missing from the ledger.
- Run `budget verify`, `history verify` and `partitions status`.

**Partition archival.** `archive partition` exports a closed month, then detaches its partitions into the `omg_archive` schema, or drops them with `--drop` ([archival](operations.md#history-partitions-rollups-and-archival-p6)). Detached partitions stay in both kinds of backup until dropped. After `--drop`, later backups no longer contain the month, and the export directory is the only copy. Keep exports for as long as you need the data, regardless of backup pruning (`backup.py prune --keep`). Restoring to a point before an archival brings the month back as live partitions.

## Monitoring

**Replication.** On the primary:

```sql
SELECT application_name, state, sync_state, write_lag, flush_lag, replay_lag
FROM pg_stat_replication;
```

Expect one row per standby with `state` `streaming`. The failover target should show `sync_state` `quorum` (with `ANY`) or `sync`. On a standby, `now() - pg_last_xact_replay_timestamp()` is the age of the last replayed commit. The gateway uses a similar check to send reads back to the primary when the reporting replica is more than `GATEWAY_REPORTING_MAX_LAG_SECONDS` behind. It logs a warning for each fallback and exports no metric for it.

**MultiXacts.** Alert on `mxid_age(datminmxid)` well before `autovacuum_multixact_freeze_max_age`, and watch `pg_stat_slru` for the `multixact_offset` and `multixact_member` caches. Queries and thresholds are in [MultiXacts](operations.md#multixacts-and-history-parent-checks).

**Autovacuum.** Watch `n_dead_tup` and `last_autovacuum` in `pg_stat_user_tables` for `budget_totals`, `rate_minute_counters`, `inflight_counters` and the current month's `governance_reservations_p*` and `inference_executions_p*` partitions. Keep long transactions off the primary. Recommended server settings are in [PostgreSQL settings](operations.md#postgresql-settings-for-hot-rows-and-group-commit).

**Partitions ahead.** `gateway_history_partition_months_ahead{table}` shows how many future months exist. Below 2, the built-in `partitions_missing` incident opens (critical below 1). `open-model-gateway partitions status` exits 1 in the same case.

**Gateway metrics that show a database problem** ([metrics](operations.md#metrics)):

- `gateway_config_listener_up`, `gateway_config_poll_failures_total`: LISTEN session and version poll.
- `gateway_settlements_total{outcome="held"}`: settlement writes that failed or timed out.
- `gateway_reservations_held` (`state` `pending` or `unknown`).
- `gateway_db_pool_connections` against `gateway_db_pool_max_connections`.
- `gateway_work_leases_held`: exactly one replica reports 1 per lease.
- `gateway_history_verify_findings`: alert on any finding.

The example rules in `deploy/monitoring/prometheus-rules.yml` include `GatewaySettlementHeld`, `GatewayUnknownReservationsGrowing`, `GatewayPendingReservationsHigh` and `GatewayDatabasePoolSaturated` ([alerting](operations.md#alerting-and-dashboards)). Also probe `/health/ready` from outside.

## Managed PostgreSQL

Amazon RDS, Google Cloud SQL and Azure Database for PostgreSQL have not been tested with the gateway. Check that your service meets these requirements:

- **PostgreSQL 17.** CI, staging and the chart's image use PostgreSQL 17.
- **No extensions.** None of the migrations in `apps/gateway/enterprise_migrations` creates an extension, and backups dump only `plpgsql`. If the service adds extensions to `template1`, create the gateway database from `template0`, as the restore drill does; readiness refuses unknown relations in the gateway database's `public` schema. `pg_stat_statements` is optional and belongs in the `postgres` database ([slow queries](operations.md#finding-slow-queries-and-lock-waits)).
- **Advisory locks.** Admission and management use transaction-scoped advisory locks, which work through transaction pooling. `migrate` and `grants apply` take a session-level advisory lock and must connect directly.
- **LISTEN/NOTIFY on a direct connection.** Point `GATEWAY_LISTEN_DATABASE_URL` at the writer endpoint, not at a transaction-pooling proxy. `NOTIFY` from triggers works through any pool. Without LISTEN, caches still follow changes through the 1 s poll.
- **Dedicated gateway database.** `grants apply` revokes TEMP from `PUBLIC` and `gateway_runtime` only on the connected database. Every role relying on that PUBLIC grant loses temporary-object creation there; the owner/migrator keeps its privileges and other databases are unchanged. Do not share the gateway database with unrelated applications.
- **Roles with explicit grants, no superuser.** The migrator owns the database, its `public` schema and migrated objects; table grants alone are insufficient to apply database-level revocations. The runtime role is named `gateway_runtime`, because `deploy/staging/runtime-grants.sql` grants to that name. Use [`deploy/staging/init-db.sql`](../deploy/staging/init-db.sql) as the template, and never run the gateway as the service's administrator user. After every `migrate`, run `open-model-gateway grants apply` as the migrator, then `grants verify` with a role that can `SET ROLE gateway_runtime` ([staging roles](staging.md#database-authority-and-releases)).
- **TLS.** Use `sslmode=verify-full` with the provider's CA.
- **Failover.** Use the service's writer endpoint for `DATABASE_URL` and `GATEWAY_LISTEN_DATABASE_URL`, and the service's PITR for point-in-time recovery. Keep encrypted logical backups as well. The failover timings in this guide are for CNPG only.

## Not yet measured

- A LISTEN connection that dies without closing (a network partition rather than a terminated backend).
- Failover with 3 database instances, and the cause of the second outage after a 2-instance failover (#149).
- What synchronous replication costs compared with asynchronous.
- Certified capacity: steady load still hits 17–40 s stalls in about half the runs, sometimes without recovery (#132). Clean runs held 100–150 requests/s with p99 under 650 ms.
