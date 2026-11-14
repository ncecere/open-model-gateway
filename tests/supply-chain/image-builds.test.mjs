// What the image build steps build: the Dockerfile, context, stage and every input (#488).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { APPROVED_RUST, KNOWN_DOCKERFILES, actionName, findDockerfiles, loadWorkflow, parseStrict, readLocalFromRepo, repoFiles, root, stepsOf, toData, unknownDockerfileViolations, workflowsDir } from "./lib.mjs";
import { BUILDKIT_IMAGE, IMAGE_BUILDS, actionPolicyViolations, buildChecks, composeBuildViolations, imageBuildViolations } from "./policy.mjs";

test("every workflow image build step builds a known Dockerfile, context and stage, with its listed inputs (#488)", () => {
  const seen = new Set();
  const violations = repoFiles().flatMap(({ file, text }) => imageBuildViolations(file, toData(parseStrict(text, file)), IMAGE_BUILDS, seen));
  assert.deepEqual(violations, []);
  assert.deepEqual([...IMAGE_BUILDS.keys()].filter(where => !seen.has(where)), [], "every listed build step exists");
  assert.deepEqual(unknownDockerfileViolations(findDockerfiles().map(p => p.slice(root.length))), [], "the walker finds only known Dockerfiles");
  assert.equal(findDockerfiles().length, KNOWN_DOCKERFILES.length, "the walker finds every known Dockerfile");
});

