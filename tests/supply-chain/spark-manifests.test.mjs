// The Spark test platform's manifests (deploy/spark, #599) run a passwordless demo issuer and a
// database forwarder next to the gateway. Nothing else reads these files, so this does: every
// image is pinned, every container is locked down, nothing is exposed, everything listens on
// loopback and three environments fit the shared CPU limit.
import assert from "node:assert/strict";
import net from "node:net";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { parseStrict, root, toData } from "./lib.mjs";
import { probeLoopback } from "../../deploy/spark/loopback-probe.mjs";

const read = name => readFileSync(join(root, "deploy/spark", name), "utf8");
const load = name => toData(parseStrict(read(name), name));
const workloads = () => ({ "bootstrap-job.yaml": load("bootstrap-job.yaml"), "demo.yaml": load("demo.yaml") });
const dockerfileNode = readFileSync(join(root, "Dockerfile"), "utf8").match(/^FROM (mirror\.gcr\.io\/library\/node:\S+@sha256:[0-9a-f]{64}) AS web$/m)[1];
const cpu = value => (String(value).endsWith("m") ? Number.parseInt(value, 10) : Number.parseFloat(value) * 1000);

const containersOf = doc => {
  const pod = doc.spec.template.spec;
  return [...(pod.initContainers ?? []), ...(pod.containers ?? [])].map(c => ({ ...c, pod }));
};

// Every violation of the manifest rules, as text, for one workload document.
function violations(file, doc, forwarderText = read("pg-forward.mjs")) {
  const found = [];
  const pod = doc.spec.template.spec;
  for (const key of ["hostNetwork", "hostPID", "hostIPC"]) if (pod[key]) found.push(`${file}: ${key}`);
  if (pod.volumes?.some(v => v.hostPath)) found.push(`${file}: a hostPath volume`);
  for (const c of containersOf(doc)) {
    const where = `${file}: ${c.name}`;
    if (!(c.image === "__IMAGE__" || /^[^\s@]+@sha256:[0-9a-f]{64}$/.test(c.image))) found.push(`${where}: image ${c.image} is neither pinned by digest nor the placeholder`);
    if (/node:/.test(c.image) && c.image !== dockerfileNode) found.push(`${where}: the Node image is not the Dockerfile's`);
    const effective = key => c.securityContext?.[key] ?? pod.securityContext?.[key];
    if (effective("runAsNonRoot") !== true) found.push(`${where}: runAsNonRoot`);
    if (!(effective("runAsUser") > 0)) found.push(`${where}: runAsUser must be a non-root uid`);
    if (c.securityContext?.allowPrivilegeEscalation !== false) found.push(`${where}: allowPrivilegeEscalation`);
    if (JSON.stringify(c.securityContext?.capabilities?.drop) !== '["ALL"]' || c.securityContext?.capabilities?.add) found.push(`${where}: capabilities`);
    if (c.securityContext?.readOnlyRootFilesystem !== true) found.push(`${where}: readOnlyRootFilesystem`);
    if (effective("seccompProfile")?.type !== "RuntimeDefault") found.push(`${where}: seccompProfile`);
    if (c.securityContext?.privileged || c.securityContext?.procMount) found.push(`${where}: privileged`);
    if (c.ports?.some(p => p.hostPort || p.hostIP)) found.push(`${where}: hostPort`);
    if (!c.resources?.limits?.cpu || !c.resources?.limits?.memory || !c.resources?.requests?.cpu) found.push(`${where}: resources`);
  }
  // Loopback only: the listener, the database and the issuer are reached by port-forward alone.
  const env = Object.fromEntries(containersOf(doc).flatMap(c => (c.env ?? []).map(e => [e.name, e.value])));
  if (env.GATEWAY_LISTEN !== "127.0.0.1:3000") found.push(`${file}: GATEWAY_LISTEN is ${env.GATEWAY_LISTEN}`);
  if (!String(env.DATABASE_URL).includes("@127.0.0.1:5432/gateway_enterprise_demo")) found.push(`${file}: DATABASE_URL is not the loopback forwarder`);
  if (file === "demo.yaml" && env.GATEWAY_OIDC_ISSUER !== "http://127.0.0.1:18084") found.push(`${file}: the issuer is ${env.GATEWAY_OIDC_ISSUER}`);
  const listens = [...forwarderText.matchAll(/\.listen\(([^)]*)\)/g)].map(m => m[1].replace(/\s/g, ""));
  if (listens.length !== 1 || listens[0] !== '5432,"127.0.0.1"') found.push(`pg-forward.mjs: listens on ${listens.join(" and ")}`);
  return found;
}

