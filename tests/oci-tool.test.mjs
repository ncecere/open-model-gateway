// #372 Phase 1: private build-oras.sh OUTPUT_FILE contract, not publication approval.
// Only this helper is copied; fetch/git/tar/go are fake. PATH has no network tools
// or host compiler. The SDK and output are DATA, never a real Go/ORAS executable.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, readlinkSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const helper = fileURLToPath(new URL("../.github/scripts/build-oras.sh", import.meta.url));
const commit = "9b1fc815f6b7fba743f08b9b5c5846639cc11d3c";
const sdkURL = "https://go.dev/dl/go1.26.10.linux-amd64.tar.gz";
const sdkSHA = "ccfe7ae822d2c2f41f4fe7c956199967a4dac22d0ee0d2b2bfdb440aab3b20c5";
const manifests = { "go.mod": "module oras.land/oras\n\ngo 1.26.0\n", "go.sum": "example.invalid/dependency v1.0.0 h1:fixture\n" };
const ldflags = `-w -s -buildid= -X oras.land/oras/internal/version.Version=1.3.6 -X oras.land/oras/internal/version.GitCommit=${commit} -X oras.land/oras/internal/version.BuildMetadata= -X oras.land/oras/internal/version.GitTreeState=clean`;

function harness(t, scenario = "ok") {
  const dir = mkdtempSync(join(tmpdir(), "omg-oras-test-"));
  t.after(() => {
    // Only this owned fixture tree; Dirents exclude symlinks from recursion/chmod.
    const restore = root => {
      chmodSync(root, 0o700);
      for (const entry of readdirSync(root, { withFileTypes: true })) {
        if (entry.isDirectory()) restore(join(root, entry.name));
      }
    };
    restore(dir);
    rmSync(dir, { recursive: true, force: true });
  });
  const bin = join(dir, "bin"), scratch = join(dir, "tmp"), output = join(dir, "oras output");
  for (const p of [bin, scratch, join(dir, "ambient-home"), join(dir, "image-cache")]) mkdirSync(p);
  // Deliberately fail (never skip) until the separately authorized implementation exists.
  copyFileSync(helper, join(dir, "build-oras.sh"));
  const log = join(dir, "events.jsonl");
  const fake = `#!${process.execPath}
const fs = require('node:fs'), p = require('node:path');
const dir = ${JSON.stringify(dir)}, log = ${JSON.stringify(log)};
const scenario = ${JSON.stringify(scenario)}, expected = ${JSON.stringify(manifests)};
const args = process.argv.slice(2), name = p.basename(process.argv[1]);
const record = extra => fs.appendFileSync(log, JSON.stringify({ name, args, cwd: process.cwd(), ...extra }) + '\\n');
const die = message => { console.error(message); process.exit(71); };
const snapshots = root => {
  if (!fs.existsSync(root) || fs.lstatSync(root).isSymbolicLink()) return [];
  if (!fs.statSync(root).isDirectory()) return [];
  return fs.readdirSync(root).flatMap(n => {
    const f = p.join(root, n);
    return n in expected ? [{ file: f, data: fs.readFileSync(f, 'utf8') }] : snapshots(f);
  });
};
record({ ...(name === 'go' ? { env: process.env, executable: process.argv[1], manifests: snapshots(process.cwd()) } : {}),
  ...(name === 'rm' ? { manifests: args.filter(a => !a.startsWith('-')).flatMap(a => snapshots(p.resolve(a))) } : {}) });
if (name === 'fetch-verified.sh') {
  if (args.length !== 3) die('fetch usage');
  if (scenario === 'fetch-failure') die('checksum mismatch');
  fs.writeFileSync(args[2], 'FAKE SDK ARCHIVE');
} else if (name === 'tar') {
  const i = args.indexOf('-C');
  if (i < 0) die('fake tar requires -C');
  const fetched = fs.readFileSync(log, 'utf8').trim().split('\\n').map(JSON.parse).find(e => e.name === 'fetch-verified.sh');
  if (!fetched || !args.includes(fetched.args[2]) || fs.readFileSync(fetched.args[2], 'utf8') !== 'FAKE SDK ARCHIVE') die('not the verified SDK archive');
  const sdk = p.join(args[i + 1], 'go', 'bin');
  fs.mkdirSync(sdk, { recursive: true });
  fs.copyFileSync(process.argv[1], p.join(sdk, 'go'));
  fs.chmodSync(p.join(sdk, 'go'), 0o755);
} else if (name === 'git') {
  let a = [...args], cwd = process.cwd();
  while (a[0] === '-C' || a[0] === '-c') { if (a[0] === '-C') cwd = p.resolve(cwd, a[1]); a.splice(0, 2); }
  const cmd = a.shift();
  if (cmd === 'clone' || cmd === 'init') {
    const dest = p.resolve(cwd, a.at(-1) ?? '.');
    fs.mkdirSync(dest, { recursive: true });
    for (const [f, data] of Object.entries(expected)) fs.writeFileSync(p.join(dest, f), data);
  } else if (cmd === 'rev-parse') {
    console.log(scenario === 'wrong-commit' ? '0'.repeat(40) : ${JSON.stringify(commit)});
  } else if (cmd === 'diff' || cmd === 'status') {
    if (snapshots(cwd).some(s => s.data !== expected[p.basename(s.file)])) die('changed manifests');
  } else if (!['remote', 'fetch', 'checkout', 'switch', 'config'].includes(cmd)) die('unexpected git command');
} else if (name === 'go' && process.argv[1].includes('/go/bin/go')) {
  if (args[0] !== 'build') die('only go build is allowed');
  if (scenario.startsWith('readonly-cache')) {
    const module = p.join(process.env.GOMODCACHE, 'module'), nested = p.join(module, 'nested');
    fs.mkdirSync(nested, { recursive: true });
    fs.writeFileSync(p.join(nested, 'dependency.go'), 'FAKE MODULE DATA');
    fs.chmodSync(nested, 0o555);
    fs.chmodSync(module, 0o555);
  }
  if (scenario === 'compiler-failure' || scenario === 'readonly-cache-compiler-failure') die('fake compilation failed');
  const i = args.indexOf('-o');
  if (i < 0) die('missing build output');
  fs.writeFileSync(args[i + 1], 'DATA\\n');
  fs.chmodSync(args[i + 1], 0o755);
} else if (name === 'rm') {
  for (const a of args.filter(a => !a.startsWith('-'))) fs.rmSync(a, { recursive: true, force: true });
} else die('forbidden native dependency or execution of emitted DATA');
`;
  const executable = (path, text) => { writeFileSync(path, text); chmodSync(path, 0o755); };
  executable(join(bin, "fetch-verified.sh"), fake);
  // Support the existing convention: bash "$script_dir/fetch-verified.sh" ...
  executable(join(dir, "fetch-verified.sh"), `#!/bin/bash\nexec ${JSON.stringify(process.execPath)} ${JSON.stringify(join(bin, "fetch-verified.sh"))} "$@"\n`);
  for (const name of ["git", "tar", "go", "rm", "DATA", "curl", "wget", "gh", "make", "gcc", "cc"]) executable(join(bin, name), fake);
  // Filesystem/shell primitives only: no inherited PATH or executable lookup fallback.
  for (const name of ["bash", "env", "mkdir", "mktemp", "dirname", "basename", "chmod", "cp", "mv", "cat", "rmdir", "readlink", "pwd", "cut", "sed", "grep", "tr", "find"]) {
    const native = ["/bin", "/usr/bin"].map(p => join(p, name)).find(existsSync);
    assert.ok(native, `missing filesystem primitive ${name}`);
    symlinkSync(native, join(bin, name));
  }
  const poison = {
    GOFLAGS: "-toolexec=ambient-tool", GOTOOLCHAIN: "auto", CGO_ENABLED: "1", GOOS: "darwin", GOARCH: "arm64", GOAMD64: "v4",
    GOENV: "/ambient/goenv", GOWORK: "/ambient/go.work", GOEXPERIMENT: "ambient", GOROOT: "/ambient/sdk",
    GOPROXY: "http://ambient.invalid", GOSUMDB: "off", GOPRIVATE: "*", GONOPROXY: "*", GONOSUMDB: "*",
    HTTP_PROXY: "http://ambient.invalid", HTTPS_PROXY: "http://ambient.invalid", ALL_PROXY: "http://ambient.invalid",
    http_proxy: "http://ambient.invalid", https_proxy: "http://ambient.invalid", all_proxy: "http://ambient.invalid",
    GH_TOKEN: "fake-credential-marker", GITHUB_TOKEN: "fake-credential-marker", ACTIONS_ID_TOKEN_REQUEST_TOKEN: "fake-credential-marker",
    ACTIONS_ID_TOKEN_REQUEST_URL: "https://oidc.invalid", ACTIONS_RUNTIME_TOKEN: "fake-credential-marker",
    HOME: join(dir, "ambient-home"), GOCACHE: join(dir, "image-cache"), GOMODCACHE: join(dir, "image-cache"), GOPATH: join(dir, "image-cache"),
  };
  return { dir, output, scratch, poison, run(args = [output]) {
    const retained = () => readdirSync(dir).filter(n => !["events.jsonl", "oras output"].includes(n));
    const before = retained();
    const result = spawnSync("/bin/bash", [join(dir, "build-oras.sh"), ...args], {
      cwd: dir, encoding: "utf8", timeout: 10_000,
      env: { ...poison, PATH: bin, TMPDIR: scratch, RUNNER_TEMP: scratch, LANG: "C" },
    });
    assert.equal(result.error, undefined, String(result.error));
    const events = existsSync(log) ? readFileSync(log, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse) : [];
    if (scenario.startsWith("readonly-cache")) t.diagnostic(JSON.stringify({ scenario, status: result.status,
      output: existsSync(output) ? readFileSync(output, "utf8") : null }));
    assert.deepEqual(readdirSync(scratch), [], `helper must clean temporary SDK/source/home/caches on every exit; status=${result.status}\n${result.stderr}`);
    assert.deepEqual(retained(), before, "only OUTPUT_FILE may be added outside the temporary directory");
    for (const e of events) {
      assert.ok(["fetch-verified.sh", "git", "tar", "go", "rm"].includes(e.name), `forbidden subprocess: ${e.name}`);
      assert.ok(!e.args.join(" ").includes("fake-credential-marker"), "no credentials in argv");
      for (const s of e.manifests ?? []) assert.equal(s.data, manifests[s.file.split("/").at(-1)], "retain dependency manifests verbatim");
    }
    return { ...result, events, builds: events.filter(e => e.name === "go") };
  } };
}

