// Repo-owned libtest sharding: no target-directory scans, artifact handoffs,
// shell interpolation, ignored-test sweeps, or third-party runners.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";

export const TERMINAL = "providers::anthropic::terminal_tests::terminal_fidelity_official_sdks_and_priced_http_evidence";
export const KEY_CONTENTION = "management::tests::authz_matrix_tests::key_issuance_contention_tests::key_issuance_contention_measurement";
const DEDICATED_LIB_PROBES = [TERMINAL, KEY_CONTENTION];
const SPECIAL = ["filestore_s3", "runtime_privileges"];
const root = fileURLToPath(new URL("../..", import.meta.url));
const compare = (a, b) => a < b ? -1 : a > b ? 1 : 0;
const fail = message => { throw new Error(message); };

function identity(packageId, target) {
  if (typeof packageId !== "string" || !packageId || typeof target?.name !== "string" || !target.name
      || !Array.isArray(target.kind) || !target.kind.length) fail("Malformed Cargo target identity");
  const libs = ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];
  let kind;
  if (target.kind.every(k => libs.includes(k))) kind = "lib";
  else if (target.kind.length === 1 && ["bin", "test", "example", "bench"].includes(target.kind[0])) kind = target.kind[0];
  else fail(`Unsupported Cargo test target kind: ${JSON.stringify(target.kind)}`);
  return { packageId, name: target.name, kind, key: JSON.stringify([packageId, [...target.kind].sort(), target.name]) };
}

export function discoverTargets(metadata, output) {
  if (!Array.isArray(metadata?.workspace_members) || !Array.isArray(metadata.packages)
      || new Set(metadata.workspace_members).size !== metadata.workspace_members.length) fail("Malformed Cargo workspace metadata");
  const expected = new Map();
  for (const id of metadata.workspace_members) {
    const packages = metadata.packages.filter(p => p.id === id);
    if (packages.length !== 1 || !Array.isArray(packages[0].targets)) fail(`Missing/duplicate workspace package: ${id}`);
    for (const target of packages[0].targets) {
      if (typeof target.test !== "boolean") fail("Missing Cargo target.test flag");
      if (!target.test) continue;
      const item = { ...identity(id, target), packageName: packages[0].name };
      if (expected.has(item.key)) fail(`Duplicate metadata target: ${item.key}`);
      expected.set(item.key, item);
    }
  }
  const found = new Set();
  let finished = false;
  for (const line of completeLines(output)) {
    const message = JSON.parse(line);
    if (finished) fail("Cargo output after build-finished");
    if (message.reason === "build-finished") {
      if (message.success !== true) fail("Cargo test compilation failed");
      finished = true;
    } else if (message.reason === "compiler-artifact" && message.profile?.test === true && message.executable !== null) {
      if (typeof message.executable !== "string" || !message.executable) fail("Missing Cargo test executable");
      const item = identity(message.package_id, message.target);
      if (!expected.has(item.key) || found.has(item.key)) fail(`Unexpected/duplicate Cargo test executable: ${item.key}`);
      found.add(item.key);
    } else if (!["compiler-artifact", "compiler-message", "build-script-executed"].includes(message.reason)) {
      fail(`Unsupported Cargo message: ${message.reason}`);
    }
  }
  if (!finished || !expected.size || found.size !== expected.size) {
    fail(`Missing Cargo test executables: ${[...expected.keys()].filter(key => !found.has(key)).join(", ")}`);
  }
  return [...expected.values()].sort((a, b) => compare(a.key, b.key));
}

function completeLines(text) {
  if (typeof text !== "string" || (text && !text.endsWith("\n")) || /[\r\x00]/.test(text)) fail("Incomplete/malformed output lines");
  return text ? text.slice(0, -1).split("\n") : [];
}

