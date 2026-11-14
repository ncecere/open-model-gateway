// What the image build steps build: the Dockerfile, context, stage, inputs and Buildx
// builder (#488). Every other input of these steps is pinned with its job (pinned-jobs.test.mjs).
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { APPROVED_RUST, OTHER_BASE, KNOWN_DOCKERFILES, findDockerfiles, loadWorkflow, parseStrict, repoFiles, root, toData, unknownDockerfileViolations } from "./lib.mjs";
import { BUILDKIT_IMAGE, buildChecks, imageBuildViolations, pinnedJobKeys, stepOf, unpinnedImageJobViolations, withStep } from "./policy.mjs";

const BUILD_JOBS = { ci: "staging-image", image: "build", "branch-images": "build", "loadtest-image": "build" };
const withBuildInputs = (name, mutate) => {
  const data = structuredClone(loadWorkflow(name));
  mutate(stepOf(data.jobs[BUILD_JOBS[name]], "docker/build-push-action").with);
  return buildChecks(name, data);
};

test("every image build step builds a known Dockerfile, context and stage, in a pinned job", () => {
  assert.deepEqual(repoFiles().flatMap(({ file, text }) => imageBuildViolations(file, toData(parseStrict(text, file)))), []);
  assert.deepEqual(repoFiles().flatMap(({ file, text }) => unpinnedImageJobViolations(file, toData(parseStrict(text, file)))), []);
  for (const [name, job] of Object.entries(BUILD_JOBS)) assert.ok(pinnedJobKeys().has(`.github/workflows/${name}.yml: job '${job}'`), `${name} ${job} is pinned`);
  assert.deepEqual(unknownDockerfileViolations(findDockerfiles().map(p => p.slice(root.length))), [], "the walker finds only known Dockerfiles");
  assert.equal(findDockerfiles().length, KNOWN_DOCKERFILES.length, "the walker finds every known Dockerfile");
});

const REFUSED_BUILD_INPUTS = [
    ["target on the publish build", "image", w => { w.target = "build"; }, /target 'build' builds another stage/],
    ["target on the pull-request scan build", "ci", w => { w.target = "build"; }, /target 'build' builds another stage/],
    ["target on the branch build", "branch-images", w => { w.target = "web"; }, /target 'web'/],
    ["an unknown Dockerfile.release", "image", w => { w.file = "Dockerfile.release"; }, /file 'Dockerfile\.release' is not one of the known Dockerfiles/],
    ["a Containerfile", "ci", w => { w.file = "Containerfile"; }, /file 'Containerfile' is not one of the known/],
    ["a *.dockerfile", "loadtest-image", w => { w.file = "deploy/loadtest/release.dockerfile"; }, /not one of the known/],
    ["a Dockerfile under target*", "image", w => { w.file = "target-release/Dockerfile"; }, /in 'target-release', a directory the Dockerfile walker skips/],
    ["a Dockerfile under dist", "ci", w => { w.file = "apps/web/dist/Dockerfile"; }, /in 'dist'/],
    ["a file from an expression", "image", w => { w.file = "${{ inputs.dockerfile }}"; }, /file '\$\{\{ inputs\.dockerfile \}\}' is an expression/],
    ["build-contexts swapping the Rust base", "image", w => { w["build-contexts"] = `${APPROVED_RUST}=docker-image://${OTHER_BASE}`; }, /build-contexts can replace a stage/],
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
];
for (const [label, name, mutate, reason] of REFUSED_BUILD_INPUTS) {
  test(`a build step refuses ${label}`, () => {
    const found = withBuildInputs(name, mutate);
    assert.ok(found.some(v => reason.test(v)), found.join("; "));
  });
}

