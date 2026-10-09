# Operations runbook

This covers health checks, metrics and alerting, backups and restore, secret rotation, upgrades, and the load-test baseline. It assumes the [staging](staging.md) role split: `gateway_bootstrap` for cluster administration, `gateway_migrator` for schema ownership, and `gateway_runtime` for the service. None of this approves a public production launch.

## Configuration reference

| Variable | Default | Purpose |
|---|---|---|
| `GATEWAY_METRICS_ADDR` | unset (disabled) | Separate Prometheus listener, for example `127.0.0.1:9464`. It must use a different port from `GATEWAY_LISTEN`. |
| `GATEWAY_DATABASE_MAX_CONNECTIONS` | `10` | Database pool size per replica (2 to 500). Size PostgreSQL `max_connections` for every replica plus migrator and backup sessions. |
| `GATEWAY_MAX_CONCURRENT_REQUESTS` | `128` (staging: 32) | Per-replica inference concurrency. Requests beyond it get 429 and are counted under `scope="gateway_capacity"`. |
| `GATEWAY_REQUEST_TIMEOUT_SECONDS` | `120` | Inference deadline. Time spent waiting for admission counts toward it. |

## Health endpoints

Both endpoints are on the public listener. They are not authenticated and never return error details.

- `GET /health/live` returns `200 {"status":"ok"}` without touching the database. Use it for liveness and restart decisions.
- `GET /health/ready` returns 200 only when every check passes, and 503 otherwise:

  ```json
  {"status":"ready","checks":{"database":"ok","schema":"ok","web":"ok"}}
  ```

  - `database`: a pooled connection answered within 2 seconds.
  - `schema`: the installation family and the full migration lineage (versions and checksums) match this binary exactly. It reports `unknown` when the database is unreachable.
  - `web`: `disabled` when `GATEWAY_WEB_DIR` is unset. Otherwise it checks that the loaded build's `index.html` still exists, which catches an unmounted volume.

  Use readiness for load-balancer membership and the container `HEALTHCHECK`. The service never migrates by itself, so a release whose schema does not match stays unready until you run `migrate`.

## Metrics

Metrics are served only when `GATEWAY_METRICS_ADDR` is set. They use a dedicated listener that answers `GET /metrics` with OpenMetrics text (`application/openmetrics-text; version=1.0.0`) and returns 404 for every other path. Metrics are never on the public port and never behind the SPA fallback. The endpoint has no authentication, so bind it to loopback or a private scrape network. In the staging Compose file the variable is passed through but empty by default, and Caddy never routes to it. Prometheus 2.x and 3.x negotiate OpenMetrics automatically. The exposition also parses as classic text: `promtool check metrics` reports only the expected HELP lint for OpenMetrics `_total` naming.

Labels are deliberately low-cardinality:

- Route templates, never raw paths. Unrouted and SPA requests share `route="unmatched"`.
- Methods come from a fixed set.
- Provider kinds and configured public model names are read back from the admitted execution row. Client-supplied model strings are never used as labels. These labels are bounded to 200 distinct values each; anything beyond that becomes `other`.
- Safe error codes and limit scopes.

There are no workspace, key, user, request IDs, or prompt and response data in any label.

| Metric | Type | Labels |
|---|---|---|
| `gateway_build_info` | gauge (1) | `version` |
| `gateway_http_requests_total` | counter | `method`, `route`, `status` |
| `gateway_http_request_duration_seconds` | histogram | `method`, `route` (time to response headers; for streams this is time to first byte) |
| `gateway_inference_attempts_total` | counter | `provider`, `model`, `outcome` (`succeeded`, `failed`, `cancelled`) |
| `gateway_inference_attempt_errors_total` | counter | `provider`, `code` |
| `gateway_upstream_duration_seconds` | histogram | `provider`, `model` (dispatch to end of upstream response) |
| `gateway_upstream_time_to_first_token_seconds` | histogram | `provider`, `model` (streams) |
| `gateway_inference_tokens_total` | counter | `provider`, `model`, `direction` (provider-reported only; unknown usage is not counted as zero) |
| `gateway_settlements_total` | counter | `outcome`: `settled` (exact cost), `unknown` (hold retained), `held` (finalization failed; the reservation stays pending until reconciliation) |
| `gateway_admission_denials_total` | counter | `code`, `scope` (`installation`, `workspace`, `api_key`, `policy` for rate/concurrency limits, `gateway_capacity`) |
| `gateway_reservations_held` | gauge | `state` (`pending`, `unknown`). Refreshed at scrape time, at most every 15 seconds, with a 2-second query timeout. |
| `gateway_alert_evaluations_total` | counter | `result` (`ok`, `skipped` when another replica holds the lock, `failed`) |
| `gateway_alert_rule_failures_total` | counter | none |
| `gateway_db_pool_connections` | gauge | `state` (`idle`, `in_use`) |
| `gateway_db_pool_max_connections` | gauge | none |
| `gateway_metrics_collection_errors_total` | counter | `collector` |

