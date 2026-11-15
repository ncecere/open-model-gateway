// Behaviour of the secret-scan job's scan step and of the local hook (#596). The step's
// `run:` text is taken from ci.yml and executed under the preflight-checked Bash in a
// temporary git repository, with a stand-in gitleaks on PATH that records its arguments
// (always), or with the real pinned scanner when GITLEAKS_BIN names one (below). Secrets
// are made up at run time.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { bashPath, runBash } from "../bash-preflight.mjs";
import { CONFIG_FILE, HOOK, RULES_IN_CANARY, ZEROS, commitAll, copy, fakes, gitEnv, git, makeRepo, pinOf, readCalls, scanStep } from "./secret-scan-lib.mjs";
import { root } from "./lib.mjs";

bashPath();

const SCRIPT = scanStep(copy()).run;
const REAL = process.env.GITLEAKS_BIN;
const stubEnv = {};

function runStep(repo, env, extra = {}) {
  const r = runBash(["--noprofile", "--norc", "-e", "-o", "pipefail", "-c", extra.script ?? SCRIPT], {
    cwd: repo.dir, encoding: "utf8", timeout: 120_000,
    env: { ...gitEnv(), PATH: `${repo.bin}:${process.env.PATH}`, STUB_LOG: repo.log, ...stubEnv, ...extra.stub, ...env },
  });
  return { ...r, calls: readCalls(repo.log) };
}
const withRepo = (fn, options) => {
  const repo = makeRepo(options);
  try { return fn(repo); } finally { repo.cleanup(); }
};
const prEnv = (repo, over = {}) => ({ EVENT_NAME: "pull_request", BASE_SHA: repo.c1, HEAD_SHA: repo.c3, BEFORE_SHA: "", AFTER_SHA: repo.c3, ...over });
const pushEnv = (repo, over = {}) => ({ EVENT_NAME: "push", BASE_SHA: "", HEAD_SHA: "", BEFORE_SHA: repo.c1, AFTER_SHA: repo.c3, ...over });
// The range scan (git) and the tree scan (dir ".") are the calls on "."; the others are canaries.
const rangeScan = calls => calls.filter(c => c.argv[0] === "git" && c.argv.at(-1) === ".");
const treeScan = calls => calls.filter(c => c.argv[0] === "dir" && c.argv.at(-1) === ".");
const canaryScans = calls => calls.filter(c => c.argv.at(-1) !== ".");
const logOpts = call => call.argv.find(a => a.startsWith("--log-opts="));

// ---- ranges ----

test("a pull request is scanned over base..head with merge commits and binary-looking files included", () => withRepo(repo => {
  const r = runStep(repo, prEnv(repo));
  assert.equal(r.status, 0, r.stderr);
  const [range] = rangeScan(r.calls);
  assert.equal(logOpts(range), `--log-opts=-m --text ${repo.c1}..${repo.c3}`);
  assert.equal(range.argv[0], "git");
  assert.equal(range.argv.at(-1), ".");
}));

test("a push is scanned over before..after", () => withRepo(repo => {
  const r = runStep(repo, pushEnv(repo));
  assert.equal(r.status, 0, r.stderr);
  assert.equal(logOpts(rangeScan(r.calls)[0]), `--log-opts=-m --text ${repo.c1}..${repo.c3}`);
}));

for (const [label, before] of [["a new ref (all-zero before)", ZEROS], ["a force push whose old tip is gone", "1234567890abcdef1234567890abcdef12345678"]]) {
  test(`${label} is scanned from the whole history of the pushed commit`, () => withRepo(repo => {
    const r = runStep(repo, pushEnv(repo, { BEFORE_SHA: before }));
    assert.equal(r.status, 0, r.stderr);
    assert.equal(logOpts(rangeScan(r.calls)[0]), `--log-opts=-m --text ${repo.c3}`);
  }));
}

test("an empty range scans no history, still scans the tree, and says so", () => withRepo(repo => {
  const r = runStep(repo, prEnv(repo, { BASE_SHA: repo.c3 }));
  assert.equal(r.status, 0, r.stderr);
  assert.equal(rangeScan(r.calls).length, 0);
  assert.equal(treeScan(r.calls).length, 1);
  assert.match(r.stdout, /holds no commits/);
}));

