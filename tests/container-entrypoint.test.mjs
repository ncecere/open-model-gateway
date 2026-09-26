import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const entrypoint = fileURLToPath(new URL("../deploy/container-entrypoint.sh", import.meta.url));
const names = ["DATABASE_URL", "GATEWAY_OIDC_CLIENT_SECRET", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"];
const fixtureProgram = `#!${process.execPath}
const fs = require("node:fs");
const names = ${JSON.stringify(names)};
fs.writeFileSync(process.env.FIXTURE_OUTPUT, JSON.stringify({
  args: process.argv.slice(2), pid: process.pid,
  secrets: Object.fromEntries(names.filter(n => n in process.env).map(n => [n, process.env[n]])),
  files: Object.fromEntries(names.filter(n => n + "_FILE" in process.env).map(n => [n, process.env[n + "_FILE"]])),
  custom: process.env.CUSTOM_PROVIDER_KEY,
}));
if (process.env.FIXTURE_MODE === "signal") {
  process.on("SIGTERM", () => process.exit(42));
  setInterval(() => {}, 1000);
  process.stdout.write("ready\\n");
} else {
  process.exit(Number(process.env.FIXTURE_EXIT || 0));
}
`;

function fixture(t) {
  const directory = mkdtempSync(join(tmpdir(), "gateway-entrypoint-test-"));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const executable = join(directory, "open-model-gateway");
  writeFileSync(executable, fixtureProgram, { mode: 0o755 });
  const output = join(directory, "result.json");
  // Do not inherit the developer's credentials, shell startup hooks, or env files.
  const env = { PATH: `${directory}${delimiter}/usr/bin${delimiter}/bin`, FIXTURE_OUTPUT: output };
  return {
    directory, output, env,
    secret(contents, filename = "secret with spaces") {
      const path = join(directory, filename);
      writeFileSync(path, contents, { mode: 0o600 });
      return path;
    },
    run(overrides = {}, args = [], shellArgs = []) {
      return spawnSync("/bin/sh", [...shellArgs, entrypoint, ...args], {
        env: { ...env, ...overrides }, encoding: "utf8", timeout: 5000,
      });
    },
    captured() { return JSON.parse(readFileSync(output, "utf8")); },
  };
}

function succeeded(result) {
  assert.equal(result.error, undefined);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, "");
  assert.equal(result.stderr, "");
}

function rejected(f, result, sensitive = []) {
  assert.equal(result.error, undefined);
  assert.equal(result.status, 1);
  assert.equal(result.stdout, "");
  assert.match(result.stderr, /^container-entrypoint: /);
  assert.equal(existsSync(f.output), false, "must fail before executing the gateway");
  for (const value of sensitive) {
    assert.equal((result.stdout + result.stderr).includes(value), false, "must not log secrets or file paths");
  }
}

test("defaults to exactly one serve invocation without bootstrapping or migrating", (t) => {
  const f = fixture(t);
  succeeded(f.run());
  assert.deepEqual(f.captured().args, ["serve"]);
  assert.deepEqual(f.captured().secrets, {});
});

for (const args of [
  ["serve"], ["migrate"], ["provision-user", "--email", "person+ops@example.org", "--platform-admin"],
  ["provision-user", "--email", "a b;$(touch never)@example.org", "", "*", "--literal=$value"],
  ["--help"],
]) {
  test(`passes CLI arguments exactly: ${JSON.stringify(args)}`, (t) => {
    const f = fixture(t);
    succeeded(f.run({ DATABASE_URL: "postgres://user:password@db/gateway" }, args));
    assert.deepEqual(f.captured().args, args);
  });
}

test("preserves direct secrets literally without shell interpolation or diagnostic output", (t) => {
  const f = fixture(t);
  const marker = join(f.directory, "must-not-exist");
  const value = `  p@ss='quoted' \\ * $HOME ; $(touch ${marker}) \`touch ${marker}\`  `;
  const env = Object.fromEntries(names.map(name => [name, value]));
  succeeded(f.run(env));
  assert.deepEqual(f.captured().secrets, env);
  assert.equal(existsSync(marker), false);
});

test("imports all four file secrets, preserves spaces, removes _FILE, accepts one final LF", (t) => {
  const f = fixture(t);
  const expected = {};
  const env = {};
  for (const [index, name] of names.entries()) {
    expected[name] = `  literal-${index}:$HOME 'quotes' \\ ; *  `;
    env[`${name}_FILE`] = f.secret(expected[name] + (index % 2 ? "\n" : ""), name);
  }
  succeeded(f.run(env));
  assert.deepEqual(f.captured().secrets, expected);
  assert.deepEqual(f.captured().files, {});
});

for (const name of names) {
  test(`${name}: rejects environment/file ambiguity even for an empty variable`, (t) => {
    const f = fixture(t);
    const path = f.secret("file-secret-marker");
    for (const value of ["direct-secret-marker", ""]) {
      rejected(f, f.run({ [name]: value, [`${name}_FILE`]: path }), [path, "file-secret-marker", "direct-secret-marker"]);
    }
  });
}