test("fixtures: a build step or Compose build that builds something else is refused (#488)", () => {
  const load = name => toData(parseStrict(readFileSync(join(workflowsDir, `${name}.yml`), "utf8"), `.github/workflows/${name}.yml`));
  const workflows = Object.fromEntries(["ci", "image", "branch-images", "loadtest-image"].map(name => [name, load(name)]));
  const buildStep = (data, job) => stepsOf(data.jobs[job]).find(s => actionName(s.uses ?? "") === "docker/build-push-action");
  const check = (name, data) => imageBuildViolations(`.github/workflows/${name}.yml`, data, IMAGE_BUILDS, new Set());
  for (const [name, data] of Object.entries(workflows)) assert.deepEqual(check(name, data), [], name);
  const jobOf = { ci: "staging-image", image: "build", "branch-images": "build", "loadtest-image": "build" };
  const OTHER = `mirror.gcr.io/library/debian:13-slim@sha256:${"b".repeat(64)}`;
  const RUST = APPROVED_RUST;
  const mutations = [
    ["target on the publish build", "image", w => { w.target = "build"; }, /target 'build' builds another stage/],
    ["target on the pull-request scan build", "ci", w => { w.target = "build"; }, /target 'build' builds another stage/],
    ["target on the branch build", "branch-images", w => { w.target = "web"; }, /target 'web'/],
    ["an unknown Dockerfile.release", "image", w => { w.file = "Dockerfile.release"; }, /file 'Dockerfile\.release' is not one of the known Dockerfiles/],
    ["a Containerfile", "ci", w => { w.file = "Containerfile"; }, /file 'Containerfile' is not one of the known/],
    ["a *.dockerfile", "loadtest-image", w => { w.file = "deploy/loadtest/release.dockerfile"; }, /not one of the known/],
    ["a Dockerfile under target*", "image", w => { w.file = "target-release/Dockerfile"; }, /in 'target-release', a directory the Dockerfile walker skips/],
    ["a Dockerfile under dist", "ci", w => { w.file = "apps/web/dist/Dockerfile"; }, /in 'dist'/],
    ["a known Dockerfile reached through ..", "loadtest-image", w => { w.file = "deploy/loadtest/../../Dockerfile"; }, /file is "deploy\/loadtest\/\.\.\/\.\.\/Dockerfile"/],
    ["a file from an expression", "image", w => { w.file = "${{ inputs.dockerfile }}"; }, /file '\$\{\{ inputs\.dockerfile \}\}' is an expression/],
    ["build-contexts swapping the Rust base", "image", w => { w["build-contexts"] = `${RUST}=docker-image://${OTHER}`; }, /build-contexts can replace a stage/],
    ["build-contexts replacing a stage", "loadtest-image", w => { w["build-contexts"] = "build=docker-image://rust:latest"; }, /build-contexts/],
    ["a BUILDKIT_SYNTAX build-arg", "image", w => { w["build-args"] = "BUILDKIT_SYNTAX=docker.io/docker/dockerfile:1"; }, /build argument 'BUILDKIT_SYNTAX=.*' changes how BuildKit builds/],
    ["a BUILDKIT_SYNTAX line beside a listed build-arg", "loadtest-image", w => { w["build-args"] += "\nBUILDKIT_SYNTAX=docker.io/docker/dockerfile:1"; }, /build argument 'BUILDKIT_SYNTAX/],
    ["context in dist", "image", w => { w.context = "apps/web/dist"; }, /context 'apps\/web\/dist' is in 'dist'/],
    ["context in target", "ci", w => { w.context = "target"; }, /context 'target' is in 'target'/],
    ["context in node_modules", "branch-images", w => { w.context = "node_modules/pkg"; }, /in 'node_modules'/],
    ["context in .git", "loadtest-image", w => { w.context = ".git"; }, /in '\.git'/],
    ["no context (the Git context)", "image", w => { delete w.context; }, /context must be one literal, non-empty path/],
    ["a remote context", "ci", w => { w.context = "https://github.com/other/repo.git#main"; }, /remote context/],
    ["a context outside the checkout", "image", w => { w.context = ".."; }, /leaves the checkout/],
    ["a context with no known Dockerfile", "ci", w => { w.context = "apps/web"; }, /default Dockerfile 'apps\/web\/Dockerfile' is not one of the known/],
    ["another key", "ci", w => { w.secrets = "x=y"; }, /with: keys .*secrets/],
    ["a removed key", "image", w => { delete w.provenance; }, /with: keys/],
    ["another listed build-arg value", "loadtest-image", w => { w["build-args"] = "CARGO_BUILD_JOBS=8"; }, /build-args is "CARGO_BUILD_JOBS=8"/],
  ];
  for (const [label, name, mutate, reason] of mutations) {
    const data = structuredClone(workflows[name]);
    mutate(buildStep(data, jobOf[name]).with);
    const found = check(name, data);
    assert.ok(found.some(v => reason.test(v)), `${label}: ${found.join("; ")}`);
  }
  // A build step that is not listed, a second one, or another builder.
  const extra = (name, step) => { const data = structuredClone(workflows[name]); data.jobs[jobOf[name]].steps.push(step); return check(name, data); };
  const listed = buildStep(workflows.ci, "staging-image");
  assert.match(extra("ci", structuredClone(listed)).join(), /more than one image build step/);
  const moved = structuredClone(workflows.ci);
  moved.jobs.other = { "runs-on": "ubuntu-24.04", steps: [structuredClone(listed)] };
  assert.match(check("ci", moved).join(), /job 'other': an image build step that is not listed/);
  assert.match(actionPolicyViolations(".github/workflows/image.yml", { jobs: { j: { steps: [{ uses: `docker/bake-action@${"0".repeat(40)}` }] } } }).join(), /docker\/bake-action looks like an image builder/);
  assert.match(imageBuildViolations("action.yml", { runs: { using: "composite", steps: [{ uses: `docker/build-push-action@${"0".repeat(40)}`, with: { context: "." } }] } }).join(), /action\.yml: runs: an image build step that is not listed/);
  // An image built by a run step.
  for (const run of ["docker build --target build -t x .", "docker buildx build -f Containerfile .", "docker image build .", "docker buildx b .", "docker buildx bake", "docker \\\n  build .", "docker compose -f deploy/staging/compose.yaml build gateway",
    "docker compose up --build -d", "docker-compose build", "podman build .", "buildah bud .", "python3 scripts/staging.py build", "set -e; docker build ."]) {
    assert.equal(extra("ci", { run }).length, 1, run);
  }
  for (const run of ["docker buildx version", "docker buildx imagetools inspect x", "cargo build --locked", "npm run build:web", "python3 scripts/staging.py up", "docker compose -f deploy/staging/compose.yaml up -d --wait"]) {
    assert.deepEqual(extra("ci", { run }), [], run);
  }

  // Compose: the staging gateway build and its merge-key copies.
  const composeFile = "deploy/staging/compose.yaml";
  const compose = toData(parseStrict(readLocalFromRepo(composeFile), composeFile));
  assert.deepEqual(composeBuildViolations(composeFile, compose), []);
  const composeMutations = [
    ["target", b => { b.target = "build"; }, /'target' is not allowed/],
    ["additional_contexts swapping the base", b => { b.additional_contexts = { [RUST]: `docker-image://${OTHER}` }; }, /'additional_contexts' is not allowed/],
    ["args with BUILDKIT_SYNTAX", b => { b.args = { BUILDKIT_SYNTAX: "docker.io/docker/dockerfile:1" }; }, /'args' is not allowed/],
    ["an inline Dockerfile", b => { b.dockerfile_inline = `FROM ${OTHER}`; }, /'dockerfile_inline' is not allowed/],
    ["an unknown Dockerfile.release", b => { b.dockerfile = "Dockerfile.release"; }, /dockerfile 'Dockerfile\.release' is not one of the known/],
    ["a Containerfile", b => { b.dockerfile = "Containerfile"; }, /dockerfile 'Containerfile' is not one of the known/],
    ["a Dockerfile in dist", b => { b.dockerfile = "apps/web/dist/Dockerfile"; }, /in 'dist'/],
    ["context in a skipped directory", b => { b.context = "../../target"; }, /context '\.\.\/\.\.\/target' is in 'target'/],
    ["context outside the checkout", b => { b.context = "../../.."; }, /leaves the checkout/],
    ["context from a variable", b => { b.context = "${BUILD_CONTEXT:-../..}"; }, /is an expression or variable/],
    ["a remote context", b => { b.context = "https://github.com/other/repo.git"; }, /remote context/],
    ["no context", b => { delete b.context; }, /context must be one literal/],
  ];
  for (const [label, mutate, reason] of composeMutations) {
    const data = structuredClone(compose);
    mutate(data.services.gateway.build);
    const found = composeBuildViolations(composeFile, data);
    assert.ok(found.some(v => reason.test(v)), `${label}: ${found.join("; ")}`);
  }
  assert.match(composeBuildViolations("deploy/staging/compose.openai.yaml", { services: { gateway: { build: { target: "build" } } } }).join(), /'target' is not allowed/, "an override file cannot add a target");
  assert.match(composeBuildViolations("compose.yaml", { services: { x: { build: "deploy/loadtest/dist" } } }).join(), /in 'dist'/, "the string form is a context");
  assert.deepEqual(composeBuildViolations("deploy/loadtest/compose.yaml", { services: { x: { build: { context: "../..", dockerfile: "deploy/loadtest/Dockerfile" } } } }), [], "the load-test Dockerfile from the root");
  assert.deepEqual(composeBuildViolations("deploy/loadtest/compose.yaml", { services: { x: { build: "." } } }), [], "the load-test Dockerfile as its directory's default");
  assert.match(composeBuildViolations("compose.yaml", { services: { x: { build: ["."] } } }).join(), /must be a context path or a mapping/);
});

test("fixtures: setup-buildx-action inputs other than the pinned BuildKit image are refused (#488 review 6)", () => {
  const buildx = d => d.jobs["staging-image"].steps[1].with;
  for (const [label, mutate] of [
    ["the remote driver", w => { w.driver = "remote"; w.endpoint = "tcp://evil:1234"; }],
    ["an endpoint", w => { w.endpoint = "tcp://evil:1234"; }],
    ["a registry mirror in config-inline", w => { w["config-inline"] = '[registry."docker.io"]\n  mirrors = ["evil.example"]'; }],
    ["a config file", w => { w.config = ".github/buildkitd.toml"; }],
    ["a buildkitd config", w => { w["buildkitd-config-inline"] = '[registry."mirror.gcr.io"]\n  mirrors = ["evil.example"]'; }],
    ["use: false", w => { w.use = false; }],
    ["another BuildKit image", w => { w["driver-opts"] = `image=mirror.gcr.io/moby/buildkit:latest`; }],
    ["another driver option", w => { w["driver-opts"] = `image=${BUILDKIT_IMAGE}\nnetwork=host`; }],
    ["a Buildx version", w => { w.version = "latest"; }],
    ["no inputs", (w, d) => { delete d.jobs["staging-image"].steps[1].with; }],
  ]) {
    const data = structuredClone(loadWorkflow("ci"));
    mutate(buildx(data), data);
    assert.match(buildChecks("ci", data).join(), /setup-buildx-action: with: must be exactly/, label);
  }
});

test("fixtures: cache, output and every other build input is pinned (#488 review 7)", () => {
  const step = (d, job) => stepsOf(d.jobs[job]).find(s => actionName(s.uses ?? "") === "docker/build-push-action").with;
  for (const [label, name, job, mutate] of [
    ["a registry cache source", "ci", "staging-image", w => { w["cache-from"] = "type=registry,ref=ghcr.io/evil/cache"; }],
    ["a second cache source", "image", "build", w => { w["cache-from"] += "\ntype=registry,ref=ghcr.io/evil/cache"; }],
    ["a registry cache target", "branch-images", "build", w => { w["cache-to"] = "type=registry,ref=ghcr.io/evil/cache,mode=max"; }],
    ["another cache scope", "ci", "staging-image", w => { w["cache-from"] = "type=gha,scope=image-linux-amd64"; }],
    ["a pushed output", "image", "build", w => { w.outputs = "type=registry,name=ghcr.io/evil/x"; }],
    ["other tags", "ci", "staging-image", w => { w.tags = "ghcr.io/evil/x:latest"; }],
    ["other platforms", "branch-images", "build", w => { w.platforms = "linux/amd64,linux/arm64"; }],
    ["another SBOM generator", "loadtest-image", "build", w => { w.attests = "type=sbom,generator=docker.io/evil/scanner:latest"; }],
    ["other labels", "loadtest-image", "build", w => { w.labels += "\norg.opencontainers.image.source=https://example.org/evil"; }],
  ]) {
    const data = structuredClone(loadWorkflow(name));
    mutate(step(data, job));
    assert.match(buildChecks(name, data).join(), /IMAGE_BUILDS lists/, label);
  }
});

test("an expression in a build input must come from the job's pinned matrix or the runner", () => {
  const withBuild = mutate => { const data = structuredClone(loadWorkflow("image")); mutate(stepsOf(data.jobs.build).find(s => actionName(s.uses ?? "") === "docker/build-push-action").with); return buildChecks("image", data); };
  const refused = [
    ["a cache scope from env, which an earlier step can write", w => { w["cache-from"] = "type=gha,scope=image-${{ env.PLATFORM_PAIR }}"; }],
    ["a cache target from env", w => { w["cache-to"] = "type=gha,mode=max,scope=image-${{ env.PLATFORM_PAIR }}"; }],
    ["an output from a step", w => { w.outputs = "type=oci,dest=${{ steps.x.outputs.path }}"; }],
    ["a build argument from another job", w => { w["build-args"] = "V=${{ needs.meta.outputs.v }}"; }],
    ["a matrix key the job does not define", w => { w.platforms = "${{ matrix.nokey }}"; }],
    ["an input", w => { w.platforms = "${{ inputs.platform }}"; }],
  ];
  for (const [label, mutate] of refused) assert.match(withBuild(mutate).join(), /is not a pinned value/, label);
  assert.doesNotMatch(withBuild(w => { w.platforms = "${{ matrix.platform }}"; w.outputs = "type=oci,dest=${{ runner.temp }}/image.tar"; }).join(), /is not a pinned value/, "the job's matrix and the runner's temporary directory");
});