test("an unexpected event is refused", () => withRepo(repo => {
  for (const event of ["workflow_dispatch", "schedule", ""]) {
    const r = runStep(repo, prEnv(repo, { EVENT_NAME: event }));
    assert.equal(r.status, 1, event);
    assert.match(r.stderr, /unexpected event/);
    assert.equal(rangeScan(r.calls).length + treeScan(r.calls).length, 0);
  }
}));

const BAD_SHAS = [
  ["empty", ""], ["not hex", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"], ["short", "abc123"], ["upper case", "ABCDEF1234567890ABCDEF1234567890ABCDEF12"],
  ["a ref name", "main"], ["with options", "--all"], ["well formed but not in the checkout", "1234567890abcdef1234567890abcdef12345678"], ["with spaces", "abc def"],
];
for (const [label, bad] of BAD_SHAS) {
  test(`a pull request with a ${label} base or head sha fails before scanning history`, () => withRepo(repo => {
    for (const over of [{ BASE_SHA: bad }, { HEAD_SHA: bad }]) {
      const r = runStep(repo, prEnv(repo, over));
      assert.notEqual(r.status, 0, JSON.stringify(over));
      assert.equal(rangeScan(r.calls).length, 0);
      assert.equal(treeScan(r.calls).length, 0);
    }
  }));
}
for (const [label, bad] of BAD_SHAS.filter(([, v]) => v !== "1234567890abcdef1234567890abcdef12345678")) {
  test(`a push with a ${label} before sha fails closed`, () => withRepo(repo => {
    const r = runStep(repo, pushEnv(repo, { BEFORE_SHA: bad }));
    assert.notEqual(r.status, 0);
    assert.equal(rangeScan(r.calls).length, 0);
  }));
}
test("a push with a bad after sha fails closed", () => withRepo(repo => {
  for (const [, bad] of BAD_SHAS) assert.notEqual(runStep(repo, pushEnv(repo, { AFTER_SHA: bad })).status, 0, bad);
}));

// ---- canaries ----

test("a canary that is not detected fails the job, in dir mode and in git mode", () => withRepo(repo => {
  for (const mode of ["DIR", "GIT"]) {
    const r = runStep(repo, prEnv(repo), { stub: { [`STUB_CANARY_${mode}`]: "miss" } });
    assert.equal(r.status, 1, mode);
    assert.match(r.stderr, new RegExp(`canary \\(${mode.toLowerCase()}\\)`));
    assert.equal(rangeScan(r.calls).length, 0);
    assert.equal(treeScan(r.calls).length, 0);
  }
}));
for (const rule of RULES_IN_CANARY) {
  test(`a canary that misses ${rule} fails the job`, () => withRepo(repo => {
    const r = runStep(repo, prEnv(repo), { stub: { STUB_CANARY_SKIP: rule } });
    assert.equal(r.status, 1);
    assert.match(r.stderr, new RegExp(`rule ${rule} did not fire`));
    assert.equal(rangeScan(r.calls).length, 0);
  }));
}

// The options of a call: everything between the mode and the path, minus --log-opts, with the
// temporary report template path replaced.
const options = call => {
  const rest = call.argv.slice(1, -1).filter(a => !a.startsWith("--log-opts="));
  return rest.map((a, i) => (rest[i - 1] === "--report-template" ? "<template>" : a));
};

test("the canaries use the same invocation form and options as the real scans", () => withRepo(repo => {
  const r = runStep(repo, prEnv(repo));
  const canaries = canaryScans(r.calls);
  assert.deepEqual(canaries.map(c => c.argv[0]), ["dir", "git"]);
  assert.equal(logOpts(canaries[1]), "--log-opts=-m --text HEAD");
  const expected = options(rangeScan(r.calls)[0]);
  assert.equal(expected.filter(a => a === "--config").length, 1);
  for (const call of [...canaries, ...rangeScan(r.calls), ...treeScan(r.calls)]) {
    assert.deepEqual(options(call), expected);
    assert.equal(call.config, ".gitleaks.toml");
  }
}));

test("the canary data is planted in a temporary directory and not left behind", () => withRepo(repo => {
  const r = runStep(repo, prEnv(repo));
  for (const call of canaryScans(r.calls)) assert.ok(!existsSync(call.argv.at(-1)), call.argv.at(-1));
  assert.deepEqual(git(repo.dir, "status", "--porcelain"), "");
}));

// ---- scans ----

test("the tree is scanned last, from the repository root", () => withRepo(repo => {
  const r = runStep(repo, prEnv(repo));
  assert.equal(r.status, 0, r.stderr);
  const last = r.calls.at(-1);
  assert.deepEqual([last.argv[0], last.argv.at(-1), last.cwd.endsWith("/repo")], ["dir", ".", true]);
  assert.equal(treeScan(r.calls).length, 1);
}));

test("a finding in the history scan or the tree scan fails the job", () => withRepo(repo => {
  const history = runStep(repo, prEnv(repo), { stub: { STUB_SCAN_GIT: "find" } });
  assert.notEqual(history.status, 0);
  assert.match(history.stdout, /finding rule=generic-api-key/);
  assert.equal(treeScan(history.calls).length, 0, "the job ends at the first failure");
  const tree = runStep(repo, prEnv(repo), { stub: { STUB_SCAN_DIR: "find" } });
  assert.notEqual(tree.status, 0);
  assert.match(tree.stdout, /finding rule=generic-api-key/);
}));

test("a history scan that never reports scanned commits fails the job", () => withRepo(repo => {
  const r = runStep(repo, prEnv(repo), { stub: { STUB_COUNT: "0" } });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /did not report scanning any commits/);
}));

