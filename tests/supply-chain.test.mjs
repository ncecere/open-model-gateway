// Supply-chain pins (#143, SECURITY.md): every GitHub Action is pinned to a full
// commit SHA with a version comment, and every Dockerfile base image to a digest.
//
// Write tokens versus third-party code (#194): a job whose GITHUB_TOKEN can write
// (packages, contents, id-token, security-events, ...) may only use the few
// actions below. Scanners, linters and setup actions run in read-only jobs. This
// is the lesson of the 2026 LiteLLM compromises: a scanner action that ran next to
// a publish credential was the way the credential was stolen.
import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const root = fileURLToPath(new URL("..", import.meta.url));
const workflows = join(root, ".github/workflows");

// Actions allowed in a job that has any `write` permission, explicit or inherited
// from the workflow level (or from the repository default when neither declares
// permissions). Keep this minimal: every entry must be used by a write job (checked
// below), and each one is either GitHub-owned or the registry/build plumbing the
// image pipeline cannot do without. Tools that only need to read (Trivy, helm,
// kubeconform, cargo-deny, Node, Rust, ...) never belong here.
export const WRITE_JOB_ACTIONS = new Map([
  // GitHub-owned. Fetches the sources to build or the release script to run.
  ["actions/checkout", "GitHub-owned; no network access beyond the repository"],
  // Docker-owned. The three steps that build and push an image to ghcr.io; the
  // pipeline cannot push without them. login-action only writes the Docker config.
  ["docker/setup-buildx-action", "Docker-owned; creates the BuildKit builder for build-push and imagetools"],
  ["docker/login-action", "Docker-owned; logs in to ghcr.io with the job token"],
  ["docker/build-push-action", "Docker-owned; builds and pushes the image with its SBOM and provenance"],
  // GitHub-owned. Uploading code-scanning results needs `security-events: write`;
  // the analysis itself cannot run without the same action's init step.
  ["github/codeql-action/init", "GitHub-owned; CodeQL setup (job needs security-events: write to upload)"],
  ["github/codeql-action/analyze", "GitHub-owned; CodeQL analysis and SARIF upload"],
]);

// Actions that download a tool at run time without a checksum that this repository
// pins. They are not used anywhere: install the tool with a pinned version and a
// SHA-256 check instead (the helm and kubeconform steps in ci.yml, the Trivy and
// cosign steps in image.yml, .github/actions/setup-node).
const UNVERIFIED_INSTALLERS = new Map([
  ["actions/setup-node", "downloads a floating Node release without a checksum; use ./.github/actions/setup-node"],
  ["azure/setup-helm", "does not verify the helm download; use the checksum-verified step in ci.yml"],
  ["aquasecurity/trivy-action", "installs Trivy through setup-trivy, whose checksum comes from the same release; install the pinned binary"],
  ["aquasecurity/setup-trivy", "the checksum comes from the same release; install the pinned binary"],
  ["sigstore/cosign-installer", "install the pinned cosign release with its SHA-256 instead"],
]);

// ---- a small YAML reader for the workflow files (no YAML dependency in the repo) ----

