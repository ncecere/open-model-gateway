// The Spark test-suite runner's manifests (deploy/spark/test-*.yaml, scripts/spark-test.py): the
// pod that builds and tests a pushed commit. Nothing else reads these files, so this does: every
// image is pinned by digest and is the Dockerfile's, the pod is locked down and exposes nothing,
// the script installs and downloads nothing of its own, and two runs fit the shared quota next to
// one demo environment.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { APPROVED_RUST, parseStrict, root, toData } from "./lib.mjs";

const read = name => readFileSync(join(root, "deploy/spark", name), "utf8");
const load = name => toData(parseStrict(read(name), name));
const dockerfile = readFileSync(join(root, "Dockerfile"), "utf8");
const dockerfileNode = dockerfile.match(/^FROM (mirror\.gcr\.io\/library\/node:\S+@sha256:[0-9a-f]{64}) AS web$/m)[1];
const job = () => load("test-job.yaml");
const podOf = doc => doc.spec.template.spec;
const containers = doc => [...(podOf(doc).initContainers ?? []), ...podOf(doc).containers];
const cpu = value => (String(value).endsWith("m") ? Number.parseInt(value, 10) : Number.parseFloat(value) * 1000);
const mib = value => {
  const m = String(value).match(/^(\d+)(Mi|Gi)$/);
  assert.ok(m, `memory ${value}`);
  return Number(m[1]) * (m[2] === "Gi" ? 1024 : 1);
};

// Every violation of the manifest rules, as text.
function violations(doc) {
  const found = [];
  const pod = podOf(doc);
  for (const key of ["hostNetwork", "hostPID", "hostIPC"]) if (pod[key]) found.push(key);
  if (pod.volumes?.some(v => v.hostPath)) found.push("a hostPath volume");
  if (pod.automountServiceAccountToken !== false) found.push("the service account token is mounted");
  for (const c of containers(doc)) {
    const where = c.name;
    if (!/^[^\s@]+@sha256:[0-9a-f]{64}$/.test(c.image)) found.push(`${where}: image ${c.image} is not pinned by digest`);
    const effective = key => c.securityContext?.[key] ?? pod.securityContext?.[key];
    if (effective("runAsNonRoot") !== true) found.push(`${where}: runAsNonRoot`);
    if (!(effective("runAsUser") > 0)) found.push(`${where}: runAsUser must be a non-root uid`);
    if (c.securityContext?.allowPrivilegeEscalation !== false) found.push(`${where}: allowPrivilegeEscalation`);
    if (JSON.stringify(c.securityContext?.capabilities?.drop) !== '["ALL"]' || c.securityContext?.capabilities?.add) found.push(`${where}: capabilities`);
    if (c.securityContext?.readOnlyRootFilesystem !== true) found.push(`${where}: readOnlyRootFilesystem`);
    if (effective("seccompProfile")?.type !== "RuntimeDefault") found.push(`${where}: seccompProfile`);
    if (c.securityContext?.privileged || c.securityContext?.procMount) found.push(`${where}: privileged`);
    if (c.ports?.length) found.push(`${where}: ports`);
    if (!c.resources?.limits?.cpu || !c.resources?.limits?.memory || !c.resources?.requests?.cpu || !c.resources?.requests?.memory) found.push(`${where}: resources`);
  }
  return found;
}

test("the test pod meets the image and hardening rules", () => {
  assert.deepEqual(violations(job()), []);
});

test("the toolchain images are the Dockerfile's", () => {
  const tests = podOf(job()).containers.find(c => c.name === "tests");
  const node = podOf(job()).initContainers.find(c => c.name === "node");
  assert.equal(tests.image, APPROVED_RUST, "the Rust image is the Dockerfile's build image");
  assert.equal(node.image, dockerfileNode, "the Node image is the Dockerfile's");
  // A line comparison, not a pattern built from the reference: no image reference becomes a regular expression.
  assert.ok(dockerfile.split("\n").includes(`FROM ${APPROVED_RUST} AS build`), "the Dockerfile's build stage is the approved Rust image");
  const toolchain = readFileSync(join(root, "rust-toolchain.toml"), "utf8").match(/^channel = "([^"]+)"$/m)[1];
  assert.ok(tests.image.includes(`rust:${toolchain}-`), `the image carries Rust ${toolchain}, as rust-toolchain.toml says`);
});

