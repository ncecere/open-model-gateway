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
  Set `gateway.trustedProxies` to the controller's pod addresses (for example the
  cluster pod CIDR) so sign-in rate limits count each user, not the controller
  ([identity](identity.md#sign-in-abuse-controls)).
- CloudNativePG 1.22+ installed cluster-wide (`cnpg-system`) if you use the chart's optional
  `Cluster`/`Pooler` (`cnpg.cluster.enabled=true`), or an external PostgreSQL 17 otherwise.
- A StorageClass for CNPG's primary/WAL volumes (bitop-talos: `local-ssd`, Miroir, `replicas: 1`,
  `WaitForFirstConsumer`, ~200 GB on `talos-cp-1`/`talos-cp-3` and ~100 GB on `talos-cp-2`; size the
  Cluster's `storage.size`/`walStorage.size` well under the smallest node's capacity and avoid
  `cnpg.cluster.additionalPodAntiAffinity=false` only if you accept co-location risk).
- `prometheus-operator` CRDs (`ServiceMonitor`/`PodMonitor`) only if you enable `metrics.serviceMonitor`
  or `metrics.podMonitor`; both are disabled by default and the chart renders cleanly without them.

## Separate inference host

If the dashboard is at `https://platform.bitop.dev` and inference is published at
`https://api.bitop.dev/v1`, set:

```yaml
gateway:
  inferenceBaseUrl: https://api.bitop.dev/v1
```

This writes `GATEWAY_INFERENCE_BASE_URL` to the ConfigMap. It only tells signed-in
users which OpenAI-compatible SDK base URL to use; configure the inference ingress separately.
Key and model pages also provide an Anthropic SDK base with only the final `/v1`
removed, because that SDK appends `/v1/messages`. Prefixes and ports are preserved.
Realtime endpoint copies use `wss://` (`ws://` for development HTTP); SDK bases stay
HTTPS/HTTP.
Keep `GATEWAY_PUBLIC_URL` set to the dashboard origin for OIDC and CSRF. Dashboard
API calls remain same-origin relative requests. An empty chart value omits the
setting, so the UI uses its current origin plus `/v1`. The gateway rejects invalid
URLs at startup: HTTPS, no credentials/query/fragment, path ending in `/v1` (one
trailing slash is removed); HTTP only on development loopback.

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
  CNPG Cluster you manage outside this chart). See [HA PostgreSQL](postgresql-ha.md).
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

## First install (including chart-managed CNPG)

A first install is one command, so it works as a single Flux `HelmRelease`:

```sh
helm install <release> deploy/helm/open-model-gateway -f <profile>-values.yaml \
  --set migrations.run=true --wait --timeout 10m
```

On a first install the migration Job is a plain resource, not a hook, so it starts alongside the
chart's CNPG `Cluster`/`Pooler`. It first waits for the database to accept connections
(`open-model-gateway wait-for-database`), then migrates and applies the runtime grants. Gateway
pods have a `wait-for-schema` init container (`wait-for-database --schema`, with the runtime
credential), so they wait for the schema instead of crash-looping, and `--wait` completes once they
pass `/health/ready`. Verified on kind with CloudNativePG 1.30.1: about two minutes, no restarts.

Every later migrating upgrade is `helm upgrade ... --set migrations.run=true --wait`; there the Job
is a `pre-upgrade` hook, so the schema changes before any new gateway pod starts.

## Migration Job and runtime grants