test("the reports name the rule, file, line and commit and never the matched text", () => withRepo(repo => {
  const r = runStep(repo, prEnv(repo));
  const template = rangeScan(r.calls)[0].template;
  assert.match(template, /RuleID.*File.*StartLine.*Commit/s);
  assert.doesNotMatch(template, /\.(Match|Secret|Line|Fingerprint|Context|Tags|Entropy|Message|Author|Email)\b/);
  assert.equal(new Set(r.calls.filter(c => c.template).map(c => c.template)).size, 1);
}));

// ---- a neutered step is noticed by its behaviour ----

// Runs a version of the step through the cases that must end in a failure, and the one that
// must end in success; returns what went wrong. The structural checks in secret-scan.test.mjs
// read the text; these read what it does.
function behaviourViolations(script) {
  return withRepo(repo => {
    const out = [];
    const must = (label, env, stub) => {
      const r = runStep(repo, env, { script, stub });
      if (r.status === 0) out.push(`${label} did not fail the job`);
    };
    const ok = runStep(repo, prEnv(repo), { script });
    if (ok.status !== 0) out.push(`the clean case fails: ${ok.stderr.slice(0, 200)}`);
    must("a dir canary that is not detected", prEnv(repo), { STUB_CANARY_DIR: "miss" });
    must("a git canary that is not detected", prEnv(repo), { STUB_CANARY_GIT: "miss" });
    for (const rule of RULES_IN_CANARY) must(`a canary that misses ${rule}`, prEnv(repo), { STUB_CANARY_SKIP: rule });
    must("a finding in the history", prEnv(repo), { STUB_SCAN_GIT: "find" });
    must("a finding in the tree", prEnv(repo), { STUB_SCAN_DIR: "find" });
    must("a history scan with no commit count", prEnv(repo), { STUB_COUNT: "0" });
    must("an empty base sha", prEnv(repo, { BASE_SHA: "" }));
    must("an empty head sha", prEnv(repo, { HEAD_SHA: "" }));
    must("an unknown base sha", prEnv(repo, { BASE_SHA: "1234567890abcdef1234567890abcdef12345678" }));
    must("a malformed before sha", pushEnv(repo, { BEFORE_SHA: "main" }));
    must("an unexpected event", prEnv(repo, { EVENT_NAME: "schedule" }));
    const calls = readCalls(repo.log);
    if (!calls.some(c => c.argv[0] === "dir" && c.argv.at(-1) === ".")) out.push("the tree is never scanned");
    return out;
  });
}

