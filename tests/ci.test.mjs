import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { parseDocument } from "yaml";

const root = fileURLToPath(new URL("..", import.meta.url));
const shards = () => import("../.github/scripts/rust-shards.mjs");
const loadWorkflow = () => {
  const doc = parseDocument(readFileSync(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8"), { uniqueKeys: true });
  assert.deepEqual([...doc.errors, ...doc.warnings], []);
  return doc.toJS({ maxAliasCount: 100 });
};
const dependencies = ["gateway-lint", "gateway-tests", "gateway-special"];
const runs = job => job.steps.filter(s => s.run).map(s => s.run);
function required(job) {
  assert.equal(job.if, undefined);
  assert.equal(job["continue-on-error"], undefined);
  for (const step of job.steps) {
    assert.equal(step.if, undefined);
    assert.equal(step["continue-on-error"], undefined);
  }
}

function validateGraph(data) {
  assert.deepEqual(data.jobs.gateway.needs, dependencies);
  assert.equal(data.jobs.gateway.if, "always()");
  assert.equal(data.jobs.gateway["continue-on-error"], undefined);
  assert.deepEqual(data.jobs.gateway.steps, [{
    uses: "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
  }, { uses: "./.github/actions/setup-node" }, {
    run: "node .github/scripts/gateway-result.mjs",
    env: { GATEWAY_NEEDS: "${{ toJSON(needs) }}" },
  }]);
  const matrix = data.jobs["gateway-tests"];
  assert.deepEqual(matrix.strategy, { "fail-fast": false, matrix: { shard: [0, 1, 2, 3] } });
  assert.ok(runs(matrix).includes("node .github/scripts/rust-shards.mjs --shard ${{ matrix.shard }} --count 4"));
  assert.deepEqual(data.jobs.image.needs, ["gateway", "web", "browser"]);
  assert.equal(data.jobs["staging-image"].needs, undefined);
  for (const name of dependencies) required(data.jobs[name]);
  for (const name of ["gateway-tests", "gateway-special"]) {
    assert.ok(data.jobs[name].services.postgres);
    assert.ok(data.jobs[name].env.DATABASE_URL);
    assert.ok(runs(data.jobs[name]).includes("npm ci"));
  }
  assert.deepEqual(runs(data.jobs["gateway-lint"]), [
    "cargo fmt --all -- --check", "cargo clippy --workspace --all-targets --all-features -- -D warnings",
  ]);
}

test("CI deduplicates branch pushes, cancels only PRs, and gives every main/tag run its own group", () => {
  const data = loadWorkflow();
  assert.deepEqual(data.on, { push: { branches: ["main"], tags: ["v*"] }, pull_request: null });
  assert.deepEqual(data.concurrency, {
    group: "${{ github.workflow }}-${{ github.event_name == 'pull_request' && format('pr-{0}', github.event.pull_request.number) || format('run-{0}', github.run_id) }}",
    "cancel-in-progress": "${{ github.event_name == 'pull_request' }}",
  });
});

test("gateway required check depends on all mandatory shards/lint/special, staging is independent", () => validateGraph(loadWorkflow()));

test("workflow validator refuses missing/extra/optional shards and weakened dependencies", () => {
  const original = loadWorkflow();
  validateGraph(original);
  const mutations = [
    d => d.jobs["gateway-tests"].strategy.matrix.shard.pop(),
    d => d.jobs["gateway-tests"].strategy.matrix.shard.push(4),
    d => { d.jobs["gateway-tests"].strategy["fail-fast"] = true; },
    d => { d.jobs["gateway-tests"]["continue-on-error"] = true; },
    d => { d.jobs["gateway-tests"].if = "false"; },
    d => { d.jobs["gateway-tests"].steps.at(-1).if = "false"; },
    d => d.jobs.gateway.needs.pop(),
    d => d.jobs.gateway.needs.push("extra"),
    d => { d.jobs.gateway.if = "success()"; },
  ];
  for (const mutate of mutations) {
    const data = structuredClone(original);
    mutate(data);
    assert.throws(() => validateGraph(data));
  }
});

test("special probes retain sequential ACL, exact contention/terminal, Docker and backup coverage", () => {
  const job = loadWorkflow().jobs["gateway-special"];
  assert.ok(job, "missing special coverage job");
  assert.deepEqual(runs(job), [
    "npm ci",
    "cargo test -p open-model-gateway --all-features --test filestore_s3",
    "cargo test -p open-model-gateway --all-features --test runtime_privileges",
    "cargo test -p open-model-gateway --all-features --test runtime_privileges -- --ignored",
    "cargo test -p open-model-gateway --all-features --lib management::tests::authz_matrix_tests::key_issuance_contention_tests::key_issuance_contention_measurement -- --ignored --exact",
    "cargo test -p open-model-gateway --all-features --lib providers::anthropic::terminal_tests::terminal_fidelity_official_sdks_and_priced_http_evidence -- --ignored --exact",
    "sudo apt-get update && sudo apt-get install -y age",
    "cargo build -p open-model-gateway --all-features --bin open-model-gateway",
    "python3 tests/backup.test.py",
  ]);
  assert.equal(job.steps.find(s => s.run?.includes("--test filestore_s3")).env.FILESTORE_REQUIRE_DOCKER, "1");
  assert.ok(runs(loadWorkflow().jobs.web).includes("npm run test:ci"));
});

test("aggregate executable accepts only the exact dependency set with every result successful", () => {
  const run = needs => spawnSync(process.execPath, [".github/scripts/gateway-result.mjs"], {
    cwd: root, env: { ...process.env, GATEWAY_NEEDS: JSON.stringify(needs) }, encoding: "utf8",
  });
  const good = Object.fromEntries(dependencies.map(name => [name, { result: "success" }]));
  assert.equal(run(good).status, 0);
  for (const name of dependencies) {
    for (const result of ["failure", "cancelled", "skipped", "", null, "SUCCESS"]) {
      const needs = structuredClone(good);
      needs[name].result = result;
      assert.notEqual(run(needs).status, 0, `${name}: ${result}`);
    }
    const missing = structuredClone(good);
    delete missing[name];
    assert.notEqual(run(missing).status, 0);
  }
  for (const bad of [null, [], {}, { ...good, extra: { result: "success" } }]) assert.notEqual(run(bad).status, 0);
  const malformed = spawnSync(process.execPath, [".github/scripts/gateway-result.mjs"], {
    cwd: root, env: { ...process.env, GATEWAY_NEEDS: "{" }, encoding: "utf8",
  });
  assert.notEqual(malformed.status, 0);
});

// Synthetic Cargo/libtest output only: these tests must never build or run Rust.
const target = (name, kind = ["test"]) => ({ name, kind, test: true });
const gateway = "path+file:///repo/apps/gateway#open-model-gateway@0.9.0";
const fixtures = () => ({
  workspace_members: [gateway, "path+file:///repo/tools/new#new@0.1.0"],
  packages: [
    { id: gateway, name: "open-model-gateway", targets: [target("open_model_gateway", ["lib"]), target("open-model-gateway", ["bin"]), target("filestore_s3"), target("runtime_privileges"), target("sdk"), { ...target("build-script-build", ["custom-build"]), test: false }] },
    { id: "path+file:///repo/tools/new#new@0.1.0", name: "new", targets: [target("new", ["proc-macro"]), target("fresh")] },
  ],
});
const artifacts = metadata => metadata.packages.flatMap(p => p.targets.filter(t => t.test).map(t => ({
  reason: "compiler-artifact", package_id: p.id, target: t, profile: { test: true }, executable: `/unused/${t.name}`,
})));
const cargoOutput = messages => [...messages, { reason: "build-finished", success: true }].map(m => JSON.stringify(m)).join("\n") + "\n";
const listing = (entries, pretty = false) => entries.map(e => `${e.name}: ${e.kind}\n`).join("") + (pretty ? `${entries.length ? "\n" : ""}${entries.filter(e => e.kind === "test").length} tests, ${entries.filter(e => e.kind === "benchmark").length} benchmarks\n` : "");
const entry = (name, kind = "test") => ({ name, kind });

test("structured Cargo artifacts reconcile every metadata test target, including new targets and proc macros", async () => {
  const { discoverTargets } = await shards();
  const meta = fixtures();
  const messages = artifacts(meta);
  assert.equal(discoverTargets(meta, cargoOutput(messages)).length, messages.length);
  for (const bad of [messages.slice(1), [...messages, messages[0]], [...messages, { ...messages[0], target: target("unexpected") }]]) {
    assert.throws(() => discoverTargets(meta, cargoOutput(bad)));
  }
  assert.throws(() => discoverTargets(meta, "not JSON\n"));
  assert.throws(() => discoverTargets(meta, cargoOutput(messages).replace('"success":true', '"success":false')));
  assert.throws(() => discoverTargets(meta, messages.map(m => JSON.stringify(m)).join("\n") + "\n"));
  assert.throws(() => discoverTargets(meta, cargoOutput(messages.map((m, i) => i ? m : { ...m, executable: null }))));
  const unsupported = fixtures();
  unsupported.packages[1].targets.push(target("mystery", ["unknown"]));
  assert.throws(() => discoverTargets(unsupported, cargoOutput(artifacts(unsupported))));
});

test("new metadata kinds and new tests join coverage automatically without a target allowlist", async () => {
  const { discoverTargets, planShards, targetArgs } = await shards();
  const meta = fixtures();
  meta.packages[1].targets.push(target("demo", ["example"]), target("timing", ["bench"]), target("multi", ["rlib", "cdylib"]));
  const targets = discoverTargets(meta, cargoOutput(artifacts(meta))).map(t => ({ ...t, entries: Array.from({ length: 9 }, (_, i) => entry(`fresh::test_${i}`)), ignored: [] }));
  const plan = planShards(targets, 4);
  for (const t of targets.filter(t => t.packageName === "new")) {
    const owners = plan.shards.flat().filter(s => s.key === t.key);
    assert.equal(owners.length, 1);
    assert.equal(owners[0].entries.length, 9);
    assert.deepEqual(targetArgs(t).slice(4), t.kind === "lib" ? ["--lib"] : [`--${t.kind}`, t.name]);
  }
});

test("listing parses complete names and validates footer, rejecting malformed/duplicate/unsupported output", async () => {
  const { parseListing } = await shards();
  const entries = [entry("module::one"), entry("module::one_more"), entry("module::bench", "benchmark")];
  assert.deepEqual(parseListing(listing(entries)), entries);
  assert.deepEqual(parseListing(listing(entries, true), true), entries);
  assert.deepEqual(parseListing(""), []);
  assert.deepEqual(parseListing("0 tests, 0 benchmarks\n", true), []);
  for (const bad of ["m::x: test", "m::x: test\nnoise\n", "m::x: ignored\n", "m::x: test\nm::x: test\n", "bad name: test\n", ": bad: test\n", "-option: test\n", "m:x: test\n", "\u001bx: test\n", "m::x: test\n\n1 test, 0 benchmarks\n"]) {
    assert.throws(() => parseListing(bad), bad);
  }
  for (const bad of [listing(entries), listing(entries, true).replace("2 tests", "9 tests"), listing(entries, true) + "1 test, 0 benchmarks\n", "m::x: test\n1 test, 0 benchmarks\n"]) {
    assert.throws(() => parseListing(bad, true), bad);
  }
});

test("every listed test belongs to exactly one shard or explicit special target, including future tests", async () => {
  const { discoverTargets, planShards, TERMINAL } = await shards();
  const meta = fixtures();
  const targets = discoverTargets(meta, cargoOutput(artifacts(meta))).map(t => ({
    ...t, entries: t.name === "open_model_gateway" ? [entry(TERMINAL), ...Array.from({ length: 19 }, (_, i) => entry(`tests::case_${String(i).padStart(2, "0")}`))] : [entry("ordinary"), entry("live_opt_in")],
    ignored: t.name === "open_model_gateway" ? [TERMINAL] : ["live_opt_in"],
  }));
  const plan = planShards(targets, 4);
  const ownership = new Map();
  for (const [shard, selections] of plan.shards.entries()) for (const selection of selections) {
    for (const e of selection.entries) {
      const key = `${selection.key}/${e.name}`;
      assert.ok(!ownership.has(key));
      ownership.set(key, shard);
    }
    assert.ok(!["filestore_s3", "runtime_privileges"].includes(selection.name));
  }
  for (const t of targets) {
    if (["filestore_s3", "runtime_privileges"].includes(t.name)) {
      assert.ok(plan.special.some(s => s.key === t.key));
    } else for (const e of t.entries) assert.ok(ownership.has(`${t.key}/${e.name}`));
  }
  const lib = targets.find(t => t.name === "open_model_gateway");
  lib.entries.forEach((e, i) => assert.equal(ownership.get(`${lib.key}/${e.name}`), i % 4));
  const terminalShard = plan.shards[ownership.get(`${lib.key}/${TERMINAL}`)].find(t => t.key === lib.key);
  assert.ok(terminalShard.ignored.includes(TERMINAL), "terminal test remains default-ignored, not unignored by sharding");
  for (const selections of plan.shards) for (const selected of selections) {
    assert.deepEqual(selected.ignored, selected.entries.filter(e => targets.find(t => t.key === selected.key).ignored.includes(e.name)).map(e => e.name).sort());
  }
  assert.deepEqual(planShards([...targets].reverse().map(t => ({ ...t, entries: [...t.entries].reverse() })), 4), plan);
  assert.throws(() => planShards(targets.filter(t => t.name !== "runtime_privileges"), 4));
  assert.throws(() => planShards(targets.map(t => ({ ...t, entries: [] })), 4), /empty/i);
});

test("Cargo target invocations preserve package identity, exact multiple filters, ignored defaults and count accounting", async () => {
  const { discoverTargets, targetArgs, validateRun } = await shards();
  const meta = fixtures();
  const ts = discoverTargets(meta, cargoOutput(artifacts(meta)));
  for (const t of ts) {
    assert.deepEqual(targetArgs(t).slice(0, 5), ["test", "-p", t.packageId, "--all-features", t.kind === "lib" ? "--lib" : `--${t.kind}`]);
  }
  const selected = { ...ts.find(t => t.kind === "lib" && t.packageId === gateway), entries: [entry("m::one"), entry("m::one_more"), entry("ignored")], ignored: ["ignored"], total: 10 };
  const good = "\nrunning 3 tests\ntest m::one ... ok\ntest m::one_more ... ok\ntest ignored ... ignored\n\ntest result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 7 filtered out; finished in 0.01s\n\n";
  assert.deepEqual(validateRun(good, selected), { passed: 2, ignored: 1, measured: 0 });
  for (const bad of ["", good.replace("2 passed", "0 passed"), good.replace("1 ignored", "0 ignored"), good.replace("7 filtered", "9 filtered"), good.replace("0 failed", "1 failed"), good.replace("0.01s", "...s"), good + good, good + "test result: malformed\n"]) assert.throws(() => validateRun(bad, selected));
  assert.deepEqual(validateRun("running 1 test\ntest result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n", { entries: [entry("live")], ignored: ["live"], total: 1 }), { passed: 0, ignored: 1, measured: 0 });
});

function syntheticCargo({ corrupt = "", failAt = -1, probe = "", probeMutation = "" } = {}) {
  const meta = fixtures();
  const calls = [];
  const run = async args => {
    calls.push(args);
    if (calls.length === failAt) throw new Error("cargo failed");
    if (args[0] === "metadata") return JSON.stringify(meta);
    if (args.includes("--no-run")) return cargoOutput(artifacts(meta));
    if (args.includes("--doc")) return "doctests passed\n";
    const name = args.includes("--lib") ? "open_model_gateway" : args[args.indexOf("--test") + 1] ?? "binary";
    const terminal = "providers::anthropic::terminal_tests::terminal_fidelity_official_sdks_and_priced_http_evidence";
    const keyContention = "management::tests::authz_matrix_tests::key_issuance_contention_tests::key_issuance_contention_measurement";
    let entries = name === "open_model_gateway" ? Array.from({ length: 12 }, (_, i) => entry(i === 11 ? terminal : i === 10 ? keyContention : `m::test_${i}`)) : [entry("normal"), entry("live")];
    let ignored = entries.filter(e => e.name === "live" || e.name === terminal || e.name === keyContention);
    if (name === "open_model_gateway") {
      const mutate = items => items.flatMap(e => e.name !== probe ? [e]
        : probeMutation === "remove" ? []
        : probeMutation === "rename" ? [{ ...e, name: `${e.name}_renamed` }]
        : probeMutation === "benchmark" ? [{ ...e, kind: "benchmark" }] : [e]);
      entries = mutate(entries);
      ignored = mutate(ignored).filter(e => probeMutation !== "unignore" || e.name !== probe);
    }
    if (args.includes("--list")) {
      if (corrupt === "listing") return "garbage\n";
      return listing(args.includes("--ignored") ? ignored : entries, args.includes("pretty"));
    }
    const filters = args.slice(args.indexOf("--exact") + 1);
    const chosen = filters.length ? entries.filter(e => filters.includes(e.name)) : entries;
    const countIgnored = chosen.filter(e => ignored.some(t => t.name === e.name)).length;
    const passed = corrupt === "zero" ? 0 : chosen.length - countIgnored;
    return `running ${chosen.length} tests\ntest result: ok. ${passed} passed; 0 failed; ${countIgnored} ignored; 0 measured; ${entries.length - chosen.length} filtered out; finished in 0.01s\n`;
  };
  return { run, calls };
}

test("orchestrator runs selected lib names once after -- --exact, runs doctests on shard0 only, and propagates refusals", async () => {
  const { runShard } = await shards();
  for (let shard = 0; shard < 4; shard++) {
    const fake = syntheticCargo();
    const logs = [];
    await runShard(shard, 4, fake.run, text => logs.push(text));
    assert.equal(fake.calls.filter(c => c.includes("--doc")).length, shard === 0 ? 1 : 0);
    const libRuns = fake.calls.filter(c => c.includes("--lib") && !c.includes("--list") && c[c.indexOf("-p") + 1] === gateway);
    assert.equal(libRuns.length, 1);
    const args = libRuns[0];
    assert.deepEqual(args.slice(args.indexOf("--"), args.indexOf("--exact") + 1), ["--", "--exact"]);
    assert.equal(args.slice(args.indexOf("--exact") + 1).length, 3, "multiple full exact names, no substring filtering");
    assert.ok(!args.includes("--ignored") && !args.includes("--include-ignored"));
    assert.ok(!fake.calls.some(c => !c.includes("--list") && (c.includes("filestore_s3") || c.includes("runtime_privileges"))));
    assert.ok(logs.some(l => /passed=.*ignored=.*duration=/.test(l)));
  }
  for (const options of [{ corrupt: "listing" }, { corrupt: "zero" }, { failAt: 1 }, { failAt: 2 }, { failAt: 3 }]) {
    await assert.rejects(() => runShard(0, 4, syntheticCargo(options).run, () => {}));
  }
  for (const [index, count] of [[-1, 4], [4, 4], [0, 0], [0.5, 4], [0, 1.5], ["0", 4]]) {
    await assert.rejects(() => runShard(index, count, async () => { throw new Error("must not run"); }, () => {}), /shard/i);
  }
});

test("orchestrator rejects each missing, renamed, unignored or non-test dedicated library probe before executing tests", async () => {
  const { runShard } = await shards();
  const probes = [
    "management::tests::authz_matrix_tests::key_issuance_contention_tests::key_issuance_contention_measurement",
    "providers::anthropic::terminal_tests::terminal_fidelity_official_sdks_and_priced_http_evidence",
  ];
  for (const probe of probes) for (const probeMutation of ["remove", "rename", "unignore", "benchmark"]) {
    // Generate all listings from the same mutation: pretty counts stay valid
    // and ignored remains a subset, so only the dedicated-probe guard can refuse.
    const fake = syntheticCargo({ probe, probeMutation });
    await assert.rejects(() => runShard(0, 4, fake.run, () => {}), error => {
      assert.match(error.message, /Dedicated/);
      assert.ok(error.message.includes(probe), error.message);
      return true;
    }, `${probeMutation}: ${probe} must be refused`);
    assert.ok(!fake.calls.some(c => c.includes("--exact")), `${probeMutation}: ${probe} must fail before test execution`);
  }
});

test("shard CLI refuses invalid arguments without invoking Cargo", () => {
  for (const args of [[], ["--shard", "4", "--count", "4"], ["--shard", "0x0", "--count", "4"], ["--shard", "0", "--count", "4", "--ignored"], ["--shard", "0", "--count", "0"]]) {
    const result = spawnSync(process.execPath, [".github/scripts/rust-shards.mjs", ...args], { cwd: root, encoding: "utf8" });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /Invalid shard|usage:/);
  }
});

test("real CLI subprocess boundary accepts clean Cargo progress and refuses nonzero/invalid/zero execution at every stage", () => {
  const dir = mkdtempSync(join(tmpdir(), "rust-shards-fixture-"));
  try {
    // A repo-test-owned stand-in, never a real Cargo invocation. Run the actual
    // CLI/spawn/status handling without touching target/ or compiling any Rust.
    const stub = `#!${process.execPath}
import { appendFileSync } from 'node:fs';
const gateway = ${JSON.stringify(gateway)};
const target = ${target.toString()};
const entry = ${entry.toString()};
const fixtures = ${fixtures.toString()};
const artifacts = ${artifacts.toString()};
const cargoOutput = ${cargoOutput.toString()};
const listing = ${listing.toString()};
const syntheticCargo = ${syntheticCargo.toString()};
const args = process.argv.slice(4); // Cargo --color never precedes helper arguments.
appendFileSync(process.env.FAKE_CARGO_CALLS, JSON.stringify(args) + '\\n');
console.error('   Compiling fixture; Finished test profile');
const stage = args[0] === 'metadata' ? 'metadata' : args.includes('--no-run') ? 'compile' : args.includes('--list') ? 'list' : args.includes('--doc') ? 'doc' : 'run';
if (process.env.FAKE_CARGO_MODE === stage) { console.log('synthetic compiler diagnostic'); process.exit(101); }
let output = await syntheticCargo({ corrupt: process.env.FAKE_CARGO_MODE }).run(args);
if (process.env.FAKE_CARGO_MODE === 'mismatch' && args.includes('pretty')) output = output.replace('m::test_0: test', 'm::changed: test');
if (process.env.FAKE_CARGO_MODE === 'ignored' && args.includes('--ignored')) output = 'unknown: test\\n';
if (process.env.FAKE_CARGO_MODE === 'terminal' && args.includes('--ignored')) output = '';
process.stdout.write(output);
`;
    writeFileSync(join(dir, "cargo"), stub);
    chmodSync(join(dir, "cargo"), 0o755);
    const invoke = mode => spawnSync(process.execPath, [".github/scripts/rust-shards.mjs", "--shard", "0", "--count", "4"], {
      cwd: root, encoding: "utf8", env: { ...process.env, PATH: dir, FAKE_CARGO_CALLS: join(dir, "calls"), FAKE_CARGO_MODE: mode },
    });
    const good = invoke("");
    assert.equal(good.status, 0, good.stderr);
    assert.match(good.stdout, /default-ignored=1/);
    assert.match(good.stdout, /listed=12 default-ignored=2/);
    assert.match(good.stdout, /workspace doctests: owner=shard0/);
    assert.match(good.stderr, /Compiling fixture/);
    for (const mode of ["compile", "metadata", "list", "run", "doc", "zero", "listing", "mismatch", "ignored", "terminal"]) {
      const result = invoke(mode);
      assert.notEqual(result.status, 0, mode);
      assert.match(result.stderr, /failed|Malformed|differ|counts|subset|terminal/, mode);
      if (mode === "compile") assert.match(result.stdout + result.stderr, /synthetic compiler diagnostic/, "failed Cargo stdout must remain visible");
    }
    const calls = readFileSync(join(dir, "calls"), "utf8").trim().split("\n").map(line => JSON.parse(line));
    assert.ok(calls.some(c => c[0] === "metadata"));
    assert.ok(calls.some(c => c.includes("--doc")));
    assert.ok(calls.every(c => !c.includes("--include-ignored")));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