const REFUSED_EXPRESSIONS = [
  ["a cache scope from env, which an earlier step can write", w => { w["cache-from"] = "type=gha,scope=image-${{ env.PLATFORM_PAIR }}"; }],
  ["a cache target from env", w => { w["cache-to"] = "type=gha,mode=max,scope=image-${{ env.PLATFORM_PAIR }}"; }],
  ["an output from a step", w => { w.outputs = "type=oci,dest=${{ steps.x.outputs.path }}"; }],
  ["a build argument from another job", w => { w["build-args"] = "V=${{ needs.meta.outputs.v }}"; }],
  ["a matrix key the job does not define", w => { w.platforms = "${{ matrix.nokey }}"; }],
  ["an input", w => { w.platforms = "${{ inputs.platform }}"; }],
];
for (const [label, mutate] of REFUSED_EXPRESSIONS) {
  test(`a build input refuses ${label}`, () => assert.match(withBuildInputs("image", mutate).join(), /is not a pinned value/));
}

test("a build input may use the job's own matrix and the runner's temporary directory", () => {
  assert.doesNotMatch(withBuildInputs("image", w => { w.platforms = "${{ matrix.platform }}"; w["cache-from"] = "type=gha,scope=image-${{ matrix.pair }}"; w.outputs = "type=oci,dest=${{ runner.temp }}/image.tar"; }).join(), /is not a pinned value/);
});

test("an image build outside the pinned jobs is refused", () => {
  const ci = loadWorkflow("ci");
  const build = stepOf(ci.jobs["staging-image"], "docker/build-push-action");
  const data = withStep("ci", "web", structuredClone(build));
  assert.match(buildChecks("ci", data).join(), /job 'web': an image build step outside the pinned jobs/);
  for (const [label, step] of [["setup-buildx", stepOf(ci.jobs["staging-image"], "docker/setup-buildx-action")], ["metadata-action", { uses: `docker/metadata-action@${"0".repeat(40)}` }], ["login-action", { uses: `docker/login-action@${"0".repeat(40)}` }]]) {
    assert.match(unpinnedImageJobViolations(".github/workflows/ci.yml", withStep("ci", "web", structuredClone(step))).join(), /job 'web' uses docker\/.* so it must be pinned/, label);
  }
  const writer = structuredClone(ci);
  writer.jobs.web.permissions = { contents: "read", packages: "write" };
  assert.match(unpinnedImageJobViolations(".github/workflows/ci.yml", writer).join(), /job 'web' can write packages, so it must be pinned/);
  const caller = structuredClone(ci);
  caller.jobs.extra = { uses: "./.github/workflows/image.yml" };
  assert.match(unpinnedImageJobViolations(".github/workflows/ci.yml", caller).join(), /job 'extra' calls another workflow, so it must be pinned/);
});

const REFUSED_BUILDX_INPUTS = [
  ["the remote driver", w => { w.driver = "remote"; w.endpoint = "tcp://evil:1234"; }],
  ["an endpoint", w => { w.endpoint = "tcp://evil:1234"; }],
  ["a registry mirror in config-inline", w => { w["config-inline"] = '[registry."docker.io"]\n  mirrors = ["evil.example"]'; }],
  ["a config file", w => { w.config = ".github/buildkitd.toml"; }],
  ["a buildkitd config", w => { w["buildkitd-config-inline"] = '[registry."mirror.gcr.io"]\n  mirrors = ["evil.example"]'; }],
  ["use: false", w => { w.use = false; }],
  ["another BuildKit image", w => { w["driver-opts"] = "image=mirror.gcr.io/moby/buildkit:latest"; }],
  ["another driver option", w => { w["driver-opts"] = `image=${BUILDKIT_IMAGE}\nnetwork=host`; }],
  ["a Buildx version", w => { w.version = "latest"; }],
];
for (const [label, mutate] of REFUSED_BUILDX_INPUTS) {
  test(`setup-buildx-action refuses ${label}`, () => {
    const data = structuredClone(loadWorkflow("ci"));
    mutate(stepOf(data.jobs["staging-image"], "docker/setup-buildx-action").with);
    assert.match(buildChecks("ci", data).join(), /setup-buildx-action: with: must be exactly/);
  });
}

