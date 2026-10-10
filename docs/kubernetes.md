# Kubernetes deployment (Helm chart)

`deploy/helm/open-model-gateway/` is the Kubernetes delivery for the scale design's phase P7
(`.local/enterprise-rebuild/scale-design.md` §8 and §10). It packages the same distroless image,
explicit migration step and runtime-role boundary as [staging](staging.md) and
[operations](operations.md), adapted for a cluster with [CloudNativePG](https://cloudnative-pg.io/)
(CNPG) and an ingress controller. It is not a production-readiness claim by itself; combine it with
the acceptance in [verification](verification.md) and your own cluster hardening.

> **Beta until validated on a production-like cluster.** As of v0.4.0 this chart has passed `helm
> lint`/`helm template` across four profiles, `kubeconform` against Kubernetes 1.36 schemas, a real
> `kubectl apply --dry-run=server` against a live CloudNativePG 1.30.1 operator, and a `kind` smoke
> test (install, migration, readiness, an idempotent upgrade, pod deletion under the PDB, primary-pod
> recovery). It has not been run under sustained production load or on a non-homelab cluster. Treat
> field names as stable but review every rendered manifest before a real rollout.

## Prerequisites

- Kubernetes 1.29+ (validated against 1.36 schemas and a live CNPG 1.30.1 webhook; see
  [Validation](#validation) below).
- An ingress controller if `ingress.enabled=true` (bitop-talos uses Traefik,
  `ingressClassName: traefik-internal`; adjust `ingress.className` for yours).
- CloudNativePG 1.22+ installed cluster-wide (`cnpg-system`) if you use the chart's optional
  `Cluster`/`Pooler` (`cnpg.cluster.enabled=true`), or an external PostgreSQL 17 otherwise.
- A StorageClass for CNPG's primary/WAL volumes (bitop-talos: `local-ssd`, Miroir, `replicas: 1`,
  `WaitForFirstConsumer`, ~200 GB on `talos-cp-1`/`talos-cp-3` and ~100 GB on `talos-cp-2`; size the
  Cluster's `storage.size`/`walStorage.size` well under the smallest node's capacity and avoid
  `cnpg.cluster.additionalPodAntiAffinity=false` only if you accept co-location risk).
- `prometheus-operator` CRDs (`ServiceMonitor`/`PodMonitor`) only if you enable `metrics.serviceMonitor`
  or `metrics.podMonitor`; both are disabled by default and the chart renders cleanly without them.

## Secrets this chart never generates

Every credential is a reference to an operator-created Secret (chart name: `existingSecret`), per
AGENTS.md "Deployment" and the task's "secrets as env/file references" requirement. The chart fails
fast with a `required()` error if a needed reference is missing, instead of silently proceeding.

| Purpose | Values field | Expected keys |
| --- | --- | --- |
| Runtime `DATABASE_URL` (through the pooler) | `secrets.database.runtime.existingSecret` | one key named by `urlKey` (default `DATABASE_URL`) holding a full `postgres://` URL |
| Direct LISTEN connection | `secrets.database.listen.existingSecret` | one key named by `urlKey` |
| Optional reporting replica | `secrets.database.reporting.existingSecret` (when `secrets.database.reporting.enabled`) | one key named by `urlKey` |
| Migrator (separate from runtime) | `secrets.migrator.existingSecret` | one key named by `urlKey` |
| OIDC | `secrets.oidc.existingSecret` | `GATEWAY_OIDC_ISSUER`, `GATEWAY_OIDC_CLIENT_ID`, `GATEWAY_OIDC_CLIENT_SECRET` (mounted as a file; the binary reads `_FILE`) |
| Provider keys | `secrets.providers.openaiApiKey`/`anthropicApiKey` | `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` (mounted as files) |
| File-store encryption keys | `secrets.fileStore.existingSecret` | key named by `encryptionKeysKey`, `kid:base64key[,kid:base64key…]` |
| CNPG roles (CNPG mode only) | `cnpg.cluster.roles.migrator.existingSecret`, `.runtime.existingSecret` | `kubernetes.io/basic-auth` Secrets (`username`, `password`); CNPG consumes them directly via `bootstrap.initdb.secret` and `managed.roles[].passwordSecret` |

Other provider credentials not covered by the four built-in `_FILE` names use `gateway.extraEnvFrom`
(an `envFrom` entry) plus `gateway.secretEnvAllowlist` (which the chart writes into
`GATEWAY_SECRET_ENV_ALLOWLIST`), exactly as `docs/operations.md` "Container image" describes.

## Choosing a database mode

- **External** (default): set `cnpg.cluster.enabled=false` and point `secrets.database.*` /
  `secrets.migrator.existingSecret` at your own PostgreSQL 17 (managed HA, Patroni, or an existing
  CNPG Cluster you manage outside this chart). See `docs/operations.md` "HA PostgreSQL".
- **Chart-managed CNPG** (`cnpg.cluster.enabled=true`): the chart renders a `Cluster` (2 instances,
  `minSyncReplicas: 1`, synchronous replication, `local-ssd` storage, anti-affinity across nodes,
  the tuning starting points from `docs/operations.md` "Tuning starting points") and a `Pooler`
  (PgBouncer, transaction mode, `max_prepared_statements: 200`). Pre-create the two role Secrets in
  [`ci/cnpg-values.yaml`](../deploy/helm/open-model-gateway/ci/cnpg-values.yaml) before installing:

  ```sh
  kubectl create secret generic gateway-migrator-role --type=kubernetes.io/basic-auth \
    --from-literal=username=gateway_migrator --from-literal=password="$(openssl rand -base64 32)"
  kubectl create secret generic gateway-runtime-role --type=kubernetes.io/basic-auth \
    --from-literal=username=gateway_runtime --from-literal=password="$(openssl rand -base64 32)"
  ```

  The chart then builds full `postgres://` URLs in-pod with Kubernetes' own `$(VAR)` dependent
  environment variable substitution (one `secretKeyRef` per username/password, then a literal
  value referencing them): the gateway Deployment's pooled `DATABASE_URL` targets the Pooler
  service, `GATEWAY_LISTEN_DATABASE_URL` and the optional `GATEWAY_REPORTING_DATABASE_URL` target
  the Cluster's own `-rw`/`-ro` services directly (never the pooler — LISTEN needs a session,
  `docs/operations.md`), and the migration Job's `DATABASE_URL` targets `-rw` as the migrator role.
  No chart template ever writes a password into rendered YAML. CNPG's default password generator
  is alnum-only, so no URL-encoding is attempted; if you supply a password with `: / @ ?` characters
  in a pre-created Secret, URL-encode it yourself first.

## Bootstrapping chart-managed CNPG (first install)

With `cnpg.cluster.enabled=true` the `Cluster`/`Pooler` are plain templates, not
hook resources, but the migration Job is a `pre-install,pre-upgrade` hook that
runs *before* them. On a genuinely fresh database there is nothing yet for the
Job to connect to, and the gateway Deployment cannot become Ready without the
schema and runtime grants. Bootstrap in two steps:

```sh
helm install <release> deploy/helm/open-model-gateway -f <profile>-values.yaml   # no --wait; migrations.run stays false
kubectl wait cluster.postgresql.cnpg.io <release>-db --for=condition=Ready --timeout=5m
helm upgrade <release> deploy/helm/open-model-gateway -f <profile>-values.yaml --set migrations.run=true --wait
```

The first command creates the Cluster/Pooler (and leaves the gateway
Deployment un-Ready, which is expected); the second runs migrate + runtime
grants as a pre-upgrade hook against the now-existing Cluster, then rolls out
gateway pods that can finally pass `/health/ready`. Every later migrating
upgrade only needs the single `--set migrations.run=true --wait` command
since the Cluster already exists.

## Migration Job and runtime grants

`AGENTS.md` forbids migrating or bootstrapping in `serve`. `migrations.run=false` by default; set
`--set migrations.run=true` as an explicit, reviewed operator action on every upgrade that ships a
migration (see `docs/operations.md` "Upgrade and migration procedure" for the full drain sequence —
mixed-schema replicas are not supported, so scale to 0 before a migrating upgrade). The Job is a
Helm `pre-install,pre-upgrade` hook with two steps, both using the **migrator** Secret only (never
mounted into the gateway Deployment):

1. `migrate` (init container, the gateway image): runs `open-model-gateway migrate`.
2. `apply-runtime-grants` (container, `mirror.gcr.io/library/postgres:17-alpine`): runs
   `psql -f /sql/runtime-grants.sql "$(DATABASE_URL)"` against a ConfigMap built from
   [`deploy/staging/runtime-grants.sql`](../deploy/staging/runtime-grants.sql).

   No Rust `apply-runtime-grants` subcommand exists yet — the scale design (§8.1) proposed one, but
   this chart deliberately does not add it: Rust code and migrations in this release are owned by a
   parallel agent, and the gateway image has no `psql` to run arbitrary SQL anyway. Running the
   reviewed grants file through the standard PostgreSQL client image reproduces exactly what
   `scripts/staging.py migrate` already does outside Kubernetes. The chart's copy at
   `deploy/helm/open-model-gateway/files/runtime-grants.sql` is kept in sync by hand with the
   canonical `deploy/staging/runtime-grants.sql`; CI's `helm` job fails the build if they diverge.
   Revisit this once/if a Rust subcommand lands (migration numbering is 0035+, out of scope here).

## PgBouncer caveats (read before enabling the Pooler)

- **Prepared statements:** `max_prepared_statements: 200` is required in `cnpg.pooler.parameters`
  for sqlx's named prepared statement cache under transaction-mode pooling (PgBouncer ≥ 1.21,
  which CNPG 1.30's bundled image satisfies). Without it, every admission/settlement query fails.
- **SCRAM auth:** CNPG issues SCRAM-SHA-256 credentials by default for managed roles; the Pooler's
  `auth_query` (CNPG-managed, via the automatic `cnpg_pooler_pgbouncer` role) validates against them
  without a separate secret.
- **LISTEN needs a direct connection.** `GATEWAY_LISTEN_DATABASE_URL` always targets the Cluster's
  `-rw` service, never the Pooler, because LISTEN/NOTIFY does not work through transaction pooling.
- **Reporting replica.** `GATEWAY_REPORTING_DATABASE_URL` (optional) targets the Cluster's `-ro`
  service for reports/usage/logs only; admission, settlement and `/v1/models` always use the primary.

## Scaling

- `replicaCount` (or `hpa.enabled=true` with `hpa.minReplicas`/`maxReplicas`) sets gateway replicas.
  CPU-based autoscaling is on by default when `hpa.enabled`; `hpa.customMetrics` accepts a raw
  `autoscaling/v2` metric block (for example `gateway_inflight_requests` through a Prometheus
  Adapter or KEDA, as proposed in the scale design §8.1) once you have a metrics adapter.
  `hpa.behavior.scaleDown.stabilizationWindowSeconds: 600` avoids cutting long-lived streams.
- `pdb.maxUnavailable: 1` and `gateway.topologySpreadConstraints` keep replicas spread across nodes
  during voluntary disruption and rollout.
- `cnpg.pooler.instances` and `cnpg.pooler.parameters.default_pool_size` size the PgBouncer tier;
  size `max_client_conn` for `replicaCount × GATEWAY_DATABASE_MAX_CONNECTIONS` plus headroom
  (`docs/operations.md` "Connection budget").
- Scale the gateway with `kubectl scale` or the HPA; scale CNPG with `cnpg.cluster.instances`
  (requires a migrating upgrade is not pending) — see `docs/operations.md` for connection sizing.

## Backups

`cnpg.cluster.backup.enabled=true` renders a `ScheduledBackup` against a Barman object store you
configure in `cnpg.cluster.backup.barmanObjectStore` (CNPG's own object-store backup, independent of
the application-level dump). For the logical, lineage-checked dump/restore workflow this project
documents everywhere else, keep running `scripts/backup.py` (see `docs/operations.md` "Backups and
restore") against the Cluster's `-rw` service from outside the cluster, or as a CronJob you add
separately; this chart does not wrap `scripts/backup.py` in a Job.

## Troubleshooting

- **`/health/ready` stays 503 after a migrating upgrade:** the Job may still be running, or
  `migrations.run` was left `false`. Check `kubectl logs job/<release>-migrate-<revision>`.
- **Admission/settlement errors referencing prepared statements:** `max_prepared_statements` is
  unset or `0` on the Pooler; see [PgBouncer caveats](#pgbouncer-caveats-read-before-enabling-the-pooler).
- **Gateway pods `CrashLoopBackOff` on `_FILE` secrets:** a variable and its `_FILE` companion are
  both set (the binary refuses to start); check that only one of `GATEWAY_OIDC_CLIENT_SECRET`/`_FILE`
  etc. is present (the chart only ever sets the `_FILE` form).
- **NetworkPolicy blocks egress to a provider:** `networkPolicy.egress.allowHttpsEgress` opens 443
  broadly because FQDN-scoped policies need Cilium/Calico Enterprise (not assumed here); add a
  narrower rule via `networkPolicy.egress.extraEgress` if your CNI supports `ipBlock`/FQDN selectors.
- **CNPG `Cluster`/`Pooler` rejected by the webhook:** usually a missing role Secret
  (`cnpg.cluster.roles.migrator`/`.runtime.existingSecret`) or a `storageClass` that doesn't exist;
  `kubectl describe cluster.postgresql.cnpg.io <release>-db` shows the CNPG-side reason.

## Validation

```sh
helm lint deploy/helm/open-model-gateway
helm lint deploy/helm/open-model-gateway -f deploy/helm/open-model-gateway/ci/external-db-values.yaml
helm lint deploy/helm/open-model-gateway -f deploy/helm/open-model-gateway/ci/cnpg-values.yaml
helm lint deploy/helm/open-model-gateway -f deploy/helm/open-model-gateway/ci/loadtest-values.yaml
npm run test:helm   # renders every profile and asserts security/probe/secret invariants
```

Rendered output from every profile was checked with `kubeconform -strict -kubernetes-version 1.36.0`
and, for the `cnpg` profile, a `kubectl apply --dry-run=server` against a real CNPG 1.30.1 operator
(bitop-talos, read-only — nothing was created or left behind): all 13 resources, including the
`Cluster` and `Pooler`, passed CNPG's validating webhooks. `ServiceMonitor`/`PodMonitor` are skipped
by `kubeconform -ignore-missing-schemas` and by the dry run unless `prometheus-operator` CRDs are
present; both are disabled by default for this reason.

## Loadtest profile (P8)

`loadtest.enabled=true` (see `ci/loadtest-values.yaml`) adds a `mock-upstream` Deployment/Service and
a one-shot `loadgen` Job, built from `tools/mock-upstream` and `tools/loadgen`
(`deploy/loadtest/Dockerfile`; there is no CI job publishing this image yet — build it locally):

```sh
docker build -f deploy/loadtest/Dockerfile -t omg-loadtest:local .
kind load docker-image omg-loadtest:local --name <your-kind-cluster>   # or push to a reachable registry
helm install omg-loadtest deploy/helm/open-model-gateway -f deploy/helm/open-model-gateway/ci/loadtest-values.yaml \
  --set loadtest.image.repository=omg-loadtest --set loadtest.image.tag=local
```

The `loadgen` Job has no Helm hook annotation on purpose (it is a manual, repeatable
trigger, not part of the release lifecycle), so it only renders when
`loadtest.loadgen.enabled=true` is also set; otherwise a plain `helm install`/`upgrade
--wait` would apply and block on it immediately, before mock-upstream or the gateway
have anything to serve. Set `--set loadtest.loadgen.enabled=true` and `helm upgrade`
(or `helm template ... --show-only templates/loadtest-loadgen-job.yaml | kubectl apply
-f-`, then unset it again) once seeded data exists to actually run a load test. Also
pin `loadtest.mockUpstream.clusterIP` (see `ci/loadtest-values.yaml`) to an address free
in your cluster's service CIDR: the approval model in
`apps/gateway/src/providers/local/endpoints.rs` never resolves DNS, so
`GATEWAY_LOCAL_UPSTREAMS` (computed by the chart from this value) must pin the
mock-upstream Service's actual ClusterIP, not its DNS name.

This profile targets the dedicated `omg-loadtest` namespace and throwaway CNPG Cluster the program
notes describe for P8 (`.local/enterprise-rebuild/enterprise-program.md` "P8 environment"); tear the
release and its namespace down afterward (`helm uninstall`, then delete the PVCs/namespace — CNPG's
default `reclaimPolicy: Retain` on `local-ssd` does not delete the underlying volume automatically).