const stepText = () => SCRIPT;
const BEHAVIOUR_MUTATIONS = [
  // Two guards each: a canary that finds nothing both exits and leaves expect_rules nothing to read.
  ["the dir canary's exit and rule check removed", t => t.replace(/(canary \(dir\): nothing was detected[^\n]*\n)\s*exit 1/, "$1").replace("expect_rules dir", "true dir")],
  ["the git canary's exit and rule check removed", t => t.replace(/(canary \(git\): nothing was detected[^\n]*\n)\s*exit 1/, "$1").replace("expect_rules git", "true git")],
  ["only the dir canary's exit removed (the rule check still fails the job)", t => t.replace(/(canary \(dir\): nothing was detected[^\n]*\n)\s*exit 1/, "$1"), true],
  ["the canary rule check made a no-op", t => t.replace('grep -q "finding rule=${rule} " <<<"$2" ||', "true ||")],
  ["the canary rule check never run", t => t.replace("expect_rules dir", "true dir").replace("expect_rules git", "true git")],
  ["the canary rule list shortened", t => t.replace("omg-url-credentials generic-api-key private-key", "generic-api-key")],
  ["the range scan's status ignored", t => t.replace('[ "${status}" -eq 0 ] || exit "${status}"', "true")],
  ["the commit count check removed", t => t.replace(/grep -Eq '\[0-9\]\+ commits scanned'[^\n]*\n/, "")],
  ["sha validation made a no-op", t => t.replace('[[ "$1" =~ ^[0-9a-f]{40}$ ]] && git cat-file -e "$1^{commit}" 2>/dev/null', "true")],
  ["the tree scan removed", t => t.replace(/\n\s*gitleaks dir "\$\{common\[@\]\}" \.\s*$/, "\n")],
  ["the event check removed", t => t.replace(/\*\)\n\s*echo "unexpected event \$\{EVENT_NAME\}" >&2\n\s*exit 1/, "*)\n              range=HEAD")],
  ["the empty base accepted", t => t.replace('is_commit "${BASE_SHA}" ||', 'true ||')],
];

test("the real step passes its behavioural checks", () => assert.deepEqual(behaviourViolations(stepText()), []));
for (const [label, mutate, redundant] of BEHAVIOUR_MUTATIONS) {
  test(`a step with ${label} is ${redundant ? "still safe" : "noticed by its behaviour"}`, () => {
    const mutated = mutate(stepText());
    assert.notEqual(mutated, stepText(), "the mutation changed the step");
    assert.equal(behaviourViolations(mutated).length > 0, !redundant);
  });
}

