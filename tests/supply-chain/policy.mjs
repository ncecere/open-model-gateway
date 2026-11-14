// The checks of #493 and #488: container images, image builds, the pinned image jobs, the
// run steps of other jobs, Compose files and actions. The tests are in
// tests/supply-chain/*.test.mjs; the shared readers in lib.mjs.
//
// What these checks are: they make a change to what CI builds, and to the jobs that build,
// publish or test an image, a visible change in review, and they catch accidental drift. They
// are not a boundary against a deliberately malicious pull request, which controls the
// workflows and these tests; mandatory review of workflow diffs and of pinned-jobs.yaml is.
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join, posix } from "node:path";
import {
  DOCKERFILE_SKIP_DIRS, KNOWN_DOCKERFILES, actionName, jobPermissions, actionsDir, isMapping, jobsOf, loadWorkflow, parseStrict, readLocalFromRepo, root, stepsOf, toData, workflowsDir,
} from "./lib.mjs";

// ---- job and service containers (#493) ----

// Every image a job runs in (`container:`, a string or a mapping's `image:`) or beside
// (`services.<name>`, a string or a mapping's `image:`) must be pinned by digest:
// `<image>[:<tag>]@sha256:<64 lower-case hex>`. A value that is not one literal string (an
// expression, a computed services mapping, a missing image) is refused unless listed here as
// `<file>: <where>` with the reason; every entry must be used. None is listed.
export const CONTAINER_IMAGE_EXPRESSIONS = new Map([]);
export const PINNED_IMAGE = /^[^\s@$]+@sha256:[0-9a-f]{64}$/;

// Container `options:` that reach the host or replace what the image runs, refused unless
// listed here as `<file>: <where>: <option>` with the reason. None is listed: today's options
// are the PostgreSQL services' --health-* settings.
export const CONTAINER_OPTIONS_ALLOWED = new Map([]);
const HOST_OPTIONS = /^--?(privileged|cap-add|volume|mount|entrypoint|pull|device|security-opt|volumes-from)(=|$)|^-v$|^-v./;
const HOST_NAMESPACES = /^--(network|net|pid|ipc|userns|uts|cgroupns)(=|$)/;

export function containerOptionViolations(key, container) {
  const out = [];
  if (container.credentials !== undefined) out.push(`${key}: credentials is not allowed on a job or service container (a registry login next to the job)`);
  if (container.options === undefined) return out;
  if (typeof container.options !== "string" || container.options.includes("${{")) return [...out, `${key}: options must be one literal string`];
  const words = container.options.trim().split(/\s+/);
  words.forEach((word, i) => {
    const value = word.includes("=") ? word.split("=")[1] : words[i + 1];
    const refused = HOST_OPTIONS.test(word) || (HOST_NAMESPACES.test(word) && /^(host|container:)/.test(value ?? ""));
    if (refused && !CONTAINER_OPTIONS_ALLOWED.has(`${key}: ${word}`)) out.push(`${key}: option '${word}' is not allowed on a job or service container (it reaches the host or replaces what the image runs)`);
  });
  return out;
}

export function containerPinViolations(file, data, allowed = CONTAINER_IMAGE_EXPRESSIONS, used = new Set()) {
  const out = [];
  const check = (where, image) => {
    const key = `${file}: ${where}`;
    if (typeof image === "string" && image.includes("${{")) {
      if (allowed.has(key)) { used.add(key); return; }
      out.push(`${key}: an expression ('${image}') cannot be checked; write the digest-pinned image or allowlist it with a reason`);
    } else if (typeof image !== "string") {
      out.push(`${key}: the image must be one literal string`);
    } else if (!PINNED_IMAGE.test(image)) {
      out.push(`${key}: ${image} is not pinned by digest (<image>[:<tag>]@sha256:<64 hex>)`);
    }
  };
  for (const [name, job] of jobsOf(data)) {
    if (job?.container !== undefined) {
      if (isMapping(job.container)) {
        check(`job '${name}' container.image`, job.container.image);
        out.push(...containerOptionViolations(`${file}: job '${name}' container`, job.container));
      } else check(`job '${name}' container`, job.container);
    }
    if (job?.services === undefined) continue;
    if (!isMapping(job.services)) { check(`job '${name}' services`, job.services); continue; }
    for (const [service, value] of Object.entries(job.services)) {
      if (!isMapping(value)) { check(`job '${name}' services.${service}`, value); continue; }
      check(`job '${name}' services.${service}.image`, value.image);
      out.push(...containerOptionViolations(`${file}: job '${name}' services.${service}`, value));
    }
  }
  return out;
}

// ---- what an image build step builds (#488) ----

// An image is built only by a docker/build-push-action step in a pinned job (below) or a
// Compose `build:` section. On every build step:
// - `file` (default `<context>/Dockerfile`) is one of KNOWN_DOCKERFILES;
// - `context` is a literal directory of the checkout outside the skipped directories
//   (without one the action builds the Git context, which no check reads);
// - no `target` (the last stage, the runtime image, is built), no `build-contexts`
//   (`<name>=docker-image://<image>` replaces a stage or base image) and no `BUILDKIT_*`
//   build argument (`BUILDKIT_SYNTAX` loads another Dockerfile frontend);
// - an expression in any input but `labels` is one of BUILD_INPUT_EXPRESSIONS. `env.*`,
//   `steps.*`, `needs.*` and `inputs.*` are refused: an earlier step can write env through
//   $GITHUB_ENV (a cache scope of `x,url=https://...` redirects the cache), and the others
//   come from outside the step. `labels` are image metadata; their expressions (the
//   repository, the source commit, the read-only meta job's labels) are pinned as text by
//   the job pin.
export const BUILD_INPUT_EXPRESSIONS = [
  "matrix.<key>, a key of every literal strategy.matrix.include leg of the same pinned job",
  "runner.temp",
];
// docker/setup-buildx-action takes exactly the pinned BuildKit image with the default
// docker-container driver: no `driver`, `endpoint`, `config`, inline BuildKit
// configuration (registry mirrors), `use` or `version`.
export const BUILDKIT_IMAGE = "mirror.gcr.io/moby/buildkit:buildx-stable-1@sha256:cec9f139f45e93c5c69c60f8b07cfad9f43f4ef6b6a6cd917527fea5ff2e3dea";
export const BUILDX_INPUTS = { "driver-opts": `image=${BUILDKIT_IMAGE}` };

