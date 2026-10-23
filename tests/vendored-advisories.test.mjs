// Offline acceptance against the real pinned scanner, not a scanner stub.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const script = fileURLToPath(new URL("../.github/scripts/vendored-advisories.mjs", import.meta.url));
const registry = "registry+https://github.com/rust-lang/crates.io-index";
const registration = { name: "smallvec", version: "1.10.0", manifest: "vendor/smallvec/Cargo.toml" };
const run = (command, args, cwd, env = process.env) => spawnSync(command, args, { cwd, env, encoding: "utf8", timeout: 120_000, maxBuffer: 16 * 1024 * 1024 });
const output = r => `${r.stdout ?? ""}${r.stderr ?? ""}`;
function ok(r) { assert.equal(r.status, 0, output(r)); return r.stdout; }
function fixture(vendor = true) {
  const root = realpathSync(mkdtempSync(join(tmpdir(), "omg-vendored-advisories-")));
  const env = { ...process.env, CARGO_HOME: join(root, "cargo-home"), CARGO_NET_OFFLINE: "true", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null", GIT_TERMINAL_PROMPT: "0" };
  const put = (path, text) => { mkdirSync(join(root, path, ".."), { recursive: true }); writeFileSync(join(root, path), text); };
  const manifest = `[workspace]\nmembers = ["app"]\nexclude = ["vendor/smallvec"]\nresolver = "3"\n`;
  put("Cargo.toml", manifest);
  put("app/Cargo.toml", `[package]\nname = "fixture"\nversion = "0.1.0"\nedition = "2024"\n${vendor ? '[dependencies]\nsmallvec = { path = "../vendor/smallvec" }\n' : ""}`);
  put("app/src/lib.rs", "");
  if (vendor) {
    put("vendor/smallvec/Cargo.toml", '[package]\nname = "smallvec"\nversion = "1.10.0"\nedition = "2021"\nlicense = "MIT"\n');
    put("vendor/smallvec/src/lib.rs", "");
  }
  const metadata = () => JSON.parse(ok(run("cargo", ["metadata", "--offline", "--all-features", "--format-version", "1"], root, env)));
  const register = () => put("Cargo.toml", `${manifest}\n[workspace.metadata.vendored.smallvec]\nname = "smallvec"\nversion = "1.10.0"\nmanifest = "vendor/smallvec/Cargo.toml"\n`);
  return { root, env, put, metadata, register, cleanup: () => rmSync(root, { recursive: true, force: true }) };
}

// The scanner's pinned URL-to-cache-directory mapping (cargo-deny 0.20.2,
// src/advisories/helpers/db.rs). This is an ISOLATED synthetic cache, never the
// user's RustSec cache; --offline forbids fetching the configured URL.
function advisoryDb(f) {
  const db = "dbs/advisory-db-3157b0e258782691";
  f.put(`${db}/crates/smallvec/RUSTSEC-2026-9999.md`, `\`\`\`toml\n[advisory]\nid = "RUSTSEC-2026-9999"\npackage = "smallvec"\ndate = "2026-01-01"\nurl = "https://example.invalid/synthetic"\n[versions]\npatched = [">= 1.11.0"]\n\`\`\`\n\n# Synthetic fixture vulnerability\n\nNot a real RustSec advisory; offline test only.\n`);
  const path = join(f.root, db);
  ok(run("git", ["init", "--initial-branch=main", path], f.root, f.env));
  ok(run("git", ["-C", path, "add", "."], f.root, f.env));
  ok(run("git", ["-C", path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", "commit", "-m", "Synthetic advisory"], f.root, { ...f.env, GIT_AUTHOR_DATE: "2026-01-01T00:00:00Z", GIT_COMMITTER_DATE: "2026-01-01T00:00:00Z" }));
  f.put("deny.toml", `[advisories]\ndb-path = ${JSON.stringify(join(f.root, "dbs"))}\ndb-urls = ["https://github.com/RustSec/advisory-db"]\ndisable-yank-checking = true\n`);
}

const scanner = process.env.CARGO_DENY ?? "cargo-deny";
const probe = run(scanner, ["--version"]);
const absent = probe.error?.code === "ENOENT";
if (!absent) assert.equal(ok(probe).trim(), "cargo-deny 0.20.2", "acceptance requires the CI-pinned scanner");
if (absent && (process.env.CI || process.env.CARGO_DENY)) throw new Error("cargo-deny 0.20.2 is required in CI / when CARGO_DENY is set");

// Import inside tests so the initial test-first failure identifies the missing implementation.
test("projection changes only registered identity and graph references; source input is untouched", async () => {
  const { projectMetadata } = await import(script);
  const f = fixture();
  try {
    f.register();
    const before = f.metadata();
    const original = structuredClone(before);
    const { metadata, count } = projectMetadata(before, f.root);
    assert.equal(count, 1);
    assert.deepEqual(before, original);
    const pkg = metadata.packages.find(p => p.name === "smallvec");
    const old = original.packages.find(p => p.name === "smallvec");
    assert.equal(pkg.source, registry);
    assert.equal(pkg.id, `${registry}#smallvec@1.10.0`);
    const expected = JSON.parse(JSON.stringify(original).split(JSON.stringify(old.id).slice(1, -1)).join(pkg.id));
    expected.packages.find(p => p.name === "smallvec").source = registry;
    assert.deepEqual(metadata, expected, "no license, dependency declaration, manifest or other provenance is rewritten");
  } finally { f.cleanup(); }
});

test("registration and graph validation fail closed", async t => {
  const { projectMetadata } = await import(script);
  const f = fixture();
  try {
    f.register();
    const base = f.metadata();
    const cases = {
      "unknown path dependency": m => { m.metadata.vendored = {}; },
      "stale registration": m => { m.metadata.vendored.stale = { ...registration, manifest: "vendor/absent/Cargo.toml" }; },
      "wrong name": m => { m.metadata.vendored.smallvec.name = "lettre"; },
      "wrong version": m => { m.metadata.vendored.smallvec.version = "1.10.1"; },
      "escaping path": m => { m.metadata.vendored.smallvec.manifest = "../outside/Cargo.toml"; },
      "absolute path": m => { m.metadata.vendored.smallvec.manifest = join(f.root, registration.manifest); },
      "noncanonical path": m => { m.metadata.vendored.smallvec.manifest = "vendor/../vendor/smallvec/Cargo.toml"; },
      "wrong registry": m => { m.metadata.vendored.smallvec.registry = "https://example.invalid"; },
      "duplicate registration": m => { m.metadata.vendored.duplicate = { ...registration }; },
      "bad registration shape": m => { m.metadata.vendored = []; },
      "null registration": m => { m.metadata.vendored = null; },
      "malformed packages": m => { m.packages = {}; },
      "missing source": m => { delete m.packages[0].source; },
      "contradictory path ID": m => { m.packages.find(p => p.name === "smallvec").id = "path+file:///elsewhere#smallvec@1.10.0"; },
      "invalid default members": m => { m.workspace_default_members.push("unknown"); },
      "workspace member registration": m => { m.workspace_members.push(m.packages.find(p => p.name === "smallvec").id); },
      "missing resolve": m => { m.resolve = null; },
      "missing graph node": m => { m.resolve.nodes.pop(); },
      "unknown graph dependency": m => { m.resolve.nodes[0].dependencies.push("unknown"); },
      "incomplete graph edges": m => { m.resolve.nodes.find(n => n.deps.length).deps = []; },
      "missing dep kinds": m => { delete m.resolve.nodes.find(n => n.deps.length).deps[0].dep_kinds; },
      "unknown root": m => { m.resolve.root = "unknown"; },
      "unknown workspace member": m => { m.workspace_members.push("unknown"); },
      "duplicate package": m => { m.packages.push(m.packages[0]); },
      "unsupported format": m => { m.version = 2; },
    };
    for (const [name, change] of Object.entries(cases)) await t.test(name, () => {
      const m = structuredClone(base); change(m);
      assert.throws(() => projectMetadata(m, f.root), /vendored advisories:/);
    });
    f.put("vendor/stale/Cargo.toml", '[package]\nname = "smallvec"\nversion = "1.10.0"\n');
    await t.test("existing but unused registration", () => {
      const m = structuredClone(base); m.metadata.vendored.stale = { ...registration, manifest: "vendor/stale/Cargo.toml" };
      assert.throws(() => projectMetadata(m, f.root), /stale vendored registration/);
    });
    symlinkSync(join(f.root, "vendor/smallvec"), join(f.root, "vendor/alias"));
    await t.test("symlink registration", () => {
      const m = structuredClone(base); m.metadata.vendored.smallvec.manifest = "vendor/alias/Cargo.toml";
      assert.throws(() => projectMetadata(m, f.root), /vendored advisories:/);
    });
  } finally { f.cleanup(); }
});

test("real cargo-deny misses path identity, flags projection and unchanged registry identity", { skip: absent ? "cargo-deny 0.20.2 absent; install CI's pinned tool or set CARGO_DENY" : false }, async t => {
  const { projectMetadata } = await import(script);
  const f = fixture();
  try {
    advisoryDb(f);
    f.register();
    const raw = f.metadata();
    const lock = readFileSync(join(f.root, "Cargo.lock"), "utf8");
    const scan = m => {
      f.put("metadata.json", JSON.stringify(m));
      return run(scanner, ["--all-features", "--offline", "--metadata-path", join(f.root, "metadata.json"), "check", "advisories"], f.root, f.env);
    };
    const missed = scan(raw);
    ok(missed);
    assert.doesNotMatch(output(missed), /RUSTSEC-2026-9999/);
    t.diagnostic(`UNPROJECTED: exit ${missed.status}; ${output(missed).trim()}`);
    const projected = projectMetadata(raw, f.root).metadata;
    const flagged = scan(projected);
    assert.equal(flagged.status, 1, output(flagged));
    assert.match(output(flagged), /RUSTSEC-2026-9999/);
    assert.match(output(flagged), /Synthetic fixture vulnerability/);
    t.diagnostic(`PROJECTED: exit ${flagged.status}; ${output(flagged).trim()}`);
    const unchanged = structuredClone(projected);
    unchanged.metadata.vendored = {};
    assert.deepEqual(projectMetadata(unchanged, f.root).metadata, unchanged);
    assert.equal(scan(unchanged).status, 1, "unchanged registry control is still scanned");
    const cli = run(process.execPath, [script, "--offline"], f.root, { ...f.env, CARGO_DENY: scanner });
    assert.equal(cli.status, 1, output(cli));
    assert.match(output(cli), /RUSTSEC-2026-9999/);
    assert.equal(readFileSync(join(f.root, "Cargo.lock"), "utf8"), lock);
  } finally { f.cleanup(); }
});

test("registered packages pruned by the real scanner fail closed instead of claiming coverage", { skip: absent ? "cargo-deny 0.20.2 absent; install CI's pinned tool or set CARGO_DENY" : false }, async t => {
  const { projectMetadata } = await import(script);
  for (const kind of ["impossible target", "policy exclusion"]) await t.test(kind, () => {
    const f = fixture();
    try {
      advisoryDb(f);
      f.register();
      if (kind === "impossible target") {
        const manifest = readFileSync(join(f.root, "app/Cargo.toml"), "utf8");
        f.put("app/Cargo.toml", manifest.replace("[dependencies]", "[target.'cfg(any())'.dependencies]"));
      } else {
        f.put("deny.toml", `${readFileSync(join(f.root, "deny.toml"), "utf8")}\n[graph]\nexclude = ["smallvec"]\n`);
      }
      const { metadata, count } = projectMetadata(f.metadata(), f.root);
      assert.equal(count, 1, "Cargo metadata still contains the registered package");
      f.put("metadata.json", JSON.stringify(metadata));
      const raw = run(scanner, ["--offline", "--all-features", "--metadata-path", join(f.root, "metadata.json"), "check", "advisories"], f.root, f.env);
      ok(raw);
      assert.doesNotMatch(output(raw), /RUSTSEC-2026-9999/, "scanner silently pruned projected package");
      const cli = run(process.execPath, [script, "--offline"], f.root, { ...f.env, CARGO_DENY: scanner });
      assert.equal(cli.status, 1, output(cli));
      assert.match(output(cli), /registered package filtered by cargo-deny: smallvec 1.10.0; upstream-base coverage not established/);
      t.diagnostic(`${kind}: scanner exit ${raw.status}; helper exit ${cli.status} (coverage refused)`);
    } finally { f.cleanup(); }
  });
});

test("no-vendor control passes unchanged, including the actual CLI", { skip: absent ? "cargo-deny 0.20.2 absent; install CI's pinned tool or set CARGO_DENY" : false }, async () => {
  const { projectMetadata } = await import(script);
  const f = fixture(false);
  try {
    advisoryDb(f);
    const m = f.metadata();
    assert.deepEqual(projectMetadata(m, f.root), { metadata: m, count: 0 });
    const r = run(process.execPath, [script, "--offline"], f.root, { ...f.env, CARGO_DENY: scanner });
    ok(r);
    assert.match(output(r), /no registered vendored packages/i);
  } finally { f.cleanup(); }
});
