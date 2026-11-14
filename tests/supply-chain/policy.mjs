// The checks of #493 and #488 and their review: container images, image builds, Compose
// builds, actions and run steps. The tests are in tests/supply-chain/*.test.mjs.
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join, posix } from "node:path";
import { APPROVED_RUNTIME, APPROVED_RUST, DOCKERFILE_NAME, DOCKERFILE_SKIP_DIRS, KNOWN_DOCKERFILES, RUNTIME_DOCKERFILES, SHA, actionName, actionsDir, containerImagesIn, findDockerfiles, isDockerfileName, isMapping, jobsOf, loadWorkflow, parseStrict, readLocalFromRepo, repoFiles, root, stepsOf, toData, unknownDockerfileViolations, walk, wf, workflowsDir } from "./lib.mjs";
// ---- job and service container images (#493) ----

// Every image a job runs in (`container:`, written as a string or as a mapping with
// `image:`) or beside (`services.<name>`, a string or a mapping with `image:`), and the
// image of a Docker container action (`runs.image`, `docker://...`), must be pinned by
// digest: `<image>[:<tag>]@sha256:<64 lower-case hex>`. A floating tag would change what
// runs next to the job's token without a commit. A value that is not a literal string (an
// expression, a computed services mapping, a missing image) cannot be checked here and is
// refused unless listed below as `<file>: <where>` with the reason; every entry must be used.
export const CONTAINER_IMAGE_EXPRESSIONS = new Map([]);
export const PINNED_IMAGE = /^[^\s@$]+@sha256:[0-9a-f]{64}$/;

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
      if (isMapping(job.container)) check(`job '${name}' container.image`, job.container.image);
      else check(`job '${name}' container`, job.container);
    }
    if (job?.services !== undefined) {
      if (!isMapping(job.services)) { check(`job '${name}' services`, job.services); continue; }
      for (const [svc, value] of Object.entries(job.services)) {
        if (isMapping(value)) check(`job '${name}' services.${svc}.image`, value.image);
        else check(`job '${name}' services.${svc}`, value);
      }
    }
  }
  if (isMapping(data?.runs) && String(data.runs.using).toLowerCase() === "docker") {
    const image = data.runs.image;
    if (typeof image === "string" && !image.includes("${{") && !image.startsWith("docker://")) out.push(`${file}: runs.image: a Docker action must name a digest-pinned docker:// image, not build ${image}`);
    else check("runs.image", typeof image === "string" ? image.replace(/^docker:\/\//, "") : image);
  }
  return out;
}



// ---- what the image builds build (#488) ----

// The checks above read the Dockerfiles the walker finds; these tie them to what is built.
// An image is built only by a docker/build-push-action step or a Compose `build:` section,
// and each names one of the Dockerfiles below through a context outside the skipped
// directories. What a build step may change is pinned here:
// - `file` (the Dockerfile; default `<context>/Dockerfile`) only one of KNOWN_DOCKERFILES;
// - `context` a literal directory of the checkout (without it the action builds the Git
//   context, which the checks never read), not in or under a skipped directory;
// - `target` (which stage is built; the default is the last, the runtime) only where
//   IMAGE_BUILDS lists it, which today is nowhere;
// - no `build-contexts` (`<name>=docker-image://<image>` replaces a stage or a FROM image,
//   pinned or not, without a change to the Dockerfile);
// - no `BUILDKIT_*` build argument (`BUILDKIT_SYNTAX` loads another Dockerfile frontend);
// and IMAGE_BUILDS holds every build step with every one of its `with:` inputs, as written in
// the workflow (#488 review): the cache sources and targets (only the job's own `type=gha`
// scope), the outputs (an OCI archive or the local daemon, never a registry), the SBOM
// generator, the platforms, tags and labels. Values with `${{ }}` are pinned as text: what
// they expand to comes from the runner, the job matrix or the read-only meta job. A new step,
// a step moved to another job, or any changed input is a reviewed change to this list.
export const SBOM_ATTEST = "type=sbom,generator=mirror.gcr.io/docker/buildkit-syft-scanner:stable-1@sha256:ae4f3b554449e7e25548e7d8ccc029d17357348e30c6e3df01b92bc93654d6a9";
export const OCI_ARCHIVE = "type=oci,dest=${{ runner.temp }}/image.tar,oci-mediatypes=true";
export const IMAGE_BUILDS = new Map([
  [".github/workflows/ci.yml: job 'staging-image'", {
    context: ".", load: true, tags: "open-model-gateway:staging", "cache-from": "type=gha,scope=staging", "cache-to": "type=gha,mode=max,scope=staging",
  }],
  [".github/workflows/image.yml: job 'build'", {
    context: ".", platforms: "${{ matrix.platform }}", labels: "${{ needs.meta.outputs.build_labels }}", outputs: OCI_ARCHIVE, attests: SBOM_ATTEST, provenance: "mode=max",
    "cache-from": "type=gha,scope=image-${{ env.PLATFORM_PAIR }}", "cache-to": "type=gha,mode=max,scope=image-${{ env.PLATFORM_PAIR }}",
  }],
  [".github/workflows/branch-images.yml: job 'build'", {
    context: ".", platforms: "linux/amd64", outputs: OCI_ARCHIVE, attests: SBOM_ATTEST, provenance: "mode=max", labels: "org.opencontainers.image.title=open-model-gateway\norg.opencontainers.image.source=https://github.com/${{ github.repository }}\norg.opencontainers.image.revision=${{ steps.name.outputs.sha }}\norg.opencontainers.image.licenses=MIT\n",
    "cache-from": "type=gha,scope=image-linux-amd64", "cache-to": "type=gha,mode=max,scope=image-linux-amd64",
  }],
  [".github/workflows/loadtest-image.yml: job 'build'", {
    context: ".", file: "deploy/loadtest/Dockerfile", platforms: "linux/amd64", outputs: OCI_ARCHIVE, "build-args": "CARGO_BUILD_JOBS=4", attests: SBOM_ATTEST, provenance: "mode=max",
    labels: "org.opencontainers.image.title=open-model-gateway-loadtest\norg.opencontainers.image.source=https://github.com/${{ github.repository }}\norg.opencontainers.image.revision=${{ steps.name.outputs.sha }}\norg.opencontainers.image.licenses=MIT\n", "cache-from": "type=gha,scope=loadtest-linux-amd64", "cache-to": "type=gha,mode=max,scope=loadtest-linux-amd64",
  }],
]);

// docker/setup-buildx-action (#488 review): the default docker-container driver running the
// pinned BuildKit image, and nothing else. `driver`, `endpoint` (a remote builder), `config`
// and the inline configs (registry mirrors), `use`, `version` (a downloaded Buildx) and other
// driver options are refused because the inputs must be exactly these.
export const BUILDKIT_IMAGE = "mirror.gcr.io/moby/buildkit:buildx-stable-1@sha256:cec9f139f45e93c5c69c60f8b07cfad9f43f4ef6b6a6cd917527fea5ff2e3dea";
export const BUILDX_INPUTS = { "driver-opts": `image=${BUILDKIT_IMAGE}` };

// Environment that selects another builder or Docker daemon, or changes the Docker client's
// configuration, the Compose files or BuildKit's behaviour (#488 review). Refused in every
// `env:` mapping (workflow, job, step) and named in no `run:` script (which could write it to
// $GITHUB_ENV or export it), except where RUN_DOCKER_ENV lists it.
export const DOCKER_ENV = /^(BUILDX_\w*|BUILDKIT_\w*|DOCKER_HOST|DOCKER_CONTEXT|DOCKER_CONFIG|DOCKER_BUILDKIT|DOCKER_TLS_VERIFY|DOCKER_CERT_PATH|COMPOSE_FILE|COMPOSE_PATH_SEPARATOR|COMPOSE_PROJECT_DIRECTORY)$/;
// DOCKER_CONFIG only, and only in jobs that run neither setup-buildx-action nor a build (checked):
// each logs in to the registry with a fresh private configuration and removes it afterwards.
export const RUN_DOCKER_ENV = new Map([
  [".github/workflows/branch-images.yml: job 'publish'", "a fresh private directory (mktemp) for registry credentials, removed afterwards; the job runs no build"],
  [".github/workflows/loadtest-image.yml: job 'publish'", "a fresh private directory (mktemp) for registry credentials, removed afterwards; the job runs no build"],
  [".github/workflows/image.yml: job 'probe-prepare'", "a fresh private directory (mktemp) for registry credentials, removed afterwards; the job runs no build"],
  [".github/workflows/image.yml: job 'probe-sign'", "a fresh private directory (mktemp) for registry credentials, removed afterwards; the job runs no build"],
  [".github/workflows/image.yml: job 'probe-verify'", "a fresh private directory (mktemp) for registry credentials, removed afterwards; the job runs no build"],
  [".github/workflows/image.yml: job 'publish'", "a fresh private directory (mktemp) for registry credentials, removed afterwards; the job runs no build"],
  [".github/workflows/image.yml: job 'release'", "a fresh private directory (mktemp) for registry credentials, removed afterwards; the job runs no build"],
]);


// A build context or Dockerfile path, relative to `base` (a directory of the checkout, "."
// for the root): returns the normalized path, or the reason it is refused.
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

export function buildStepsOf(file, data) {
  const steps = [];
  for (const [name, job] of jobsOf(data)) stepsOf(job).forEach(step => steps.push([`${file}: job '${name}'`, step]));
  stepsOf(isMapping(data?.runs) ? data.runs : null).forEach(step => steps.push([`${file}: runs`, step]));
  return steps;
}

// Every image build command in a `run:` script: a container builder (docker, podman, ...) with
// a build or bake subcommand, or Compose with --build, on one line (continuations joined).
export const RUN_BUILDERS = new Set(["docker", "docker-compose", "podman", "nerdctl", "buildah", "buildctl", "kaniko", "staging.py"]);
export const RUN_BUILD_WORDS = new Set(["build", "bake", "bud", "--build"]);
export function runBuildViolations(where, script, composeFiles = checkedComposeFiles()) {
  return script.replace(/\\\n/g, " ").split(/\n|;|&&|\|\|/).flatMap(line => {
    const words = line.split(/[^A-Za-z0-9_.+-]+/).map(word => word.toLowerCase());
    const builder = words.some(word => RUN_BUILDERS.has(word) || word.endsWith("/staging.py"));
    const build = words.some(word => RUN_BUILD_WORDS.has(word)) || (words.includes("buildx") && words.includes("b"));
    const out = builder && build ? [`${where}: a run step builds an image (${JSON.stringify(line.trim().slice(0, 60))}); use a docker/build-push-action step listed in IMAGE_BUILDS`] : [];
    const docker = words.includes("docker") || words.includes("buildx");
    const selects = /\bbuildx\s+(?:-\S+\s+)*(?:create|use)\b/.test(line) || /\bdocker\s+(?:-\S+\s+)*context\s+(?:create|use|update|import)\b/.test(line)
      || ["--builder", "--context", "-c", "-h", "--host"].some(flag => words.includes(flag));
    if (docker && selects) out.push(`${where}: a run step selects a builder or Docker endpoint (${JSON.stringify(line.trim().slice(0, 60))}); the runner's default Docker and the setup-buildx-action builder are the only ones`);
    if (words.includes("docker-compose") || (words.includes("docker") && words.includes("compose"))) out.push(...composeFileViolations(where, line, composeFiles));
    return out;
  });
}

// A Compose invocation may name only Compose files this file checks (-f/--file, as a literal).
export function composeFileViolations(where, line, composeFiles) {
  const out = [];
  for (const m of line.matchAll(/(?:^|\s)(?:-f|--file)(?:=|\s+)("[^"]*"|'[^']*'|\S+)/g)) {
    const path = m[1].replace(/^["']|["']$/g, "");
    if (path.includes("$") || !composeFiles.has(posix.normalize(path))) out.push(`${where}: Compose file '${path}' is not one of the checked Compose files (repoComposeFiles)`);
  }
  if (/(?:^|\s)--project-directory\b/.test(line)) out.push(`${where}: --project-directory moves where the Compose files are read from`);
  return out;
}

// The environment names a run script mentions that DOCKER_ENV covers.
export function dockerEnvViolations(where, script) {
  const allowed = RUN_DOCKER_ENV.has(where) ? "DOCKER_CONFIG" : null;
  return [...new Set(script.match(/[A-Z][A-Z0-9_]*/g) ?? [])].filter(name => DOCKER_ENV.test(name) && name !== allowed)
    .map(name => `${where}: ${name} selects a builder or changes the Docker client; it may not be set or written by a workflow`);
}

export function envMappingViolations(where, env) {
  if (env === undefined) return [];
  if (!isMapping(env)) return [`${where}: env must be a literal mapping (an expression could set ${DOCKER_ENV.source})`];
  return Object.keys(env).filter(name => DOCKER_ENV.test(name)).map(name => `${where}: env ${name} selects a builder or changes the Docker client`);
}

export function imageBuildViolations(file, data, approved = IMAGE_BUILDS, seen = new Set(), { composeFiles = checkedComposeFiles() } = {}) {
  const out = [...envMappingViolations(`${file}: workflow`, data?.env)];
  for (const [name, job] of jobsOf(data)) out.push(...envMappingViolations(`${file}: job '${name}'`, job?.env));
  for (const [where, step] of buildStepsOf(file, data)) {
    out.push(...envMappingViolations(where, step.env));
    if (typeof step.run === "string") out.push(...runBuildViolations(where, step.run, composeFiles), ...dockerEnvViolations(where, step.run));
    if (typeof step.uses !== "string") continue;
    const action = actionName(step.uses);
    if ((action === "docker/setup-buildx-action" || action === "docker/build-push-action") && RUN_DOCKER_ENV.has(where)) out.push(`${where}: RUN_DOCKER_ENV lets this job set DOCKER_CONFIG, so it may not set up Buildx or build`);
    if (action === "docker/setup-buildx-action") {
      if (!isMapping(step.with) || JSON.stringify(Object.entries(step.with).sort()) !== JSON.stringify(Object.entries(BUILDX_INPUTS))) out.push(`${where}: setup-buildx-action: with: must be exactly driver-opts: image=${BUILDKIT_IMAGE}`);
      continue;
    }
    if (action !== "docker/build-push-action") continue;
    const inputs = step.with;
    if (!isMapping(inputs)) { out.push(`${where}: a build step needs a literal with: mapping`); continue; }
    if ("build-contexts" in inputs) out.push(`${where}: build-contexts can replace a stage or base image of the Dockerfile; it is not allowed`);
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
    const expected = approved.get(where);
    if ("target" in inputs && expected?.target === undefined) out.push(`${where}: target '${inputs.target}' builds another stage than the runtime image; it is not listed in IMAGE_BUILDS`);
    if (!expected) { out.push(`${where}: an image build step that is not listed in IMAGE_BUILDS`); continue; }
    if (seen.has(where)) { out.push(`${where}: more than one image build step`); continue; }
    seen.add(where);
    const keys = Object.keys(inputs).sort(), listed = Object.keys(expected).sort();
    if (JSON.stringify(keys) !== JSON.stringify(listed)) out.push(`${where}: with: keys ${keys.join(", ")}; IMAGE_BUILDS lists ${listed.join(", ")}`);
    for (const input of new Set([...keys, ...listed])) if (inputs[input] !== expected[input]) out.push(`${where}: ${input} is ${JSON.stringify(inputs[input])}; IMAGE_BUILDS lists ${JSON.stringify(expected[input])}`);
  }
  return out;
}

// Compose `build:` sections (#488): a build of a service takes only an explicit `context`
// (relative to the Compose file, in the checkout, outside the skipped directories) and an
// optional `dockerfile` (relative to the context, one of KNOWN_DOCKERFILES). Every other key
// is refused, among them `target` (another stage), `additional_contexts` (replaces a stage or
// base image, as build-contexts does), `args` (BUILDKIT_SYNTAX), `dockerfile_inline`, `ssh`
// and `secrets`. Each Compose file is checked on its own, so an override file cannot add one.
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

// Every Compose file of the repository (#488 review): a YAML file named like one, or any YAML
// file with a top-level `services:` key, anywhere outside the skipped directories and
// .github. Only these may be passed to `docker compose -f`.
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
export let composeFilesCache;
export const checkedComposeFiles = () => (composeFilesCache ??= new Set(repoComposeFiles()));





// ---- review of #488: bypasses of the build checks ----

// Every action a workflow or local action uses (#488 review). A new third-party action is a
// change to this list. Names, not commits: the pin check above requires a full SHA and
// Dependabot moves it; the two Docker actions, whose inputs the build checks interpret, are
// also pinned to their commit, so a new version is read before it is approved.
export const APPROVED_ACTIONS = new Map([
  ["actions/checkout", "GitHub-owned; checks out the repository"],
  ["actions/upload-artifact", "GitHub-owned; hands files between jobs"],
  ["actions/download-artifact", "GitHub-owned; hands files between jobs"],
  ["docker/setup-buildx-action", "starts the pinned BuildKit image (BUILDX_INPUTS)"],
  ["docker/build-push-action", "the only image builder (IMAGE_BUILDS)"],
  ["docker/metadata-action", "computes tags and labels in the read-only meta job"],
  ["dtolnay/rust-toolchain", "installs the pinned Rust toolchain (#354)"],
  ["Swatinem/rust-cache", "caches Cargo build output in read-only jobs"],
  ["taiki-e/install-action", "installs cargo-deny at an exact version with its checksum check"],
  ["github/codeql-action/init", "CodeQL, in the read-only analyze job"],
  ["github/codeql-action/analyze", "CodeQL, in the read-only analyze job"],
  ["github/codeql-action/upload-sarif", "uploads the SARIF file in the upload job"],
].map(([name, reason]) => [name.toLowerCase(), reason]));
export const PINNED_ACTION_SHAS = new Map([
  ["docker/build-push-action", "c3c9e263c25d99ce0380d002d59b67737d91b0dc"],
  ["docker/setup-buildx-action", "f87e5991a6d7451dcb8d9637bfbc97413f497069"],
]);
export const IMAGE_BUILDER_NAME = /build-?push|buildx|bake|buildah|kaniko|podman|(^|[/-])img([/-]|$)|build-?image/i;

// Local actions are read only under .github/actions, and local reusable workflows only in
// .github/workflows: a `uses:` of any other path runs code no check here reads.
export function actionPolicyViolations(file, data, { approved = APPROVED_ACTIONS, used = new Set() } = {}) {
  const out = [];
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
    const name = actionName(uses), sha = uses.split("@")[1];
    if (!approved.has(name)) out.push(`${where}: ${name} is not an approved action (APPROVED_ACTIONS in tests/supply-chain.test.mjs)`);
    else used.add(name);
    if (IMAGE_BUILDER_NAME.test(name) && !PINNED_ACTION_SHAS.has(name)) out.push(`${where}: ${name} looks like an image builder; only docker/build-push-action builds images`);
    if (PINNED_ACTION_SHAS.has(name) && sha !== PINNED_ACTION_SHAS.get(name)) out.push(`${where}: ${name} must be pinned to ${PINNED_ACTION_SHAS.get(name)}, the commit whose inputs these checks were written for`);
  }
  return out;
}


// The checks of one workflow after adding a job, a step or env to the real parsed file.
export const buildChecks = (name, data) => imageBuildViolations(`.github/workflows/${name}.yml`, data, IMAGE_BUILDS, new Set(), { composeFiles: new Set(repoComposeFiles()) });
export const actionChecks = (name, data, options) => actionPolicyViolations(`.github/workflows/${name}.yml`, data, options);
export const withStep = (name, job, step) => { const data = structuredClone(loadWorkflow(name)); data.jobs[job].steps.push(step); return data; };









// ---- pinned image jobs (#488) ----

// Every job that builds, publishes, signs, attests, imports or tests an image is pinned in
// full in pinned-jobs.yaml: a run step can do anything (check out other code over the
// Dockerfile, put a wrapper first on PATH, retag the image under test), so the only reliable
// tripwire is that any change to such a job is a visible change to that file, which a
// reviewer must read. `"*"` pins every job of the workflow; the workflow's own keys (name, on,
// permissions, env, concurrency, defaults) are pinned with it.
export const PINNED_WORKFLOWS = new Map([
  [".github/workflows/image.yml", "*"],
  [".github/workflows/branch-images.yml", "*"],
  [".github/workflows/loadtest-image.yml", "*"],
  [".github/workflows/oci-import.yml", "*"],
  [".github/workflows/ci.yml", ["staging-image", "image"]],
]);
export const PINNED_JOBS_FILE = "tests/supply-chain/pinned-jobs.yaml";

// The pins as the workflows define them today.
export function currentPins(read = file => toData(parseStrict(readLocalFromRepo(file), file))) {
  const workflows = {}, jobs = [];
  for (const [file, which] of PINNED_WORKFLOWS) {
    const { jobs: all, ...rest } = read(file);
    workflows[file] = rest;
    for (const job of which === "*" ? Object.keys(all) : which) jobs.push({ file, job, definition: all[job] ?? null });
  }
  return { workflows, jobs };
}

export const readPinnedJobs = () => toData(parseStrict(readLocalFromRepo(PINNED_JOBS_FILE), PINNED_JOBS_FILE));

// Implemented in the next commit; the fixtures show what it must refuse.
export function pinnedJobViolations() { return []; }
export function composeDotenvViolations() { return []; }