// A build context or Dockerfile path relative to `base` (a directory of the checkout): the
// normalized path, or the reason it is refused.
export function buildPath(value, base = ".") {
  if (typeof value !== "string" || value === "") return { refused: "must be one literal, non-empty path" };
  if (value.includes("${")) return { refused: `'${value}' is an expression or variable, which cannot be checked` };
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(value) || /^git@/i.test(value) || value.includes("#")) return { refused: `'${value}' is a remote context, which the checks never read` };
  if (value.startsWith("/") || value.includes("\\")) return { refused: `'${value}' must be a relative path in the checkout` };
  const path = posix.normalize(posix.join(base, value)).replace(/\/+$/, "") || ".";
  if (path === ".." || path.startsWith("../")) return { refused: `'${value}' leaves the checkout` };
  const skipped = path.split("/").find(segment => DOCKERFILE_SKIP_DIRS.test(segment));
  if (skipped) return { refused: `'${value}' is in '${skipped}', a directory the Dockerfile walker skips` };
  return { path };
}

const matrixKeys = job => {
  const include = job?.strategy?.matrix?.include;
  if (!Array.isArray(include) || include.length === 0 || !include.every(isMapping)) return new Set();
  return new Set(Object.keys(include[0]).filter(key => include.every(leg => typeof leg[key] === "string")));
};

function buildStepViolations(where, inputs, job) {
  const out = [];
  if (!isMapping(inputs)) return [`${where}: a build step needs a literal with: mapping`];
  if ("build-contexts" in inputs) out.push(`${where}: build-contexts can replace a stage or base image of the Dockerfile; it is not allowed`);
  if ("target" in inputs) out.push(`${where}: target '${inputs.target}' builds another stage than the runtime image; it is not allowed`);
  if ("build-args" in inputs) {
    if (typeof inputs["build-args"] !== "string") out.push(`${where}: build-args must be a literal string`);
    else for (const line of inputs["build-args"].split("\n")) if (/^\s*BUILDKIT_/i.test(line)) out.push(`${where}: build argument '${line.trim()}' changes how BuildKit builds (BUILDKIT_SYNTAX loads another frontend); it is not allowed`);
  }
  const context = buildPath(inputs.context);
  if (context.refused) out.push(`${where}: context ${context.refused}`);
  if ("file" in inputs) {
    const dockerfile = buildPath(inputs.file);
    if (dockerfile.refused) out.push(`${where}: file ${dockerfile.refused}`);
    else if (!KNOWN_DOCKERFILES.includes(dockerfile.path)) out.push(`${where}: file '${inputs.file}' is not one of the known Dockerfiles (${KNOWN_DOCKERFILES.join(", ")})`);
  } else if (context.path && !KNOWN_DOCKERFILES.includes(posix.join(context.path, "Dockerfile"))) {
    out.push(`${where}: the default Dockerfile '${posix.join(context.path, "Dockerfile")}' is not one of the known Dockerfiles`);
  }
  const keys = matrixKeys(job);
  for (const [input, value] of Object.entries(inputs)) {
    if (input === "labels" || typeof value !== "string") continue;
    for (const [, expression] of value.matchAll(/\$\{\{\s*(.*?)\s*\}\}/g)) {
      const matrix = expression.match(/^matrix\.([A-Za-z0-9_-]+)$/);
      if (expression === "runner.temp" || (matrix && keys.has(matrix[1]))) continue;
      out.push(`${where}: ${input} uses \${{ ${expression} }}, which is not a pinned value (only ${BUILD_INPUT_EXPRESSIONS.join("; ")})`);
    }
  }
  return out;
}

// ---- pinned image jobs (#488) ----

// Every job that builds, publishes, signs, attests, imports or tests an image is pinned in
// full in pinned-jobs.yaml: a run step can do anything (check out other code over the
// Dockerfile, put a wrapper first on PATH, write DOCKER_HOST to $GITHUB_ENV, retag the image
// under test after the scan), so the tripwire is that any change to such a job is a visible
// change to that file. `"*"` pins every job of the workflow; the workflow's own keys (name,
// on, permissions, env, concurrency, defaults) are pinned with its jobs.
export const PINNED_WORKFLOWS = new Map([
  [".github/workflows/image.yml", "*"],
  [".github/workflows/branch-images.yml", "*"],
  [".github/workflows/loadtest-image.yml", "*"],
  [".github/workflows/oci-import.yml", "*"],
  // Deletes dev image versions with a packages: write token.
  [".github/workflows/prune-dev-images.yml", "*"],
  [".github/workflows/ci.yml", ["staging-image", "image"]],
]);
export const PINNED_JOBS_FILE = "tests/supply-chain/pinned-jobs.yaml";
const UPDATE = `update the pinned job in ${PINNED_JOBS_FILE}, which a reviewer must read (node tests/supply-chain/update-pinned-jobs.mjs rewrites it)`;

const readWorkflow = file => toData(parseStrict(readLocalFromRepo(file), file));