test("the database image is the PostgreSQL 17 image of deploy/spark/values.yaml", () => {
  const cluster = load("test-cluster.yaml");
  assert.equal(cluster.spec.imageName, load("values.yaml").cnpg.cluster.image);
  assert.match(cluster.spec.imageName, /^ghcr\.io\/cloudnative-pg\/postgresql:17\.\d+@sha256:[0-9a-f]{64}$/);
  assert.equal(cluster.spec.instances, 1);
  assert.equal(cluster.spec.storage.storageClass, "local-path");
});

test("the database password is a mounted file, never an environment variable", () => {
  const doc = job();
  const tests = podOf(doc).containers.find(c => c.name === "tests");
  assert.ok(!(tests.env ?? []).some(e => e.valueFrom || /PASS|SECRET|TOKEN/i.test(e.name)), "no secret in the environment");
  assert.deepEqual(podOf(doc).volumes.find(v => v.name === "pg").secret.items, [{ key: "password", path: "password" }]);
  assert.ok(tests.volumeMounts.some(m => m.name === "pg" && m.mountPath === "/secrets" && m.readOnly === true));
  assert.equal(load("test-cache.yaml").spec.resources.requests.storage, "20Gi");
  assert.equal(load("test-cluster.yaml").spec.postgresql, undefined, "PostgreSQL keeps its defaults (max_connections 100), as CI's service container");
});

test("nothing in the test manifests exposes a port or reaches the host", () => {
  for (const name of ["test-job.yaml", "test-cluster.yaml", "test-cache.yaml"]) {
    const text = read(name);
    assert.doesNotMatch(text, /^kind:\s*(Service|Ingress|NetworkPolicy|Pooler)\b/m, name);
    assert.doesNotMatch(text, /NodePort|LoadBalancer|hostNetwork|hostPort|hostPath|privileged:\s*true/, name);
  }
});

test("the script installs and downloads nothing of its own", () => {
  const script = read("test-run.sh");
  const code = script.split("\n").filter(line => !line.trim().startsWith("#")).join("\n");
  for (const forbidden of [/\bcurl\b/, /\bwget\b/, /\bapt(-get)?\b/, /\bdpkg\b/, /\bpip3?\s+install\b/, /\bcargo\s+install\b/, /\bnpm\s+(install|i)\s+-g\b/, /rustup-init/, /rustup\s+(default|override|self)/, /\bsudo\b/, /set -x/, /\|\s*(ba)?sh\b/]) {
    assert.doesNotMatch(code, forbidden, String(forbidden));
  }
  // The one network install is rustup's, of the channel rust-toolchain.toml names, with that file's components.
  const installs = code.match(/^.*rustup toolchain install.*$/gm);
  assert.equal(installs.length, 1);
  assert.match(installs[0], /"\$channel" --profile minimal --component rustfmt --component clippy --no-self-update/);
  // Only the public repository, over HTTPS, at an exact commit.
  assert.match(read("test-job.yaml"), /OMG_REPO_URL, value: "https:\/\/github\.com\/ncecere\/open-model-gateway\.git"/);
  assert.match(code, /git rev-parse HEAD/);
  assert.match(code, /\( main 2>&1; echo "\$\?" >"\$status_file" \) \| redact &/, "all output passes the redaction");
  assert.match(code, /trap terminate TERM INT/, "bash is PID 1 in the pod: SIGTERM must be trapped for `down` and the deadline to stop it");
  assert.doesNotMatch(code, /PGPASSWORD/, "the database password is a file read inside with_db, not an environment variable");
});