test("the Spark manifests meet the image, hardening and loopback rules", () => {
  for (const [file, doc] of Object.entries(workloads())) assert.deepEqual(violations(file, doc), [], file);
  // Both workloads use the Dockerfile's Node image, so the pin follows a Dockerfile refresh.
  assert.ok(Object.values(workloads()).every(doc => containersOf(doc).some(c => c.image === dockerfileNode)));
});

test("nothing in deploy/spark exposes the demo", () => {
  for (const name of ["bootstrap-job.yaml", "demo.yaml", "values.yaml"]) {
    const text = read(name);
    assert.doesNotMatch(text, /^kind:\s*(Service|Ingress|NetworkPolicy)\b/m, name);
    assert.doesNotMatch(text, /NodePort|LoadBalancer|hostNetwork|hostPort|hostPath|privileged:\s*true/, name);
  }
  const values = load("values.yaml");
  assert.ok(!values.ingress?.enabled);
  assert.equal(values.replicaCount, 0, "the chart's own gateway stays off: the demo gateway is loopback-only");
  assert.equal(values.cnpg.pooler.enabled, false);
});

test("the CloudNativePG image is PostgreSQL 17, pinned by digest", () => {
  assert.match(load("values.yaml").cnpg.cluster.image, /^ghcr\.io\/cloudnative-pg\/postgresql:17\.\d+@sha256:[0-9a-f]{64}$/);
});

test("the demo gateway probes liveness on /health/live and readiness on /health/ready", () => {
  const gateway = containersOf(workloads()["demo.yaml"]).find(c => c.name === "gateway");
  assert.deepEqual(gateway.livenessProbe.exec.command.slice(0, 4), ["/usr/local/bin/open-model-gateway", "healthcheck", "--url", "http://127.0.0.1:3000/health/live"]);
  assert.ok(!gateway.readinessProbe.exec.command.includes("--url"), "readiness uses the default /health/ready");
});

test("three environments fit the 8 CPU limit and 4 CPU request quotas", () => {
  const pod = workloads()["demo.yaml"].spec.template.spec;
  // Native sidecars and main containers run together; plain init containers have finished.
  const running = [...pod.initContainers.filter(c => c.restartPolicy === "Always"), ...pod.containers];
  const { cluster } = load("values.yaml").cnpg;
  const limit = running.reduce((sum, c) => sum + cpu(c.resources.limits.cpu), 0) + cpu(cluster.resources.limits.cpu);
  const request = running.reduce((sum, c) => sum + cpu(c.resources.requests.cpu), 0) + cpu(cluster.resources.requests.cpu);
  assert.equal(running.length, 3, "the forwarder, the issuer and the gateway");
  assert.ok(3 * limit <= 8000, `one environment limits ${limit}m CPU`);
  assert.ok(3 * request <= 4000, `one environment requests ${request}m CPU`);
});

