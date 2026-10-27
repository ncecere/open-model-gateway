// Helm chart (deploy/helm/open-model-gateway) rendered-output assertions.
// No cluster is needed: `helm template` renders every profile and this file
// checks security, probe, secret-reference and migration-gating invariants
// with plain text/regex checks (no new YAML-parsing dependency). Pairs with
// `helm lint` (run separately; see docs/kubernetes.md#validation).
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { test } from "node:test";

const root = fileURLToPath(new URL("..", import.meta.url));
const chart = join(root, "deploy/helm/open-model-gateway");

function helmTemplate(extraArgs = []) {
  const result = spawnSync("helm", ["template", "omg-test", chart, ...extraArgs], {
    encoding: "utf8",
    timeout: 30_000,
  });
  assert.equal(result.status, 0, `helm template ${extraArgs.join(" ")} failed:\n${result.stderr}`);
  return result.stdout;
}

function docsOf(rendered) {
  // Split multi-document YAML without a YAML parser: `---` on its own line,
  // skipping the leading `# Source:` comment Helm prepends to each document.
  return rendered.split(/\n---\n/).map((d) => d.trim()).filter(Boolean);
}

function docsOfKind(rendered, kind) {
  return docsOf(rendered).filter((d) => new RegExp(`^kind: ${kind}$`, "m").test(d));
}

const profiles = {
  "external-db": ["-f", join(chart, "ci/external-db-values.yaml")],
  cnpg: ["-f", join(chart, "ci/cnpg-values.yaml")],
  loadtest: ["-f", join(chart, "ci/loadtest-values.yaml")],
};

for (const [name, args] of Object.entries(profiles)) {
  test(`${name} profile renders`, () => {
    const rendered = helmTemplate(args);
    assert.ok(docsOf(rendered).length > 0, "no documents rendered");
  });
}

test("inference base URL is optional, display-only configuration", () => {
  const args = ["-f", join(chart, "ci/external-db-values.yaml")];
  const [unset] = docsOfKind(helmTemplate(args), "ConfigMap");
  assert.doesNotMatch(unset, /GATEWAY_INFERENCE_BASE_URL/);
  const [configured] = docsOfKind(helmTemplate([...args, "--set", "gateway.inferenceBaseUrl=https://api.bitop.dev/v1"]), "ConfigMap");
  assert.match(configured, /GATEWAY_INFERENCE_BASE_URL: "https:\/\/api.bitop.dev\/v1"/);
  const prefix = "https://api.example:8443/prefix@blue/v1";
  const [prefixed] = docsOfKind(helmTemplate([...args, "--set", `gateway.inferenceBaseUrl=${prefix}`]), "ConfigMap");
  assert.ok(prefixed.includes(`GATEWAY_INFERENCE_BASE_URL: "${prefix}"`));
  const invalid = spawnSync("helm", ["template", "omg-test", chart, ...args, "--set", "gateway.inferenceBaseUrl=/v1"], { encoding: "utf8" });
  assert.notEqual(invalid.status, 0);
  assert.match(invalid.stderr, /inferenceBaseUrl/);
});

test("loadgen Job passes the arguments `loadgen run` accepts", () => {
  const rendered = helmTemplate(["-f", join(chart, "ci/loadtest-values.yaml"), "--set", "loadtest.loadgen.enabled=true",
    "--set", "loadtest.loadgen.rate=750", "--set", "loadtest.loadgen.duration=120", "--set", "loadtest.loadgen.extraArgs={--readers,4}"]);
  const [job] = docsOfKind(rendered, "Job").filter((doc) => /name: omg-test-[\w-]*loadgen/.test(doc));
  assert.ok(job, "no loadgen Job rendered");
  const args = [...job.matchAll(/^\s+- "?([^"\n]*)"?$/gm)].map((m) => m[1]);
  const at = (flag) => args[args.indexOf(flag) + 1];
  assert.equal(args[args.indexOf("run")], "run");
  assert.match(at("--target"), /^http:\/\/omg-test[\w-]*:8080$/);
  assert.equal(at("--rate"), "750");
  assert.equal(at("--duration"), "120");
  assert.match(at("--mock-url"), /^http:\/\/omg-test[\w-]*-mock-upstream:8000$/);
  assert.equal(at("--readers"), "4");
  for (const stale of ["--base-url", "--requests", "--concurrency"]) assert.ok(!args.includes(stale), `${stale} is not a loadgen flag`);
});