function stripComment(s) {
  let quote = null;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (quote) {
      if (c === quote) quote = null;
    } else if ((c === '"' || c === "'") && (i === 0 || /[\s[{,(]/.test(s[i - 1]))) {
      quote = c;
    } else if (c === "#" && (i === 0 || /\s/.test(s[i - 1]))) {
      return s.slice(0, i);
    }
  }
  return s;
}

const isDash = content => content === "-" || content.startsWith("- ");

function logicalLines(text) {
  const raw = text.split("\n").map(l => l.replace(/\r$/, ""));
  const out = [];
  for (let i = 0; i < raw.length; i++) {
    const line = raw[i];
    if (/^\s*(#.*)?$/.test(line)) continue;
    const indent = line.match(/^ */)[0].length;
    const content = stripComment(line.slice(indent)).trimEnd();
    const entry = { indent, content, no: i + 1 };
    out.push(entry);
    if (/(^|[:-]\s+)[|>][+-]?\d?$/.test(content)) {
      // Block scalar: the key's column decides where the body ends.
      let eff = indent;
      let rest = content;
      while (rest.startsWith("- ")) {
        const t = rest.slice(2);
        eff += 2 + (t.length - t.trimStart().length);
        rest = t.trimStart();
      }
      const body = [];
      while (i + 1 < raw.length && (/^\s*$/.test(raw[i + 1]) || raw[i + 1].match(/^ */)[0].length > eff)) {
        body.push(raw[++i]);
      }
      while (body.length && /^\s*$/.test(body[body.length - 1])) body.pop();
      const min = Math.min(...body.filter(l => l.trim()).map(l => l.match(/^ */)[0].length), Infinity);
      entry.block = body.map(l => l.slice(Math.min(min, l.length))).join("\n");
    }
  }
  return out;
}

function splitKey(s) {
  const m = s.match(/^("[^"]*"|'[^']*'|[^\s:"'[{-][^:]*?)\s*:(?:\s+(.*))?$/);
  if (!m) return null;
  const key = m[1].replace(/^(["'])(.*)\1$/, "$2");
  return [key, (m[2] ?? "").trim()];
}

function parseScalar(v) {
  if (/^".*"$/.test(v) || /^'.*'$/.test(v)) return v.slice(1, -1);
  if (/^\[.*\]$/.test(v)) {
    const inner = v.slice(1, -1).trim();
    return inner ? inner.split(",").map(x => parseScalar(x.trim())) : [];
  }
  if (/^\{.*\}$/.test(v)) {
    const obj = {};
    const inner = v.slice(1, -1).trim();
    for (const part of inner ? inner.split(",") : []) {
      const kv = splitKey(part.trim());
      if (kv) obj[kv[0]] = parseScalar(kv[1]);
    }
    return obj;
  }
  return v;
}

function parseBlock(lines, st, indent) {
  return isDash(lines[st.i].content) ? parseList(lines, st, indent) : parseMap(lines, st, indent);
}

function parseList(lines, st, indent) {
  const arr = [];
  while (st.i < lines.length && lines[st.i].indent === indent && isDash(lines[st.i].content)) {
    const ln = lines[st.i];
    const rest = ln.content === "-" ? "" : ln.content.slice(2).trimStart();
    const col = indent + (ln.content.length - rest.length);
    if (rest === "") {
      st.i++;
      const nxt = lines[st.i];
      arr.push(nxt && nxt.indent > indent ? parseBlock(lines, st, nxt.indent) : null);
    } else if (ln.block !== undefined && /^[|>]/.test(rest)) {
      arr.push(ln.block);
      st.i++;
    } else if (splitKey(rest) && !/^[[{]/.test(rest)) {
      lines[st.i] = { ...ln, indent: col, content: rest };
      arr.push(parseMap(lines, st, col));
    } else {
      arr.push(parseScalar(rest));
      st.i++;
    }
  }
  return arr;
}

function parseMap(lines, st, indent) {
  const obj = {};
  while (st.i < lines.length && lines[st.i].indent === indent && !isDash(lines[st.i].content)) {
    const ln = lines[st.i];
    const kv = splitKey(ln.content);
    if (!kv) throw new Error(`workflow YAML line ${ln.no}: cannot read '${ln.content}'`);
    const [key, val] = kv;
    st.i++;
    if (ln.block !== undefined && /^[|>]/.test(val)) {
      obj[key] = ln.block;
    } else if (val === "") {
      const nxt = lines[st.i];
      obj[key] = nxt && (nxt.indent > indent || (nxt.indent === indent && isDash(nxt.content)))
        ? parseBlock(lines, st, nxt.indent)
        : null;
    } else {
      obj[key] = parseScalar(val);
    }
  }
  return obj;
}

export function parseYaml(text) {
  const lines = logicalLines(text);
  if (!lines.length) return {};
  return parseBlock(lines, { i: 0 }, lines[0].indent);
}

// ---- analysis ----

// A job's token permissions: its own block, else the workflow's, else the repository
// default, which can be read/write, so treat it as write.
export function effectivePermissions(doc, job) {
  if (job.permissions !== undefined) return job.permissions;
  if (doc.permissions !== undefined) return doc.permissions;
  return "write-all";
}

export function hasWrite(perms) {
  if (perms === null || perms === undefined) return false;
  if (typeof perms === "string") return perms !== "read-all";
  return Object.values(perms).some(v => v === "write");
}

const actionName = ref => ref.split("@")[0].toLowerCase();

// Everything a job runs from outside its own steps: the reusable workflow it calls,
// and each step's action.
export function jobUses(job) {
  const uses = [];
  if (typeof job.uses === "string") uses.push({ ref: job.uses, where: "job" });
  (Array.isArray(job.steps) ? job.steps : []).forEach((step, i) => {
    if (step && typeof step.uses === "string") uses.push({ ref: step.uses, where: `step ${i + 1}` });
  });
  return uses;
}

const isLocalWorkflow = ref => ref.startsWith("./.github/workflows/");

// Violations in one workflow document: write jobs using actions off the allowlist.
// A local reusable workflow call is fine: its own jobs are checked on their own and
// can only narrow the permissions the caller grants.
export function writeJobViolations(doc, file, allowed = WRITE_JOB_ACTIONS) {
  const violations = [];
  for (const [name, job] of Object.entries(doc.jobs ?? {})) {
    if (!hasWrite(effectivePermissions(doc, job))) continue;
    for (const { ref, where } of jobUses(job)) {
      if (where === "job" && isLocalWorkflow(ref)) continue;
      if (allowed.has(actionName(ref))) continue;
      violations.push(`${file}: job '${name}' has a write permission and uses ${ref} (${where})`);
    }
  }
  return violations;
}

function loadWorkflows() {
  return readdirSync(workflows)
    .filter(f => /\.ya?ml$/.test(f))
    .map(file => ({ file, doc: parseYaml(readFileSync(join(workflows, file), "utf8")) }));
}

// Every run: script and composite action step in the repository's workflows.
function allSteps() {
  const out = [];
  for (const { file, doc } of loadWorkflows()) {
    for (const [name, job] of Object.entries(doc.jobs ?? {})) {
      (Array.isArray(job.steps) ? job.steps : []).forEach((step, i) => out.push({ where: `${file} job '${name}' step ${i + 1}`, step }));
    }
  }
  const actionsDir = join(root, ".github/actions");
  if (existsSync(actionsDir)) {
    for (const dir of readdirSync(actionsDir)) {
      const file = join(actionsDir, dir, "action.yml");
      if (!existsSync(file)) continue;
      const doc = parseYaml(readFileSync(file, "utf8"));
      (doc.runs?.steps ?? []).forEach((step, i) => out.push({ where: `.github/actions/${dir} step ${i + 1}`, step }));
    }
  }
  return out;
}

test("every workflow action is pinned to a commit SHA with a version comment", () => {
  const unpinned = [];
  for (const file of readdirSync(workflows).filter(f => /\.ya?ml$/.test(f))) {
    readFileSync(join(workflows, file), "utf8").split("\n").forEach((line, i) => {
      const m = line.match(/^\s*(?:-\s*)?uses:\s*(\S+)(.*)$/);
      if (!m || m[1].startsWith("./")) return; // local reusable workflows
      if (!/@[0-9a-f]{40}$/.test(m[1]) || !/#\s*\S+/.test(m[2])) unpinned.push(`${file}:${i + 1} ${m[1]}`);
    });
  }
  assert.deepEqual(unpinned, []);
});

test("every Dockerfile base image is pinned by digest", () => {
  const unpinned = [];
  for (const file of ["Dockerfile", "deploy/loadtest/Dockerfile"]) {
    readFileSync(join(root, file), "utf8").split("\n").forEach((line, i) => {
      const m = line.match(/^FROM\s+(\S+)/i);
      if (m && !/@sha256:[0-9a-f]{64}$/.test(m[1])) unpinned.push(`${file}:${i + 1} ${m[1]}`);
    });
  }
  assert.deepEqual(unpinned, []);
});

test("jobs with a write permission use only allowlisted actions (#194)", () => {
  const violations = loadWorkflows().flatMap(({ file, doc }) => writeJobViolations(doc, file));
  assert.deepEqual(violations, []);
});

test("every allowlisted write-job action is used by a write job", () => {
  const used = new Set();
  for (const { doc } of loadWorkflows()) {
    for (const job of Object.values(doc.jobs ?? {})) {
      if (!hasWrite(effectivePermissions(doc, job))) continue;
      for (const { ref } of jobUses(job)) used.add(actionName(ref));
    }
  }
  const stale = [...WRITE_JOB_ACTIONS.keys()].filter(a => !used.has(a));
  assert.deepEqual(stale, [], "remove allowlist entries no write job uses");
});

test("no write job runs a local composite action (its steps would not be checked)", () => {
  const found = [];
  for (const { file, doc } of loadWorkflows()) {
    for (const [name, job] of Object.entries(doc.jobs ?? {})) {
      if (!hasWrite(effectivePermissions(doc, job))) continue;
      for (const { ref } of jobUses(job)) if (ref.startsWith("./") && !isLocalWorkflow(ref)) found.push(`${file}: ${name} ${ref}`);
    }
  }
  assert.deepEqual(found, []);
});

test("the write-permission check reads inherited, overridden and default permissions", () => {
  const act = "uses: example/third-party@" + "0".repeat(40);
  const check = yaml => writeJobViolations(parseYaml(yaml), "fixture.yml");
  const wf = (top, jobPerms) => `
${top}
jobs:
  job:
    runs-on: ubuntu-24.04
${jobPerms}
    steps:
      - uses: actions/checkout@${"1".repeat(40)} # v1
      - ${act} # v1
`;
  // inherited from the workflow level
  assert.equal(check(wf("permissions:\n  contents: read\n  packages: write", "")).length, 1);
  assert.equal(check(wf("permissions: write-all", "")).length, 1);
  // no permissions anywhere: the repository default may write
  assert.equal(check(wf("", "")).length, 1);
  // a job-level block replaces the workflow's
  assert.equal(check(wf("permissions: write-all", "    permissions:\n      contents: read")).length, 0);
  assert.equal(check(wf("permissions:\n  contents: read", "    permissions:\n      id-token: write")).length, 1);
  assert.equal(check(wf("permissions: read-all", "")).length, 0);
  assert.equal(check(wf("permissions: {}", "")).length, 0);
  assert.equal(check(wf("permissions:\n  contents: read", "    permissions: { contents: read, packages: write }")).length, 1);
  // read-only jobs may use any pinned action; allowlisted actions pass in write jobs
  assert.equal(check(wf("permissions:\n  contents: read", "    permissions:\n      contents: read\n      packages: read")).length, 0);
  assert.equal(check(`
permissions: write-all
jobs:
  job:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@${"1".repeat(40)} # v1
      - uses: docker/login-action@${"2".repeat(40)} # v1
`).length, 0);
  // a write job may call a local reusable workflow but not a remote one
  assert.equal(check(`
permissions:
  contents: read
jobs:
  local:
    permissions:
      packages: write
    uses: ./.github/workflows/image.yml
  remote:
    permissions:
      packages: write
    uses: example/repo/.github/workflows/x.yml@${"3".repeat(40)}
`).length, 1);
});

test("the YAML reader handles the workflow constructs the checks rely on", () => {
  const doc = parseYaml(`
name: x
on:
  push:
    branches: ['**']
permissions:
  contents: read
jobs:
  a:
    permissions: { contents: read, packages: write }
    strategy:
      matrix:
        include:
          - platform: linux/amd64
            runner: ubuntu-24.04
    steps:
      - name: one
        run: |
          echo "- not: a key" # kept
          curl https://example.org
      - uses: foo/bar@${"4".repeat(40)} # v1.2.3
        with:
          x: >-
            folded
            text
      - run: echo done
`);
  assert.deepEqual(doc.jobs.a.permissions, { contents: "read", packages: "write" });
  assert.equal(doc.jobs.a.strategy.matrix.include[0].runner, "ubuntu-24.04");
  assert.match(doc.jobs.a.steps[0].run, /curl https:\/\/example.org/);
  assert.equal(doc.jobs.a.steps[1].uses, `foo/bar@${"4".repeat(40)}`);
  assert.equal(doc.jobs.a.steps[2].run, "echo done");
});

test("no workflow uses an installer that downloads a tool without a pinned checksum", () => {
  const found = [];
  for (const { file, doc } of loadWorkflows()) {
    for (const [name, job] of Object.entries(doc.jobs ?? {})) {
      for (const { ref } of jobUses(job)) {
        const why = UNVERIFIED_INSTALLERS.get(actionName(ref));
        if (why) found.push(`${file}: job '${name}' uses ${ref}: ${why}`);
      }
    }
  }
  assert.deepEqual(found, []);
});

test("every run-time download is checksum-verified and nothing is piped into a shell", () => {
  const found = [];
  for (const { where, step } of allSteps()) {
    const script = typeof step?.run === "string" ? step.run : "";
    if (!/\b(curl|wget)\b/.test(script)) continue;
    if (/\|\s*(sudo\s+)?(ba|z|da)?sh\b/.test(script)) found.push(`${where}: pipes a download into a shell`);
    const urls = script.match(/https?:\/\/[^\s"')]+/g) ?? [];
    const external = urls.filter(u => !/^https?:\/\/(localhost|127\.0\.0\.1)[:/]/.test(u));
    if (external.length && !/sha256sum\s+-c/.test(script)) found.push(`${where}: downloads ${external[0]} without 'sha256sum -c'`);
  }
  assert.deepEqual(found, []);
});

test("taiki-e/install-action installs exact versions with its checksum check on", () => {
  const found = [];
  for (const { file, doc } of loadWorkflows()) {
    for (const [name, job] of Object.entries(doc.jobs ?? {})) {
      for (const step of Array.isArray(job.steps) ? job.steps : []) {
        if (!step?.uses || actionName(step.uses) !== "taiki-e/install-action") continue;
        const tools = String(step.with?.tool ?? "").split(",").map(t => t.trim()).filter(Boolean);
        if (!tools.length || tools.some(t => !/^[\w.-]+@\d+\.\d+\.\d+$/.test(t))) found.push(`${file}: job '${name}' tool '${step.with?.tool}' is not name@x.y.z`);
        if (String(step.with?.checksum ?? "true") !== "true") found.push(`${file}: job '${name}' disables the checksum`);
      }
    }
  }
  assert.deepEqual(found, []);
});