function success(h) {
  const r = h.run();
  assert.equal(r.status, 0, r.stderr);
  assert.equal(readFileSync(h.output, "utf8"), "DATA\n", "publish the fake compiler's bytes without executing them");
  assert.equal(r.builds.length, 1, "one build, no version/install/run/generate probes");
  return r;
}

test("ORAS helper selects the verified patched SDK and exact official source commit", t => {
  const h = harness(t), r = success(h);
  const downloads = r.events.filter(e => e.name === "fetch-verified.sh");
  assert.equal(downloads.length, 1);
  assert.deepEqual(downloads[0].args.slice(0, 2), [sdkURL, sdkSHA]);
  const git = r.events.filter(e => e.name === "git").map(e => e.args);
  assert.ok(git.some(a => a.some(v => /^https:\/\/github\.com\/oras-project\/oras(?:\.git)?$/.test(v))), "official HTTPS source only");
  assert.ok(git.some(a => (a.includes("checkout") || a.includes("switch")) && a.includes(commit)), "checkout immutable source commit");
  assert.ok(git.some(a => a.includes("rev-parse") && a.some(v => v.startsWith("HEAD"))), "verify checked-out HEAD");
  assert.ok(r.events.indexOf(downloads[0]) < r.events.indexOf(r.builds[0]));
});