test("loadtest profile lets loadgen reach the gateway and the gateway reach the mock", () => {
  const rendered = helmTemplate(["--namespace", "omg-loadtest", "-f", join(chart, "ci/loadtest-values.yaml"), "--set", "loadtest.loadgen.enabled=true"]);
  const [job] = docsOfKind(rendered, "Job").filter((doc) => /component: loadtest-loadgen/.test(doc));
  assert.match(job, /name: LOADGEN_VERIFY_DATABASE_URL\s+value: "postgres:\/\/\$\(OMG_DB_VERIFY_USERNAME\):\$\(OMG_DB_VERIFY_PASSWORD\)@omg-test[\w-]*-rw:5432\//);
  assert.match(job, /secretKeyRef:\s+name: gateway-runtime-role/);
  assert.doesNotMatch(job, /password=|:[^$\s]+@/i, "no literal credentials");
  const policies = docsOfKind(rendered, "NetworkPolicy");
  const ingress = policies.find((d) => /name: omg-test[\w-]*-ingress/.test(d));
  assert.match(ingress, /kubernetes.io\/metadata.name: omg-loadtest[\s\S]*component: loadtest-loadgen[\s\S]*port: 8080/);
  assert.match(ingress, /port: 9464/);
  const egress = policies.find((d) => /name: omg-test[\w-]*-egress/.test(d));
  assert.match(egress, /component: loadtest-mock-upstream/);
  assert.match(egress, /port: 8000/);
});

test("CNPG fresh bootstrap restricts database ACLs without locking out operational roles (#315)", () => {
  const base = ["-f", join(chart, "ci/cnpg-values.yaml")];
  const defaults = { databaseName: "gateway", runtimeName: "gateway_runtime", ownerName: "gateway_migrator", database: '"gateway"', runtime: '"gateway_runtime"' };
  const special = {
    databaseName: '"db # tag: value', runtimeName: '"runtime # tag: value', ownerName: '"owner # tag: value',
    database: '"""db # tag: value"', runtime: '"""runtime # tag: value"',
  };
  const cases = [
    { ...defaults, extra: [] },
    { ...defaults, databaseName: "custom-db", runtimeName: 'runtime"role', database: '"custom-db"', runtime: '"runtime""role"',
      extra: ["--set", "cnpg.cluster.database=custom-db", "--set", 'cnpg.cluster.roles.runtime.name=runtime"role'] },
    { ...defaults, extra: ["--set", "cnpg.pooler.enabled=false"] },
    { ...special, extra: [
      "--set-json", `cnpg.cluster.database=${JSON.stringify(special.databaseName)}`,
      "--set-json", `cnpg.cluster.roles.runtime.name=${JSON.stringify(special.runtimeName)}`,
      "--set-json", `cnpg.cluster.roles.migrator.name=${JSON.stringify(special.ownerName)}`,
    ] },
  ];
  for (const { extra, databaseName, runtimeName, ownerName, database, runtime } of cases) {
    const [cluster] = docsOfKind(helmTemplate([...base, ...extra]), "Cluster");
    // Require quoted YAML strings and round-trip punctuation/leading quotes exactly.
    assert.equal(JSON.parse(cluster.match(/^      database: (.+)$/m)[1]), databaseName);
    assert.equal(JSON.parse(cluster.match(/^      owner: (.+)$/m)[1]), ownerName);
    assert.equal(JSON.parse(cluster.match(/^      - name: (.+)$/m)[1]), runtimeName);
    // SQL list entries use JSON-compatible YAML scalars, so escaped SQL identifiers
    // are checked as executed SQL rather than matching their YAML escapes.
    const init = cluster.match(/^      postInitSQL:\n((?:        - .*\n)+)/m)?.[1];
    const app = cluster.match(/^      postInitApplicationSQL:\n((?:        - .*\n)+)/m)?.[1];
    assert.ok(init, "missing fresh-init maintenance ACL SQL");
    assert.ok(app, "missing fresh-init application ACL SQL");
    const statements = section => section.trim().split("\n").map(line => JSON.parse(line.trim().slice(2)));
    const initSQL = statements(init);
    const appSQL = statements(app);
    for (const role of ["cnpg_pooler_pgbouncer", "cnpg_metrics_exporter", "streaming_replica"]) {
      assert.ok(initSQL.includes(`CREATE ROLE ${role} LOGIN PASSWORD NULL;`), `${role} must exist before its grant (CNPG creates it later)`);
    }
    assert.ok(initSQL.includes(`CREATE ROLE ${runtime} NOLOGIN;`), "managed runtime must exist before explicit revokes; CNPG supplies its login/password later");
    const grant = "GRANT CONNECT ON DATABASE postgres, template1 TO cnpg_pooler_pgbouncer, cnpg_metrics_exporter, streaming_replica;";
    const revoke = `REVOKE CONNECT ON DATABASE postgres, template1 FROM PUBLIC, ${runtime};`;
    assert.ok(initSQL.includes(grant));
    assert.ok(initSQL.indexOf(grant) > 3, "create roles before granting CONNECT");
    assert.ok(initSQL.indexOf(revoke) > initSQL.indexOf(grant), "preserve operations before revoking PUBLIC");
    // Pin the complete ordered arrays, not just required statements: an extra
    // later grant must never silently undo these revocations.
    assert.deepEqual(initSQL, [
      "CREATE ROLE cnpg_pooler_pgbouncer LOGIN PASSWORD NULL;",
      "CREATE ROLE cnpg_metrics_exporter LOGIN PASSWORD NULL;",
      "CREATE ROLE streaming_replica LOGIN PASSWORD NULL;",
      `CREATE ROLE ${runtime} NOLOGIN;`,
      "GRANT CONNECT ON DATABASE postgres, template1 TO cnpg_pooler_pgbouncer, cnpg_metrics_exporter, streaming_replica;",
      `REVOKE CONNECT ON DATABASE postgres, template1 FROM PUBLIC, ${runtime};`,
    ]);
    assert.deepEqual(appSQL, [`REVOKE TEMPORARY ON DATABASE ${database} FROM PUBLIC, ${runtime};`]);
    assert.doesNotMatch([...initSQL, ...appSQL].join("\n"), /REVOKE ALL|GRANT TEMP|REVOKE CONNECT ON DATABASE "(?:gateway|custom-db)"/);
  }
  assert.equal(docsOfKind(helmTemplate(profiles["external-db"]), "Cluster").length, 0);
});

test("CNPG primary shuts down quickly so failover is not held up by pooled connections", () => {
  const [cluster] = docsOfKind(helmTemplate(["-f", join(chart, "ci/cnpg-values.yaml")]), "Cluster");
  // CNPG's default smart shutdown waits 180 s for clients; pooled and LISTEN sessions never leave.
  assert.match(cluster, /^  smartShutdownTimeout: 15$/m);
  const [tuned] = docsOfKind(helmTemplate(["-f", join(chart, "ci/cnpg-values.yaml"), "--set", "cnpg.cluster.smartShutdownTimeout=40"]), "Cluster");
  assert.match(tuned, /^  smartShutdownTimeout: 40$/m);
});

test("CNPG backups render the barmanObjectStore map as given, and the chart names no nonexistent settings", () => {
  const [cluster] = docsOfKind(helmTemplate(["-f", join(chart, "ci/cnpg-values.yaml"), "--set", "cnpg.cluster.backup.enabled=true",
    "--set", "cnpg.cluster.backup.barmanObjectStore.destinationPath=s3://bucket/omg"]), "Cluster");
  assert.match(cluster, /barmanObjectStore:\s+destinationPath: s3:\/\/bucket\/omg/);
  assert.doesNotMatch(cluster, /plugins:/, "default method is barmanObjectStore, not the plugin");
  const rendered = helmTemplate(["-f", join(chart, "ci/cnpg-values.yaml"), "--set", "cnpg.cluster.backup.enabled=true",
    "--set", "cnpg.cluster.backup.barmanObjectStore.destinationPath=s3://bucket/omg"]);
  assert.doesNotMatch(rendered, /GATEWAY_DRAIN_SECONDS/);
  assert.equal(docsOfKind(rendered, "ObjectStore").length, 0, "barmanObjectStore mode renders no plugin ObjectStore");
  const [scheduled] = docsOfKind(rendered, "ScheduledBackup");
  assert.doesNotMatch(scheduled, /method: plugin|pluginConfiguration/, "default mode's ScheduledBackup is unchanged");
  assert.match(scheduled, /backupOwnerReference: self/);
});

function pluginBackupArgs(extra = []) {
  return ["-f", join(chart, "ci/cnpg-values.yaml"),
    "--set", "cnpg.cluster.backup.enabled=true",
    "--set", "cnpg.cluster.backup.method=plugin",
    "--set", "cnpg.cluster.backup.plugin.objectStore.destinationPath=s3://omg-backups/omg",
    "--set", "cnpg.cluster.backup.plugin.objectStore.endpointURL=http://rustfs-svc.object-storage.svc.cluster.local:9000",
    "--set", "cnpg.cluster.backup.plugin.objectStore.retentionPolicy=14d",
    "--set-json", 'cnpg.cluster.backup.plugin.objectStore.s3Credentials.accessKeyId={"name":"omg-backup-s3","key":"ACCESS_KEY_ID"}',
    "--set-json", 'cnpg.cluster.backup.plugin.objectStore.s3Credentials.secretAccessKey={"name":"omg-backup-s3","key":"ACCESS_SECRET_KEY"}',
    "--set-json", 'cnpg.cluster.backup.plugin.objectStore.wal={"compression":"gzip","maxParallel":2}',
    "--set-json", 'cnpg.cluster.backup.plugin.objectStore.data={"compression":"gzip","jobs":2}',
    "--set-json", 'cnpg.cluster.backup.plugin.objectStore.instanceSidecarConfiguration={"resources":{"requests":{"cpu":"10m","memory":"64Mi"},"limits":{"memory":"256Mi"}}}',
    ...extra];
}

test("CNPG backups in plugin mode render an ObjectStore, Cluster plugins block and ScheduledBackup method: plugin (#261)", () => {
  const rendered = helmTemplate(pluginBackupArgs());

  const [store] = docsOfKind(rendered, "ObjectStore");
  assert.ok(store, "no ObjectStore rendered in plugin mode");
  assert.match(store, /^apiVersion: barmancloud\.cnpg\.io\/v1$/m);
  assert.match(store, /name: omg-test[\w-]*-db-backup-store$/m);
  assert.match(store, /retentionPolicy: "14d"/);
  assert.match(store, /destinationPath: s3:\/\/omg-backups\/omg/);
  assert.match(store, /endpointURL: http:\/\/rustfs-svc\.object-storage\.svc\.cluster\.local:9000/);
  assert.match(store, /accessKeyId:\s+name: omg-backup-s3\s+key: ACCESS_KEY_ID/);
  assert.match(store, /secretAccessKey:\s+name: omg-backup-s3\s+key: ACCESS_SECRET_KEY/);
  assert.match(store, /wal:\s+compression: gzip\s+maxParallel: 2/);
  assert.match(store, /data:\s+compression: gzip\s+jobs: 2/);
  assert.match(store, /instanceSidecarConfiguration:\s+resources:/);
  assert.doesNotMatch(store, /ACCESS_KEY_ID_VALUE|AKIA|secretAccessKey:\s*\n\s*-/, "no literal credential value, only secret refs");

  const [cluster] = docsOfKind(rendered, "Cluster");
  assert.match(cluster, /plugins:\s+- name: barman-cloud\.cloudnative-pg\.io\s+isWALArchiver: true\s+parameters:\s+barmanObjectName: omg-test[\w-]*-db-backup-store/);
  assert.doesNotMatch(cluster, /^\s+backup:\s*$/m, "plugin mode renders no in-tree backup: block");

  const [scheduled] = docsOfKind(rendered, "ScheduledBackup");
  assert.match(scheduled, /backupOwnerReference: self/);
  assert.match(scheduled, /method: plugin/);
  assert.match(scheduled, /pluginConfiguration:\s+name: barman-cloud\.cloudnative-pg\.io/);
  assert.match(scheduled, /schedule: "0 0 3 \* \* \*"/);
});

test("CNPG backup plugin mode refuses inline S3 credential strings; only secret refs validate (#261)", () => {
  const inline = spawnSync("helm", ["template", "omg-test", chart,
    ...pluginBackupArgs(["--set", "cnpg.cluster.backup.plugin.objectStore.s3Credentials.accessKeyId=AKIAEXAMPLE"])],
    { encoding: "utf8", timeout: 30_000 });
  assert.notEqual(inline.status, 0, "an inline accessKeyId string must be refused by the schema");
  assert.match(inline.stderr, /accessKeyId|s3Credentials|values don't meet the specifications/i);
});

test("CNPG backup plugin mode without S3 credentials fails with the required-value message, not a template panic (#261)", () => {
  const missing = spawnSync("helm", ["template", "omg-test", chart, "-f", join(chart, "ci/cnpg-values.yaml"),
    "--set", "cnpg.cluster.backup.enabled=true", "--set", "cnpg.cluster.backup.method=plugin",
    "--set", "cnpg.cluster.backup.plugin.objectStore.destinationPath=s3://omg-backups/omg"],
    { encoding: "utf8", timeout: 30_000 });
  assert.notEqual(missing.status, 0, "missing credentials must be refused");
  assert.doesNotMatch(missing.stderr, /nil pointer/, "must not be a raw template panic");
  assert.match(missing.stderr, /s3Credentials\.accessKeyId\.name is required when cnpg\.cluster\.backup\.method is "plugin"/);
});

test("cnpg.cluster.backup.method is validated against its enum (#261)", () => {
  const invalid = spawnSync("helm", ["template", "omg-test", chart, "-f", join(chart, "ci/cnpg-values.yaml"),
    "--set", "cnpg.cluster.backup.enabled=true", "--set", "cnpg.cluster.backup.method=rsync"],
    { encoding: "utf8", timeout: 30_000 });
  assert.notEqual(invalid.status, 0, "an unknown backup method must be refused");
  assert.match(invalid.stderr, /method|values don't meet the specifications/i);
});

test("the default image tag is the release this chart belongs to (v + appVersion)", () => {
  const appVersion = readFileSync(join(chart, "Chart.yaml"), "utf8").match(/^appVersion: "([^"]+)"$/m)[1];
  const tag = readFileSync(join(chart, "values.yaml"), "utf8").match(/^  tag: (\S+)$/m)[1];
  assert.equal(tag, `v${appVersion}`);
});

test("default values fail fast with a clear message instead of rendering an unconfigured database", () => {
  const result = spawnSync("helm", ["template", "omg-test", chart], { encoding: "utf8", timeout: 30_000 });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /secrets\.database\.runtime\.existingSecret is required/);
});

test("gateway Deployment never runs as root and has a read-only root filesystem", () => {
  const rendered = helmTemplate(["-f", join(chart, "ci/external-db-values.yaml")]);
  const [deployment] = docsOfKind(rendered, "Deployment").filter((d) => /app.kubernetes.io\/component: migrate/.test(d) === false);
  assert.match(deployment, /runAsNonRoot: true/);
  assert.match(deployment, /runAsUser: 10001/);
  assert.match(deployment, /readOnlyRootFilesystem: true/);
  assert.match(deployment, /drop:\s*\n?\s*-\s*ALL|drop: \[ALL\]/);
  assert.doesNotMatch(deployment, /allowPrivilegeEscalation: true/);
  assert.doesNotMatch(deployment, /automountServiceAccountToken: true/);
});

test("gateway probes are httpGet, never exec (distroless has no shell)", () => {
  const rendered = helmTemplate(["-f", join(chart, "ci/external-db-values.yaml")]);
  const [deployment] = docsOfKind(rendered, "Deployment");
  for (const probe of ["startupProbe", "livenessProbe", "readinessProbe"]) {
    const section = deployment.slice(deployment.indexOf(`${probe}:`));
    assert.match(section.slice(0, 200), /httpGet:/, `${probe} should be httpGet`);
  }
  assert.doesNotMatch(deployment, /exec:\s*\n\s*command:/);
});

test("migration Job is absent unless migrations.run=true, and never shares the runtime Secret", () => {
  const withoutMigrate = helmTemplate(["-f", join(chart, "ci/external-db-values.yaml")]);
  assert.equal(docsOfKind(withoutMigrate, "Job").length, 0, "no Job without migrations.run");

  const withMigrate = helmTemplate(["-f", join(chart, "ci/external-db-values.yaml"), "--set", "migrations.run=true"]);
  const jobs = docsOfKind(withMigrate, "Job");
  assert.equal(jobs.length, 1);
  assert.match(jobs[0], /gateway-migrator-db/, "migration Job must use the migrator Secret");
  assert.doesNotMatch(jobs[0], /gateway-runtime-db/, "migration Job must never reference the runtime Secret");
});

test("first install is one step: a plain migration Job waits for the database, gateway pods wait for the schema", () => {
  const values = ["-f", join(chart, "ci/cnpg-values.yaml"), "--set", "migrations.run=true"];
  const [install] = docsOfKind(helmTemplate(values), "Job");
  // Not a pre-install hook: it must run after the chart's CNPG Cluster exists.
  assert.doesNotMatch(install, /helm\.sh\/hook/);
  assert.match(install, /args: \["wait-for-database"\][\s\S]*args: \["migrate"\][\s\S]*args: \["grants", "apply"\]/);

  const [upgrade] = docsOfKind(helmTemplate([...values, "--is-upgrade"]), "Job");
  assert.match(upgrade, /helm\.sh\/hook: pre-upgrade\n/);

  const [deployment] = docsOfKind(helmTemplate(values), "Deployment");
  const init = deployment.slice(deployment.indexOf("initContainers:"), deployment.indexOf("containers:\n        - name: gateway"));
  assert.match(init, /args: \["wait-for-database", "--schema"\]/, "gateway pods wait instead of crash-looping");
  assert.match(init, /OMG_DB_RUNTIME/);
  assert.doesNotMatch(init, /MIGRATOR/, "the migrator credential stays in the Job");
});

test("no secret value is ever written as a literal into rendered YAML (CNPG profile)", () => {
  const rendered = helmTemplate(["-f", join(chart, "ci/cnpg-values.yaml"), "--set", "migrations.run=true"]);
  // Only secretKeyRef/valueFrom and Kubernetes' own $(VAR) substitution may
  // appear; a literal password would show up as a bare high-entropy value
  // next to `password:` or inside a postgres:// URL with real credentials.
  assert.doesNotMatch(rendered, /postgres:\/\/(?!\$\()/, "a postgres:// URL must build from $(VAR), never a literal credential");
  assert.match(rendered, /secretKeyRef:/);
});

test("NetworkPolicy default-denies and scopes ingress to the configured namespace", () => {
  const rendered = helmTemplate(["-f", join(chart, "ci/cnpg-values.yaml")]);
  const policies = docsOfKind(rendered, "NetworkPolicy");
  assert.ok(policies.some((p) => /policyTypes:\s*\[Ingress, Egress\]/.test(p)), "expected a default-deny policy");
  assert.ok(policies.some((p) => /kubernetes\.io\/metadata\.name: traefik/.test(p)));
});

test("CNPG Cluster uses synchronous replication and the local-ssd StorageClass", () => {
  const rendered = helmTemplate(["-f", join(chart, "ci/cnpg-values.yaml")]);
  const [cluster] = docsOfKind(rendered, "Cluster");
  assert.match(cluster, /minSyncReplicas: 1/);
  assert.match(cluster, /storageClass: local-ssd/);
  assert.match(cluster, /podAntiAffinityType: required/);
});

test("CNPG Pooler runs transaction mode with prepared statements enabled", () => {
  const rendered = helmTemplate(["-f", join(chart, "ci/cnpg-values.yaml")]);
  const [pooler] = docsOfKind(rendered, "Pooler");
  assert.match(pooler, /poolMode: transaction/);
  assert.match(pooler, /max_prepared_statements: "200"/);
});

test("migration Job applies runtime grants with the gateway image itself, from no copied SQL", () => {
  const rendered = helmTemplate(["-f", join(chart, "ci/external-db-values.yaml"), "--set", "migrations.run=true"]);
  const [job] = docsOfKind(rendered, "Job");
  const images = [...job.matchAll(/^\s+image: (.+)$/gm)].map((m) => m[1]);
  assert.ok(images.length >= 2, "migrate and grants steps");
  assert.equal(new Set(images).size, 1, `one image for every step, got ${images.join(", ")}`);
  assert.match(images[0], /ghcr\.io\/ncecere\/open-model-gateway/);
  assert.match(job, /args: \["grants", "apply"\]/);
  assert.doesNotMatch(job, /psql|configMap/);
  assert.equal(docsOf(rendered).filter((d) => /runtime-grants/.test(d) && /^kind: ConfigMap$/m.test(d)).length, 0);
  assert.equal(existsSync(join(chart, "files/runtime-grants.sql")), false, "the chart must not carry a copy of runtime-grants.sql");
});

test("sign-in abuse controls (#78) reach the gateway only when set", () => {
  const base = ["-f", join(chart, "ci/cnpg-values.yaml")];
  const unset = helmTemplate(base);
  assert.doesNotMatch(unset, /GATEWAY_TRUSTED_PROXIES|GATEWAY_LOGIN_RATE_PER_MINUTE/);
  const set = helmTemplate([...base, "--set-json", 'gateway.trustedProxies=["10.42.0.0/16","fd00::/8"]', "--set", "gateway.loginRatePerMinute=120"]);
  assert.match(set, /GATEWAY_TRUSTED_PROXIES: "10\.42\.0\.0\/16,fd00::\/8"/);
  assert.match(set, /GATEWAY_LOGIN_RATE_PER_MINUTE: "120"/);
});

test("session lifetime and idle timeout (#80) reach the gateway only when set", () => {
  const base = ["-f", join(chart, "ci/cnpg-values.yaml")];
  assert.doesNotMatch(helmTemplate(base), /GATEWAY_SESSION_(LIFETIME|IDLE_TIMEOUT)/);
  const set = helmTemplate([...base, "--set", "gateway.sessionLifetime=8h", "--set", "gateway.sessionIdleTimeout=off"]);
  assert.match(set, /GATEWAY_SESSION_LIFETIME: "8h"/);
  assert.match(set, /GATEWAY_SESSION_IDLE_TIMEOUT: "off"/);
});

test("SMTP relay host allowlist (#168) reaches the gateway only when set", () => {
  const base = ["-f", join(chart, "ci/cnpg-values.yaml")];
  assert.doesNotMatch(helmTemplate(base), /GATEWAY_SMTP_HOST_ALLOWLIST/);
  const set = helmTemplate([...base, "--set-json", 'gateway.smtpHostAllowlist=["smtp.example.org","127.0.0.1"]']);
  assert.match(set, /GATEWAY_SMTP_HOST_ALLOWLIST: "smtp\.example\.org,127\.0\.0\.1"/);
});

test("HTTPS egress can be narrowed to CIDRs or to anywhere-except; the default stays open (#169)", () => {
  const base = ["-f", join(chart, "ci/cnpg-values.yaml")];
  const egress = args => docsOfKind(helmTemplate([...base, ...args]), "NetworkPolicy").find(p => /name: omg-test-open-model-gateway-egress$/m.test(p));
  const open = egress([]);
  assert.match(open, /port: 443/);
  assert.doesNotMatch(open, /ipBlock|^\s+to:\s*$/m, "default: 443 to any destination, unchanged");
  const narrowed = egress(["--set-json", 'networkPolicy.egress.httpsCidrs=["203.0.113.0/24","2001:db8::/32"]']);
  assert.match(narrowed, /cidr: "203\.0\.113\.0\/24"/);
  assert.match(narrowed, /cidr: "2001:db8::\/32"/);
  assert.doesNotMatch(narrowed, /except:|0\.0\.0\.0\/0/);
  // Anywhere except the private ranges, per address family.
  const publicOnly = egress(["--set-json", 'networkPolicy.egress.httpsExceptCidrs=["10.0.0.0/8","192.168.0.0/16","fc00::/7"]']);
  assert.match(publicOnly, /cidr: "0\.0\.0\.0\/0"\n\s+except:\n\s+- "10\.0\.0\.0\/8"\n\s+- "192\.168\.0\.0\/16"/);
  assert.match(publicOnly, /cidr: "::\/0"\n\s+except:\n\s+- "fc00::\/7"/);
  // Both together could put an exception outside its CIDR, which the API server rejects.
  assert.throws(() => egress(["--set-json", 'networkPolicy.egress.httpsCidrs=["10.0.0.0/8"]', "--set-json", 'networkPolicy.egress.httpsExceptCidrs=["169.254.169.254/32"]']), /applies only when httpsCidrs is empty/);
  for (const doc of [open, narrowed, publicOnly]) assert.doesNotMatch(doc, /port: 587/, "SMTP is opened only through extraEgress");
});

test("trace export settings (#98 PR 3) reach the gateway container only, never the migrate Job", () => {
  const base = ["-f", join(chart, "ci/external-db-values.yaml")];
  const unset = helmTemplate(base);
  assert.doesNotMatch(unset, /GATEWAY_TRACES_/);

  const set = helmTemplate([
    ...base,
    "--set", "migrations.run=true",
    "--set", "gateway.env.GATEWAY_TRACES_OTLP_URL=https://collector.observability.svc.cluster.local:4318/v1/traces",
    "--set", "gateway.env.GATEWAY_TRACES_SAMPLE_RATIO=0.05",
    "--set", "gateway.env.GATEWAY_TRACES_OTLP_HEADERS_ENV=GATEWAY_TRACES_HEADERS",
  ]);
  const [configMap] = docsOfKind(set, "ConfigMap");
  assert.match(configMap, /GATEWAY_TRACES_OTLP_URL: "https:\/\/collector\.observability\.svc\.cluster\.local:4318\/v1\/traces"/);
  assert.match(configMap, /GATEWAY_TRACES_SAMPLE_RATIO: "0\.05"/);
  assert.match(configMap, /GATEWAY_TRACES_OTLP_HEADERS_ENV: "GATEWAY_TRACES_HEADERS"/);

  const [deployment] = docsOfKind(set, "Deployment").filter((d) => /component: migrate/.test(d) === false);
  assert.match(deployment, /configMapRef:\s+name: omg-test[\w-]*/, "the gateway container must read the ConfigMap via envFrom");

  // The migrate Job never uses envFrom/configMapRef at all (AGENTS.md: the
  // migrator Secret is separate and the Job never shares the gateway's
  // non-secret ConfigMap), so a trace setting placed in gateway.env can
  // never reach it - the one existing jobs/cronjob template in this chart.
  const [job] = docsOfKind(set, "Job");
  assert.ok(job, "no migrate Job rendered");
  assert.doesNotMatch(job, /GATEWAY_TRACES_/, "trace settings must never leak into the migrate Job");
  assert.doesNotMatch(job, /configMapRef/, "the migrate Job must never read the shared ConfigMap");
});

test("the actual secret header value for GATEWAY_TRACES_OTLP_HEADERS_ENV reaches the gateway only through extraEnvFrom, never the ConfigMap", () => {
  const base = ["-f", join(chart, "ci/external-db-values.yaml")];
  const rendered = helmTemplate([
    ...base,
    "--set", "gateway.env.GATEWAY_TRACES_OTLP_HEADERS_ENV=GATEWAY_TRACES_HEADERS",
    "--set-json", 'gateway.extraEnvFrom=[{"secretRef":{"name":"omg-trace-headers"}}]',
  ]);
  const [configMap] = docsOfKind(rendered, "ConfigMap");
  // The ConfigMap may name the variable (GATEWAY_TRACES_OTLP_HEADERS_ENV's
  // value), but must never define GATEWAY_TRACES_HEADERS itself as a key:
  // that would mean the chart put the secret-bearing value in the shared,
  // unencrypted ConfigMap instead of a Secret.
  assert.doesNotMatch(configMap, /^\s*GATEWAY_TRACES_HEADERS:/m, "the header value's own variable must never become a ConfigMap key");
  assert.match(configMap, /GATEWAY_TRACES_OTLP_HEADERS_ENV: "GATEWAY_TRACES_HEADERS"/, "the ConfigMap may still name which variable carries the secret");
  const [deployment] = docsOfKind(rendered, "Deployment");
  assert.match(deployment, /secretRef:\s+name: omg-trace-headers/);
});

test("values.yaml documents the trace exporter settings and a 4318 collector egress example (#98 PR 3)", () => {
  const raw = readFileSync(join(chart, "values.yaml"), "utf8");
  assert.match(raw, /GATEWAY_TRACES_OTLP_URL/, "values.yaml must document the trace endpoint setting");
  assert.match(raw, /GATEWAY_TRACES_SAMPLE_RATIO/, "values.yaml must document the sample ratio setting");
  assert.match(raw, /GATEWAY_TRACES_OTLP_HEADERS_ENV/, "values.yaml must document the headers-variable-name setting");
  assert.match(raw, /extraEnvFrom/, "values.yaml must point the headers value at extraEnvFrom/a Secret");
  assert.match(raw, /4318/, "values.yaml must document the OTLP collector's port for extraEgress");
});

test("an extraEgress rule for the OTLP collector port renders in the egress NetworkPolicy", () => {
  const base = ["-f", join(chart, "ci/cnpg-values.yaml")];
  const rendered = helmTemplate([
    ...base,
    "--set-json", 'networkPolicy.egress.extraEgress=[{"to":[{"ipBlock":{"cidr":"10.50.0.10/32"}}],"ports":[{"protocol":"TCP","port":4318}]}]',
  ]);
  const policies = docsOfKind(rendered, "NetworkPolicy");
  const egress = policies.find((p) => /name: omg-test[\w-]*-egress$/m.test(p));
  assert.match(egress, /cidr: 10\.50\.0\.10\/32/);
  assert.match(egress, /port: 4318/);
});

const scim60Values = join(root, "deploy/staging/scim-values.yaml");
const scim60Bootstrap = join(root, "deploy/staging/scim-bootstrap-job.yaml");
const scim60Image = "ghcr.io/ncecere/open-model-gateway@sha256:31d34b3bcde748148fa96b252c9d73b6964b5e224a7269aaecdf636342c3dc4a";
const scim60Postgres = "ghcr.io/cloudnative-pg/postgresql:18-minimal-trixie@sha256:37ade18dbdddba430858c72725aceeec66f33fa5333e82ea1df4942f6c1c83a3";
const scim60Args = ["--namespace", "dev-omg", "-f", scim60Values];

function scim60Template(extraArgs = []) {
  const result = spawnSync("helm", ["template", "omg-scim60", chart, ...scim60Args, ...extraArgs], { encoding: "utf8", timeout: 30_000 });
  assert.equal(result.status, 0, `SCIM #60 helm template failed:\n${result.stderr}`);
  return result.stdout;
}

function assertScim60NoLiteralCredentials(raw) {
  assert.doesNotMatch(raw, /postgres(?:ql)?:\/\/(?!\$\()/, "database URLs must use dependent env substitution");
  assert.doesNotMatch(raw, /^\s*(?:password|clientSecret|token|stringData):/mi);
  assert.doesNotMatch(raw, /bootstrap-demo|bootstrap-dev|18084|localhost|127\.0\.0\.1/);
}

function assertScim60RoleRefs(raw, prefix, secret, count) {
  for (const [suffix, key] of [["USERNAME", "username"], ["PASSWORD", "password"]]) {
    const entries = [...raw.matchAll(new RegExp(`- name: ${prefix}_${suffix}\\s+valueFrom:\\s+secretKeyRef:\\s+name: ${secret}\\s+key: ${key}(?=\\s|$)`, "g"))];
    assert.equal(entries.length, count, `${prefix}_${suffix} must reference only ${secret}/${key}`);
  }
}

test("SCIM #60 preparation stays stopped, private and fresh-init-only", () => {
  const raw = readFileSync(scim60Values, "utf8");
  const rendered = scim60Template();
  const [deployment] = docsOfKind(rendered, "Deployment");
  assert.match(deployment, /^  name: omg-scim60$/m);
  assert.match(deployment, /^  namespace: dev-omg$/m);
  assert.match(deployment, /^  replicas: 0$/m);
  for (const kind of ["HorizontalPodAutoscaler", "PodDisruptionBudget", "Ingress", "Job", "Secret"]) {
    assert.equal(docsOfKind(rendered, kind).length, 0, `no ${kind} before explicit activation`);
  }
  const [service] = docsOfKind(rendered, "Service");
  assert.match(service, /^  type: ClusterIP$/m);
  assert.doesNotMatch(rendered, /NodePort|LoadBalancer|nodePort:|hostPort:|hostNetwork: true/);
  assert.doesNotMatch(raw, /namespace:|extraEgress:|poolerImage:/, "namespace is a Helm argument; no added egress or invented Pooler pin");
  assertScim60NoLiteralCredentials(raw);
  assertScim60NoLiteralCredentials(rendered);
  assert.deepEqual([...deployment.matchAll(/^\s+image: (.+)$/gm)].map(m => m[1]), [scim60Image, scim60Image]);
  assert.match(raw, /^  tag: dev-731ca715e202$/m);
  const [cluster] = docsOfKind(rendered, "Cluster");
  assert.match(cluster, /^  name: omg-scim60-db$/m);
  assert.match(cluster, /^  instances: 1$/m);
  assert.ok(cluster.includes(`imageName: ${scim60Postgres}`));
  assert.match(cluster, /^  minSyncReplicas: 0$/m);
  assert.match(cluster, /^  maxSyncReplicas: 0$/m);
  assert.match(cluster, /storage:\s+storageClass: local-dev\s+size: 5Gi/);
  assert.match(cluster, /walStorage:\s+storageClass: local-dev\s+size: 1Gi/);
  assert.match(cluster, /max_wal_size: "512MB"/);
  assert.match(cluster, /database: "omg_scim60"\s+owner: "gateway_migrator"\s+(?:#[^\n]*\n\s+)*secret:\s+name: omg-scim60-migrator-role/);
  assert.match(cluster, /name: "gateway_runtime"[\s\S]*superuser: false[\s\S]*passwordSecret:\s+name: omg-scim60-runtime-role/);
  assert.doesNotMatch(cluster, /CREATE EXTENSION|template:|vector|imageCatalogRef:/i);
  const [pooler] = docsOfKind(rendered, "Pooler");
  assert.match(pooler, /^  name: omg-scim60-pooler$/m);
  assert.match(pooler, /cluster:\s+name: omg-scim60-db/);
  assert.match(pooler, /^  instances: 1$/m);
  assert.match(pooler, /poolMode: transaction/);
  assert.doesNotMatch(pooler, /^\s+image:/m, "operator chooses the Pooler image; preflight must verify it");
});

test("SCIM #60 native OIDC/SCIM secrets and runtime/direct database wiring", () => {
  const rendered = scim60Template();
  const [config] = docsOfKind(rendered, "ConfigMap");
  for (const line of [
    'GATEWAY_ENV: "production"', 'GATEWAY_LISTEN: "0.0.0.0:8080"',
    'GATEWAY_PUBLIC_URL: "https://omg-scim60.bitop.dev"',
    'GATEWAY_OIDC_GROUPS_CLAIM: "groups"', 'GATEWAY_SCIM_TOKEN_ENV: "OMG_SCIM60_TOKEN"',
  ]) assert.ok(config.includes(line), line);
  assert.doesNotMatch(config, /^\s*(?:OMG_SCIM60_TOKEN|GATEWAY_OIDC_ISSUER|GATEWAY_OIDC_CLIENT_ID|GATEWAY_OIDC_CLIENT_SECRET|GATEWAY_SECRET_ENV_ALLOWLIST):/m);
  const [deployment] = docsOfKind(rendered, "Deployment");
  for (const key of ["GATEWAY_OIDC_ISSUER", "GATEWAY_OIDC_CLIENT_ID"]) {
    assert.match(deployment, new RegExp(`- name: ${key}\\s+valueFrom:\\s+secretKeyRef:\\s+name: omg-scim60-oidc\\s+key: ${key}`));
  }
  assert.match(deployment, /name: GATEWAY_OIDC_CLIENT_SECRET_FILE\s+value: \/var\/run\/omg-secrets\/oidc\/GATEWAY_OIDC_CLIENT_SECRET/);
  assert.match(deployment, /secretName: omg-scim60-oidc\s+items:\s+- key: GATEWAY_OIDC_CLIENT_SECRET/);
  assert.match(deployment, /name: OMG_SCIM60_TOKEN\s+valueFrom:\s+secretKeyRef:\s+key: OMG_SCIM60_TOKEN\s+name: omg-scim60-scim/);
  assertScim60RoleRefs(deployment, "OMG_DB_RUNTIME", "omg-scim60-runtime-role", 2);
  assertScim60RoleRefs(deployment, "OMG_DB_LISTEN", "omg-scim60-runtime-role", 1);
  assert.match(deployment, /name: DATABASE_URL\s+value: "postgres:\/\/\$\(OMG_DB_RUNTIME_USERNAME\):\$\(OMG_DB_RUNTIME_PASSWORD\)@omg-scim60-pooler:5432\/omg_scim60"/);
  assert.match(deployment, /name: GATEWAY_LISTEN_DATABASE_URL\s+value: "postgres:\/\/\$\(OMG_DB_LISTEN_USERNAME\):\$\(OMG_DB_LISTEN_PASSWORD\)@omg-scim60-db-rw:5432\/omg_scim60"/);
  assert.doesNotMatch(deployment, /MIGRATOR|omg-scim60-migrator-role|OPENAI_API_KEY|ANTHROPIC_API_KEY/);
});

test("SCIM #60 migration is an explicit zero-replica initial-install action", () => {
  const rendered = scim60Template(["--set", "migrations.run=true"]);
  const [deployment] = docsOfKind(rendered, "Deployment");
  assert.match(deployment, /^  replicas: 0$/m);
  const jobs = docsOfKind(rendered, "Job");
  assert.equal(jobs.length, 1);
  const [job] = jobs;
  assert.doesNotMatch(job, /helm\.sh\/hook|bootstrap|RUNTIME|omg-scim60-pooler|configMapRef/);
  assert.match(job, /args: \["wait-for-database"\][\s\S]*args: \["migrate"\][\s\S]*args: \["grants", "apply"\]/);
  assertScim60RoleRefs(job, "OMG_DB_MIGRATOR", "omg-scim60-migrator-role", 3);
  assert.equal([...job.matchAll(/value: "postgres:\/\/\$\(OMG_DB_MIGRATOR_USERNAME\):\$\(OMG_DB_MIGRATOR_PASSWORD\)@omg-scim60-db-rw:5432\/omg_scim60"/g)].length, 3);
  assert.deepEqual([...job.matchAll(/^\s+image: (.+)$/gm)].map(m => m[1]), [scim60Image, scim60Image, scim60Image]);
  assertScim60NoLiteralCredentials(job);
});

test("SCIM #60 standalone bootstrap uses only owner refs, supported commands and hardened security", () => {
  const job = readFileSync(scim60Bootstrap, "utf8");
  assert.equal(docsOfKind(job, "Job").length, 1);
  assert.match(job, /^  name: omg-scim60-bootstrap$/m);
  assert.match(job, /^  namespace: dev-omg$/m);
  assert.match(job, /^  backoffLimit: 0$/m);
  const deadline = Number(job.match(/^  activeDeadlineSeconds: (\d+)$/m)?.[1]);
  assert.ok(deadline > 0 && deadline <= 900);
  assert.match(job, /restartPolicy: Never/);
  assert.match(job, /automountServiceAccountToken: false/);
  for (const setting of ["runAsNonRoot: true", "runAsUser: 10001", "runAsGroup: 10001", "fsGroup: 10001", "type: RuntimeDefault"]) assert.ok(job.includes(setting));
  for (const setting of ["allowPrivilegeEscalation: false", "privileged: false", "readOnlyRootFilesystem: true", "drop: [ALL]"]) {
    assert.equal(job.split(setting).length - 1, 2, `both containers: ${setting}`);
  }
  assert.match(job, /args: \["wait-for-database", "--schema", "--timeout-seconds", "600"\]/);
  assert.match(job, /args: \["provision-user", "--email", "omg-scim60-bootstrap@bitop.invalid", "--platform-admin"\]/);
  assert.deepEqual([...job.matchAll(/^\s+image: (.+)$/gm)].map(m => m[1]), [scim60Image, scim60Image]);
  assertScim60RoleRefs(job, "OMG_DB_MIGRATOR", "omg-scim60-migrator-role", 2);
  assert.equal([...job.matchAll(/value: "postgres:\/\/\$\(OMG_DB_MIGRATOR_USERNAME\):\$\(OMG_DB_MIGRATOR_PASSWORD\)@omg-scim60-db-rw:5432\/omg_scim60"/g)].length, 2);
  assert.doesNotMatch(job, /configMapRef|envFrom:|secretRef:|runtime-role|OIDC|SCIM_TOKEN|pooler|helm\.sh\/hook|ttlSecondsAfterFinished|args: \["(?:serve|migrate|grants)"/);
  assertScim60NoLiteralCredentials(job);
});

test("SCIM #60 bootstrap migration labels are conventions, not invented NetworkPolicy grants", () => {
  const job = readFileSync(scim60Bootstrap, "utf8");
  const rendered = scim60Template();
  const policies = docsOfKind(rendered, "NetworkPolicy");
  assert.equal(policies.length, 3, "only the existing gateway policies");
  const podLabels = job.match(/template:\s+metadata:\s+labels:\n((?:        [^\n]+\n)+)/)?.[1];
  assert.ok(podLabels, "bootstrap Pod must have explicit ownership labels");
  for (const line of ["app.kubernetes.io/name: open-model-gateway", "app.kubernetes.io/instance: omg-scim60", "app.kubernetes.io/component: migrate"]) assert.ok(podLabels.includes(line));
  for (const policy of policies) {
    const selector = policy.match(/  podSelector:\n    matchLabels:\n((?:      [^\n]+\n)+)/)?.[1];
    assert.ok(selector?.includes("app.kubernetes.io/component: gateway"));
    assert.doesNotMatch(policy, /component: migrate|omg-scim60-bootstrap/);
  }
});