test("fixtures: every rule is enforced", () => {
  const mutations = [
    ["a floating image", d => { d.spec.template.spec.containers[0].image = "ghcr.io/bitop-dev/open-model-gateway:v0.16.0"; }],
    ["an unpinned node image", d => { d.spec.template.spec.initContainers[0].image = "mirror.gcr.io/library/node:22"; }],
    ["another node digest", d => { d.spec.template.spec.initContainers[0].image = `mirror.gcr.io/library/node:22.23.4-bookworm-slim@sha256:${"a".repeat(64)}`; }],
    ["root", d => { d.spec.template.spec.securityContext.runAsNonRoot = false; }],
    ["uid 0", d => { d.spec.template.spec.securityContext.runAsUser = 0; }],
    ["privilege escalation", d => { d.spec.template.spec.containers[0].securityContext.allowPrivilegeEscalation = true; }],
    ["capabilities kept", d => { d.spec.template.spec.containers[0].securityContext.capabilities = { drop: ["NET_RAW"] }; }],
    ["capabilities added", d => { d.spec.template.spec.containers[0].securityContext.capabilities = { drop: ["ALL"], add: ["NET_ADMIN"] }; }],
    ["a writable root", d => { d.spec.template.spec.containers[0].securityContext.readOnlyRootFilesystem = false; }],
    ["no seccomp", d => { delete d.spec.template.spec.securityContext.seccompProfile; }],
    ["host networking", d => { d.spec.template.spec.hostNetwork = true; }],
    ["a host port", d => { d.spec.template.spec.containers[0].ports = [{ containerPort: 3000, hostPort: 3000 }]; }],
    ["a host path", d => { d.spec.template.spec.volumes.push({ name: "h", hostPath: { path: "/" } }); }],
    ["a privileged container", d => { d.spec.template.spec.containers[0].securityContext.privileged = true; }],
    ["a public listener", d => { d.spec.template.spec.containers[0].env.find(e => e.name === "GATEWAY_LISTEN").value = "0.0.0.0:3000"; }],
    ["a remote database", d => { d.spec.template.spec.containers[0].env.find(e => e.name === "DATABASE_URL").value = "postgres://u:p@omg-db-rw:5432/gateway_enterprise_demo"; }],
    ["no CPU limit", d => { delete d.spec.template.spec.containers[0].resources.limits.cpu; }],
  ];
  for (const file of ["bootstrap-job.yaml", "demo.yaml"]) {
    for (const [label, mutate] of mutations) {
      const doc = structuredClone(workloads()[file]);
      try { mutate(doc); } catch { continue; }
      assert.notDeepEqual(violations(file, doc), [], `${file}: ${label}`);
    }
  }
  const forwarder = read("pg-forward.mjs");
  for (const bad of [forwarder.replace('"127.0.0.1"', '"0.0.0.0"'), forwarder.replace('server.listen(5432, "127.0.0.1");', "server.listen(5432);"), `${forwarder}\nnet.createServer().listen(8080, "127.0.0.1");`]) {
    assert.notDeepEqual(violations("demo.yaml", workloads()["demo.yaml"], bad), []);
  }
});

test("dependent native sidecars gate schema wait and gateway startup on loopback listeners", () => {
  const pod = load("demo.yaml").spec.template.spec;
  for (const [name, port] of [["pg-forward", 5432], ["demo-issuer", 18084]]) {
    const index = pod.initContainers.findIndex(c => c.name === name);
    const sidecar = pod.initContainers[index];
    assert.equal(sidecar.restartPolicy, "Always", name);
    assert.ok(index < pod.initContainers.findIndex(c => c.name === "wait-for-schema"), name);
    const probe = sidecar.startupProbe;
    assert.ok(probe?.exec, `${name}: an in-container startup probe gates subsequent containers`);
    assert.equal(probe.tcpSocket, undefined, "kubelet TCP probes target the Pod IP, not pod loopback");
    assert.equal(probe.httpGet, undefined);
    assert.deepEqual(probe.exec.command, ["node", "/scripts/loopback-probe.mjs", String(port)]);
    assert.ok(probe.timeoutSeconds > 0 && probe.failureThreshold > 0 && probe.periodSeconds > 0);
  }
});


test("the shared sidecar probe succeeds on loopback and fails after the listener closes", async () => {
  const server = net.createServer(socket => socket.end());
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = server.address().port;
  try {
    assert.equal(await probeLoopback(port), true);
  } finally {
    await new Promise(resolve => server.close(resolve));
  }
  assert.equal(await probeLoopback(port), false);
});
