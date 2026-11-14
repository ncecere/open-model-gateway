// Shared readers for the supply-chain tests (tests/supply-chain.test.mjs and
// tests/supply-chain/*.test.mjs): the strict YAML reader, the repository's workflows and
// local actions, the Dockerfile walker and the approved image references.
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { isMap, parseDocument } from "yaml";

export const root = fileURLToPath(new URL("../..", import.meta.url));
export const workflowsDir = join(root, ".github/workflows");
export const actionsDir = join(root, ".github/actions");

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

export const walk = dir => readdirSync(dir, { withFileTypes: true }).flatMap(e => (e.isDirectory() ? walk(join(dir, e.name)) : [join(dir, e.name)]));

export function repoFiles() {
  const rel = p => p.slice(root.length);
  const workflows = readdirSync(workflowsDir).filter(f => /\.ya?ml$/.test(f)).map(f => join(workflowsDir, f));
  const actions = existsSync(actionsDir) ? walk(actionsDir).filter(f => /(^|\/)action\.ya?ml$/.test(f)) : [];
  return [...workflows, ...actions].map(path => ({ file: rel(path), text: readFileSync(path, "utf8") }));
}

export function readLocalFromRepo(path) {
  const full = join(root, path);
  return existsSync(full) ? readFileSync(full, "utf8") : null;
}


export const stepsOf = job => (Array.isArray(job?.steps) ? job.steps.filter(s => s && typeof s === "object") : []);
export const jobsOf = data => Object.entries(data?.jobs && typeof data.jobs === "object" ? data.jobs : {});
export const actionName = ref => String(ref).split("@")[0].toLowerCase();


export const RUNTIME_DOCKERFILES = ["Dockerfile", "deploy/loadtest/Dockerfile"];
export const APPROVED_RUNTIME = "gcr.io/distroless/cc-debian13:nonroot@sha256:cc66d222f0a7ec996b78a99fe76a969bc80b539a5016ad866c14d1949bc6d0ab";


// Every Dockerfile anywhere in the repository (not a fixed list): a file named
// `Dockerfile*`, `*.dockerfile` or `Containerfile*`, in any case (#488), the names Docker,
// BuildKit and Podman look for or are commonly given. It skips the directories that are
// not source: `.git` (Git's own objects),
// `node_modules` (installed npm packages), `target*` (Cargo's build output, also
// `target-<name>` directories of parallel builds), `dist` (the built SPA) and `.turbo`
// (a build cache). Nothing may build from them: a build step or Compose `build:` whose
// context or Dockerfile is in one of them is refused (#488, below), because the checks
// here never read what is there.
export const DOCKERFILE_SKIP_DIRS = /^(\.git|node_modules|target.*|dist|\.turbo)$/;
// A `.dockerignore` named after its Dockerfile (`Dockerfile.dockerignore`) is not one.
export const DOCKERFILE_NAME = /^(dockerfile.*|.*\.dockerfile|containerfile.*)$/i;
export const isDockerfileName = name => DOCKERFILE_NAME.test(name) && !/\.dockerignore$/i.test(name);
export function findDockerfiles(dir = root) {
  const out = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    if (e.isDirectory()) {
      if (DOCKERFILE_SKIP_DIRS.test(e.name)) continue;
      out.push(...findDockerfiles(join(dir, e.name)));
    } else if (isDockerfileName(e.name)) {
      out.push(join(dir, e.name));
    }
  }
  return out;
}
// Each file the walker finds must be a known Dockerfile; anything else fails here by name,
// before a reader error about its contents (#488).
export function unknownDockerfileViolations(files) {
  return files.filter(file => !KNOWN_DOCKERFILES.includes(file)).map(file => `${file} is named like a Dockerfile but is not one of the known Dockerfiles (${KNOWN_DOCKERFILES.join(", ")}): `
    + "if it is built, add it to KNOWN_DOCKERFILES with its build in IMAGE_BUILDS; otherwise rename it");
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


export const APPROVED_RUST = "mirror.gcr.io/library/rust:1.99.1-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c";

export const SHA = n => String(n).repeat(40);
export const wf = body => `name: fixture\non: push\n${body}\n`;

export const loadWorkflow = name => toData(parseStrict(readFileSync(join(workflowsDir, `${name}.yml`), "utf8"), `.github/workflows/${name}.yml`));
export const KNOWN_DOCKERFILES = RUNTIME_DOCKERFILES;
export const isMapping = v => v !== null && typeof v === "object" && !Array.isArray(v);
// A digest-pinned base image that is not an approved one, for fixtures.
export const OTHER_BASE = `mirror.gcr.io/library/debian:13-slim@sha256:${"b".repeat(64)}`;

// ---- permissions ----

// What a job's GITHUB_TOKEN may do. Without a permissions block anywhere the repository
// default applies, which these checks take as write-all (docs/verification.md).
export const DEFAULT_PERMISSIONS = "write-all";

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
