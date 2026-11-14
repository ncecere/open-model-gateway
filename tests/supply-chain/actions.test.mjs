// Which actions and local actions a workflow may use (#488).
import assert from "node:assert/strict";
import { join } from "node:path";
import { test } from "node:test";
import { SHA, loadWorkflow, parseStrict, repoFiles, toData } from "./lib.mjs";
import { APPROVED_ACTIONS, RUN_DOCKER_ENV, actionChecks, actionPolicyViolations, buildStepsOf, containerPinViolations, withStep } from "./policy.mjs";

test("every action is approved, and every local action or reusable workflow is one this suite reads (#488 review)", () => {
  const used = new Set();
  assert.deepEqual(repoFiles().flatMap(({ file, text }) => actionPolicyViolations(file, toData(parseStrict(text, file)), { used })), []);
  assert.deepEqual([...APPROVED_ACTIONS.keys()].filter(name => !used.has(name)), [], "every approved action is used");
  const scripts = repoFiles().flatMap(({ file, text }) => buildStepsOf(file, toData(parseStrict(text, file))).map(([where, step]) => [where, step.run ?? ""]));
  assert.deepEqual([...RUN_DOCKER_ENV.keys()].filter(where => !scripts.some(([w, run]) => w === where && run.includes("DOCKER_CONFIG"))), [], "every RUN_DOCKER_ENV job sets DOCKER_CONFIG");
});

test("fixtures: an action that is not approved, or a builder under another name, is refused (#488 review 1)", () => {
  const forked = structuredClone(loadWorkflow("ci"));
  forked.jobs.fork = { permissions: { contents: "read" }, "runs-on": "ubuntu-24.04", steps: [{ uses: `someone/build-push-action@${SHA(1)}`, with: { context: ".", target: "build", "build-contexts": "build=docker-image://alpine:latest" } }] };
  assert.match(actionChecks("ci", forked).join(), /job 'fork': someone\/build-push-action is not an approved action/, "a forked build action in a new read-only job");
  for (const [label, uses, reason] of [
    ["an unlisted third-party action", `someone/tool@${SHA(2)}`, /someone\/tool is not an approved action/],
    ["kaniko", `someone/kaniko-action@${SHA(3)}`, /not an approved action/],
    ["docker/build-push-action at another commit", `docker/build-push-action@${SHA(4)}`, /docker\/build-push-action must be pinned to c3c9e263/],
    ["docker/setup-buildx-action at another commit", `docker/setup-buildx-action@${SHA(5)}`, /docker\/setup-buildx-action must be pinned to f87e5991/],
    ["a docker:// step", `docker://mirror.gcr.io/library/alpine@sha256:${"a".repeat(64)}`, /docker:\/\/ step/],
  ]) assert.match(actionChecks("ci", withStep("ci", "web", { uses })).join(), reason, label);
  // Even on the approved list, only docker/build-push-action may be an image builder.
  const approved = new Map([...APPROVED_ACTIONS, ["someone/buildah-build", "fixture"]]);
  assert.match(actionChecks("ci", withStep("ci", "web", { uses: `someone/buildah-build@${SHA(6)}` }), { approved }).join(), /someone\/buildah-build looks like an image builder/);
});

test("fixtures: a local action or reusable workflow the suite does not read is refused (#488 review 2, 3)", () => {
  for (const [label, uses, reason] of [
    ["a local action under tools", "./tools/build", /'\.\/tools\/build' is outside \.github\/actions/],
    ["a local Docker action with docker://alpine:latest", "./tools/docker-action", /outside \.github\/actions/],
    ["a path out of .github/actions", "./.github/actions/../../tools/build", /outside \.github\/actions/],
    ["a parent path", "../build", /outside \.github\/actions/],
    ["a missing local action", "./.github/actions/missing", /'\.\/\.github\/actions\/missing' has no action\.yml/],
  ]) assert.match(actionChecks("ci", withStep("ci", "web", { uses })).join(), reason, label);
  // What such a Docker action would have run is refused where the suite reads it.
  assert.match(containerPinViolations("tools/docker-action/action.yml", { runs: { using: "docker", image: "docker://alpine:latest" } }).join(), /alpine:latest is not pinned by digest/);
  for (const [label, uses] of [["a missing reusable workflow", "./.github/workflows/missing.yml"], ["a reusable workflow outside .github/workflows", "./tools/release.yml"], ["a path out of .github/workflows", "./.github/workflows/../../tools/x.yml"]]) {
    const data = structuredClone(loadWorkflow("ci"));
    data.jobs.called = { uses };
    assert.match(actionChecks("ci", data).join(), /job 'called': .*not a workflow file of this checkout/, label);
  }
});
