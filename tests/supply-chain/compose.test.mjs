// Compose files: which ones are checked, which a run step may name, and what their build
// sections may do (#488).
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { APPROVED_RUST, OTHER_BASE, parseStrict, readLocalFromRepo, toData } from "./lib.mjs";
import { buildChecks, checkedComposeFiles, composeBuildViolations, composeDotenvViolations, repoComposeFiles, withStep } from "./policy.mjs";

const COMPOSE_FILE = "deploy/staging/compose.yaml";
const staging = () => toData(parseStrict(readLocalFromRepo(COMPOSE_FILE), COMPOSE_FILE));

test("every Compose build section builds a known Dockerfile from the checkout", () => {
  const files = repoComposeFiles();
  for (const expected of ["compose.yaml", "deploy/demo/compose.yaml", "deploy/loadtest/compose.yaml", "deploy/staging/compose.yaml", "deploy/staging/compose.openai.yaml"]) assert.ok(files.includes(expected), expected);
  const builds = [];
  assert.deepEqual(files.flatMap(file => composeBuildViolations(file, toData(parseStrict(readLocalFromRepo(file), file)), builds)), []);
  // The staging services that share the gateway's build (through the merge key) are read.
  assert.deepEqual([...new Set(builds.map(b => `${b.file} ${b.context} ${b.dockerfile}`))], ["deploy/staging/compose.yaml . Dockerfile"]);
  assert.ok(builds.length >= 2 && builds.some(b => b.service === "gateway"), "the gateway service and the ops services built with it");
});

test("no tracked .env sits beside a checked Compose file", () => {
  assert.deepEqual(composeDotenvViolations(checkedComposeFiles()), []);
  assert.match(composeDotenvViolations(["deploy/staging/compose.yaml"], path => path === "deploy/staging/.env").join(), /deploy\/staging\/\.env sets Compose variables/);
  assert.match(composeDotenvViolations(["compose.yaml"], path => path === ".env").join(), /^\.env sets Compose variables/);
  assert.deepEqual(composeDotenvViolations(["deploy/staging/compose.yaml"], () => false), []);
});

test("every YAML file with Compose services is checked, wherever it is", () => {
  const dir = mkdtempSync(join(tmpdir(), "omg-compose-"));
  try {
    mkdirSync(join(dir, "tools/x"), { recursive: true });
    writeFileSync(join(dir, "tools/x/stack.yml"), "services:\n  app:\n    build:\n      context: .\n      target: build\n");
    writeFileSync(join(dir, "notes.yml"), "title: not compose\n");
    assert.deepEqual(repoComposeFiles(dir), ["tools/x/stack.yml"]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

const UNCHECKED_COMPOSE_RUNS = [
  "docker compose -f /tmp/evil.yml up -d",
  "docker compose -f deploy/other/stack.yml up -d",
  'docker compose --file="$F" up -d',
  "docker compose -f - up -d",
  "docker compose -fevil.yml up -d",
  "docker-compose -f evil.yaml up",
];
for (const run of UNCHECKED_COMPOSE_RUNS) {
  test(`a run step refuses an unchecked Compose file: ${run}`, () => {
    assert.match(buildChecks("ci", withStep("ci", "web", { run })).join(), /Compose file .* is not one of the checked Compose files/);
  });
}

test("a run step may name checked Compose files", () => {
  assert.deepEqual(buildChecks("ci", withStep("ci", "web", { run: "docker compose -f deploy/staging/compose.yaml -f deploy/staging/compose.openai.yaml up -d" })), []);
});

const REFUSED_COMPOSE_EDITS = [
  ["target", d => { d.services.gateway.build.target = "build"; }, /'target' is not allowed/],
  ["additional_contexts swapping the Rust base", d => { d.services.gateway.build.additional_contexts = { [APPROVED_RUST]: `docker-image://${OTHER_BASE}` }; }, /'additional_contexts' is not allowed/],
  ["args with BUILDKIT_SYNTAX", d => { d.services.gateway.build.args = { BUILDKIT_SYNTAX: "docker.io/docker/dockerfile:1" }; }, /'args' is not allowed/],
  ["an inline Dockerfile", d => { d.services.gateway.build.dockerfile_inline = `FROM ${OTHER_BASE}`; }, /'dockerfile_inline' is not allowed/],
  ["an unknown Dockerfile.release", d => { d.services.gateway.build.dockerfile = "Dockerfile.release"; }, /dockerfile 'Dockerfile\.release' is not one of the known/],
  ["a Containerfile", d => { d.services.gateway.build.dockerfile = "Containerfile"; }, /dockerfile 'Containerfile' is not one of the known/],
  ["a Dockerfile in dist", d => { d.services.gateway.build.dockerfile = "apps/web/dist/Dockerfile"; }, /in 'dist'/],
  ["a context in a skipped directory", d => { d.services.gateway.build.context = "../../target"; }, /context '\.\.\/\.\.\/target' is in 'target'/],
  ["a context outside the checkout", d => { d.services.gateway.build.context = "../../.."; }, /leaves the checkout/],
  ["a context from a variable", d => { d.services.gateway.build.context = "${BUILD_CONTEXT:-../..}"; }, /is an expression or variable/],
  ["a remote context", d => { d.services.gateway.build.context = "https://github.com/other/repo.git"; }, /remote context/],
  ["no context", d => { delete d.services.gateway.build.context; }, /context must be one literal/],
  ["extends from another file", d => { d.services.gateway.extends = { file: "../evil/compose.yml", service: "gateway" }; }, /'extends' is not allowed/],
  ["extends in the same file", d => { d.services.ingress.extends = "gateway"; }, /'extends' is not allowed/],
  ["a top-level include", d => { d.include = ["../evil/compose.yml"]; }, /top-level 'include' is not allowed/],
];
for (const [label, mutate, reason] of REFUSED_COMPOSE_EDITS) {
  test(`the staging Compose file refuses ${label}`, () => {
    const data = staging();
    mutate(data);
    assert.match(composeBuildViolations(COMPOSE_FILE, data).join(), reason);
  });
}

test("Compose build sections in other forms", () => {
  assert.match(composeBuildViolations("deploy/staging/compose.openai.yaml", { services: { gateway: { build: { target: "build" } } } }).join(), /'target' is not allowed/, "an override file cannot add a target");
  assert.match(composeBuildViolations("compose.yaml", { services: { x: { build: "deploy/loadtest/dist" } } }).join(), /in 'dist'/, "the string form is a context");
  assert.deepEqual(composeBuildViolations("deploy/loadtest/compose.yaml", { services: { x: { build: { context: "../..", dockerfile: "deploy/loadtest/Dockerfile" } } } }), [], "the load-test Dockerfile from the root");
  assert.deepEqual(composeBuildViolations("deploy/loadtest/compose.yaml", { services: { x: { build: "." } } }), [], "the load-test Dockerfile as its directory's default");
  assert.match(composeBuildViolations("compose.yaml", { services: { x: { build: ["."] } } }).join(), /must be a context path or a mapping/);
});