`AGENTS.md` forbids migrating or bootstrapping in `serve`. `migrations.run=false` by default; set
`--set migrations.run=true` as an explicit, reviewed operator action on every upgrade that ships a
migration (see `docs/operations.md` "Upgrade and migration procedure" for the full drain sequence —
mixed-schema replicas are not supported, so scale to 0 before a migrating upgrade). The Job is a
plain resource on a first install and a Helm `pre-upgrade` hook on upgrades (see
[First install](#first-install-including-chart-managed-cnpg)). Its steps all use the **migrator**
Secret only (never mounted into the gateway Deployment):

1. `wait-for-database` (init container): waits until PostgreSQL accepts connections.
2. `migrate` (init container): runs `open-model-gateway migrate`.
3. `apply-runtime-grants` (container): runs `open-model-gateway grants apply`, which applies the
   reviewed [`deploy/staging/runtime-grants.sql`](../deploy/staging/runtime-grants.sql) embedded in
   the binary.

Both steps use the gateway image, so there is no second image to pin and no copy of the SQL to keep
in sync.

## Outbound traffic

Two layers decide where gateway pods may connect, and they should agree:

- **The gateway** ([outbound connections](operations.md#outbound-connections)) refuses metadata, link-local and reserved addresses, allows loopback only in development and private networks only from `GATEWAY_EGRESS_PRIVATE_ALLOW`. Set it with `gateway.env`, for example `GATEWAY_EGRESS_PRIVATE_ALLOW: "10.42.0.0/16"` for an in-cluster IdP or S3 store. The SMTP relay must also be on `gateway.smtpHostAllowlist`.
- **The NetworkPolicy** default-denies, then allows DNS, the database, TCP 443 and `networkPolicy.egress.extraEgress`. By default 443 goes anywhere, because plain NetworkPolicy can't name hosts (FQDN rules need Cilium or Calico Enterprise). Narrow it:

  ```yaml
  networkPolicy:
    egress:
      # Either: 443 only to these networks (your providers' ranges, an egress
      # proxy, the in-cluster IdP) ...
      # httpsCidrs: ["203.0.113.0/24"]
      # ... or: 443 anywhere except these (public addresses only, as the
      # gateway's own policy). Not both.
      httpsExceptCidrs: ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10", "fc00::/7"]
      # SMTP isn't opened by default (587/465/25): add your relay.
      extraEgress:
        - to: [{ ipBlock: { cidr: 203.0.113.25/32 } }]
          ports: [{ protocol: TCP, port: 587 }]
  ```

  Instance metadata (`169.254.169.254`, `fd00:ec2::254`) and EKS Pod Identity (`169.254.170.23`) listen on port 80, which the default-deny already blocks. That matters because the AWS SDK's credential lookup uses its own HTTP client, outside the gateway's policy: with instance-metadata or Pod Identity credentials, add an `extraEgress` rule for that address on port 80. IRSA needs STS on 443 instead (the public endpoint or an STS VPC endpoint), so keep it reachable under `httpsCidrs`.

A private destination needs both: its network in `GATEWAY_EGRESS_PRIVATE_ALLOW` and, if `httpsCidrs` is set, in `httpsCidrs` (or an `extraEgress` rule). With AWS PrivateLink (interface endpoints for Bedrock, STS or S3), list the VPC range in both. Over Tailscale, list only the tailnet addresses you need, never all of `100.64.0.0/10`.

### Traces (#98)

`GATEWAY_TRACES_OTLP_URL`, `GATEWAY_TRACES_SAMPLE_RATIO` and `GATEWAY_TRACES_OTLP_HEADERS_ENV` (the collector URL, sample ratio, and the *name* of the variable holding its auth headers; see [operations](operations.md#traces)) are plain, non-secret settings and go in `gateway.env` like any other setting. The variable `GATEWAY_TRACES_OTLP_HEADERS_ENV` names, however, is itself secret-bearing and must come from a Secret through `gateway.extraEnvFrom`, never `gateway.env`:

```yaml
gateway:
  env:
    GATEWAY_TRACES_OTLP_URL: "https://otel-collector.observability.svc.cluster.local:4318/v1/traces"
    GATEWAY_TRACES_SAMPLE_RATIO: "0.01"
    GATEWAY_TRACES_OTLP_HEADERS_ENV: "GATEWAY_TRACES_HEADERS"
  extraEnvFrom:
    - secretRef:
        name: omg-trace-headers
networkPolicy:
  egress:
    extraEgress:
      - to: [{ podSelector: { matchLabels: { app: otel-collector } } }]
        ports: [{ protocol: TCP, port: 4318 }]
```

The Secret `omg-trace-headers` needs one key named `GATEWAY_TRACES_HEADERS` (matching the name given to `GATEWAY_TRACES_OTLP_HEADERS_ENV` above), holding `Key=Value,Key2=Value2` header pairs such as `Authorization=Bearer ...`. A private collector destination also needs `GATEWAY_EGRESS_PRIVATE_ALLOW` covering its address, exactly like any other private destination above; a collector outside the cluster still needs 4318 reachable under `httpsCidrs`/`httpsExceptCidrs` if those are set and the URL uses HTTPS (plain HTTP to a collector is accepted only on development loopback). This chart's `configmap.yaml`/`deployment.yaml` route every `gateway.env` entry to the gateway container alone: the migration Job never mounts this ConfigMap, so a trace setting placed here never reaches it.

## Failover timing

`cnpg.cluster.smartShutdownTimeout` (default 15 s) is how long a stopping primary waits for clients
before a fast shutdown. CloudNativePG's own default is 180 s. The Pooler's server connections and
each replica's LISTEN session never disconnect on their own, so with 180 s a failover kept writes
down for about three minutes on bitop-talos; with 15 s the standby was promoted in 18 s under load.
With `instances: 2`, writes stalled again while the old primary rejoined as the synchronous standby
(#149); see [HA PostgreSQL](postgresql-ha.md#measured-failover).
Requests interrupted by the failover keep their holds and are reconciled as unknown; nothing is
refunded or charged twice (#25).

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
  ([connection sizing](postgresql-ha.md#connection-sizing)).
- Scale the gateway with `kubectl scale` or the HPA; scale CNPG with `cnpg.cluster.instances`
  (requires a migrating upgrade is not pending) — see `docs/operations.md` for connection sizing.

## Backups

`cnpg.cluster.backup.enabled=true` renders a CNPG object-store backup (continuous WAL archiving plus
a scheduled base backup), independent of the application-level dump. `cnpg.cluster.backup.method`
picks one of two mutually exclusive ways to reach the object store (#261):

- **`barmanObjectStore` (default, unchanged since earlier releases).** CNPG's in-tree backup: the
  Cluster gets a `spec.backup.barmanObjectStore` block rendered as-is from
  `cnpg.cluster.backup.barmanObjectStore`, and the `ScheduledBackup` has no `method`/`pluginConfiguration`.
- **`plugin`.** The Barman Cloud plugin, which upstream CNPG now recommends over the in-tree backup.
  The chart additionally renders a `barmancloud.cnpg.io/v1` `ObjectStore` (named `<cluster>-backup-store`,
  or `cnpg.cluster.backup.plugin.objectStoreName`) from `cnpg.cluster.backup.plugin.objectStore`; the
  Cluster gets `spec.plugins: [{name: barman-cloud.cloudnative-pg.io, isWALArchiver: true, parameters:
  {barmanObjectName: <that ObjectStore's name>}}]` and **no** `spec.backup` block; the `ScheduledBackup`
  gets `method: plugin` and `pluginConfiguration.name: barman-cloud.cloudnative-pg.io`, keeping its
  `schedule` and `backupOwnerReference: self`. The [Barman Cloud plugin](https://github.com/cloudnative-pg/plugin-barman-cloud)
  must already be installed cluster-wide, the same way CNPG itself is.

  S3 credentials are **Secret references only**: `s3Credentials.accessKeyId`/`secretAccessKey` are each
  `{name: <Secret>, key: <data key>}`, validated by `values.schema.json` (an inline key string is
  refused at `helm template`/`install` time). The chart never generates or stores that Secret.
  `wal`/`data` (compression, parallelism) and `instanceSidecarConfiguration` (the plugin sidecar's
  resources) are rendered as given; see the plugin's `ObjectStore` CRD for the full field set.

  ```yaml
  cnpg:
    cluster:
      backup:
        enabled: true
        method: plugin
        scheduledBackup:
          schedule: "0 15 3 * * *" # seconds first: 03:15 UTC daily
        plugin:
          objectStore:
            destinationPath: s3://omg-db-backups/
            endpointURL: http://rustfs-svc.object-storage.svc.cluster.local:9000
            retentionPolicy: "14d"
            s3Credentials:
              accessKeyId: { name: omg-backup-s3, key: ACCESS_KEY_ID }
              secretAccessKey: { name: omg-backup-s3, key: ACCESS_SECRET_KEY }
            wal:
              compression: gzip
              maxParallel: 2
            data:
              compression: gzip
              jobs: 2
            instanceSidecarConfiguration:
              resources:
                requests: { cpu: 10m, memory: 64Mi }
                limits: { memory: 256Mi }
  ```

  This mirrors the homelab's own `pi-durable-platform` `ObjectStore`/`ScheduledBackup` pair; the
  bucket, its scoped S3 user and the Secret that backs `s3Credentials` are provisioned outside this
  chart (see that cluster's `tools/provision-*-backups.py`), not by OMG itself.

**Switching an existing cluster between modes is a one-way cutover, not a migration.** Changing
`cnpg.cluster.backup.method` on a running release (for example `barmanObjectStore` → `plugin`)
removes `spec.backup.barmanObjectStore` and adds `spec.plugins` on the live Cluster. WAL archiving
stops and restarts under the new mode, and the plugin does not take over the old backup catalog. Take
a fresh base backup in the new mode right away, keep the old object-store prefix until its retention
passes, and don't rely on point-in-time recovery across the switch. To be able to restore from either
chain during the transition, point the new mode at a different `destinationPath`.

**NetworkPolicy:** this chart's `NetworkPolicy` templates (`networkPolicy.enabled`) govern only the
gateway Deployment's own pods (`app.kubernetes.io/component: gateway`); they do not select CNPG's
instance pods (`cnpg.io/cluster: <name>`) at all, in either backup mode. If your namespace runs a
default-deny `NetworkPolicy`, add your own rule allowing egress from the CNPG instance pods
(`podSelector: {matchLabels: {cnpg.io/cluster: <release>-db}}`) to the object store's namespace and
port (for example `object-storage`/`9000` for an in-cluster RustFS), the same way
`pi-durable-platform`'s `postgres-egress` policy does — this chart does not render that policy for you.

For the logical, lineage-checked dump/restore workflow this project documents everywhere else, keep
running `scripts/backup.py` (see `docs/operations.md` "Backups and restore") against the Cluster's
`-rw` service from outside the cluster, or as a CronJob you add separately; this chart does not wrap
`scripts/backup.py` in a Job. Either CNPG backup mode above restores through CNPG's own
[bootstrap.recovery](https://cloudnative-pg.io/documentation/current/recovery/) into a fresh Cluster,
not through this chart; after any restore (CNPG or `scripts/backup.py`), re-apply revocations for keys,
membership and platform grants revoked after the backup was taken and expect spend recorded since the
backup to be lost — see #146 and `docs/operations.md` "Backups and restore" for that runbook.

## Troubleshooting

- **`/health/ready` stays 503 after a migrating upgrade:** the Job may still be running, or
  `migrations.run` was left `false`. Check `kubectl logs job/<release>-migrate-<revision>`.
- **Admission/settlement errors referencing prepared statements:** `max_prepared_statements` is
  unset or `0` on the Pooler; see [PgBouncer caveats](#pgbouncer-caveats-read-before-enabling-the-pooler).
- **Gateway pods `CrashLoopBackOff` on `_FILE` secrets:** a variable and its `_FILE` companion are
  both set (the binary refuses to start); check that only one of `GATEWAY_OIDC_CLIENT_SECRET`/`_FILE`
  etc. is present (the chart only ever sets the `_FILE` form).
- **NetworkPolicy blocks egress to a provider:** with `networkPolicy.egress.httpsCidrs` set, 443 reaches
  only those networks; add the provider's range or an egress proxy (see [outbound traffic](#outbound-traffic)).
  Email that never arrives with `connection` errors usually means SMTP isn't opened: add an
  `extraEgress` rule for the relay's port.
- **The gateway refuses a destination** (`gateway_egress_denied_total`, a log naming
  `GATEWAY_EGRESS_PRIVATE_ALLOW`): the address is private or reserved; list its network in
  `GATEWAY_EGRESS_PRIVATE_ALLOW` if it's meant to be reached.
- **CNPG `Cluster`/`Pooler` rejected by the webhook:** usually a missing role Secret
  (`cnpg.cluster.roles.migrator`/`.runtime.existingSecret`) or a `storageClass` that doesn't exist;
  `kubectl describe cluster.postgresql.cnpg.io <release>-db` shows the CNPG-side reason.
- **`kind load docker-image` fails with `ctr: content digest sha256:... not found`:** the published
  image is a multi-platform index with signed SBOM/provenance attestations, and Docker Desktop keeps
  only your platform's content, so kind can't import the whole index. A real cluster pulling from the
  registry is unaffected. For a local kind cluster, either skip `kind load` and let the node pull the
  public image from GHCR (the chart's default `IfNotPresent` pull policy does this), or load a
  single-platform archive:

  ```sh
  IMAGE=ghcr.io/ncecere/open-model-gateway:v0.7.2
  docker pull "$IMAGE"
  docker save --platform "linux/$(docker version --format '{{.Server.Arch}}')" "$IMAGE" -o omg.tar
  kind load image-archive omg.tar --name <your-kind-cluster>
  ```

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
(`deploy/loadtest/Dockerfile`). For a cluster, build it on request with the `loadtest-image`
workflow (`gh workflow run loadtest-image.yml -f ref=<branch|tag|commit>`), which publishes
`ghcr.io/ncecere/open-model-gateway-loadtest:dev-<first 12 characters of the commit>`; for kind,
build it locally:

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
-f-`, then unset it again) once seeded data exists to actually run a load test. The Job
runs `loadgen run` against the gateway Service at `loadtest.loadgen.rate` requests per second
for `duration` seconds (the first `warmup` seconds left out of latency statistics), spread over
`keys` seeded keys; add further `loadgen run` flags with `loadtest.loadgen.extraArgs`. With the
chart-managed CNPG cluster, the Job also gets `LOADGEN_VERIFY_DATABASE_URL` (the runtime role on the
primary, built in-pod from the role Secret), so every run checks settlement against the database.
The profile's NetworkPolicies admit the loadgen Job to the gateway and its metrics port and let the
gateway reach the mock; they assume the `omg-loadtest` namespace. Also
pin `loadtest.mockUpstream.clusterIP` (see `ci/loadtest-values.yaml`) to an address free
in your cluster's service CIDR: the approval model in
`apps/gateway/src/providers/local/endpoints.rs` never resolves DNS, so
`GATEWAY_LOCAL_UPSTREAMS` (computed by the chart from this value) must pin the
mock-upstream Service's actual ClusterIP, not its DNS name.

This profile targets the dedicated `omg-loadtest` namespace and throwaway CNPG Cluster the program
notes describe for P8 (`.local/enterprise-rebuild/enterprise-program.md` "P8 environment"); tear the
release and its namespace down afterward (`helm uninstall`, then delete the PVCs/namespace — CNPG's
default `reclaimPolicy: Retain` on `local-ssd` does not delete the underlying volume automatically).