// The pins as the workflows define them now.
// Local actions (`uses: ./.github/actions/<name>`) that a pinned job or pinned local action uses
// are pinned too, as their parsed action.yml.
export function currentPins(read = readWorkflow) {
  const workflows = {}, jobs = [], local_actions = {};
  const addLocalActions = steps => {
    for (const step of steps) {
      const m = typeof step.uses === "string" && step.uses.match(/^\.\/(\.github\/actions\/[A-Za-z0-9._-]+)\/?$/);
      if (!m) continue;
      const file = [`${m[1]}/action.yml`, `${m[1]}/action.yaml`].find(f => existsSync(join(root, f))) ?? `${m[1]}/action.yml`;
      if (file in local_actions) continue;
      local_actions[file] = existsSync(join(root, file)) || read !== readWorkflow ? read(file) ?? null : null;
      addLocalActions(stepsOf(local_actions[file]?.runs));
    }
  };
  for (const [file, which] of PINNED_WORKFLOWS) {
    const { jobs: all, ...rest } = read(file);
    workflows[file] = rest;
    for (const job of which === "*" ? Object.keys(all) : which) {
      jobs.push({ file, job, definition: all[job] ?? null });
      addLocalActions(stepsOf(all[job]));
    }
  }
  return { workflows, local_actions, jobs };
}

export const readPinnedJobs = () => toData(parseStrict(readLocalFromRepo(PINNED_JOBS_FILE), PINNED_JOBS_FILE));

// The keys of the pinned jobs, as `<file>: job '<name>'`.
export const pinnedJobKeys = (pins = currentPins()) => new Set(pins.jobs.map(({ file, job }) => `${file}: job '${job}'`));

// The path of the first difference between two values, or null.
function firstDifference(a, b, path = "") {
  if (Object.is(a, b)) return null;
  if (isMapping(a) && isMapping(b)) {
    for (const key of new Set([...Object.keys(a), ...Object.keys(b)])) {
      const found = firstDifference(a[key], b[key], `${path}.${key}`);
      if (found !== null) return found;
    }
    return Object.keys(a).join() === Object.keys(b).join() ? null : `${path} (key order)`;
  }
  if (Array.isArray(a) && Array.isArray(b)) {
    for (let i = 0; i < Math.max(a.length, b.length); i++) {
      const found = firstDifference(a[i], b[i], `${path}[${i}]`);
      if (found !== null) return found;
    }
    return null;
  }
  return path || "(the whole value)";
}

export function pinnedJobViolations(current, pinned) {
  const out = [];
  const files = new Set([...Object.keys(current.workflows ?? {}), ...Object.keys(pinned?.workflows ?? {})]);
  for (const file of files) {
    const at = firstDifference(current.workflows?.[file], pinned?.workflows?.[file]);
    if (at !== null) out.push(`${file}: the workflow's own keys differ from the pin at ${at.slice(1) || "the top"}; ${UPDATE}`);
  }
  for (const file of new Set([...Object.keys(current.local_actions ?? {}), ...Object.keys(pinned?.local_actions ?? {})])) {
    const at = firstDifference(current.local_actions?.[file], pinned?.local_actions?.[file]);
    if (at !== null) out.push(`${file}: the local action used by a pinned job differs from the pin at ${at.slice(1) || "the whole action"}; ${UPDATE}`);
  }
  const byKey = list => new Map((Array.isArray(list) ? list : []).map(entry => [`${entry.file}: job '${entry.job}'`, entry.definition]));
  const now = byKey(current.jobs), then = byKey(pinned?.jobs);
  for (const key of new Set([...now.keys(), ...then.keys()])) {
    if (!now.has(key)) out.push(`${key} is pinned but no longer listed in PINNED_WORKFLOWS; ${UPDATE}`);
    else if (!then.has(key)) out.push(`${key} is not pinned yet; ${UPDATE}`);
    else {
      const at = firstDifference(now.get(key), then.get(key));
      if (at !== null) out.push(`${key} differs from the pinned job at ${at.slice(1) || "the whole job"}; ${UPDATE}`);
    }
  }
  return out;
}

// The repository files the pinned jobs and their local actions run or read by name: a word of
// a `run:` command that is a tracked file (or looks like a script path), each local action, and
// each npm script they run (`package.json: <name>`) with the files that script names. Their
// contents are not pinned (no source hashing or copies, AGENTS.md); a change to one of them is
// a change to the image jobs and is reviewed as one. A new or renamed file is a change here.
export const PINNED_JOB_INPUTS = [
  ".github/actions/setup-node/action.yml",
  ".github/scripts/build-oras.sh",
  ".github/scripts/fetch-verified.sh",
  ".github/scripts/oci-index.py",
  ".github/scripts/oci-layout.py",
  ".github/scripts/oci-stage.py",
  ".github/scripts/publication-tags.py",
  ".github/tools/oras.json",
  ".github/workflows/image.yml",
  "apps/gateway/Cargo.toml",
  "package.json: test:container",
  "scripts/github-release.sh",
  "scripts/staging.py",
  "tests/container-image.test.mjs",
];
const SCRIPT_PATH = /^(\.github|scripts|tests|deploy|tools|apps)\/[\w./-]+\.(sh|bash|py|mjs|js|cjs|ts)$/;
let trackedCache;
const tracked = () => (trackedCache ??= new Set(spawnSync("git", ["ls-files", "-z"], { cwd: root, encoding: "utf8" }).stdout.split("\0").filter(Boolean)));
const packageScripts = () => JSON.parse(readLocalFromRepo("package.json")).scripts ?? {};

