// What the Spark suite runner runs of .github/workflows/ci.yml and what it does not (#653). The run commands of
// the CI jobs gateway-lint, gateway-tests, gateway-special and web must each be listed in
// deploy/spark/test-coverage.json, as covered (and then present in deploy/spark/test-run.sh) or as not covered with
// a reason. A step added to CI later fails here until the list, and the script if it is covered, follow.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { loadWorkflow, root, stepsOf } from "./lib.mjs";

const coverage = JSON.parse(readFileSync(join(root, "deploy/spark/test-coverage.json"), "utf8"));
const script = readFileSync(join(root, "deploy/spark/test-run.sh"), "utf8");
// Shell glue of the CI steps that install tools: not test commands.
const GLUE = [/^set /, /^bash \.github\/scripts\/fetch-verified\.sh /, /^tar /, /^echo /, /^"\$\{RUNNER_TEMP\}\//];

export function ciCommands(workflow = loadWorkflow("ci"), jobs = coverage.ci_jobs) {
  const found = [];
  for (const name of jobs) {
    assert.ok(workflow.jobs[name], `ci.yml has the job ${name}`);
    for (const step of stepsOf(workflow.jobs[name])) {
      if (typeof step.run !== "string") continue;
      for (const line of step.run.split("\n").map(l => l.trim()).filter(l => l && !l.startsWith("#"))) {
        if (!GLUE.some(re => re.test(line))) found.push({ job: name, line });
      }
    }
  }
  return found;
}

export function problems(commands, list = coverage, scriptText = script) {
  const out = [];
  const covered = new Map(list.covered.map(entry => [entry.ci, entry]));
  const uncovered = new Map(list.uncovered.map(entry => [entry.ci, entry]));
  for (const { job, line } of commands) {
    const entry = covered.get(line);
    if (entry) {
      if (!scriptText.includes(entry.script)) out.push(`${job}: '${line}' is listed as covered but deploy/spark/test-run.sh does not contain '${entry.script}'`);
    } else if (!uncovered.has(line)) {
      out.push(`${job}: '${line}' is in neither the covered nor the uncovered list of deploy/spark/test-coverage.json`);
    }
  }
  const present = new Set(commands.map(c => c.line));
  for (const key of [...covered.keys(), ...uncovered.keys()]) if (!present.has(key)) out.push(`'${key}' is listed but ci.yml no longer runs it`);
  for (const entry of list.uncovered) if (!entry.why || !entry.suites?.length) out.push(`'${entry.ci}' has no reason or no suites`);
  return out;
}

test("every cargo, npm, node and python command of the CI test jobs is covered by the script or listed as not covered", () => {
  assert.deepEqual(problems(ciCommands()), []);
});

test("the covered commands are the script's, character for character", () => {
  for (const entry of coverage.covered) assert.ok(script.includes(entry.script), entry.script);
  for (const suite of ["checks", "lib", "integration", "web"]) assert.ok(coverage.covered.some(e => e.suite.split(" ").includes(suite)), `${suite} has covered commands`);
});

test("the library split is CI's: sorted names, element i to shard i modulo the count, in C order", () => {
  const shards = readFileSync(join(root, ".github/scripts/rust-shards.mjs"), "utf8");
  assert.match(shards, /entries\.filter\(\(_, i\) => i % count === index\)/);
  assert.match(shards, /const compare = \(a, b\) => a < b \? -1 : a > b \? 1 : 0/);
  assert.match(script, /\(NR - 1\) % n == k/);
  assert.match(script, /LC_ALL=C sort/);
  assert.match(shards, /\["test", "-p", target\.packageId, "--all-features", `--\$\{target\.kind\}`/, "the library is run with -p <package> --all-features --lib");
  assert.ok(script.includes("LIB=(cargo test -p open-model-gateway --all-features --lib)"));
});

test("fixtures: a step added to CI, a removed one and a command the script lost are all refused", () => {
  const base = ciCommands();
  assert.deepEqual(problems(base), []);
  const added = [...base, { job: "gateway-special", line: "cargo test -p open-model-gateway --all-features --test brand_new" }];
  assert.match(problems(added).join(), /brand_new' is in neither/);
  const removed = base.filter(c => c.line !== "npm run test:ci");
  assert.match(problems(removed).join(), /npm run test:ci' is listed but ci.yml no longer runs it/);
  assert.match(problems(base, coverage, script.replace("cargo fmt --all -- --check", "cargo fmt --all")).join(), /does not contain 'cargo fmt --all -- --check'/);
  const noReason = structuredClone(coverage);
  noReason.uncovered[0].why = "";
  assert.match(problems(base, noReason).join(), /has no reason/);
});