test("ORAS compiler gets only the reproducible upstream build recipe and unchanged manifests", t => {
  const r = success(harness(t)), b = r.builds[0], args = [...b.args];
  const i = args.indexOf("-o");
  assert.ok(i > 0 && args[i + 1], "explicit output file");
  args.splice(i, 2);
  assert.deepEqual(args, ["build", "-mod=readonly", "-trimpath", "-buildvcs=false", `-ldflags=${ldflags}`, "./cmd/oras"]);
  assert.deepEqual(b.manifests.map(s => s.file.split("/").at(-1)).sort(), ["go.mod", "go.sum"]);
  assert.ok(b.executable.includes("/go/bin/go"), "use the fetched SDK, never ambient go");
});

test("compiler environment removes ambient tool injection, proxies, credentials and shared image caches", t => {
  const h = harness(t), { env } = success(h).builds[0];
  for (const [k, v] of Object.entries({ CGO_ENABLED: "0", GOOS: "linux", GOARCH: "amd64", GOAMD64: "v1", GOTOOLCHAIN: "local", GOENV: "off", GOWORK: "off", GOFLAGS: "", GOEXPERIMENT: "" })) assert.equal(env[k], v, k);
  assert.equal(env.GOPROXY, "https://proxy.golang.org");
  assert.equal(env.GOSUMDB, "sum.golang.org");
  for (const k of ["GOPRIVATE", "GONOPROXY", "GONOSUMDB", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy", "GH_TOKEN", "GITHUB_TOKEN", "ACTIONS_ID_TOKEN_REQUEST_TOKEN", "ACTIONS_ID_TOKEN_REQUEST_URL", "ACTIONS_RUNTIME_TOKEN"]) assert.ok(!env[k], `${k} must not reach compiler`);
  for (const k of ["HOME", "GOCACHE", "GOMODCACHE", "GOPATH"]) assert.ok(env[k]?.startsWith(`${h.scratch}/`) && env[k] !== h.poison[k], `${k} must be private temporary storage`);
  assert.notEqual(env.GOROOT, h.poison.GOROOT);
});

for (const scenario of ["fetch-failure", "wrong-commit", "compiler-failure"]) test(`${scenario} fails closed and cleans temporary files`, t => {
  const h = harness(t, scenario), r = h.run();
  assert.notEqual(r.status, 0, "must reject failed checksum, wrong HEAD or failed compiler");
  assert.equal(r.builds.length, scenario === "compiler-failure" ? 1 : 0);
  assert.equal(existsSync(h.output), false, "no output on failure");
  assert.ok(r.events.some(e => scenario === "wrong-commit" ? e.name === "git" && e.args.includes("rev-parse") : e.name === (scenario === "fetch-failure" ? "fetch-verified.sh" : "go")), "exercise the intended failure boundary");
});

test("readonly module cache is fully removed after success without changing emitted DATA", t => {
  assert.notEqual(process.getuid?.(), 0, "readonly-cache regression requires a nonroot process");
  success(harness(t, "readonly-cache"));
});

test("readonly module cache is fully removed after compiler failure without masking exit 71", t => {
  assert.notEqual(process.getuid?.(), 0, "readonly-cache regression requires a nonroot process");
  const h = harness(t, "readonly-cache-compiler-failure"), r = h.run();
  assert.equal(r.status, 71, r.stderr);
  assert.equal(r.builds.length, 1, "exercise the compiler failure boundary");
  assert.match(r.stderr, /fake compilation failed/);
  assert.equal(existsSync(h.output), false, "no output on compiler failure");
});

for (const kind of ["existing", "symlink", "dangling-symlink"]) test(`refuses ${kind} output without overwriting or compiling`, t => {
  const h = harness(t), target = join(h.dir, "target");
  if (kind === "existing") writeFileSync(h.output, "KEEP");
  else { if (kind === "symlink") writeFileSync(target, "KEEP"); symlinkSync(target, h.output); }
  const r = h.run();
  assert.notEqual(r.status, 0);
  assert.equal(r.builds.length, 0);
  if (kind !== "existing") assert.equal(readlinkSync(h.output), target, "do not replace the symlink");
  if (kind === "dangling-symlink") assert.equal(existsSync(target), false);
  else assert.equal(readFileSync(kind === "existing" ? h.output : target, "utf8"), "KEEP");
});

test("accepts exactly OUTPUT_FILE, never ref/URL/compiler/flags override arguments", t => {
  const h = harness(t);
  for (const args of [[], [""], [h.output, "--ref=main"], [h.output, "--url=https://evil.invalid"], [h.output, "--compiler=go"], [h.output, "--flags=-toolexec=evil"]]) {
    const r = h.run(args);
    assert.notEqual(r.status, 0, JSON.stringify(args));
    assert.equal(r.builds.length, 0);
    assert.equal(existsSync(h.output), false);
  }
});
