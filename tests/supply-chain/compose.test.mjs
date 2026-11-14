// Compose files: which ones are checked and what their build sections may do (#488).
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { parseStrict, readLocalFromRepo, toData } from "./lib.mjs";
import { buildChecks, composeBuildViolations, repoComposeFiles, withStep } from "./policy.mjs";

test("every Compose build section builds a known Dockerfile from the checkout (#488)", () => {
  const files = repoComposeFiles();
  for (const expected of ["compose.yaml", "deploy/demo/compose.yaml", "deploy/loadtest/compose.yaml", "deploy/staging/compose.yaml", "deploy/staging/compose.openai.yaml"]) assert.ok(files.includes(expected), expected);
  const builds = [];
  const violations = files.flatMap(file => composeBuildViolations(file, toData(parseStrict(readLocalFromRepo(file), file)), builds));
  assert.deepEqual(violations, []);
  // The staging services that share the gateway's build (through the merge key) are read.
  assert.deepEqual([...new Set(builds.map(b => `${b.file} ${b.context} ${b.dockerfile}`))], ["deploy/staging/compose.yaml . Dockerfile"]);
  assert.ok(builds.length >= 2 && builds.some(b => b.service === "gateway"), "the gateway service and the ops services built with it");
});

test("fixtures: Compose files outside the checked set, include and extends are refused (#488 review 8, 9)", () => {
  for (const run of ["docker compose -f /tmp/evil.yml up -d", "docker compose -f deploy/other/stack.yml up -d", 'docker compose --file="$F" up -d', "docker compose -f - up -d", "docker-compose -f evil.yaml up", "docker compose --project-directory /tmp -f compose.yml up"]) {
    assert.match(buildChecks("ci", withStep("ci", "staging-image", { run })).join(), /Compose file .* is not one of the checked Compose files/, run);
  }
  assert.deepEqual(buildChecks("ci", withStep("ci", "staging-image", { run: "docker compose -f deploy/staging/compose.yaml -f deploy/staging/compose.openai.yaml up -d" })), [], "checked files pass");
  // Every YAML file with Compose services is checked, wherever it is.
  const dir = mkdtempSync(join(tmpdir(), "omg-compose-"));
  try {
    mkdirSync(join(dir, "tools/x"), { recursive: true });
    writeFileSync(join(dir, "tools/x/stack.yml"), "services:\n  app:\n    build:\n      context: .\n      target: build\n");
    writeFileSync(join(dir, "notes.yml"), "title: not compose\n");
    assert.deepEqual(repoComposeFiles(dir), ["tools/x/stack.yml"]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  const composeFile = "deploy/staging/compose.yaml";
  const compose = () => toData(parseStrict(readLocalFromRepo(composeFile), composeFile));
  for (const [label, mutate, reason] of [
    ["extends from another file", d => { d.services.gateway.extends = { file: "../evil/compose.yml", service: "gateway" }; }, /'extends' is not allowed/],
    ["extends in the same file", d => { d.services.ingress.extends = "gateway"; }, /'extends' is not allowed/],
    ["a top-level include", d => { d.include = ["../evil/compose.yml"]; }, /top-level 'include' is not allowed/],
  ]) {
    const data = compose();
    mutate(data);
    assert.match(composeBuildViolations(composeFile, data).join(), reason, label);
  }
});
