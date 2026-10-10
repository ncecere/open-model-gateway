# Operations runbook

This covers health checks, metrics and alerting, backups and restore, secret rotation, Platform Admin lockout protection, upgrades, and the load-test baseline. It assumes the [staging](staging.md) role split: `gateway_bootstrap` for cluster administration, `gateway_migrator` for schema ownership, and `gateway_runtime` for the service. None of this approves a public production launch.

## Configuration reference

| Variable | Default | Purpose |
|---|---|---|
| `GATEWAY_METRICS_ADDR` | unset (disabled) | Separate Prometheus listener, for example `127.0.0.1:9464`. It must use a different port from `GATEWAY_LISTEN`. |
| `GATEWAY_DATABASE_MAX_CONNECTIONS` | `10` | Database pool size per replica (2 to 500). Size PostgreSQL `max_connections` for every replica plus migrator and backup sessions. |
| `GATEWAY_MAX_CONCURRENT_REQUESTS` | `128` (staging: 32) | Per-replica inference concurrency. Requests beyond it get 429 and are counted under `scope="gateway_capacity"`. |
| `GATEWAY_REQUEST_TIMEOUT_SECONDS` | `120` | Inference deadline. Time spent waiting for admission counts toward it. |
| `GATEWAY_FILE_STORE` | `off` | Encrypted file store: `off`, `local` or `s3`. See [file storage](file-storage.md) for the backend and encryption-key variables. |

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

### `open-model-gateway healthcheck`

The runtime image has no shell or curl, so the binary probes itself:

```sh
open-model-gateway healthcheck [--url http://127.0.0.1:8080/health/ready] [--timeout 3s] [--allow-non-loopback]
```

It exits 0 for a `2xx` response and 1 otherwise (non-2xx, redirect, refused connection, timeout). Without `--url` it probes `/health/ready` on the port of `GATEWAY_LISTEN` over loopback (`127.0.0.1`, or `::1` for `[::]`). It accepts plain `http://` only, never reads proxy variables, never follows redirects or retries, refuses non-loopback destinations unless `--allow-non-loopback` is given, and reads no configuration, `.env` file or secret. The image's `HEALTHCHECK` is `["/usr/local/bin/open-model-gateway", "healthcheck", "--timeout", "3s"]`. Compose can override it in exec form:

```yaml
healthcheck:
  test: ["CMD", "/usr/local/bin/open-model-gateway", "healthcheck"]
```

In Kubernetes, prefer native HTTP probes (`httpGet` on `/health/live` for liveness and `/health/ready` for readiness, port 8080). If a probe must be an exec probe, use the same command: `command: ["/usr/local/bin/open-model-gateway", "healthcheck"]`. Never use `sh -c` or `curl`; neither exists in the image.

## Container image

The runtime stage is `gcr.io/distroless/cc-debian12` pinned by digest: glibc, libgcc_s, CA certificates and the gateway binary, with the built SPA in `/app/web`. There is no shell, package manager, curl, perl, Node or Cargo. It runs as UID/GID `10001:10001` with `ENTRYPOINT ["/usr/local/bin/open-model-gateway"]` and `CMD ["serve"]`, works with a read-only root filesystem and needs no writable path (staging still mounts a small `/tmp` tmpfs). The binary is PID 1 and handles `SIGTERM` itself; Compose's `init: true` is optional.

What the former `container-entrypoint.sh` did is now done by the binary at startup, for every subcommand except `healthcheck`, before it reads `.env` files or configuration:

- Sets the process umask to `077`.
- Imports `DATABASE_URL`, `GATEWAY_OIDC_CLIENT_SECRET`, `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` from a file named by `<NAME>_FILE` (Compose or Kubernetes secret mounts; symlinks are followed). Only these four names; other `*_FILE` variables and custom provider references (`GATEWAY_SECRET_ENV_ALLOWLIST`) are never interpreted, so inject those directly. The file must be a readable regular file; one terminal LF is removed. The `_FILE` variable is removed after import.
- Refuses to start (exit 1) if both a variable and its `_FILE` companion are set, even to an empty value, or if any of the four values is empty, longer than 4096 bytes, or contains CR, LF or NUL. Messages name the variable and the reason, never the value or file path.
- Runs exactly the given subcommand: `serve` never migrates or bootstraps. Run one-off commands with the same image, for example `docker run --rm … IMAGE migrate`.

To debug a running container without a shell, use `docker exec CONTAINER /usr/local/bin/open-model-gateway healthcheck`, `docker logs`, `docker inspect` (user, health log), or attach a debug container that shares its namespaces (`docker debug`, or `kubectl debug --target`).

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
| `gateway_admission_seconds` | histogram | `phase`, `outcome`. Wall-clock time of each phase of a durable interactive admission transaction: `queue` (in-process wait for an installation-lock slot), `connect` (pool acquire and `BEGIN`), `locks` (catalog advisory lock plus the installation row lock: the lock wait), `read` (live authorization, deployment, clock, price), `limits` (policies, per-minute rate accounting, budget totals), `write` (execution, reservation and hold inserts with trigger fan-out), `commit`, and `total`. `outcome` is `admitted`, `denied` (a limit or budget denial), `rejected` (other refusals such as an unavailable model) or `error` (database failure). Phases after an early return are not observed. Buckets run from 100 µs to about 52 s. Batch-line and realtime-window admissions are not included. |
| `gateway_settlement_seconds` | histogram | `phase` (`queue`, `connect`, `locks`, `read`, `write`, `commit`, `total`), `outcome` (`settled`, `unknown`, `replay`, `conflict`, `error`). Terminal settlement of an interactive attempt (`finish`); a replay stops after `locks`. |
| `gateway_admission_denials_total` | counter | `code`, `scope` (`installation`, `workspace`, `api_key`, `policy` for rate/concurrency limits, `gateway_capacity`). `code="job_limit_exceeded"` counts video/batch jobs refused by a "Jobs at once" limit, with the scope that refused them. |
| `gateway_reservations_held` | gauge | `state` (`pending`, `unknown`). Refreshed at scrape time, at most every 15 seconds, with a 2-second query timeout. |
| `gateway_alert_evaluations_total` | counter | `result` (`ok`, `skipped` when another replica holds the lock, `failed`) |
| `gateway_alert_rule_failures_total` | counter | none |
| `gateway_db_pool_connections` | gauge | `state` (`idle`, `in_use`) |
| `gateway_db_pool_max_connections` | gauge | none |
| `gateway_metrics_collection_errors_total` | counter | `collector` |
| `gateway_file_store_operations_total` | counter | `backend` (`local`, `s3`), `op` (`put`, `get`, `read`, `head`, `delete`, `health`), `outcome` (`ok` or a safe error code such as `integrity`, `denied`, `unavailable`). `op="read"` counts failures while streaming an object. |
| `gateway_file_store_bytes_total` | counter | `backend`, `op` (`put`; `get` counts objects read to the end). Plaintext bytes. |

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
- **Pool saturation or rising latency:** read the [load test](#load-test-baseline) section before raising pool sizes. Compare `histogram_quantile(0.99, sum by (le, phase) (rate(gateway_admission_seconds_bucket{outcome="admitted"}[5m])))` across phases: a large `queue` or `locks` share means requests wait for the installation lock (more replicas or connections will not help); a large `limits` share means per-minute rate accounting or budget reads are slow (see [pg_stat_statements](#finding-slow-queries-and-lock-waits)).

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
- **S3 static keys for the file store:** create the new key pair, update the variables named by `GATEWAY_S3_ACCESS_KEY_ID_ENV`/`GATEWAY_S3_SECRET_ACCESS_KEY_ENV`, restart, use **Test storage**, then revoke the old pair. Prefer `aws:role:` or workload identity on AWS.
- **File store encryption keys:** see [below](#file-store-encryption-keys).
- **Inference API keys** belong to users and workspaces. Rotate them through the dashboard; rotation keeps policy and spend lineage. Revoked keys never reactivate.

## File store encryption keys

The [file store](file-storage.md) encrypts every object under the first key in the variable named by `GATEWAY_FILE_ENCRYPTION_KEYS_ENV` (`kid:base64key[,kid:base64key…]`).

**Backup.** Keep the key list in your secret manager and in the same encrypted recovery plan as the database secrets, **separately from object and database backups**. A database backup holds `stored_files` metadata (owners, sizes, hashes, key IDs) but no contents. An object backup (bucket replication or versioning, or a copy of `GATEWAY_FILE_STORE_DIR`) holds ciphertext only. Without the keys, objects are unrecoverable. After a restore, run `open-model-gateway files verify`. It reports missing objects, size mismatches and key IDs that are no longer configured.

**Rotation.**

1. Generate a key (`openssl rand -base64 32`) with a new ID and put it **first**: `k2027a:<new>,k2026a:<old>`. Restart every replica. New objects use `k2027a`, and old ones still decrypt.
2. Watch `files verify`: its `by_key_id` counts show how many live files still use `k2026a`. Files with a retention window drain as they expire (at most 365 days; exports after 1 day, batch files after 7 days by default).
3. Remove the old key only when its count reaches 0. Removing it earlier makes those files fail with `key_unavailable` (and `verify` lists the ID under `unknown_key_ids`).

Branding objects never expire. Re-encrypting them (a header-only re-wrap is possible because the format keeps the key ID and wrapped data key out of the chunk authentication) is planned but not implemented; until then keep their key as decrypt-only.

**Compromise.** If a master key leaks, add a new active key at once. Then treat the files under the old key as exposed only to someone who also has the ciphertext, and expire or delete them (shorten the group's retention and run `files sweep --once`) before removing the key.

## Platform Admin lockout protection

Neither SCIM nor the management API can remove platform access from the last active Platform Admin (a live Admin grant of any provenance on an account that is not suspended or cleaned up).

- **SCIM:** deactivation, `DELETE`, and Group changes that would drop the last effective Admin grant are refused whole with `409` (`scimType: "mutability"`). See [SCIM](scim.md#the-last-platform-admin-is-protected).
- **Manual:** Admin › Users refuses to revoke the last Admin grant or suspend the last Admin, and group mappings that hold it cannot be changed or deleted (`409 Cannot remove the last platform administrator`).
- Both checks run under the installation lock, so concurrent changes cannot both pass.

When SCIM is refused, the gateway writes the audit event `scim.last_admin_protected` and opens the built-in installation alert "SCIM tried to remove the last Platform Admin" (Notifications, Admin › Settings › Alerts › History, and email to Platform Admins when a relay is set up). It means the identity provider wants to remove the only Admin. Grant Admin to a second person (a manual grant, or a mapped Admin group), then let the provider retry or repeat the change. The alert clears at the next evaluation once a second active Platform Admin exists. Keeping two Admins avoids the situation entirely. A sign-in claim that drops the last group-provenance Admin grant is not covered by this check.

## Upgrade and migration procedure

1. Read the release notes for migrations and any grant changes. Build or pull the image by digest.
2. Take a backup and `verify` it. Run a restore drill for releases that include migrations.
3. For incompatible migrations, drain traffic: stop ingress, then the gateway. Mixed-schema replicas are not supported.
4. Run `open-model-gateway migrate` as the migrator, then reapply `runtime-grants.sql`. `staging.py migrate` does both.
5. Start the new release. Wait for `/health/ready` to report `schema: ok`, then check metrics, sign-in, and one bounded inference request.
6. To roll back, redeploy the previous image digest only if no migration ran. Otherwise restore the pre-upgrade backup into a new database and cut over. There are no down migrations.

`0018_job_limits.sql` adds the "Jobs at once" limit to every policy layer and sets the workspace-type defaults to 2 active video/batch jobs per workspace. After upgrading, review Admin › Settings › Defaults & limits if workspaces routinely run more jobs at once. It also adds the SCIM last-admin alert kind. Reapply `runtime-grants.sql` (step 4).

## Load test baseline

`apps/gateway/tests/load_test.rs` is a reproducible, ignored harness with no paid calls. For real gateway processes, several replicas and PgBouncer, see the [multi-replica harness](#multi-replica-load-test-harness) and the [capacity baseline](#capacity-baseline). An in-process mock adapter replaces the upstream: about 20 ms per non-stream response, and four deltas for streams. Everything else runs for real: authentication, routing, durable admission (installation lock, rate and budget policies), settlement and the ledger, against PostgreSQL over a real TCP listener. The harness creates and drops its own `omg_load_<random>` database on a loopback server.

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

## Multi-replica load-test harness

The in-process harness above runs one gateway inside the test binary. The multi-replica harness (scale plan phase P0) runs real gateway processes against PostgreSQL and PgBouncer, so it measures what an operator would deploy. It is test-only: no provider is ever called, and it only creates throwaway databases named `omg_loadtest*`.

| Part | What it does |
|---|---|
| `tools/mock-upstream` | Deterministic OpenAI-compatible upstream (`POST /v1/chat/completions`, complete and streamed). Configurable latency, time to first token, completion tokens, inter-token delay and a deterministic error rate. Usage is exact (12 prompt tokens, N completion tokens, a complete cache split), so every attempt settles at a known cost. It stores no prompt text, only a `nonce:<id>` marker, and lists its calls at `GET /__mock/calls`. |
| `tools/loadgen run` | Open-loop generator: request *i* is sent at `start + i/rate` whatever earlier requests are doing, so overload shows up as latency and errors instead of a silently lower rate. It round-robins over several gateway URLs, spreads requests over many seeded keys (and therefore workspaces), mixes streamed and complete requests, and measures latency from the scheduled time. Each prompt carries a nonce; the gateway's `x-request-id` (its `root_request_id`) is recorded per request. |
| `tools/loadgen seed` | Generates users, personal and shared workspaces, service accounts, keys, the mock catalog, policies and optional settled history server-side with set-based SQL (`tools/loadgen/seed.sql`). Keys are derived from a seed string by both the seeder and the generator, so no token is printed or stored. It refuses any database not named `omg_loadtest*` (in the client and in SQL), and must connect as the schema owner. |
| `deploy/loadtest/compose.yaml` | Compose project `omg-loadtest`: PostgreSQL 17 (tuned, `pg_stat_statements`), PgBouncer 1.25 in transaction mode, three gateway replicas with metrics listeners, the mock upstream at a pinned private address (`GATEWAY_LOCAL_UPSTREAMS`), and the generator. Images come from `mirror.gcr.io`, `ghcr.io` and the local `deploy/loadtest/Dockerfile` build; nothing from Docker Hub. The gateways use the restricted runtime role and `deploy/staging/runtime-grants.sql`. |
| `scripts/loadtest.py` | Runner: `up`, `reset-db`, `seed`, `gateways --replicas N --via pgbouncer\|direct`, `run`, `report`, `baseline`, `down`. Passwords are generated into `.local/loadtest/stack.env`; results are JSON files in `.local/loadtest/results/`. |

After every run, `loadgen` checks, per request id:

- every successful request reached the upstream exactly once (no implicit retries); denied requests never did; and no upstream call lacks a client request;
- every successful request has exactly one execution, one settled reservation, one hold and one settlement ledger entry, with the mock's exact usage, the exact cost (20 µUSD each at the seeded price) and ledger sums equal to the reservation sums;
- denied requests have no execution, nothing admitted during the run is still pending, and no execution lacks a reservation.

The runner then runs `open-model-gateway budget verify`, saves the top statements from `pg_stat_statements` and PgBouncer's `SHOW STATS`, and fails the run on any violation. The JSON report also contains client latency (all, streamed, complete), time to first byte, gateway overhead (client service time minus the mock's own service time for the same nonce), per-replica counts, and the `gateway_admission_seconds` / `gateway_settlement_seconds` deltas summed over all replicas.

```sh
python3 scripts/loadtest.py up
python3 scripts/loadtest.py reset-db
python3 scripts/loadtest.py seed --users 5000 --shared 2000 --keys 20000 --history 0
python3 scripts/loadtest.py gateways --replicas 3 --via pgbouncer
python3 scripts/loadtest.py run --label 3r-pgb-60 --rate 60 --duration 45 --warmup 5
python3 scripts/loadtest.py report
python3 scripts/loadtest.py down          # removes containers, network, data volume and the image
```

`tools/loadgen/tests/end_to_end.rs` runs the same seed, real gateway (with the real `openai_compatible` adapter), mock, generator and verification in one process against a throwaway database on `DATABASE_URL`, as part of `cargo test --workspace --all-features`.

The generator, mock, gateways and PostgreSQL share one machine in the laptop stack, so they compete for CPU. For certification runs (scale plan P8) put the generator and mock on separate hosts.

## Capacity baseline

Measured 2026-10-09/10 at commit `e05035f` plus the P0 changes (no admission changes), with `scripts/loadtest.py baseline`.

- **Hardware:** Apple M4 Pro (12 cores, 48 GB), macOS 26.6. Docker Desktop 29.8.1 VM with 12 vCPUs and 7.7 GiB. Every container, including the generator and mock, ran in that VM.
- **PostgreSQL 17 (alpine):** `shared_buffers=1GB`, `effective_cache_size=3GB`, `work_mem=16MB`, `max_wal_size=8GB`, `wal_compression=lz4`, `random_page_cost=1.1`, `jit=off`, `synchronous_commit=on` (default), `pg_stat_statements` and `track_io_timing` on.
- **Gateways:** release build, `GATEWAY_DATABASE_MAX_CONNECTIONS=10` and `GATEWAY_MAX_CONCURRENT_REQUESTS=128` per replica, `info` logging.
- **PgBouncer 1.25.0:** transaction mode, `default_pool_size=40`, `max_prepared_statements=200`.
- **Data:** 5,000 users, 7,000 workspaces (2,000 shared), 20,000 keys; an installation rate policy, an installation monthly budget and workspace-type monthly budgets, so every admission evaluates the rate layer and budget totals. "Seeded" adds 2,000,000 settled historical attempts over 120 days (plus 1,000 unknown last month): 2 M executions, 2 M reservations, 4 M ledger entries, 2.7 M budget-total buckets, a 3.8 GB database. Seeding took 391 s.
- **Traffic:** 45 s per run (the first 5 s excluded), 2,000 hot keys, half streamed, `max_completion_tokens` 16. The mock answers complete requests after 50 ms and streams 4 tokens (30 ms to the first, then 5 ms apart).

Overhead is client time minus mock time. Admission, lock wait (`phase="locks"`) and settlement come from the gateway histograms (bucket-interpolated). 429s are `rate_limit_error` from the per-replica capacity limit.

| History | Replicas | DB path | Offered/s | OK/s | Client p50 / p95 / p99 ms | Overhead p50 / p99 ms | Admission p50 / p99 ms | Lock wait p50 / p99 ms | Settlement p50 / p99 ms | 429s |
|---|---|---|---|---|---|---|---|---|---|---|
| empty | 1 | PgBouncer | 60 | 59.9 | 64 / 68 / 70 | 11 / 15 | 6.3 / 12.7 | 0.2 / 0.4 | 2.0 / 5.9 | 0 |
| empty | 1 | direct | 60 | 59.9 | 63 / 67 / 69 | 10 / 14 | 6.9 / 12.7 | 0.1 / 0.2 | 1.3 / 5.3 | 0 |
| empty | 3 | PgBouncer | 60 | 59.9 | 65 / 374 / 481 | 11 / 426 | 7.5 / 374 | 0.2 / 98 | 2.8 / 186 | 0 |
| empty | 3 | direct | 60 | 59.9 | 67 / 72 / 85 | 14 / 31 | 9.5 / 25 | 0.1 / 1.6 | 1.4 / 19 | 0 |
| empty | 1 | PgBouncer | 250 | 79.6 | 1,591 / 2,038 / 2,070 | 1,539 / 2,017 | 1,372 / 3,213 | 14.5 / 25 | 85 / 202 | 7,059 |
| empty | 1 | direct | 250 | 99.1 | 1,284 / 1,743 / 1,783 | 1,232 / 1,731 | 1,113 / 2,582 | 11.2 / 25 | 115 / 381 | 6,589 |
| empty | 3 | PgBouncer | 250 | 83.0 | 4,450 / 5,947 / 6,098 | 4,401 / 6,047 | 4,139 / 6,505 | 57 / 102 | 99 / 203 | 6,775 |
| empty | 3 | direct | 250 | 69.7 | 4,950 / 6,610 / 6,705 | 4,898 / 6,653 | 4,593 / 6,514 | 68 / 102 | 143 / 394 | 6,965 |
| 2 M | 1 | PgBouncer | 60 | 59.9 | 67 / 85 / 163 | 14 / 108 | 9.5 / 59 | 0.2 / 18 | 2.4 / 32 | 0 |
| 2 M | 1 | direct | 60 | 59.9 | 65 / 72 / 86 | 12 / 32 | 8.3 / 25 | 0.1 / 1.6 | 2.1 / 17 | 0 |
| 2 M | 3 | PgBouncer | 60 | 59.9 | 66 / 453 / 568 | 13 / 516 | 9.9 / 441 | 0.2 / 101 | 2.8 / 197 | 0 |
| 2 M | 3 | direct | 60 | 59.9 | 66 / 85 / 121 | 13 / 58 | 9.8 / 26 | 0.1 / 20 | 1.6 / 25 | 0 |
| 2 M | 1 | PgBouncer | 250 | 63.5 | 1,887 / 2,875 / 2,886 | 1,835 / 2,834 | 1,708 / 3,245 | 17.5 / 40 | 135 / 392 | 7,722 |
| 2 M | 1 | direct | 250 | 86.1 | 1,423 / 2,214 / 2,433 | 1,371 / 2,380 | 1,273 / 3,198 | 12.1 / 26 | 73 / 202 | 6,957 |
| 2 M | 3 | PgBouncer | 250 | 67.9 | 4,745 / 7,174 / 7,364 | 4,694 / 7,311 | 4,759 / ≥6,554 | 67 / 158 | 140 / 401 | 7,322 |
| 2 M | 3 | direct | 250 | 72.4 | 4,928 / 7,145 / 7,356 | 4,877 / 7,301 | 4,575 / ≥6,554 | 63 / 109 | 153 / 402 | 7,023 |

Every run passed every invariant: each successful request matched exactly one upstream call and one settled reservation with exact sums, no denied request executed, nothing stayed pending, and `budget verify` was consistent (0.3 s on the empty database, 21 s over 2.7 M buckets on the seeded one). "≥6,554" means the top bucket of the histogram build used for the baseline (later builds extend the buckets to 52 s).

Findings:

1. **The installation-wide ceiling on this laptop is about 65–100 successful requests/s, and replicas do not raise it.** Three replicas never beat one at overload; they only queue more work (3 × 128 permits), so p50 latency goes from 1.3–1.9 s to 4.4–4.9 s. Under overload the in-process `queue` phase is almost all of the admission time. A shorter 25 s probe reached 120/s; throughput falls as each UTC minute fills up (next finding).
2. **The critical section is about 10 ms per admission at overload, and most of it is per-minute rate accounting.** At 250/s offered (1 replica, empty) the serialized phases were `locks` 14.5 ms p50 (waiting for the previous holder), `limits` 7.9 ms, `write` 1.2 ms, `read` 1.2 ms and `commit` 0.25 ms. The `RATE_ACCOUNTING` statement averaged 6.2 ms and the installation row lock statement accumulated 137 s of waiting in 45 s. This matches the scale design's §2.1/§2.3 analysis: removing the lock alone is not enough; the per-minute scan must become a maintained counter (P2).
3. **History still costs admission time.** With 2 M historical attempts, rate accounting averaged 4.8 ms instead of 3.0 ms at 60/s, the `limits` phase 6.5 ms instead of 3.9 ms, and overload throughput through PgBouncer fell from 80 to 64/s.
4. **Below saturation, admission meets the design target and settlement is cheap:** admission p50 6–10 ms, p99 13–26 ms; settlement p50 1.3–2.8 ms; gateway overhead p50 10–14 ms (two HTTP hops, authentication, routing, admission and settlement).
5. **Three replicas through PgBouncer showed p95 spikes (374–453 ms) at 60/s** that the direct path did not, with lock-wait p99 around 100 ms. A 30 s repeat on the seeded database had p95 70 ms and p99 182 ms, and PgBouncer reported an average client wait of 0 µs and few server-side re-parses, so pooling is not the cause. The likely cause is periodic installation-lock work multiplied by replicas (lifecycle cleanup, reconciliation and the alert evaluator on every replica, design §2.6). The P0 histograms do not cover those jobs; this is not yet proven.
6. **With `max_prepared_statements=0`, PgBouncer breaks the gateway (decision gate D4):** 98 of 100 requests failed (`503 accounting_unavailable`/`api_error` and some 401s from failed authentication lookups), with no ledger damage. With 200 it worked in every run.

## PgBouncer

Use transaction pooling with PgBouncer **1.21 or later and `max_prepared_statements` > 0** (200 in the load-test stack). sqlx caches *named* prepared statements per connection; without protocol-level prepared-statement support, statements land on server connections that never prepared them, and the gateway fails closed (see finding 6 above). With support enabled, PgBouncer re-prepares transparently; the baseline runs saw a few hundred server-side parses for tens of thousands of transactions.

- Put the server's SCRAM secrets in `auth_file` (copy `rolpassword` from `pg_authid`), or use `auth_query`. With plaintext passwords in the userlist, PgBouncer authenticated clients against a secret with its own salt and then intermittently failed the server login ("password authentication failed"). `scripts/loadtest.py up` writes the userlist from the server's secrets.
- Add `ignore_startup_parameters = extra_float_digits`; sqlx sends it at connect time.
- Run `migrate` directly against PostgreSQL, never through PgBouncer. It takes a session-level advisory lock.
- Everything the gateway does inside transactions works in transaction mode: transaction-scoped advisory locks, `SET LOCAL`, and `SET TRANSACTION ISOLATION LEVEL`.
- Size `default_pool_size` for the sum of replica pools that are actually busy, not their maximum. Today the installation lock keeps only a few transactions active at a time.

## Finding slow queries and lock waits

Enable `pg_stat_statements` (and I/O timing) on the server. This needs a restart:

```
shared_preload_libraries = 'pg_stat_statements'
pg_stat_statements.track = all     # include statements inside the budget-totals triggers
track_io_timing = on
```

Create the extension in the `postgres` maintenance database, **not** in the gateway database. Readiness refuses unknown relations in the gateway database's `public` schema, and the extension's view is one. The statistics cover every database anyway. Read them as a role with `pg_read_all_stats`:

```sql
-- In the postgres database: CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
-- Reset before a measurement window:
SELECT pg_stat_statements_reset();
-- Top statements by total time for the gateway database:
SELECT left(regexp_replace(query, '\s+', ' ', 'g'), 120) AS query, calls,
       round(total_exec_time::numeric, 1) AS total_ms, round(mean_exec_time::numeric, 3) AS mean_ms,
       rows, toplevel, shared_blks_read, round(shared_blk_read_time::numeric, 1) AS read_ms
FROM pg_stat_statements
WHERE dbid = (SELECT oid FROM pg_database WHERE datname = 'gateway')
ORDER BY total_exec_time DESC LIMIT 20;
```

What to look for in the gateway's statements:

- `SELECT id FROM installation WHERE singleton FOR NO KEY UPDATE`: its total time is time spent **waiting** for the installation lock, not work. When it dominates, the database is idle behind one serializer.
- `WITH accounting AS (…)`: per-minute rate accounting. Its mean grows with traffic in the current UTC minute.
- `INSERT INTO budget_totals …` (non-top-level): trigger fan-out, about 3 calls per reservation or execution write.
- A `budget verify` full scan, if one ran in the window.

Lock waits, live, in the gateway database (any role that can see `pg_stat_activity`, such as `pg_monitor`):

```sql
-- Who waits for whom right now:
SELECT waiting.pid, waiting.wait_event_type, waiting.wait_event,
       round(extract(epoch FROM clock_timestamp() - waiting.query_start)::numeric * 1000, 1) AS waiting_ms,
       pg_blocking_pids(waiting.pid) AS blocked_by,
       left(regexp_replace(waiting.query, '\s+', ' ', 'g'), 80) AS query
FROM pg_stat_activity waiting
WHERE waiting.datname = current_database() AND cardinality(pg_blocking_pids(waiting.pid)) > 0
ORDER BY waiting.query_start;
-- Wait events of active sessions, and ungranted locks by type:
SELECT wait_event_type, wait_event, count(*) FROM pg_stat_activity
WHERE datname = current_database() AND state <> 'idle' GROUP BY 1, 2 ORDER BY 3 DESC;
SELECT locktype, mode, count(*) FROM pg_locks l JOIN pg_database d ON d.oid = l.database
WHERE d.datname = current_database() AND NOT l.granted GROUP BY 1, 2;
```

Under overload these show a convoy on the installation row: one session waiting on `transactionid` and the rest on `tuple` `ExclusiveLock`, each blocked by the sessions ahead of it. For a history of waits, set `log_lock_waits = on` (with `deadlock_timeout`, default 1 s, as the threshold) and read the server log. Use the gateway's `gateway_admission_seconds{phase="locks"}` for the client-side view.

## Budget totals verification

`budget_totals` (migration 0015) holds settled spend, active holds and unresolved counters per scope (installation, workspace, key lineage), period (day, ISO week, month, lifetime) and UTC period start, in exact integer micro-USD. Admission reads it instead of scanning history. Check it against a full scan of reservations and executions:

```sh
open-model-gateway budget verify
```

- **What it does:** it runs read-only in one `REPEATABLE READ` snapshot and takes no installation lock. Traffic can continue, but the scan reads every reservation, so run it off-peak on large installations. It prints JSON (`buckets`, `mismatch_count`, up to 20 example `mismatches`) and exits nonzero on any difference. It works with the runtime role's grants.
- **When to run it:** after `migrate` (the migration backfills from existing history), after restores (restore drill step 5), and periodically.
- **If it reports a mismatch:** do not edit, delete or "fix" reservations or the ledger. Drift is only possible through manual owner-level edits, such as deleting history, re-keying lineages, or disabling triggers. The runtime role cannot update keys, delete rows or truncate the table. Preserve the report and escalate. The table can be rebuilt by the migrator from the same scan the migration uses.