// ---- the real scanner ----
// These run when GITLEAKS_BIN names the pinned gitleaks (npm run test:supply-chain with
// GITLEAKS_BIN=/path/to/gitleaks). They are not registered otherwise: the CI job proves the
// same with its canaries, which run the real scanner in git mode with a -diff file. To run
// them in CI, fetch the pinned release with fetch-verified.sh in the `web` job and export
// GITLEAKS_BIN before `npm run test:supply-chain`.
if (REAL) {
  const version = spawnSync(REAL, ["version"], { encoding: "utf8" }).stdout.trim();
  test("GITLEAKS_BIN is the pinned scanner", () => assert.equal(version, pinOf(copy().jobs["secret-scan"]).version));

  const real = fn => () => withRepo(fn, { real: REAL });
  const failsWith = (r, rule) => {
    assert.notEqual(r.status, 0, r.stderr);
    assert.match(r.stdout, new RegExp(`finding rule=${rule} `));
  };
  const noLeak = (r, ...values) => {
    for (const v of values) for (const piece of [v, v.slice(4, 30)]) { assert.ok(!r.stdout.includes(piece) && !r.stderr.includes(piece), "a secret reached the log"); }
  };

  test("real: a clean range passes with the canaries", real(repo => {
    const r = runStep(repo, prEnv(repo));
    assert.equal(r.status, 0, r.stderr + r.stdout);
  }));

  test("real: a secret added in a commit and removed in the next is found", real(repo => {
    const key = fakes.aws();
    writeFileSync(join(repo.dir, "k.txt"), `aws = ${key}\n`);
    commitAll(repo.dir, "add");
    git(repo.dir, "rm", "-q", "k.txt");
    const head = commitAll(repo.dir, "remove");
    const r = runStep(repo, prEnv(repo, { HEAD_SHA: head, AFTER_SHA: head }));
    failsWith(r, "aws-access-token");
    noLeak(r, key);
  }));

  test("real: a secret added in a merge commit and removed in the next is found", real(repo => {
    git(repo.dir, "checkout", "-q", "-b", "side", repo.c2);
    writeFileSync(join(repo.dir, "s.txt"), "side\n");
    commitAll(repo.dir, "side");
    git(repo.dir, "checkout", "-q", "main");
    writeFileSync(join(repo.dir, "m.txt"), "main\n");
    commitAll(repo.dir, "main");
    git(repo.dir, "merge", "--no-ff", "--no-commit", "-q", "side");
    const key = fakes.aws();
    writeFileSync(join(repo.dir, "merged.txt"), `aws = ${key}\n`);
    commitAll(repo.dir, "merge");
    git(repo.dir, "rm", "-q", "merged.txt");
    const head = commitAll(repo.dir, "drop");
    const r = runStep(repo, prEnv(repo, { HEAD_SHA: head, AFTER_SHA: head }));
    failsWith(r, "aws-access-token");
    noLeak(r, key);
  }));

  test("real: a secret in a file marked -diff, or holding a NUL byte, is found", real(repo => {
    writeFileSync(join(repo.dir, ".gitattributes"), "*.dat -diff\n");
    const key = fakes.gateway();
    writeFileSync(join(repo.dir, "hidden.dat"), `token = ${key}\n`);
    writeFileSync(join(repo.dir, "nul.cfg"), Buffer.concat([Buffer.from("\0\0"), Buffer.from(`token = ${fakes.session()}\n`)]));
    commitAll(repo.dir, "hidden");
    git(repo.dir, "rm", "-q", "hidden.dat", "nul.cfg");
    const head = commitAll(repo.dir, "remove");
    const r = runStep(repo, prEnv(repo, { HEAD_SHA: head, AFTER_SHA: head }));
    failsWith(r, "omg-gateway-key");
    assert.match(r.stdout, /file=nul\.cfg/);
    noLeak(r, key);
  }));

  test("real: a secret fused to an identifier does not reach the log, only rule, file, line and commit", real(repo => {
    const key = fakes.gateway();
    const line = `some_identifier_before_it${key}\n`;
    writeFileSync(join(repo.dir, "fused.txt"), line);
    writeFileSync(join(repo.dir, "api.txt"), `export const apiKey = "${key.slice(4, 40)}${"Zx".repeat(8)}";\n`);
    const head = commitAll(repo.dir, "fused");
    const r = runStep(repo, prEnv(repo, { HEAD_SHA: head, AFTER_SHA: head }));
    assert.notEqual(r.status, 0);
    assert.match(r.stdout, /finding rule=omg-gateway-key file=fused\.txt line=1 commit=[0-9a-f]{40}/);
    noLeak(r, key);
    assert.ok(!r.stdout.includes("some_identifier_before_it") && !r.stderr.includes("some_identifier_before_it"), "the matched line reached the log");
  }));

  test("real: an unreachable or empty sha fails before gitleaks runs on the range", real(repo => {
    assert.notEqual(runStep(repo, prEnv(repo, { BASE_SHA: "" })).status, 0);
    assert.notEqual(runStep(repo, prEnv(repo, { BASE_SHA: "1234567890abcdef1234567890abcdef12345678" })).status, 0);
  }));

  test("real: each new rule finds its made-up value in the tree", real(repo => {
    const values = { session: fakes.session(), file: fakes.fileKey(), url: fakes.url(), pem: fakes.pem() };
    for (const [name, value] of Object.entries(values)) writeFileSync(join(repo.dir, `${name}.cfg`), `${value}\n`);
    const r = runStep(repo, prEnv(repo, { BASE_SHA: repo.c3, HEAD_SHA: repo.c3, AFTER_SHA: repo.c3 }));
    for (const rule of ["omg-session-cookie", "omg-file-encryption-key", "omg-url-credentials", "private-key"]) assert.match(r.stdout, new RegExp(`finding rule=${rule} `), rule);
    noLeak(r, ...Object.values(values).filter(v => !v.includes("\n")));
  }));

  test("real: an allowlisted fixture key is excused only as a whole", real(repo => {
    // The config's audit_export.rs entry is anchored to the whole key; a different key that
    // contains the same text in a header line is not excused.
    const begin = ["-----BEGIN", "PRIVATE", "KEY-----"].join(" ");
    const end = ["-----END", "PRIVATE", "KEY-----"].join(" ");
    const known = readFileSync(join(root, "apps/gateway/src/audit_export.rs"), "utf8").match(new RegExp(`SERVER_KEY: &str = "(${begin}\\n[\\s\\S]*?${end})`))[1];
    const lines = known.split("\n");
    const planted = [lines[0], `${lines[1]}`, ...fakes.pem().split("\n").slice(1, 3), "-----END PRIVATE KEY-----"].join("\n");
    const dir = join(repo.dir, "apps/gateway/src");
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "audit_export.rs"), `const SERVER_KEY: &str = "${known}";\n`);
    const ok = runStep(repo, prEnv(repo, { BASE_SHA: repo.c3, HEAD_SHA: repo.c3, AFTER_SHA: repo.c3 }));
    assert.equal(ok.status, 0, ok.stdout + ok.stderr);
    writeFileSync(join(dir, "audit_export.rs"), `const SERVER_KEY: &str = "${planted}";\n`);
    failsWith(runStep(repo, prEnv(repo, { BASE_SHA: repo.c3, HEAD_SHA: repo.c3, AFTER_SHA: repo.c3 })), "private-key");
  }));
}