for (const [label, contents] of [
  ["empty", ""], ["just a newline", "\n"], ["multiline", "first-sensitive\nsecond-sensitive"],
  ["repeated trailing newline", "sensitive\n\n"], ["carriage return", "sensitive\r"],
  ["CRLF", "sensitive\r\n"], ["NUL", Buffer.from("sensitive\0hidden")],
  ["terminal NUL", Buffer.from("sensitive\0")], ["oversized", "s".repeat(4097)],
]) {
  test(`rejects ${label} file secrets without logging their contents`, (t) => {
    const f = fixture(t);
    const path = f.secret(contents);
    rejected(f, f.run({ DATABASE_URL_FILE: path }), [path, "sensitive", "s".repeat(100)]);
  });
}

for (const [label, value] of [
  ["empty", ""], ["multiline", "sensitive\nhidden"], ["terminal LF", "sensitive\n"],
  ["CR", "sensitive\r"], ["oversized", "s".repeat(4097)],
]) {
  test(`rejects ${label} direct secrets without disclosure`, (t) => {
    const f = fixture(t);
    rejected(f, f.run({ ANTHROPIC_API_KEY: value }), ["sensitive", "s".repeat(100)]);
  });
}

test("enforces a 4096-byte limit, not a character limit", (t) => {
  const f = fixture(t);
  rejected(f, f.run({ OPENAI_API_KEY: "é".repeat(2049) }), ["é".repeat(10)]);
  rejected(f, f.run({ OPENAI_API_KEY_FILE: f.secret("é".repeat(2049)) }), ["é".repeat(10)]);
  succeeded(f.run({ OPENAI_API_KEY: "s".repeat(4096) }));
  assert.equal(f.captured().secrets.OPENAI_API_KEY.length, 4096);
  succeeded(f.run({ OPENAI_API_KEY_FILE: f.secret("s".repeat(4096)) }));
  assert.equal(f.captured().secrets.OPENAI_API_KEY.length, 4096);
});

test("rejects empty, missing, directory and unreadable file paths", (t) => {
  const f = fixture(t);
  for (const path of ["", join(f.directory, "missing-sensitive-path"), f.directory]) {
    rejected(f, f.run({ DATABASE_URL_FILE: path }), path ? [path] : []);
  }
});

test("rejects a permission-denied secret file", { skip: process.getuid?.() === 0 ? "root bypasses Unix read permission checks" : false }, (t) => {
  const f = fixture(t);
  const path = f.secret("private-sensitive-value");
  chmodSync(path, 0o000);
  rejected(f, f.run({ DATABASE_URL_FILE: path }), [path, "private-sensitive-value"]);
});

test("does not evaluate custom allowlist names or import arbitrary *_FILE variables", (t) => {
  const f = fixture(t);
  const marker = join(f.directory, "must-not-exist");
  succeeded(f.run({
    GATEWAY_SECRET_ENV_ALLOWLIST: `CUSTOM_PROVIDER_KEY,$(touch ${marker}),BAD;NAME`,
    CUSTOM_PROVIDER_KEY_FILE: f.secret("not-imported"),
  }));
  assert.equal(f.captured().custom, undefined);
  assert.equal(existsSync(marker), false);
});

test("disables inherited shell tracing before expanding secrets", (t) => {
  const f = fixture(t);
  const result = f.run({ DATABASE_URL: "sensitive-no-tracing" }, [], ["-x"]);
  assert.equal(result.status, 0);
  assert.equal(result.stdout, "");
  assert.doesNotMatch(result.stderr, /sensitive-no-tracing/);
  assert.deepEqual(f.captured().secrets, { DATABASE_URL: "sensitive-no-tracing" });
});

test("propagates the gateway exit status", (t) => {
  const f = fixture(t);
  const result = f.run({ FIXTURE_EXIT: "37" });
  assert.equal(result.status, 37);
  assert.equal(result.stdout + result.stderr, "");
});

test("exec preserves PID and sends SIGTERM directly to the foreground gateway", { timeout: 5000 }, async (t) => {
  const f = fixture(t);
  const child = spawn("/bin/sh", [entrypoint], {
    env: { ...f.env, FIXTURE_MODE: "signal", DATABASE_URL_FILE: f.secret("postgres://fixture") },
    stdio: ["ignore", "pipe", "pipe"],
  });
  t.after(() => { if (child.exitCode === null) child.kill("SIGKILL"); });
  const exit = once(child, "exit");
  const [ready] = await once(child.stdout, "data");
  assert.equal(ready.toString(), "ready\n");
  assert.equal(f.captured().pid, child.pid, "entrypoint must exec, not fork the gateway");
  assert.deepEqual(f.captured().args, ["serve"]);
  child.kill("SIGTERM");
  assert.deepEqual(await exit, [42, null]);
});