test("setup-buildx-action without inputs is refused", () => {
  const data = structuredClone(loadWorkflow("ci"));
  delete stepOf(data.jobs["staging-image"], "docker/setup-buildx-action").with;
  assert.match(buildChecks("ci", data).join(), /setup-buildx-action: with: must be exactly/);
});

// ---- jobs that can write or publish must be pinned ----

const RETAG = `name: retag
on:
  workflow_run:
    workflows: [image]
    types: [completed]
permissions:
  packages: write
jobs:
  retag:
    runs-on: ubuntu-24.04
    steps:
      - run: |
          echo "\${{ secrets.GITHUB_TOKEN }}" | docker login ghcr.io -u x --password-stdin
          docker pull busybox
          docker tag busybox ghcr.io/ncecere/open-model-gateway:latest-release
          docker push ghcr.io/ncecere/open-model-gateway:latest-release
`;
const retag = () => toData(parseStrict(RETAG, ".github/workflows/retag.yml"));

test("a job that can write through workflow-level permissions must be pinned", () => {
  assert.match(unpinnedImageJobViolations(".github/workflows/retag.yml", retag()).join(), /job 'retag' can write packages, so it must be pinned/);
});

test("a job with write-all permissions must be pinned", () => {
  const data = retag();
  delete data.permissions;
  data.jobs.retag.permissions = "write-all";
  assert.match(unpinnedImageJobViolations(".github/workflows/retag.yml", data).join(), /job 'retag' can write .*, so it must be pinned/);
});

test("a job with no permissions anywhere (the repository default token) must be pinned", () => {
  const data = retag();
  delete data.permissions;
  assert.match(unpinnedImageJobViolations(".github/workflows/retag.yml", data).join(), /job 'retag' can write .*, so it must be pinned/);
});

test("a job that can write contents must be pinned", () => {
  const data = structuredClone(loadWorkflow("ci"));
  data.jobs.web.permissions = { contents: "write" };
  assert.match(unpinnedImageJobViolations(".github/workflows/ci.yml", data).join(), /job 'web' can write contents, so it must be pinned/);
});

const PUBLISHING_RUNS = [
  'echo "${{ secrets.GHCR_PAT }}" | docker login ghcr.io -u x --password-stdin',
  "docker push ghcr.io/ncecere/open-model-gateway:latest-release",
  "docker image push ghcr.io/ncecere/open-model-gateway:x",
  "docker tag busybox ghcr.io/ncecere/open-model-gateway:latest-release",
  'docker buildx imagetools create --tag ghcr.io/ncecere/open-model-gateway:x "${SRC}"',
  "oras push ghcr.io/ncecere/x:y file",
  "oras copy ghcr.io/a/b:c ghcr.io/ncecere/x:y",
  "cosign sign --yes ghcr.io/ncecere/x@sha256:abc",
  "cosign attest --yes --predicate p.json ghcr.io/ncecere/x@sha256:abc",
  "gh release create v9.9.9 dist/x",
  "gh release upload v9.9.9 dist/x",
  "skopeo copy docker://busybox docker://ghcr.io/ncecere/x:y",
];
for (const run of PUBLISHING_RUNS) {
  test(`a job that is not pinned may not publish: ${run.slice(0, 50)}`, () => {
    const data = structuredClone(loadWorkflow("ci"));
    data.jobs.web.steps.push({ run });
    assert.match(buildChecks("ci", data).join(), /publishes, signs or retags an image or release; only a pinned job may/);
  });
}

test("a job that is not pinned may still tag a local image", () => {
  assert.deepEqual(buildChecks("ci", withStep("ci", "web", { run: "docker tag open-model-gateway:staging local/test:1" })), []);
});

for (const scope of ["checks", "statuses"]) {
  test(`a job that can write ${scope} must be pinned (it could post under a required check's name)`, () => {
    const data = structuredClone(loadWorkflow("ci"));
    data.jobs.web.permissions = { contents: "read", [scope]: "write" };
    assert.match(unpinnedImageJobViolations(".github/workflows/ci.yml", data).join(), new RegExp(`job 'web' can write ${scope}, so it must be pinned`));
  });
}