// ---- the local hook ----

const tools = JSON.parse(spawnSync("python3", ["-I", "-c", "import json, shutil, sys; print(json.dumps([sys.executable, shutil.which('git')]))"], { encoding: "utf8" }).stdout);
const toolPath = [...new Set(tools.map(dirname))].join(":");

function hook(repo, args, { stub = {}, bin } = {}) {
  const env = { PATH: `${repo.bin}:${toolPath}`, HOME: repo.base, STUB_LOG: repo.log, STUB_NO_CANARY: "1", STUB_TREE: "1", GITLEAKS_BIN: bin ?? join(repo.bin, "gitleaks"), ...stub };
  const r = spawnSync("python3", ["-I", join(root, HOOK), ...args], { cwd: repo.dir, encoding: "utf8", env: { ...gitEnv(), ...env } });
  return { ...r, calls: readCalls(repo.log) };
}
const version = () => pinOf(copy().jobs["secret-scan"]).version;

test("the hook script pins the same scanner version as the CI job and keeps no second copy of the download", () => {
  const text = readFileSync(join(root, HOOK), "utf8");
  assert.ok(text.includes(`PINNED_VERSION = "${version()}"`));
  assert.doesNotMatch(text, /https:\/\/github\.com\/gitleaks\/gitleaks\/releases\/download|[0-9a-f]{64}/, "the URL and checksum live only in ci.yml and docs/verification.md");
  assert.ok(text.includes("docs/verification.md#secret-scanning"));
  assert.ok(text.includes(copy().jobs["secret-scan"].steps.find(s => s.name?.startsWith("Scan")).run.match(/printf '%s\\n' '(\{\{ range \. \}\}[^']*)'/)[1]), "the report template is the one in ci.yml");
});

test("the hook fails closed with install instructions when the scanner is missing", () => withRepo(repo => {
  const r = hook(repo, ["staged"], { bin: join(repo.base, "absent") });
  assert.equal(r.status, 2);
  assert.match(r.stderr, new RegExp(`gitleaks ${version().replaceAll(".", "\\.")} is not installed`));
  assert.match(r.stderr, /docs\/verification\.md#secret-scanning/);
  assert.equal(r.stdout, "");
  assert.equal(r.calls.length, 0);
}));

test("the hook refuses a scanner of another version", () => withRepo(repo => {
  const r = hook(repo, ["staged"], { stub: { STUB_VERSION: "8.0.0" } });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /version 8\.0\.0/);
  assert.deepEqual(r.calls.map(c => c.argv), [["version"]]);
}));

test("GITLEAKS_BIN given as a bare name is looked up on PATH", () => withRepo(repo => {
  writeFileSync(join(repo.dir, "x.txt"), "x\n");
  git(repo.dir, "add", "x.txt");
  const r = hook(repo, ["staged"], { bin: "gitleaks" });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.calls[0].argv[0], "version");
}));

