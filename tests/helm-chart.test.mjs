// Helm chart (deploy/helm/open-model-gateway) rendered-output assertions.
// No cluster is needed: `helm template` renders every profile and this file
// checks security, probe, secret-reference and migration-gating invariants
// with plain text/regex checks (no new YAML-parsing dependency). Pairs with
// `helm lint` (run separately; see docs/kubernetes.md#validation).
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
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
  assert.match(jobs[0], /helm\.sh\/hook: pre-install,pre-upgrade/);
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

test("chart's runtime-grants.sql copy matches deploy/staging/runtime-grants.sql", () => {
  const canonical = readFileSync(join(root, "deploy/staging/runtime-grants.sql"), "utf8");
  const copy = readFileSync(join(chart, "files/runtime-grants.sql"), "utf8");
  assert.equal(copy, canonical, "re-copy deploy/staging/runtime-grants.sql into deploy/helm/open-model-gateway/files/runtime-grants.sql");
});