export function pinnedJobInputs(pins = currentPins(), scripts = packageScripts()) {
  const found = new Set(Object.keys(pins.local_actions ?? {}));
  const read = (script, depth = 0) => {
    // A path anywhere in the text, also under a checkout directory (control/.github/...) or
    // inside a quoted program.
    const code = script.replace(/^[ \t]*#.*$/gm, "");
    for (const [, path] of code.matchAll(/(?<![\w.-])(?:[\w-]+\/)?((?:\.github|scripts|tests|deploy|tools|apps)\/[\w./-]*\w)/g)) {
      if (tracked().has(path) || SCRIPT_PATH.test(path)) found.add(path);
    }
    for (const words of shellCommands(script)) {
      const texts = words.map(w => w.text);
      for (const text of texts) {
        const path = text.replace(/^\.\//, "");
        if (tracked().has(path) || SCRIPT_PATH.test(path)) found.add(path);
      }
      const npm = texts.indexOf("npm");
      if (npm === -1) continue;
      const [sub, name] = texts.slice(npm + 1).filter(t => !t.startsWith("-"));
      const script = sub === "test" ? "test" : ["run", "run-script"].includes(sub) ? name : null;
      if (script === null || script === undefined) continue;
      found.add(`package.json: ${script}`);
      if (typeof scripts[script] === "string" && depth < 5) read(scripts[script], depth + 1);
    }
  };
  const steps = [...pins.jobs.flatMap(({ definition }) => stepsOf(definition)), ...Object.values(pins.local_actions ?? {}).flatMap(action => stepsOf(action?.runs))];
  for (const step of steps) if (typeof step.run === "string") read(step.run);
  return [...found].sort();
}

export function pinnedJobInputViolations(pins = currentPins(), listed = PINNED_JOB_INPUTS) {
  const found = pinnedJobInputs(pins);
  return [
    ...found.filter(path => !listed.includes(path)).map(path => `${path} is run by a pinned job but not listed in PINNED_JOB_INPUTS (tests/supply-chain/policy.mjs and docs/verification.md); a change to it is a change to the image jobs`),
    ...listed.filter(path => !found.includes(path)).map(path => `${path} is listed in PINNED_JOB_INPUTS but no pinned job runs it`),
  ];
}

// The staging rehearsal in ci.yml's staging-image runs the image that job built: the tag its
// build step loads. Every service of deploy/staging/compose.yaml that is built from the
// gateway's Dockerfile (the gateway and the ops services that share its definition) must
// default to it, and so must deploy/staging/staging.env.example, from which staging.py init
// writes the environment. image.yml's rehearsal replaces the value with the digest it tests.
export function stagingCandidateViolations(compose, envExample, ci) {
  const out = [];
  const candidate = stepOf(ci?.jobs?.["staging-image"], "docker/build-push-action")?.with?.tags;
  if (typeof candidate !== "string" || candidate.includes("${{") || candidate.includes("\n")) return ["ci.yml: staging-image must load one literal candidate tag"];
  const expected = `\${GATEWAY_IMAGE:-${candidate}}`;
  const services = isMapping(compose?.services) ? compose.services : {};
  if (!isMapping(services.gateway)) out.push("deploy/staging/compose.yaml: there is no gateway service");
  for (const [name, service] of Object.entries(services)) {
    if (!isMapping(service) || (name !== "gateway" && service.build === undefined)) continue;
    if (service.image !== expected) out.push(`deploy/staging/compose.yaml: service '${name}' image '${service.image}' is not the candidate under test (${expected})`);
  }
  const lines = String(envExample ?? "").split("\n").filter(line => line.startsWith("GATEWAY_IMAGE="));
  if (JSON.stringify(lines) !== JSON.stringify([`GATEWAY_IMAGE=${candidate}`])) out.push(`deploy/staging/staging.env.example: GATEWAY_IMAGE must be exactly the candidate ${candidate}`);
  return out;
}

// A job outside the pinned set that builds, logs in, publishes or calls another workflow.
const IMAGE_ACTIONS = new Set(["docker/build-push-action", "docker/setup-buildx-action", "docker/metadata-action", "docker/login-action"]);
const IMAGE_WRITE_SCOPES = ["packages", "id-token", "attestations", "contents"];
// What a job may write among IMAGE_WRITE_SCOPES, from its effective permissions (its own, else
// the workflow's, else the repository default, taken as write-all), or null.
function imageWrites(data, job) {
  const perms = jobPermissions(data, job ?? {});
  if (perms === "read-all") return null;
  if (typeof perms === "string") return `everything (${perms})`;
  if (!isMapping(perms)) return "unknown permissions";
  return IMAGE_WRITE_SCOPES.find(scope => perms[scope] === "write") ?? null;
}
export function unpinnedImageJobViolations(file, data, pinned = pinnedJobKeys()) {
  const out = [];
  for (const [name, job] of jobsOf(data)) {
    const key = `${file}: job '${name}'`;
    if (pinned.has(key)) continue;
    const action = stepsOf(job).map(s => actionName(s.uses ?? "")).find(a => IMAGE_ACTIONS.has(a));
    const writes = job?.uses === undefined && imageWrites(data, job);
    const why = action ? `uses ${action}` : writes ? `can write ${writes}` : job?.uses !== undefined ? "calls another workflow" : null;
    if (why) out.push(`${key} ${why}, so it must be pinned: add it to PINNED_WORKFLOWS and ${UPDATE}`);
  }
  return out;
}

// ---- run steps outside the pinned jobs (#488) ----

// A cheap extra check for jobs that are not pinned; the pinned jobs are checked whole. It reads
// each `run:` script for the ordinary spellings of an image build, a builder or daemon switch
// and an unchecked Compose file. A guard, not a proof: it does not run the shell, so a command
// assembled at run time, a script file a step runs, or a Compose file written at run time is
// not understood (a command word taken from a variable is refused for that reason).

// Environment that selects another builder or Docker daemon, or changes the Docker client's
// configuration, the Compose files or BuildKit: refused in every env: mapping and named in no
// run script of a job that is not pinned.
export const DOCKER_ENV = /^(BUILDX_\w*|BUILDKIT_\w*|DOCKER_HOST|DOCKER_CONTEXT|DOCKER_CONFIG|DOCKER_BUILDKIT|DOCKER_TLS_VERIFY|DOCKER_CERT_PATH|COMPOSE_FILE|COMPOSE_PATH_SEPARATOR|COMPOSE_PROJECT_DIRECTORY|COMPOSE_ENV_FILES)$/;
export const STEP_SHELLS = new Set(["bash", "sh"]);

// Simple commands of a script: continuations joined, split at newlines, ; & | && || ( ) $( and
// backticks, comments dropped, words shell-split (quotes removed; `DOCKER_""HOST` is one word).
// Each word records whether it holds a `$` expansion.
export function shellCommands(script) {
  const s = script.replace(/\\\n/g, " ");
  const commands = [], stack = [];
  let words = [], word = null, quoted = false;
  const add = (text, variable = false) => { word ??= { text: "", variable: false }; word.text += text; word.variable ||= variable; };
  const endWord = () => { if (word !== null) words.push(word); word = null; };
  const endCommand = () => { endWord(); if (words.length) commands.push(words); words = []; };
  // A command substitution is read as commands of its own; its value is a variable word.
  const open = close => { stack.push({ words, word, quoted, close }); words = []; word = null; quoted = false; };
  const close = () => { endCommand(); ({ words, word, quoted } = stack.pop()); add("", true); };
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (c === "`") { if (stack.at(-1)?.close === "`") close(); else open("`"); continue; }
    if (s.startsWith("$(", i)) { open(")"); i++; continue; }
    if (quoted) {
      if (c === '"') quoted = false;
      else if (c === "\\" && i + 1 < s.length) add(s[++i]);
      else add(c, c === "$");
      continue;
    }
    if (c === '"') { add(""); quoted = true; continue; }
    if (c === "'") { const j = s.indexOf("'", i + 1); add(s.slice(i + 1, j === -1 ? s.length : j)); i = j === -1 ? s.length : j; continue; }
    if (c === "\\" && i + 1 < s.length) { add(s[++i]); continue; }
    if (c === "#" && word === null) { const j = s.indexOf("\n", i); i = j === -1 ? s.length : j - 1; continue; }
    if (c === ")" && stack.at(-1)?.close === ")") { close(); continue; }
    if ("\n;&|()".includes(c)) { endCommand(); continue; }
    if (c === " " || c === "\t") { endWord(); continue; }
    if (c === ">" || c === "<") { endWord(); add(c); endWord(); continue; }
    add(c, c === "$");
  }
  while (stack.length) close();
  endCommand();
  // Redirections and their targets are not words of the command.
  return commands.map(ws => ws.filter((w, i) => !/^[<>]$/.test(w.text) && !(i > 0 && /^[<>]$/.test(ws[i - 1].text)))).filter(ws => ws.length);
}

const PREFIX_COMMANDS = new Set(["sudo", "env", "exec", "command", "time", "nohup", "xargs", "nice"]);
const ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*=/;

const refuse = reason => ({ refuse: reason });
const DOCKER_GLOBALS = {
  "-c": refuse("--context selects another Docker endpoint"), "--context": refuse("--context selects another Docker endpoint"),
  "-H": refuse("--host selects another Docker daemon"), "--host": refuse("--host selects another Docker daemon"),
  "--config": refuse("--config selects another Docker client configuration (contexts, plugins)"),
  "--tls": refuse("TLS options are for a remote daemon"), "--tlsverify": refuse("TLS options are for a remote daemon"),
  "--tlscacert": refuse("TLS options are for a remote daemon"), "--tlscert": refuse("TLS options are for a remote daemon"), "--tlskey": refuse("TLS options are for a remote daemon"),
  "-l": "value", "--log-level": "value", "-D": "flag", "--debug": "flag", "-v": "flag", "--version": "flag", "-h": "flag", "--help": "flag",
};
const BUILDX_GLOBALS = { "--builder": refuse("--builder selects another builder"), "-D": "flag", "--debug": "flag", "-h": "flag", "--help": "flag" };
const COMPOSE_GLOBALS = {
  "-f": "file", "--file": "file",
  "--project-directory": refuse("--project-directory moves where the Compose files and .env are read from"),
  "--env-file": refuse("--env-file sets Compose variables such as COMPOSE_FILE"),
  "-p": "value", "--project-name": "value", "--profile": "value", "--ansi": "value", "--progress": "value", "--parallel": "value",
  "--dry-run": "flag", "--compatibility": "flag", "--all-resources": "flag", "-h": "flag", "--help": "flag",
};

// Reads options up to the first word that is not one. `table` maps an option to "value"
// (takes one), "file" (a Compose file, collected), "flag" (takes none) or a refusal, for
// `--name value`, `--name=value` and short `-nvalue`. An option not in the table is refused.
function options(args, table, command) {
  const files = [];
  let i = 0;
  while (i < args.length && args[i].startsWith("-") && args[i] !== "-") {
    const arg = args[i];
    const long = arg.startsWith("--");
    const name = long ? arg.split("=")[0] : arg.slice(0, 2);
    const inline = long ? arg.includes("=") : arg.length > 2;
    const kind = table[name];
    if (kind === undefined) return { refused: `unknown ${command} option '${arg}'` };
    if (isMapping(kind)) return { refused: kind.refuse };
    if (kind === "value" || kind === "file") {
      const value = inline ? arg.slice(long ? name.length + 1 : 2) : args[i + 1];
      if (kind === "file") files.push(value ?? "");
      if (!inline) i++;
    }
    i++;
  }
  return { files, rest: args.slice(i) };
}

function composeCommand(args, composeFiles) {
  const read = options(args, COMPOSE_GLOBALS, "compose");
  if (read.refused) return read.refused;
  for (const file of read.files) if (file.includes("$") || !composeFiles.has(posix.normalize(file))) return `Compose file '${file}' is not one of the checked Compose files (repoComposeFiles)`;
  const [sub, ...rest] = read.rest;
  if (sub === "build") return "compose build builds an image";
  if (["up", "run", "create"].includes(sub) && rest.includes("--build")) return `compose ${sub} --build builds an image`;
  return null;
}

function dockerCommand(args, composeFiles) {
  const read = options(args, DOCKER_GLOBALS, "docker");
  if (read.refused) return read.refused;
  const [sub, ...rest] = read.rest;
  if (sub === "build") return "docker build builds an image";
  if ((sub === "image" || sub === "builder") && rest[0] === "build") return `docker ${sub} build builds an image`;
  if (sub === "buildx") {
    const bx = options(rest, BUILDX_GLOBALS, "buildx");
    if (bx.refused) return bx.refused;
    if (["build", "b", "bake"].includes(bx.rest[0])) return `buildx ${bx.rest[0]} builds an image`;
    if (["create", "use"].includes(bx.rest[0])) return `buildx ${bx.rest[0]} selects another builder`;
  }
  if (sub === "context" && ["create", "use", "update", "import"].includes(rest[0])) return `docker context ${rest[0]} selects another Docker endpoint`;
  if (sub === "compose") return composeCommand(rest, composeFiles);
  const positional = rest.filter(a => !a.startsWith("-"));
  if (sub === "login" || sub === "push" || (sub === "image" && positional[0] === "push")) return PUBLISHES;
  if (sub === "tag" && isRegistryRef(positional[1])) return PUBLISHES;
  if (sub === "image" && positional[0] === "tag" && isRegistryRef(positional[2])) return PUBLISHES;
  if (sub === "buildx" && positional[0] === "imagetools" && positional[1] === "create") return PUBLISHES;
  return null;
}

const PUBLISHES = "publishes, signs or retags an image or release; only a pinned job may";
// A reference whose first part names a registry host (`ghcr.io/...`, `localhost:5000/...`).
const isRegistryRef = ref => typeof ref === "string" && ref.includes("/") && /[.:]|^localhost$/.test(ref.split("/")[0]);
const PUBLISHING_TOOLS = {
  oras: ["push", "copy", "cp", "attach", "tag"],
  cosign: ["sign", "attest", "copy", "sign-blob", "attest-blob"],
  skopeo: ["copy", "sync"],
  crane: ["push", "copy", "cp", "tag", "append", "mutate"],
};

// The reason one simple command is refused, or null.
function commandViolation(words, composeFiles) {
  let i = 0;
  for (;;) {
    while (i < words.length && ASSIGNMENT.test(words[i].text)) i++;
    if (i < words.length && PREFIX_COMMANDS.has(words[i].text)) { i++; while (i < words.length && words[i].text.startsWith("-")) i++; continue; }
    break;
  }
  if (i >= words.length) return null;
  const word = words[i];
  if (word.variable && !/^\$\{?[A-Za-z_][A-Za-z0-9_]*\}?(\/[A-Za-z0-9._-]+)+$/.test(word.text)) return `the command word '${word.text}' is taken from a variable, which cannot be checked`;
  const name = posix.basename(word.text).toLowerCase();
  const args = words.slice(i + 1).map(w => w.text);
  if (/^python3?$/.test(name) && args[0]?.endsWith("staging.py")) return args.find((a, k) => k > 0 && !a.startsWith("-")) === "build" ? "staging.py build builds an image" : null;
  if (name === "staging.py") return args.find(a => !a.startsWith("-")) === "build" ? "staging.py build builds an image" : null;
  if (name === "docker") return dockerCommand(args, composeFiles);
  if (name === "docker-compose") return composeCommand(args, composeFiles);
  if (["podman", "nerdctl"].includes(name)) {
    const words2 = args.filter(a => !a.startsWith("-"));
    if (words2[0] === "build" || (["image", "builder", "buildx"].includes(words2[0]) && ["build", "b", "bake"].includes(words2[1]))) return `${name} builds an image`;
    if (words2[0] === "compose" && words2.includes("build")) return `${name} compose builds an image`;
    return null;
  }
  if (name === "buildah") return ["bud", "build"].includes(args.find(a => !a.startsWith("-"))) ? "buildah builds an image" : null;
  if (["buildctl", "kaniko", "executor"].includes(name)) return `${name} builds an image`;
  const positional = args.filter(a => !a.startsWith("-"));
  if (PUBLISHING_TOOLS[name]?.includes(positional[0])) return PUBLISHES;
  if (name === "gh" && positional[0] === "release" && ["create", "upload", "edit"].includes(positional[1])) return PUBLISHES;
  return null;
}

let composeFilesCache;
export const checkedComposeFiles = () => (composeFilesCache ??= new Set(repoComposeFiles()));

export function runBuildViolations(where, script, composeFiles = checkedComposeFiles()) {
  return shellCommands(script).flatMap(words => {
    const reason = commandViolation(words, composeFiles);
    return reason ? [`${where}: ${reason} (${JSON.stringify(words.map(w => w.text).join(" ").slice(0, 60))}); only a pinned job builds or publishes an image`] : [];
  });
}

// The DOCKER_ENV names a run script mentions, after the shell removes quotes.
// printf and echo -e escapes (\xHH, \uHHHH, \NNN) are decoded first.
const decodeEscapes = text => text.replace(/\\x([0-9a-fA-F]{2})|\\u([0-9a-fA-F]{4})|\\0?([0-7]{1,3})/g, (_, x, u, o) => String.fromCharCode(parseInt(x ?? u ?? o, x || u ? 16 : 8)));
function dockerEnvViolations(where, script) {
  script = decodeEscapes(script);
  const words = shellCommands(script).flat().map(w => w.text).join(" ");
  const names = new Set([...(`${script} ${words}`.match(/[A-Z][A-Z0-9_]*/g) ?? [])].filter(name => DOCKER_ENV.test(name)));
  return [...names].map(name => `${where}: ${name} selects a builder or changes the Docker client; only a pinned job may set it`);
}

function envMappingViolations(where, env) {
  if (env === undefined) return [];
  if (!isMapping(env)) return [`${where}: env must be a literal mapping`];
  return Object.keys(env).filter(name => DOCKER_ENV.test(name)).map(name => `${where}: env ${name} selects a builder or changes the Docker client`);
}

const shellViolation = (where, shell) => (shell === undefined || STEP_SHELLS.has(shell) ? [] : [`${where}: shell '${shell}' is not bash or sh, so the run-step checks cannot read the step`]);

export function buildStepsOf(file, data) {
  const steps = [];
  for (const [name, job] of jobsOf(data)) stepsOf(job).forEach(step => steps.push([`${file}: job '${name}'`, step, job]));
  stepsOf(isMapping(data?.runs) ? data.runs : null).forEach(step => steps.push([`${file}: runs`, step, null]));
  return steps;
}

// Every step of every workflow and local action: the build-step and Buildx rules everywhere;
// for jobs that are not pinned, also the run-step, env and shell checks.
export function imageBuildViolations(file, data, { composeFiles = checkedComposeFiles(), pinned = pinnedJobKeys() } = {}) {
  const out = [...envMappingViolations(`${file}: workflow`, data?.env), ...shellViolation(`${file}: workflow defaults`, data?.defaults?.run?.shell)];
  for (const [name, job] of jobsOf(data)) {
    out.push(...envMappingViolations(`${file}: job '${name}'`, job?.env));
    if (!pinned.has(`${file}: job '${name}'`)) out.push(...shellViolation(`${file}: job '${name}' defaults`, job?.defaults?.run?.shell));
  }
  for (const [where, step, job] of buildStepsOf(file, data)) {
    const isPinned = pinned.has(where);
    out.push(...envMappingViolations(where, step.env));
    if (!isPinned) {
      out.push(...shellViolation(where, step.shell));
      if (typeof step.run === "string") out.push(...runBuildViolations(where, step.run, composeFiles), ...dockerEnvViolations(where, step.run));
    }
    const action = actionName(step.uses ?? "");
    if (action === "docker/setup-buildx-action" && (!isMapping(step.with) || JSON.stringify(Object.entries(step.with).sort()) !== JSON.stringify(Object.entries(BUILDX_INPUTS)))) {
      out.push(`${where}: setup-buildx-action: with: must be exactly driver-opts: image=${BUILDKIT_IMAGE}`);
    }
    if (action !== "docker/build-push-action") continue;
    if (!isPinned) out.push(`${where}: an image build step outside the pinned jobs (PINNED_WORKFLOWS)`);
    out.push(...buildStepViolations(where, step.with, job));
  }
  return out;
}

// ---- Compose files (#488) ----

// A Compose `build:` takes only an explicit `context` (relative to the Compose file, in the
// checkout, outside the skipped directories) and an optional `dockerfile` (relative to the
// context, one of KNOWN_DOCKERFILES): no `target`, `additional_contexts`, `args`,
// `dockerfile_inline`, `ssh` or `secrets`. No Compose file may `include` another or `extends`
// a service. Each file is checked on its own, so an override file cannot add one of these.
export const COMPOSE_BUILD_KEYS = ["context", "dockerfile"];
export const COMPOSE_FILE = /^(docker-)?compose(\.[^/]+)?\.ya?ml$/i;

export function composeBuildViolations(file, data, builds = []) {
  const out = [];
  if (data?.include !== undefined) out.push(`${file}: top-level 'include' is not allowed (it reads Compose files this check does not)`);
  const services = isMapping(data?.services) ? data.services : {};
  for (const [name, service] of Object.entries(services)) {
    if (isMapping(service) && service.extends !== undefined) out.push(`${file}: service '${name}': 'extends' is not allowed (it takes a build from another service or file unchecked)`);
    if (!isMapping(service) || service.build === undefined) continue;
    const where = `${file}: service '${name}' build`;
    const build = typeof service.build === "string" ? { context: service.build } : service.build;
    if (!isMapping(build)) { out.push(`${where}: must be a context path or a mapping`); continue; }
    for (const key of Object.keys(build)) if (!COMPOSE_BUILD_KEYS.includes(key)) out.push(`${where}: '${key}' is not allowed (only ${COMPOSE_BUILD_KEYS.join(" and ")})`);
    const context = buildPath(build.context, posix.dirname(file));
    if (context.refused) { out.push(`${where}: context ${context.refused}`); continue; }
    const dockerfile = buildPath(build.dockerfile ?? "Dockerfile", context.path);
    if (dockerfile.refused) out.push(`${where}: dockerfile ${dockerfile.refused}`);
    else if (!KNOWN_DOCKERFILES.includes(dockerfile.path)) out.push(`${where}: dockerfile '${dockerfile.path}' is not one of the known Dockerfiles (${KNOWN_DOCKERFILES.join(", ")})`);
    else builds.push({ file, service: name, context: context.path, dockerfile: dockerfile.path });
  }
  return out;
}

// Every Compose file of the repository: a YAML file named like one, or any YAML file with a
// top-level `services:` key, anywhere outside the skipped directories and .github. Only these
// may be passed to `docker compose -f`.
export function repoComposeFiles(dir = root) {
  const base = dir.endsWith("/") ? dir : `${dir}/`;
  const visit = d => readdirSync(d, { withFileTypes: true }).flatMap(e => {
    const full = join(d, e.name);
    if (e.isDirectory()) return DOCKERFILE_SKIP_DIRS.test(e.name) || (d === dir && e.name === ".github") ? [] : visit(full);
    if (!/\.ya?ml$/i.test(e.name)) return [];
    return COMPOSE_FILE.test(e.name) || /^services[ \t]*:/m.test(readFileSync(full, "utf8")) ? [full.slice(base.length)] : [];
  });
  return visit(dir).sort();
}

// Compose reads `.env` from the project directory (that of the first file) and takes
// COMPOSE_FILE and other settings from it. A tracked `.env` next to a checked Compose file is
// refused unless listed here with the reason; none is. (A developer's untracked `.env` is
// not read: CI never has one.)
export const COMPOSE_DOTENV_ALLOWED = new Map([]);
const trackedFiles = () => new Set(spawnSync("git", ["ls-files", "-z"], { cwd: root, encoding: "utf8" }).stdout.split("\0").filter(Boolean));
export function composeDotenvViolations(composeFiles, exists = path => trackedFiles().has(path)) {
  const dirs = new Set([...composeFiles].map(file => posix.dirname(file)));
  return [...dirs].map(dir => (dir === "." ? ".env" : `${dir}/.env`)).filter(path => exists(path) && !COMPOSE_DOTENV_ALLOWED.has(path))
    .map(path => `${path} sets Compose variables (COMPOSE_FILE and others) for the Compose files beside it; it is not allowed unless listed in COMPOSE_DOTENV_ALLOWED`);
}

// ---- actions (#488) ----

// Every action a workflow or local action uses. A new third-party action is a change to this
// list. Names, not commits: the pin check requires a full SHA and Dependabot moves it; the
// jobs that build or publish pin every action's commit in pinned-jobs.yaml.
export const APPROVED_ACTIONS = new Map([
  ["actions/checkout", "GitHub-owned; checks out the repository"],
  ["actions/upload-artifact", "GitHub-owned; hands files between jobs"],
  ["actions/download-artifact", "GitHub-owned; hands files between jobs"],
  ["docker/setup-buildx-action", "starts the pinned BuildKit image (BUILDX_INPUTS); only in pinned jobs"],
  ["docker/build-push-action", "the only image builder; only in pinned jobs"],
  ["docker/metadata-action", "computes tags and labels in the read-only meta job (pinned)"],
  ["dtolnay/rust-toolchain", "installs the pinned Rust toolchain (#354)"],
  ["Swatinem/rust-cache", "caches Cargo build output in read-only jobs"],
  ["taiki-e/install-action", "installs cargo-deny at an exact version with its checksum check"],
  ["github/codeql-action/init", "CodeQL, in the read-only analyze job"],
  ["github/codeql-action/analyze", "CodeQL, in the read-only analyze job"],
  ["github/codeql-action/upload-sarif", "uploads the SARIF file in the upload job"],
].map(([name, reason]) => [name.toLowerCase(), reason]));
// Any other action whose name looks like an image builder is refused, even when listed.
export const IMAGE_BUILDER_NAME = /build-?push|buildx|bake|buildah|kaniko|podman|(^|[/-])img([/-]|$)|build-?image/i;
const DOCKER_ACTIONS = new Set(["docker/build-push-action", "docker/setup-buildx-action"]);

// Local actions are read only under .github/actions and local reusable workflows only in
// .github/workflows: a `uses:` of any other path runs code no check reads. A Docker container
// action, local or `docker://`, runs an image outside the approved actions.
export function actionPolicyViolations(file, data, { approved = APPROVED_ACTIONS, used = new Set() } = {}) {
  const out = [];
  if (isMapping(data?.runs) && String(data.runs.using).toLowerCase() === "docker") out.push(`${file}: a Docker container action runs an image outside the approved actions`);
  for (const [name, job] of jobsOf(data)) {
    if (job?.uses === undefined) continue;
    const m = typeof job.uses === "string" ? job.uses.match(/^\.\/\.github\/workflows\/([A-Za-z0-9_-][A-Za-z0-9._-]*\.ya?ml)$/) : null;
    if (!m || !existsSync(join(workflowsDir, m[1]))) out.push(`${file}: job '${name}': '${job.uses}' is not a workflow file of this checkout (./.github/workflows/<file>)`);
  }
  for (const [where, step] of buildStepsOf(file, data)) {
    const uses = step.uses;
    if (typeof uses !== "string") continue;
    if (uses.startsWith("./") || uses.startsWith("../")) {
      const m = uses.match(/^\.\/\.github\/actions\/([A-Za-z0-9_-][A-Za-z0-9._-]*)\/?$/);
      if (!m || m[1].includes("..")) out.push(`${where}: '${uses}' is outside .github/actions, which this suite reads`);
      else if (!["action.yml", "action.yaml"].some(f => existsSync(join(actionsDir, m[1], f)))) out.push(`${where}: '${uses}' has no action.yml`);
      continue;
    }
    if (uses.startsWith("docker://")) { out.push(`${where}: a docker:// step runs an image outside the approved actions`); continue; }
    const name = actionName(uses);
    if (!approved.has(name)) out.push(`${where}: ${name} is not an approved action (APPROVED_ACTIONS in tests/supply-chain/policy.mjs)`);
    else used.add(name);
    if (IMAGE_BUILDER_NAME.test(name) && !DOCKER_ACTIONS.has(name)) out.push(`${where}: ${name} looks like an image builder; only docker/build-push-action builds images`);
  }
  return out;
}

// ---- fixture helpers ----

// The checks of one workflow after an edit to the real parsed file.
export const buildChecks = (name, data) => imageBuildViolations(`.github/workflows/${name}.yml`, data);
export const actionChecks = (name, data, options) => actionPolicyViolations(`.github/workflows/${name}.yml`, data, options);
export const withStep = (name, job, step) => { const data = structuredClone(loadWorkflow(name)); data.jobs[job].steps.push(step); return data; };
export const stepOf = (job, action) => stepsOf(job).find(s => actionName(s.uses ?? "") === action);
