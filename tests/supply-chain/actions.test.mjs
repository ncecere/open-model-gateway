// Which actions, local actions and reusable workflows a workflow may use (#488).
import assert from "node:assert/strict";
import { test } from "node:test";
import { SHA, loadWorkflow, parseStrict, readLocalFromRepo, repoFiles, toData } from "./lib.mjs";
import { APPROVED_ACTIONS, actionChecks, actionPolicyViolations, withStep } from "./policy.mjs";

test("every action is approved, and every local action or reusable workflow is one this suite reads", () => {
  const used = new Set();
  assert.deepEqual(repoFiles().flatMap(({ file, text }) => actionPolicyViolations(file, toData(parseStrict(text, file)), { used })), []);
  assert.deepEqual([...APPROVED_ACTIONS.keys()].filter(name => !used.has(name)), [], "every approved action is used");
});

test("a forked build action in a new read-only job is refused", () => {
  const data = structuredClone(loadWorkflow("ci"));
  data.jobs.fork = { permissions: { contents: "read" }, "runs-on": "ubuntu-24.04", steps: [{ uses: `someone/build-push-action@${SHA(1)}`, with: { context: ".", target: "build", "build-contexts": "build=docker-image://alpine:latest" } }] };
  assert.match(actionChecks("ci", data).join(), /job 'fork': someone\/build-push-action is not an approved action/);
});

const REFUSED_STEPS = [
  ["an unlisted third-party action", `someone/tool@${SHA(2)}`, /someone\/tool is not an approved action/],
  ["a kaniko action", `someone/kaniko-action@${SHA(3)}`, /not an approved action/],
  ["a docker:// step", `docker://mirror.gcr.io/library/alpine@sha256:${"a".repeat(64)}`, /docker:\/\/ step/],
  ["a local action under tools", "./tools/build", /'\.\/tools\/build' is outside \.github\/actions/],
  ["a local Docker action under tools", "./tools/docker-action", /outside \.github\/actions/],
  ["a path out of .github/actions", "./.github/actions/../../tools/build", /outside \.github\/actions/],
  ["a parent path", "../build", /outside \.github\/actions/],
  ["a missing local action", "./.github/actions/missing", /'\.\/\.github\/actions\/missing' has no action\.yml/],
];
for (const [label, uses, reason] of REFUSED_STEPS) {
  test(`a step using ${label} is refused`, () => assert.match(actionChecks("ci", withStep("ci", "web", { uses })).join(), reason));
}

test("an approved action whose name looks like an image builder is still refused", () => {
  const approved = new Map([...APPROVED_ACTIONS, ["someone/buildah-build", "fixture"]]);
  assert.match(actionChecks("ci", withStep("ci", "web", { uses: `someone/buildah-build@${SHA(6)}` }), { approved }).join(), /someone\/buildah-build looks like an image builder/);
});

const REFUSED_CALLS = [
  ["a missing reusable workflow", "./.github/workflows/missing.yml"],
  ["a reusable workflow outside .github/workflows", "./tools/release.yml"],
  ["a path out of .github/workflows", "./.github/workflows/../../tools/x.yml"],
];
for (const [label, uses] of REFUSED_CALLS) {
  test(`a job calling ${label} is refused`, () => {
    const data = structuredClone(loadWorkflow("ci"));
    data.jobs.called = { uses };
    assert.match(actionChecks("ci", data).join(), /job 'called': .*not a workflow file of this checkout/);
  });
}

test("a local Docker container action is refused like a docker:// step", () => {
  const file = ".github/actions/setup-node/action.yml";
  const data = toData(parseStrict(readLocalFromRepo(file), file));
  for (const image of [`docker://mirror.gcr.io/library/alpine@sha256:${"a".repeat(64)}`, "docker://alpine:latest", "Dockerfile"]) {
    assert.match(actionPolicyViolations(file, { ...data, runs: { using: "docker", image } }).join(), /a Docker container action runs an image outside the approved actions/, image);
  }
});