Gauges and counters are per replica. Aggregate them with `sum`. The reservation gauge counts the whole installation, so take `max` across replicas instead of summing it.

### Alerting and dashboards

Example monitoring files are in [`deploy/monitoring/`](../deploy/monitoring/):

- `prometheus.example.yml`: scrape configuration for `job="open-model-gateway"`.
- `prometheus-rules.yml`: example alerts, validated with `promtool check rules`. They cover metrics down, 5xx rate, inference p95 latency, held settlements, growing unknown reservations, a stuck pending floor, budget denials, capacity saturation, upstream failure rate, time to first token, pool saturation, alert-evaluator failures, and collector errors. The thresholds are starting points.
- `grafana-dashboard.json`: import it and select a Prometheus data source. It has `instance` and `provider` variables and overview, HTTP, inference, accounting, and internals rows.

Also probe `/health/ready` from outside, for example with blackbox-exporter, because metrics alone cannot tell you the public listener is reachable.

How to respond:

- **Held settlements:** these are database or finalization failures. Holds are retained and never refunded as zero. The maintenance loop moves expired leases to `unknown`. Fix database health first, then review unknown usage in the dashboard and resolve it deliberately.
- **Unknown reservations:** they keep budget holds and can block budgeted admission (`unresolved_usage`). Resolve them; never delete reservations.
- **Pool saturation or rising latency:** read the [load test](#load-test-baseline) section before raising pool sizes.

## Backups and restore

`scripts/backup.py` wraps `pg_dump` and `pg_restore` with checksums and lineage checks:

```sh
# Backup: custom format, without owner or ACLs. Writes <dump> and <dump>.manifest.json (both 0600).
python3 scripts/backup.py backup --url-file .local/staging/secrets/migrator_database_url --out-dir /secure/backups

# Verify the checksum, size and archive readability (pg_restore --list).
python3 scripts/backup.py verify /secure/backups/gateway-...dump.manifest.json

# Restore only into an explicitly named EMPTY database (or a new one with --create),
# checking the lineage against the binary of the release that will serve it.
python3 scripts/backup.py restore /secure/backups/gateway-...dump.manifest.json \
  --url-env MIGRATOR_ADMIN_URL --target-db gateway_restore_20261009 --create \
  --gateway-binary /path/to/open-model-gateway
```

What the script guarantees:

- **Connection handling:** URLs come from `--url-env` (default `DATABASE_URL`) or `--url-file`. They are passed through `PG*` environment variables and are never printed or placed on a command line.
- **Containers:** `--pg-container NAME` runs the PostgreSQL 17 client tools inside that container with `docker exec`. The URL is then resolved inside the container, for example `127.0.0.1:5432`.
- **Manifest contents:** the SHA-256 and size of the dump, plus `schema_family`, `latest_version` and the full migration list (version and checksum), the server version, and the source database name.
- **Backup refusals:** a backup refuses empty or dirty lineages, or a lineage that changed while the dump ran. Partial files are deleted.
- **Restore checks:** restore verifies the checksum first. It then requires the backup lineage to equal `open-model-gateway schema-version`, which prints the binary's embedded migrations as JSON without database access. The weaker `--expect-version N` is available when no binary is at hand.
- **Restore safety:** target names must be lowercase identifiers. System and demo databases are refused. The target must contain no relations, schemas, functions, types or extensions. Restore uses `pg_restore --single-transaction --exit-on-error`. If it fails, it drops only a database it created in that same run. It never cleans, drops or overwrites existing data. Afterwards it re-reads the restored lineage.
- **Not encrypted:** backups contain identities and accounting data. Encrypt them with your approved system before off-host storage, restrict access, and set retention. Keep secrets, configuration, and Caddy certificate volumes in a separate encrypted recovery plan; the dump does not contain them.

`python3 scripts/staging.py backup|restore-check` remains the staging-stack shortcut. Its restore-check always uses a newly named disposable database.

### Point-in-time recovery

A logical dump only gives you a recovery point at the time it was taken. For a smaller RPO, use your PostgreSQL platform's PITR: a managed service's PITR, or base backups plus continuous WAL archiving with pgBackRest, WAL-G or `archive_command`, retained for at least the RPO window and stored encrypted off host. To recover:

1. Restore the base backup to a **new** cluster.
2. Set `recovery_target_time` to just before the incident.
3. Promote the recovered cluster.
4. Run the restore drill checks below against it before cutting over.

Never recover over the live cluster. Never run down migrations. Never delete pending or unknown reservations to make the numbers reconcile.

### Restore drill (quarterly and after schema changes)

1. Take a fresh backup and run `verify`.
2. Restore with `--create` into `gateway_restore_<date>` on a non-production cluster, using `--gateway-binary` from the release you intend to run.
3. Apply `deploy/staging/runtime-grants.sql` as the migrator, then run `verify-privileges.sql`.
4. Start the matching release against the restored database. `/health/ready` must report `schema: ok`.
5. Compare the restored ledger and execution counts (printed by `restore`) with the source. Spot-check costs, and confirm that pending and unknown reservations survived. Run `open-model-gateway budget verify` against it.
6. Record the elapsed time (your measured RTO) and the backup age (RPO), then drop the drill database.

Local evidence (2026-10-09) on a freshly migrated v14 database on the disposable 54339 cluster, with tools in the container:

- Backup took 0.3 s (151 kB) and `verify` passed.
- `restore --create --gateway-binary` succeeded, and the restored copy passed the binary's enterprise preflight.
- A second restore into the now non-empty target was refused, as was `--expect-version 99`.
- All drill databases were dropped.

`tests/backup.test.py` covers the refusal paths with mocks. Its opt-in integration case runs in CI against the service database.

## Secret rotation

General rule: add the new credential, deploy, verify, then revoke the old one. Never paste secrets into tickets or logs.

- **Database runtime/migrator passwords:**
  1. As `gateway_bootstrap`, run `ALTER ROLE gateway_runtime PASSWORD '<new>'`.
  2. Update `secrets/runtime_password` and `runtime_database_url`.
  3. Restart the gateway. Existing pooled sessions keep working until they reconnect, so restart promptly.
  4. Rotate the migrator the same way, between releases.

  Do not rerun `staging.py init`.
- **Provider API keys:** keys are referenced as `env:NAME` and must be listed in `GATEWAY_SECRET_ENV_ALLOWLIST`. Write the new key to the secret file, then restart. To rotate with no gap, create the new key at the provider first and revoke the old one after `gateway_inference_attempt_errors_total{code="upstream_rejected"}` stays flat. Prefer workload identity for Bedrock.
- **OIDC client secret:** register the new secret at the IdP, replace `secrets/oidc_client_secret`, restart, test a sign-in, then revoke the old secret. See [identity](identity.md) for IdP signing-key handling.
- **SMTP password:** update the referenced secret, restart, then send a test from Settings.
- **Inference API keys** belong to users and workspaces. Rotate them through the dashboard; rotation keeps policy and spend lineage. Revoked keys never reactivate.

## Upgrade and migration procedure

1. Read the release notes for migrations and any grant changes. Build or pull the image by digest.
2. Take a backup and `verify` it. Run a restore drill for releases that include migrations.
3. For incompatible migrations, drain traffic: stop ingress, then the gateway. Mixed-schema replicas are not supported.
4. Run `open-model-gateway migrate` as the migrator, then reapply `runtime-grants.sql`. `staging.py migrate` does both.
5. Start the new release. Wait for `/health/ready` to report `schema: ok`, then check metrics, sign-in, and one bounded inference request.
6. To roll back, redeploy the previous image digest only if no migration ran. Otherwise restore the pre-upgrade backup into a new database and cut over. There are no down migrations.

## Load test baseline

`apps/gateway/tests/load_test.rs` is a reproducible, ignored harness with no paid calls. An in-process mock adapter replaces the upstream: about 20 ms per non-stream response, and four deltas for streams. Everything else runs for real: authentication, routing, durable admission (installation lock, rate and budget policies), settlement and the ledger, against PostgreSQL over a real TCP listener. The harness creates and drops its own `omg_load_<random>` database on a loopback server.

```sh
DATABASE_URL=postgres://gateway:gateway@127.0.0.1:54339/gateway \
  cargo test --release -p open-model-gateway --features integration-tests \
  --test load_test -- --ignored --nocapture
# LOAD_TEST_REQUESTS (1000), LOAD_TEST_CONCURRENCY (50,200), LOAD_TEST_UPSTREAM_MS (20),
# LOAD_TEST_POOL (10), LOAD_TEST_MAX_CONCURRENT (256), LOAD_TEST_PRESEED (0)
# Example used below: LOAD_TEST_PRESEED=200000 LOAD_TEST_REQUESTS=500 LOAD_TEST_CONCURRENCY=50
```

After each scenario the harness asserts these ledger invariants:

- No pending or unknown reservations remain.
- Every execution has exactly one reservation, one hold and one settlement.
- The hold sum equals 120 µUSD × successes, and the settled sum equals 11 µUSD × successes, exactly. The ledger matches the reservations.
- Denied requests never executed, and upstream calls equal admitted attempts.
- In the budget scenario, settled spend never exceeds the budget.
- `gateway_settlements_total{outcome="settled"}` matches the count, with no `held` settlements.
- `budget verify` reports every maintained budget-totals bucket equal to a full scan.

Results (2026-10-09; Apple Silicon laptop, Docker PostgreSQL 17 on 54339, release build, 10-connection pool; half of the requests stream). Latency is client-observed full response time, in milliseconds. "Before" is the previous release (budget windows scanned), and "after" uses maintained budget totals (`0015_budget_totals.sql`). Both were measured on the same machine in the same session:

| Scenario | Requests | Before req/s (p50 / p99) | After req/s (p50 / p99) | Errors |
|---|---|---|---|---|
| mixed, 50 concurrent | 1000 | 113 (434 / 576) | 125 (395 / 461) | 0 |
| budget contention, 50 concurrent | 1000 | 97 (483 / 702) | 123 (363 / 534) | 490 ok / 510 clean 429 `budget_exceeded`; 5390 of 5500 µUSD settled |
| mixed, 50 concurrent, 200k history rows this month | 500 | **19** (2574 / 2724) | **124** (392 / 526) | 0 |
| budget contention, 50 concurrent, 200k history rows | 500 | **20** (2436 / 2655) | **150** (280 / 467) | 240 ok / 260 clean 429 |
| mixed, 200 concurrent, 200k history rows | 1000 | n/a | 116 (1697 / 1802) | 0 |
| budget contention, 200 concurrent, 200k history rows | 1000 | n/a | 124 (1409 / 2041) | 490 ok / 510 clean 429 |

After each run, the harness also runs `budget verify`. It found 55 buckets consistent with the full scan, which took 1.04 s over 200k rows.

Findings:

1. **Fixed: pool starvation under concurrency.** Before the fix, at 200 concurrent requests on a 10-connection pool, transactions waiting for the installation lock held every pooled connection. Authentication timed out (105 × 503 out of 1000), and 40 streams could not be finalized (settlement `held`, stream ended with an error rather than `[DONE]`). Admission and settlement now queue in process (`governance::LockGates`). At most a quarter of the pool waits on the lock per queue, and settlements never queue behind admissions. Repeated runs now show zero errors. The pool size is configurable through `GATEWAY_DATABASE_MAX_CONNECTIONS`.
2. **Throughput ceiling: about 115 to 150 req/s per installation in this environment.** Admission and settlement serialize on the installation row by design, so this ceiling is shared by all replicas. Adding replicas or connections does not raise it. Latency at higher concurrency is queueing (Little's law), not failure.
3. **Fixed: budget checks no longer scale with history.** Before, each budget check aggregated every reservation in its window (plus the legacy execution anti-join) while holding the lock. With 200k reservations this month the installation check took about 59 ms, and throughput fell to 19 req/s. Admission now reads `budget_totals`: one indexed lookup for all budget layers, O(layers). Statement-level triggers maintain the table in the same transaction as every reservation and execution write. Rate and concurrency limits read only the current minute and live leases. With 200k history rows, throughput matches the no-history case. Monitoring, policy and alert *reports* outside admission (`alerts`, usage pages) still aggregate their own windows and are not on the admission path.

## Budget totals verification

`budget_totals` (migration 0015) holds settled spend, active holds and unresolved counters per scope (installation, workspace, key lineage), period (day, ISO week, month, lifetime) and UTC period start, in exact integer micro-USD. Admission reads it instead of scanning history. Check it against a full scan of reservations and executions:

```sh
open-model-gateway budget verify
```

- **What it does:** it runs read-only in one `REPEATABLE READ` snapshot and takes no installation lock. Traffic can continue, but the scan reads every reservation, so run it off-peak on large installations. It prints JSON (`buckets`, `mismatch_count`, up to 20 example `mismatches`) and exits nonzero on any difference. It works with the runtime role's grants.
- **When to run it:** after `migrate` (the migration backfills from existing history), after restores (restore drill step 5), and periodically.
- **If it reports a mismatch:** do not edit, delete or "fix" reservations or the ledger. Drift is only possible through manual owner-level edits, such as deleting history, re-keying lineages, or disabling triggers. The runtime role cannot update keys, delete rows or truncate the table. Preserve the report and escalate. The table can be rebuilt by the migrator from the same scan the migration uses.