test("two runs fit the shared quota next to one demo environment", () => {
  const quota = { rcpu: 4000, rmem: 16 * 1024, lcpu: 8000, lmem: 24 * 1024, pods: 40, pvcs: 12, storage: 100 * 1024 };
  const resources = c => ({ rcpu: cpu(c.resources.requests.cpu), rmem: mib(c.resources.requests.memory), lcpu: cpu(c.resources.limits.cpu), lmem: mib(c.resources.limits.memory) });
  const add = (a, b) => Object.fromEntries(Object.keys(quota).map(k => [k, (a[k] ?? 0) + (b[k] ?? 0)]));
  const main = podOf(job()).containers.find(c => c.name === "tests");
  const db = load("test-cluster.yaml").spec;
  const dbUse = { ...resources({ resources: db.resources }), pods: 2, pvcs: 1, storage: mib(db.storage.size) }; // the instance and its initdb pod
  const cacheStorage = mib(load("test-cache.yaml").spec.resources.requests.storage);
  const run = add(add({ ...resources(main), pods: 1 }, dbUse), {});
  // The demo: its sidecars and gateway, the Cluster of values.yaml, two volumes.
  const demoPod = podOf(load("demo.yaml"));
  const demoContainers = [...demoPod.initContainers.filter(c => c.restartPolicy === "Always"), ...demoPod.containers];
  const cluster = load("values.yaml").cnpg.cluster;
  const demo = demoContainers.map(resources).reduce((a, b) => add(a, b), {});
  const demoTotal = add(add(demo, resources({ resources: cluster.resources })), { pods: 2, pvcs: 2, storage: mib(cluster.storageSize) + mib(cluster.walStorageSize) });
  const caches = { pvcs: 2, storage: 2 * cacheStorage };
  const total = add(add(add(run, run), demoTotal), caches);
  for (const [key, limit] of Object.entries(quota)) assert.ok(total[key] <= limit, `two runs and a demo use ${total[key]} of ${limit} ${key}`);
  // A third run does not fit: the cap of two slots is the quota's, not a preference.
  const three = add(add(add(run, run), run), demoTotal);
  assert.ok(["rcpu", "lcpu", "lmem", "rmem"].some(k => three[k] > quota[k]), "three runs and a demo exceed some CPU or memory quota");
});

test("fixtures: every rule is enforced", () => {
  const mutations = [
    ["a floating image", d => { podOf(d).containers[0].image = "mirror.gcr.io/library/rust:1.99.1-bookworm"; }],
    ["root", d => { podOf(d).securityContext.runAsNonRoot = false; }],
    ["uid 0", d => { podOf(d).securityContext.runAsUser = 0; }],
    ["privilege escalation", d => { podOf(d).containers[0].securityContext.allowPrivilegeEscalation = true; }],
    ["capabilities added", d => { podOf(d).containers[0].securityContext.capabilities = { drop: ["ALL"], add: ["NET_ADMIN"] }; }],
    ["a writable root", d => { podOf(d).containers[0].securityContext.readOnlyRootFilesystem = false; }],
    ["no seccomp", d => { delete podOf(d).securityContext.seccompProfile; }],
    ["host networking", d => { podOf(d).hostNetwork = true; }],
    ["a host path", d => { podOf(d).volumes.push({ name: "h", hostPath: { path: "/" } }); }],
    ["a privileged container", d => { podOf(d).containers[0].securityContext.privileged = true; }],
    ["a port", d => { podOf(d).containers[0].ports = [{ containerPort: 80 }]; }],
    ["a mounted token", d => { delete podOf(d).automountServiceAccountToken; }],
    ["no memory limit", d => { delete podOf(d).containers[0].resources.limits.memory; }],
  ];
  for (const [label, mutate] of mutations) {
    const doc = structuredClone(job());
    mutate(doc);
    assert.notDeepEqual(violations(doc), [], label);
  }
});