test("staged mode scans the staged blobs themselves, in a temporary directory that is removed", () => withRepo(repo => {
  writeFileSync(join(repo.dir, ".gitattributes"), "*.dat -diff\n");
  writeFileSync(join(repo.dir, "hidden.dat"), "staged content\n");
  git(repo.dir, "add", ".gitattributes", "hidden.dat");
  writeFileSync(join(repo.dir, "hidden.dat"), "changed after staging\n");
  writeFileSync(join(repo.dir, "unstaged.txt"), "not staged\n");
  const r = hook(repo, ["staged"]);
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.calls[0].argv[0], "version");
  const scan = r.calls[1];
  assert.equal(scan.argv[0], "dir");
  assert.equal(scan.argv.at(-1), ".");
  assert.equal(scan.argv[scan.argv.indexOf("--config") + 1], join(repo.dir, CONFIG_FILE));
  for (const flag of ["--redact", "--no-banner", "--ignore-gitleaks-allow"]) assert.ok(scan.argv.includes(flag), flag);
  assert.deepEqual(Object.keys(scan.tree).sort(), [".gitattributes", "hidden.dat"], "only the staged files, from the index");
  assert.equal(scan.tree["hidden.dat"], "staged content\n", "the staged version, not the working copy");
  assert.notEqual(scan.cwd, repo.dir);
  assert.ok(!existsSync(scan.cwd), "the exported files are removed");
  assert.ok(!existsSync(scan.argv[scan.argv.indexOf("--report-template") + 1]), "the report template is removed");
  assert.match(scan.template, /RuleID.*File.*StartLine.*Commit/s);
}));

test("staged mode with nothing staged scans nothing", () => withRepo(repo => {
  const r = hook(repo, ["staged"]);
  assert.equal(r.status, 0);
  assert.deepEqual(r.calls.map(c => c.argv[0]), ["version"]);
}));

test("the hook fails on a finding, says what to do, and still removes its temporary directory", () => withRepo(repo => {
  writeFileSync(join(repo.dir, "x.txt"), "x\n");
  git(repo.dir, "add", "x.txt");
  const r = hook(repo, ["staged"], { stub: { STUB_SCAN_DIR: "find" } });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /rotate/i);
  assert.match(r.stdout, /finding rule=generic-api-key/);
  assert.ok(!existsSync(r.calls[1].cwd));
}));

test("the history command scans what CI scans for a push of the checked-out commit and no other refs", () => withRepo(repo => {
  const r = hook(repo, ["history"]);
  assert.equal(r.status, 0, r.stderr);
  const scan = r.calls[1];
  assert.equal(scan.argv[0], "git");
  assert.ok(scan.argv.includes("--log-opts=-m --text HEAD"));
  assert.ok(!scan.argv.some(a => /--all\b|--branches|--remotes|--tags/.test(a)));
}));

test("the hook refuses an unknown command", () => withRepo(repo => {
  assert.notEqual(hook(repo, ["everything"]).status, 0);
  assert.notEqual(hook(repo, []).status, 0);
}));

if (REAL) {
  test("real: the hook blocks a staged secret in a -diff file and prints no part of the value", () => withRepo(repo => {
    writeFileSync(join(repo.dir, ".gitattributes"), "*.dat -diff\n");
    const key = fakes.gateway();
    writeFileSync(join(repo.dir, "hidden.dat"), `prefix_before_${key}\n`);
    git(repo.dir, "add", "-A");
    const r = hook(repo, ["staged"], { stub: { STUB_TREE: "0" } });
    assert.equal(r.status, 1, r.stderr);
    assert.match(r.stdout, /finding rule=omg-gateway-key file=hidden\.dat line=1/);
    for (const piece of [key, key.slice(4, 30), "prefix_before_"]) assert.ok(!r.stdout.includes(piece) && !r.stderr.includes(piece));
  }, { real: REAL }));
}