export function parseListing(text, footer = false) {
  const lines = completeLines(text);
  let summary;
  if (footer) {
    summary = lines.pop()?.match(/^(\d+) tests?, (\d+) benchmarks?$/);
    if (!summary) fail("Missing/malformed libtest listing footer");
    if (lines.length && lines.pop() !== "") fail("Missing listing footer separator");
  }
  const seen = new Set();
  const entries = lines.map(line => {
    // Names remain whole. Only Rust identifier paths are supported; unfamiliar
    // harnesses fail rather than being silently omitted or loosely split on ':'.
    const match = line.match(/^((?:r#)?[\p{ID_Start}_][\p{ID_Continue}]*(?:::(?:r#)?[\p{ID_Start}_][\p{ID_Continue}]*)*): (test|benchmark)$/u);
    if (!match || seen.has(match[1])) fail(`Malformed/duplicate libtest listing: ${line}`);
    seen.add(match[1]);
    return { name: match[1], kind: match[2] };
  });
  if (summary && (Number(summary[1]) !== entries.filter(e => e.kind === "test").length
      || Number(summary[2]) !== entries.filter(e => e.kind === "benchmark").length)) fail("Libtest listing footer count mismatch");
  return entries;
}

export function targetArgs(target) {
  return ["test", "-p", target.packageId, "--all-features", `--${target.kind}`, ...(target.kind === "lib" ? [] : [target.name])];
}

function validateShard(index, count) {
  if (!Number.isSafeInteger(index) || !Number.isSafeInteger(count) || count < 1 || index < 0 || index >= count) fail("Invalid shard index/count");
}

export function planShards(targets, count) {
  validateShard(0, count);
  const shards = Array.from({ length: count }, () => []);
  const special = [];
  let other = 0;
  let libraries = 0;
  for (const target of [...targets].sort((a, b) => compare(a.key, b.key))) {
    const entries = [...target.entries].sort((a, b) => compare(a.name, b.name));
    const whole = { ...target, entries, ignored: [...target.ignored].sort(), total: entries.length };
    if (target.packageName === "open-model-gateway" && target.kind === "test" && SPECIAL.includes(target.name)) {
      special.push(whole);
    } else if (target.packageName === "open-model-gateway" && target.kind === "lib") {
      libraries++;
      for (let index = 0; index < count; index++) {
        const chosen = entries.filter((_, i) => i % count === index);
        if (!chosen.length) fail(`Empty gateway library selection for shard ${index}`);
        shards[index].push({ ...whole, entries: chosen, ignored: whole.ignored.filter(name => chosen.some(e => e.name === name)) });
      }
    } else {
      shards[other++ % count].push(whole);
    }
  }
  if (libraries !== 1 || special.length !== SPECIAL.length || SPECIAL.some(name => !special.some(t => t.name === name))) fail("Missing/ambiguous gateway library or special-owned targets");
  for (const [index, selected] of shards.entries()) if (!selected.some(t => t.entries.length)) fail(`Empty shard ${index}`);
  return { shards, special };
}

export function validateRun(output, selected) {
  const lines = completeLines(output);
  const summaries = lines.filter(line => line.startsWith("test result:")).map(line => line.match(/^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out; finished in \d+\.\d{2}s$/));
  const starts = lines.filter(line => line.startsWith("running ")).map(line => line.match(/^running (\d+) tests?$/));
  if (summaries.length !== 1 || starts.length !== 1 || !summaries[0] || !starts[0]) fail("Missing/malformed/duplicate libtest run summary");
  const [, status, passed, failed, ignored, measured, filtered] = summaries[0];
  if (status !== "ok" || Number(failed) !== 0 || Number(starts[0][1]) !== selected.entries.length
      || Number(passed) !== selected.entries.length - selected.ignored.length
      || Number(ignored) !== selected.ignored.length || Number(measured) !== 0
      || Number(filtered) !== selected.total - selected.entries.length) fail("Libtest execution counts do not match shard selection");
  return { passed: Number(passed), ignored: Number(ignored), measured: Number(measured) };
}

// https://doc.rust-lang.org/rustc/tests/index.html#filters and #--exact:
// libtest accepts multiple positional FILTERS with --exact (also cli.rs usage
// and filter_tests in library/test/src/lib.rs). Keeping names after `--` avoids
// Cargo's single-filter parser. Count checks also refuse a runner that drops any.
export async function runShard(index, count, run = runCargo, log = console.log) {
  validateShard(index, count);
  const started = performance.now();
  const output = await run(["test", "--workspace", "--all-features", "--no-run", "--message-format=json"]);
  const metadata = JSON.parse(await run(["metadata", "--no-deps", "--all-features", "--format-version", "1"]));
  const targets = discoverTargets(metadata, output);
  for (const target of targets) {
    const args = targetArgs(target);
    target.entries = parseListing(await run([...args, "--", "--list", "--format", "terse", "--color", "never"]));
    // Terse listing deliberately has NO footer in libtest. Cross-check the
    // complete pretty listing and its count footer instead of inventing one.
    const pretty = parseListing(await run([...args, "--", "--list", "--format", "pretty", "--color", "never"]), true);
    assert.deepEqual(target.entries, pretty, "Terse and pretty libtest inventories differ");
    const ignored = parseListing(await run([...args, "--", "--list", "--ignored", "--format", "terse", "--color", "never"]));
    if (ignored.some(e => !target.entries.some(t => t.name === e.name && t.kind === e.kind))) fail("Ignored listing is not a subset of complete inventory");
    target.ignored = ignored.map(e => e.name);
    log(`inventory ${target.key}: listed=${target.entries.length} default-ignored=${target.ignored.length}`);
  }
  for (const target of targets.filter(t => t.packageName === "open-model-gateway" && t.kind === "lib")) {
    for (const probe of DEDICATED_LIB_PROBES) {
      if (!target.entries.some(e => e.name === probe && e.kind === "test") || !target.ignored.includes(probe)) {
        fail(`Dedicated library test ${probe} must remain present as a test and ignored in ordinary shards`);
      }
    }
  }
  const plan = planShards(targets, count);
  for (const target of plan.special) log(`special-owned ${target.key}: listed=${target.total}`);
  for (const target of plan.shards[index]) {
    const start = performance.now();
    const isGatewayLib = target.packageName === "open-model-gateway" && target.kind === "lib";
    // Empty whole targets (e.g. a binary with no unit tests) are still exercised
    // and must report zero. An empty exact-filter selection is never permitted.
    const filters = isGatewayLib ? target.entries.map(e => e.name) : [];
    const result = await run([...targetArgs(target), "--", "--exact", ...filters], { echo: true });
    const counts = validateRun(result, target);
    log(`shard=${index}/${count} ${target.key}: selected=${target.entries.length}/${target.total} passed=${counts.passed} ignored=${counts.ignored} duration=${((performance.now() - start) / 1000).toFixed(2)}s`);
  }
  if (index === 0) {
    const start = performance.now();
    await run(["test", "--workspace", "--all-features", "--doc"], { echo: true });
    log(`workspace doctests: owner=shard0 duration=${((performance.now() - start) / 1000).toFixed(2)}s`);
  }
  log(`shard=${index}/${count} total-duration=${((performance.now() - started) / 1000).toFixed(2)}s (includes build/discovery)`);
}

// stderr is Cargo's normal progress channel, not part of the strict stdout
// inventory. No shell; invoke Cargo per identity to preserve CARGO_* and cwd.
export function runCargo(args, { echo = false } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn("cargo", ["--color", "never", ...args], { cwd: root, stdio: ["ignore", "pipe", "inherit"] });
    const chunks = [];
    let bytes = 0;
    child.stdout.on("data", chunk => {
      bytes += chunk.length;
      if (bytes > 64 * 1024 * 1024) {
        child.kill("SIGKILL");
        reject(new Error("Cargo stdout exceeded 64 MiB"));
        return;
      }
      chunks.push(chunk);
      if (echo) process.stdout.write(chunk);
    });
    child.on("error", reject);
    child.on("close", (code, signal) => {
      if (code !== 0) {
        // --message-format=json puts compiler diagnostics on stdout too. Keep
        // failures visible even for captured build/discovery subprocesses.
        if (!echo) process.stderr.write(Buffer.concat(chunks));
        reject(new Error(`cargo ${args.join(" ")} failed (exit=${code}, signal=${signal})`));
      } else resolve(Buffer.concat(chunks).toString("utf8"));
    });
  });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    const args = process.argv.slice(2);
    if (args.length !== 4 || args[0] !== "--shard" || args[2] !== "--count"
        || !/^(0|[1-9]\d*)$/.test(args[1]) || !/^[1-9]\d*$/.test(args[3])) fail("usage: rust-shards.mjs --shard INDEX --count COUNT");
    await runShard(Number(args[1]), Number(args[3]));
  } catch (error) {
    console.error(error.stack);
    process.exitCode = 1;
  }
}
