// Secret scanning (#596): the CI job, the scanner pin, the allowlist, the optional local
// hook and the ignore rules that keep generated secrets out of git and the image build
// context. Every fixture below is built from pieces at run time, so this file contains no
// string that a scanner (or GitHub's own scanning) would take for a credential.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { loadWorkflow, readLocalFromRepo, root, stepsOf } from "./lib.mjs";

const CONFIG_FILE = ".gitleaks.toml";
const HOOK = "scripts/secret-scan.py";
const FETCH = "bash .github/scripts/fetch-verified.sh";
const ZEROS = "0".repeat(40);
const ASSET = /^https:\/\/github\.com\/gitleaks\/gitleaks\/releases\/download\/v(\d+\.\d+\.\d+)\/gitleaks_\1_linux_x64\.tar\.gz$/;

// ---- the CI job ----

function pinOf(job) {
  const calls = stepsOf(job).filter(s => typeof s.run === "string" && s.run.includes("fetch-verified.sh"));
  if (calls.length !== 1) return null;
  const m = calls[0].run.match(/fetch-verified\.sh (\S+) ([0-9a-f]{64}) "\$\{RUNNER_TEMP\}\/gitleaks\.tar\.gz"/);
  const asset = m && m[1].match(ASSET);
  return asset ? { url: m[1], version: asset[1], sha256: m[2] } : null;
}

// Everything the job must keep. Returns the problems found in a parsed ci.yml.
function jobViolations(data) {
  const job = data?.jobs?.["secret-scan"];
  if (!job) return ["ci.yml has no secret-scan job"];
  const out = [];
  const bad = (cond, message) => { if (cond) out.push(message); };
  bad(JSON.stringify(job.permissions) !== JSON.stringify({ contents: "read" }), "the job must set exactly permissions: contents: read");
  bad(job.if !== undefined || job["continue-on-error"] !== undefined || job.defaults !== undefined || job.container !== undefined || job.services !== undefined, "the job must always run as written (no if, continue-on-error, defaults, container or services)");
  const steps = stepsOf(job);
  for (const step of steps) {
    bad(step.if !== undefined || step["continue-on-error"] !== undefined || step.shell !== undefined || step["working-directory"] !== undefined, `step '${step.name ?? step.uses ?? "run"}' must always run as written`);
    bad(step.uses !== undefined && !/^actions\/checkout@[0-9a-f]{40}$/.test(String(step.uses)), `step uses ${step.uses}: only actions/checkout may run here`);
  }
  const checkout = steps.find(s => String(s.uses ?? "").startsWith("actions/checkout@"));
  bad(!checkout, "the job needs actions/checkout");
  bad(checkout && String(checkout.with?.["fetch-depth"]) !== "0", "checkout must fetch the whole history (fetch-depth: 0) so every commit in the range is scanned");
  bad(checkout && checkout.with?.["persist-credentials"] !== false, "checkout must not persist credentials");
  const pin = pinOf(job);
  bad(!pin, `the scanner must be fetched by one ${FETCH} call with an exact-version gitleaks linux_x64 release URL and a 64-digit checksum`);
  const env = JSON.stringify(job.env ?? {}) + JSON.stringify(steps.map(s => s.env ?? {}));
  for (const [name, expression] of [["BASE_SHA", "github.event.pull_request.base.sha"], ["HEAD_SHA", "github.event.pull_request.head.sha"], ["BEFORE_SHA", "github.event.before"], ["EVENT_NAME", "github.event_name"]]) {
    bad(!env.includes(name) || !env.includes(expression), `${name} must come from ${expression}`);
  }
  const scans = steps.filter(s => typeof s.run === "string" && /\bgit\b.*--log-opts/s.test(s.run) && s.run.includes("gitleaks"));
  bad(scans.length !== 1, "exactly one step must run the commit-range scan (gitleaks git --log-opts)");
  const run = scans[0]?.run ?? "";
  for (const [needle, why] of [
    ["--config .gitleaks.toml", "the repository's own config"],
    ["--ignore-gitleaks-allow", "inline gitleaks:allow comments must not suppress findings"],
    ["--no-banner", "quiet output"],
    ['"${BASE_SHA}..${HEAD_SHA}"', "the pull request's commit range"],
    ['"${BEFORE_SHA}..${AFTER_SHA}"', "the pushed range"],
    ["dir ", "the tree scan (merge-commit resolutions and the final state)"],
    ["canary", "a canary that proves the scanner and config still detect"],
  ]) bad(!run.includes(needle), `the scan step must use ${needle}: ${why}`);
  bad(!/--redact(?:=100)?(?=\s)/.test(run), "the scan must redact findings completely (--redact)");
  bad(/--redact=(?!100\b)/.test(run), "the scan must not use a partial --redact");
  bad(/--exit-code(?:\s|=)/.test(run) || /\|\|\s*(true|:)/.test(run) || /--no-git|--baseline|-b\s|--gitleaks-ignore-path|--report-path|-r\s/.test(run), "the scan must fail on a finding and use no baseline, report, exit-code override or ignore file");
  bad(/--max-target-megabytes|--enable-rule/.test(run), "no size or rule restriction on the scan");
  bad(/set -euo pipefail/.test(run) === false, "the scan step must run with set -euo pipefail");
  bad(/tags|branches/.test(JSON.stringify(data.on?.pull_request ?? {})), "pull_request must not be limited by branch");
  return out;
}

const copy = () => structuredClone(loadWorkflow("ci"));
const scanStep = data => stepsOf(data.jobs["secret-scan"]).find(s => typeof s.run === "string" && s.run.includes("--log-opts"));
const installStep = data => stepsOf(data.jobs["secret-scan"]).find(s => typeof s.run === "string" && s.run.includes("fetch-verified.sh"));

test("ci.yml scans every commit of a pull request and every push in a read-only job (#596)", () => {
  assert.deepEqual(jobViolations(copy()), []);
  const data = copy();
  assert.equal(data.permissions.contents, "read");
  assert.ok(data.on.pull_request !== undefined && data.on.push !== undefined);
});

test("the scanner pin is an exact version with the checksum of the upstream checksums file (#596)", () => {
  const pin = pinOf(copy().jobs["secret-scan"]);
  assert.ok(pin, "the pin is readable");
  assert.match(pin.version, /^\d+\.\d+\.\d+$/);
  const docs = readLocalFromRepo("docs/verification.md");
  assert.ok(docs.includes(`gitleaks ${pin.version}`), "docs/verification.md names the pinned version");
  assert.ok(docs.includes(pin.sha256), "docs/verification.md records the checksum");
});

const MUTATIONS = [
  ["no job", d => { delete d.jobs["secret-scan"]; }, /no secret-scan job/],
  ["write permission", d => { d.jobs["secret-scan"].permissions = { contents: "write" }; }, /exactly permissions: contents: read/],
  ["extra permission", d => { d.jobs["secret-scan"].permissions = { contents: "read", "id-token": "write" }; }, /exactly permissions/],
  ["no job permissions", d => { delete d.jobs["secret-scan"].permissions; }, /exactly permissions/],
  ["job skipped on pull requests", d => { d.jobs["secret-scan"].if = "github.event_name == 'push'"; }, /always run as written/],
  ["job allowed to fail", d => { d.jobs["secret-scan"]["continue-on-error"] = true; }, /always run as written/],
  ["step allowed to fail", d => { scanStep(d)["continue-on-error"] = true; }, /always run as written/],
  ["step skipped", d => { scanStep(d).if = "false"; }, /always run as written/],
  ["no-op step interpreter", d => { scanStep(d)["working-directory"] = "/tmp"; }, /always run as written/],
  ["shallow checkout", d => { stepsOf(d.jobs["secret-scan"])[0].with = { "persist-credentials": false }; }, /fetch-depth/],
  ["depth one", d => { stepsOf(d.jobs["secret-scan"])[0].with = { "fetch-depth": 1, "persist-credentials": false }; }, /fetch-depth/],
  ["persisted credentials", d => { stepsOf(d.jobs["secret-scan"])[0].with = { "fetch-depth": 0 }; }, /persist credentials/],
  ["a third-party action", d => { d.jobs["secret-scan"].steps.push({ uses: `gitleaks/gitleaks-action@${"1".repeat(40)}` }); }, /only actions\/checkout may run here/],
  ["a local action", d => { d.jobs["secret-scan"].steps.push({ uses: "./.github/actions/setup-node" }); }, /only actions\/checkout may run here/],
  ["an unpinned checkout", d => { stepsOf(d.jobs["secret-scan"])[0].uses = "actions/checkout@v4"; }, /only actions\/checkout may run here/],
  ["a download by curl", d => { installStep(d).run = installStep(d).run.replace(FETCH, "curl -fsSL -o x"); }, /fetched by one/],
  ["a floating version", d => { installStep(d).run = installStep(d).run.replace(/download\/v[\d.]+\/gitleaks_[\d.]+_/, "download/latest/gitleaks_"); }, /fetched by one/],
  ["a short checksum", d => { installStep(d).run = installStep(d).run.replace(/ [0-9a-f]{64} /, " abc123 "); }, /fetched by one/],
  ["another release host", d => { installStep(d).run = installStep(d).run.replace("https://github.com/gitleaks/", "https://example.org/gitleaks/"); }, /fetched by one/],
  ["no redaction", d => { scanStep(d).run = scanStep(d).run.replaceAll("--redact ", ""); }, /redact/],
  ["partial redaction", d => { scanStep(d).run = scanStep(d).run.replaceAll("--redact ", "--redact=20 "); }, /partial --redact/],
  ["inline allow comments honoured", d => { scanStep(d).run = scanStep(d).run.replaceAll("--ignore-gitleaks-allow", ""); }, /ignore-gitleaks-allow/],
  ["another config", d => { scanStep(d).run = scanStep(d).run.replaceAll("--config .gitleaks.toml", "--config /dev/null"); }, /--config \.gitleaks\.toml/],
  ["findings swallowed", d => { scanStep(d).run += '\ngitleaks version || true\ngitleaks git --log-opts="x" . || true\n'; }, /fail on a finding/],
  ["exit code overridden", d => { scanStep(d).run = scanStep(d).run.replaceAll("--redact ", "--redact --exit-code 0 "); }, /fail on a finding/],
  ["a baseline", d => { scanStep(d).run = scanStep(d).run.replaceAll("--redact ", "--redact --baseline-path /tmp/b.json "); }, /fail on a finding/],
  ["only the last commit", d => { scanStep(d).run = scanStep(d).run.replace('"${BASE_SHA}..${HEAD_SHA}"', '"${HEAD_SHA}^!"'); }, /commit range/],
  ["no pushed range", d => { scanStep(d).run = scanStep(d).run.replace('"${BEFORE_SHA}..${AFTER_SHA}"', '"${AFTER_SHA}^!"'); }, /pushed range/],
  ["no tree scan", d => { scanStep(d).run = scanStep(d).run.replaceAll("dir ", "dr "); }, /tree scan/],
  ["no canary", d => { scanStep(d).run = scanStep(d).run.replaceAll("canary", "sample"); }, /canary/],
  ["no strict mode", d => { scanStep(d).run = scanStep(d).run.replace("set -euo pipefail", "set +e"); }, /pipefail/],
  ["base sha from the wrong event field", d => { d.jobs["secret-scan"].env.BASE_SHA = "${{ github.sha }}"; }, /BASE_SHA/],
  ["pull requests limited to branches", d => { d.on.pull_request = { branches: ["main"] }; }, /limited by branch/],
];
for (const [label, mutate, reason] of MUTATIONS) {
  test(`the secret-scan job check refuses: ${label}`, () => {
    const data = copy();
    mutate(data);
    assert.match(jobViolations(data).join("\n"), reason);
  });
}

// ---- the config and its allowlist ----

function readConfig(text) {
  const dir = mkdtempSync(join(tmpdir(), "gitleaks-config-"));
  try {
    writeFileSync(join(dir, "c.toml"), text);
    const r = spawnSync("python3", ["-I", "-c", "import json, sys, tomllib; print(json.dumps(tomllib.load(open(sys.argv[1], 'rb'))))", join(dir, "c.toml")], { encoding: "utf8" });
    assert.equal(r.status, 0, r.stderr);
    return JSON.parse(r.stdout);
  } finally { rmSync(dir, { recursive: true, force: true }); }
}

const EXACT_PATH = /^\^[A-Za-z0-9_\-/]+(?:\\\.[A-Za-z0-9_-]+)+\$$/; // ^dir/file\.ext$ : one file, no wildcard
// A value regex must contain a literal run of at least 8 characters (escaped characters
// count as literals; classes, groups and quantified characters do not).
function longestLiteralRun(regex) {
  const stripped = regex.replace(/\[(?:\\.|[^\]\\])*\]/g, "\0").replace(/\{\d+(?:,\d*)?\}/g, "\0").replace(/\\([^\w])/g, "$1").replace(/\\[a-zA-Z]/g, "\0").replace(/[^\0]?[*+?]/g, "\0").replace(/\(\?[:a-z]*|[()|^$]/g, "\0");
  return Math.max(0, ...stripped.split("\0").map(run => run.length));
}

function allowlistViolations(config) {
  const out = [];
  const lists = [
    ...(config.allowlists ?? []).map((a, i) => ({ where: `allowlists[${i}]`, a })),
    ...(config.rules ?? []).flatMap(r => (r.allowlists ?? []).map((a, i) => ({ where: `rule ${r.id} allowlists[${i}]`, a }))),
  ];
  if (config.allowlist) out.push("the legacy [allowlist] table is not used; use [[allowlists]]");
  for (const { where, a } of lists) {
    const known = new Set(["description", "condition", "paths", "regexes", "commits", "targetRules", "regexTarget"]);
    for (const key of Object.keys(a)) if (!known.has(key)) out.push(`${where}: '${key}' is not allowed (stopwords and other broad switches are refused)`);
    if (typeof a.description !== "string" || a.description.trim().length < 20) out.push(`${where}: needs a description that gives the reason`);
    const paths = a.paths ?? [];
    const regexes = a.regexes ?? [];
    const commits = a.commits ?? [];
    if (paths.length === 0) out.push(`${where}: needs the exact file it applies to (paths)`);
    if (regexes.length + commits.length === 0) out.push(`${where}: a path alone would allow the whole file; add the exact value (regexes) or the commit (commits)`);
    if (a.condition !== "AND") out.push(`${where}: condition must be "AND" (the default OR makes every criterion allow on its own)`);
    for (const p of paths) if (!EXACT_PATH.test(p)) out.push(`${where}: path ${p} must be one anchored file (^dir/file\\.ext$), not a directory or a pattern`);
    for (const c of commits) if (!/^[0-9a-f]{40}$/.test(c)) out.push(`${where}: commit ${c} must be a full 40-digit id`);
    for (const r of regexes) {
      if (longestLiteralRun(r) < 8) out.push(`${where}: regex ${JSON.stringify(r)} is too broad (it needs a literal run of 8 or more characters)`);
    }
    if ((a.targetRules ?? []).length === 0) out.push(`${where}: name the rule it silences (targetRules)`);
  }
  return out;
}

test("the gitleaks config extends the default rules, adds the gateway's own key shapes and has a narrow allowlist (#596)", () => {
  const config = readConfig(readLocalFromRepo(CONFIG_FILE));
  assert.equal(config.extend?.useDefault, true, "the default rules stay on");
  assert.deepEqual(allowlistViolations(config), []);
  const ids = (config.rules ?? []).map(r => r.id);
  assert.ok(ids.includes("omg-gateway-key"), "the gateway key rule exists");
  assert.equal(new Set(ids).size, ids.length);
  assert.equal(existsSync(join(root, ".gitleaksignore")), false, "no .gitleaksignore (a fingerprint list would silence findings outside the config)");
  const tracked = spawnSync("git", ["ls-files"], { cwd: root, encoding: "utf8" }).stdout.split("\n");
  assert.deepEqual(tracked.filter(f => /(^|\/)\.gitleaksignore$/.test(f)), []);
  assert.deepEqual(tracked.filter(f => /(^|\/)\.gitleaks\.toml$/.test(f) && f !== CONFIG_FILE), [], "one config only");
  assert.ok(existsSync(join(root, CONFIG_FILE)));
});

test("every allowlist entry in the real config has a unique reason", () => {
  const config = readConfig(readLocalFromRepo(CONFIG_FILE));
  const lists = [...(config.allowlists ?? []), ...(config.rules ?? []).flatMap(r => r.allowlists ?? [])];
  const reasons = lists.map(a => a.description);
  assert.equal(new Set(reasons).size, reasons.length);
});

const FIXTURE_LIST = { description: "Test-only value in one named file; see the test module.", condition: "AND", paths: ["^apps/gateway/src/example\\.rs$"], regexes: ["exact-test-value-1234"], targetRules: ["generic-api-key"] };
const LIST_MUTATIONS = [
  ["a whole directory", { paths: ["^tests/.*$"] }, /one anchored file/],
  ["a directory prefix", { paths: ["^tests/"] }, /one anchored file/],
  ["every rust file", { paths: ["^.*\\.rs$"] }, /one anchored file/],
  ["a glob", { paths: ["tests/**"] }, /one anchored file/],
  ["an unanchored path", { paths: ["example\\.rs"] }, /one anchored file/],
  ["no path", { paths: [] }, /exact file/],
  ["no value", { regexes: [] }, /whole file/],
  ["a match-all regex", { regexes: [".*"] }, /too broad/],
  ["a short regex", { regexes: ["secret"] }, /too broad/],
  ["a class-only regex", { regexes: ["[A-Za-z0-9]+"] }, /too broad/],
  ["OR semantics", { condition: "OR" }, /condition must be "AND"/],
  ["default semantics", { condition: undefined }, /condition must be "AND"/],
  ["no reason", { description: "" }, /description/],
  ["a thin reason", { description: "test" }, /description/],
  ["stopwords", { stopwords: ["example"] }, /stopwords/],
  ["no rule named", { targetRules: [] }, /name the rule/],
  ["a short commit", { commits: ["abc123"] }, /40-digit/],
];
test("a fixture allowlist entry passes the narrowness check", () => assert.deepEqual(allowlistViolations({ allowlists: [FIXTURE_LIST] }), []));
for (const [label, change, reason] of LIST_MUTATIONS) {
  test(`the allowlist check refuses ${label}`, () => {
    const entry = { ...FIXTURE_LIST, ...change };
    for (const key of Object.keys(entry)) if (entry[key] === undefined) delete entry[key];
    assert.match(allowlistViolations({ allowlists: [entry] }).join("\n"), reason);
  });
}
test("a rule-level allowlist and the legacy table are checked too", () => {
  assert.match(allowlistViolations({ rules: [{ id: "r", allowlists: [{ ...FIXTURE_LIST, paths: ["^src/"] }] }] }).join("\n"), /rule r allowlists\[0\].*one anchored file/);
  assert.match(allowlistViolations({ allowlist: { paths: ["^.*$"] } }).join("\n"), /legacy/);
});

// The gateway key rule: its shapes are in apps/gateway/src/auth.rs and admin_tokens.rs.
test("the omg-gateway-key rule matches inference keys and admin tokens and nothing looser", () => {
  const rule = readConfig(readLocalFromRepo(CONFIG_FILE)).rules.find(r => r.id === "omg-gateway-key");
  const re = new RegExp(rule.regex);
  const hex = (digit, n) => digit.repeat(n);
  const uuid = hex("a1", 16);
  const secret = hex("c3", 32);
  for (const token of [`omg_${uuid}.${secret}`, `omg_admin_${uuid}.${secret}`]) {
    assert.ok(re.test(token), token.slice(0, 12));
    assert.ok(re.test(`Authorization: Bearer ${token}`));
    assert.ok(re.test(`"${token}"`));
  }
  for (const text of [`omg_${uuid}.${hex("c3", 31)}`, `omg_${hex("a1", 15)}.${secret}`, `omg_${uuid}:${secret}`, `omg_bad.secret`, `omg_admin_${"0".repeat(32)}.${"z".repeat(64)}`, "omg_admin_", `xomg_${uuid}.${secret}`]) {
    assert.equal(re.test(text), false, text.slice(0, 16));
  }
  assert.ok(rule.keywords.includes("omg_"));
});

// ---- the optional local hook ----

function hookRun(args, { stub, env = {} } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "secret-scan-hook-"));
  try {
    const log = join(dir, "args.log");
    const bin = join(dir, "gitleaks");
    if (stub) {
      writeFileSync(bin, `#!/usr/bin/env python3\nimport sys\nopen(${JSON.stringify(log)}, "a").write(" ".join(sys.argv[1:]) + "\\n")\n${stub}\n`);
      chmodSync(bin, 0o755);
    }
    const r = spawnSync("python3", ["-I", join(root, HOOK), ...args], { cwd: root, encoding: "utf8", env: { PATH: "/usr/bin:/bin", HOME: dir, GITLEAKS_BIN: stub ? bin : join(dir, "absent"), ...env } });
    return { ...r, calls: existsSync(log) ? readFileSync(log, "utf8").trim().split("\n") : [] };
  } finally { rmSync(dir, { recursive: true, force: true }); }
}
const versionStub = (version, status = 0) => `if sys.argv[1:] == ["version"]:\n    print(${JSON.stringify(version)})\n    sys.exit(0)\nsys.exit(${status})`;

test("the hook script pins the same scanner as the CI job", () => {
  const pin = pinOf(copy().jobs["secret-scan"]);
  const text = readFileSync(join(root, HOOK), "utf8");
  assert.ok(text.includes(`PINNED_VERSION = "${pin.version}"`));
  assert.ok(text.includes(`"${pin.sha256}"`) && text.includes(`"${pin.url}"`));
});

test("the hook fails closed with install instructions when the scanner is missing", () => {
  const r = hookRun(["staged"]);
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /gitleaks .* is not installed/);
  assert.match(r.stderr, /fetch-verified\.sh/);
  assert.match(r.stderr, new RegExp(pinOf(copy().jobs["secret-scan"]).sha256));
  assert.equal(r.stdout, "");
});

test("the hook refuses a scanner of another version", () => {
  const r = hookRun(["staged"], { stub: versionStub("8.0.0") });
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /version 8\.0\.0/);
  assert.deepEqual(r.calls, ["version"]);
});

test("the hook runs the pinned scanner on the staged changes with the repository config and full redaction", () => {
  const version = pinOf(copy().jobs["secret-scan"]).version;
  const r = hookRun(["staged"], { stub: versionStub(version) });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.calls[0], "version");
  const scan = r.calls[1];
  for (const part of ["git", "--staged", "--redact", `--config ${join(root, CONFIG_FILE)}`, "--ignore-gitleaks-allow", "--no-banner"]) assert.ok(scan.includes(part), part);
  assert.equal(r.calls.length, 2);
});

test("the hook fails when the scanner reports a finding, and says what to do", () => {
  const version = pinOf(copy().jobs["secret-scan"]).version;
  const r = hookRun(["staged"], { stub: `if sys.argv[1:] == ["version"]:\n    print(${JSON.stringify(version)})\n    sys.exit(0)\nsys.exit(1)` });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /rotate/i);
});

test("the history command scans branches, remotes and tags but not other tools' private refs", () => {
  const version = pinOf(copy().jobs["secret-scan"]).version;
  const r = hookRun(["history"], { stub: versionStub(version) });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.calls[1], /--log-opts=--branches --remotes --tags/);
  assert.doesNotMatch(r.calls[1], /--all\b/);
});

test("the hook refuses an unknown command", () => {
  assert.notEqual(hookRun(["everything"]).status, 0);
  assert.notEqual(hookRun([]).status, 0);
});

// ---- ignored secret locations ----

// Paths where this repository's tools write secrets or where people keep them. A file here
// must stay out of git and out of the image build context.
const SECRET_PLACES = [
  ".env", ".env.local", ".env.production", "apps/web/.env.local", "apps/web/public/.env", "apps/web/public/.env.local", "apps/gateway/src/.env",
  "ai.env", "boat.env", "deploy/anything.env", "apps/gateway/src/anything.env", "apps/web/public/anything.env", "tools/loadgen/src/anything.env",
  ".local/staging/staging.env", ".local/staging/local-ca.key", ".local/loadtest/stack.env", ".local/loadtest/userlist.txt", ".local/enterprise-rebuild/demo-runtime.env",
];
// Tracked examples: they must stay tracked, so the ignore rules may not catch them.
const TRACKED_EXAMPLES = [".env.example", ".env.demo.example", "apps/web/.env.example", "deploy/staging/staging.env.example"];
// Where the tools write: the paths must still appear in the scripts, or this list is stale.
const WRITERS = [["scripts/staging.py", ".local"], ["scripts/staging.py", "staging.env"], ["scripts/loadtest.py", ".local/loadtest"], ["scripts/demo-runtime.py", ".local/enterprise-rebuild/demo-runtime.env"]];

function gitIgnores(gitignore, paths) {
  const dir = mkdtempSync(join(tmpdir(), "gitignore-"));
  try {
    const git = (...args) => spawnSync("git", ["-c", "core.excludesFile=/dev/null", ...args], { cwd: dir, encoding: "utf8" });
    assert.equal(git("init", "-q").status, 0);
    writeFileSync(join(dir, ".gitignore"), gitignore);
    return Object.fromEntries(paths.map(p => [p, git("check-ignore", "-q", "--no-index", p).status === 0]));
  } finally { rmSync(dir, { recursive: true, force: true }); }
}

// Docker's .dockerignore rules (moby/patternmatcher): patterns in order, the last one that
// matches the path or one of its parent directories decides; `!` re-includes.
function dockerPatternToRegExp(pattern) {
  let out = "";
  for (let i = 0; i < pattern.length; i++) {
    const c = pattern[i];
    if (c === "*" && pattern[i + 1] === "*") {
      if (pattern[i + 2] === "/") { out += "(?:.*/)?"; i += 2; } else { out += ".*"; i += 1; }
    } else if (c === "*") out += "[^/]*";
    else if (c === "?") out += "[^/]";
    else out += c.replace(/[.+^${}()|[\]\\]/g, "\\$&");
  }
  return new RegExp(`^${out}$`);
}
function inBuildContext(dockerignore, path) {
  const patterns = dockerignore.split("\n").map(l => l.trim()).filter(l => l && !l.startsWith("#"));
  const parents = [];
  const parts = path.split("/");
  for (let i = 1; i <= parts.length; i++) parents.push(parts.slice(0, i).join("/"));
  let excluded = false;
  for (const raw of patterns) {
    const negated = raw.startsWith("!");
    const re = dockerPatternToRegExp(negated ? raw.slice(1) : raw);
    if (parents.some(p => re.test(p))) excluded = !negated;
  }
  return !excluded;
}

function secretPlaceViolations(gitignore, dockerignore) {
  const out = [];
  const ignored = gitIgnores(gitignore, [...SECRET_PLACES, ...TRACKED_EXAMPLES]);
  for (const p of SECRET_PLACES) if (!ignored[p]) out.push(`${p} is not ignored by git`);
  for (const p of TRACKED_EXAMPLES) if (ignored[p]) out.push(`${p} is a tracked example but .gitignore ignores it`);
  for (const p of SECRET_PLACES) if (inBuildContext(dockerignore, p)) out.push(`${p} would enter the image build context`);
  for (const p of ["Dockerfile", "apps/gateway/src/main.rs", "apps/web/src/main.tsx", "Cargo.toml"]) if (!inBuildContext(dockerignore, p)) out.push(`${p} is a build input but .dockerignore excludes it (the matcher or the file is wrong)`);
  return out;
}

const realIgnores = () => [readLocalFromRepo(".gitignore"), readLocalFromRepo(".dockerignore")];

test("generated-secret locations are ignored by git and absent from the image build context (#596)", () => {
  assert.deepEqual(secretPlaceViolations(...realIgnores()), []);
});

test("the tracked example files really are tracked", () => {
  const tracked = new Set(spawnSync("git", ["ls-files"], { cwd: root, encoding: "utf8" }).stdout.split("\n"));
  for (const p of TRACKED_EXAMPLES) assert.ok(tracked.has(p), p);
  assert.deepEqual([...tracked].filter(f => /\.env$/.test(f)), [], "no tracked file ends in .env");
});

test("the secret location list matches what the tools write", () => {
  for (const [script, needle] of WRITERS) assert.ok(readLocalFromRepo(script).includes(needle), `${script} no longer mentions ${needle}`);
});

const lines = text => text.split("\n");
const drop = (text, pattern) => lines(text).filter(l => l.trim() !== pattern).join("\n");

test("the git ignore check catches an un-ignored secret file", () => {
  const [gitignore, dockerignore] = realIgnores();
  const found = (text) => secretPlaceViolations(text, dockerignore).join("\n");
  assert.match(found(drop(gitignore, "*.env")), /boat\.env is not ignored by git/);
  assert.match(found(drop(gitignore, "*.env")), /apps\/web\/public\/anything\.env is not ignored by git/);
  assert.match(found(drop(gitignore, ".env.*")), /\.env\.local is not ignored by git/);
  // `.env` and `*.env` both match a file called `.env`: only dropping both un-ignores it.
  assert.match(found(drop(drop(gitignore, ".env"), "*.env")), /(^|\n)\.env is not ignored by git/);
  assert.match(found(drop(gitignore, "/.local/")), /\.local\/loadtest\/userlist\.txt is not ignored by git/);
  assert.match(found(`${gitignore}\n!ai.env\n`), /ai\.env is not ignored by git/);
  assert.match(found(`${gitignore}\n!*.env\n`), /boat\.env is not ignored by git/);
  assert.match(found(`${gitignore}\n.env.example\n`), /\.env\.example is a tracked example/);
  assert.match(found(`${gitignore}\n*.example\n`), /is a tracked example/);
});

test("the build context check catches a .dockerignore that lets a secret file in", () => {
  const [gitignore, dockerignore] = realIgnores();
  const found = text => secretPlaceViolations(gitignore, text).join("\n");
  assert.match(found(drop(dockerignore, "**/*.env")), /apps\/web\/public\/anything\.env would enter the image build context/);
  assert.match(found(drop(dockerignore, "**/.env.*")), /apps\/web\/public\/\.env\.local would enter the image build context/);
  assert.match(found(drop(drop(dockerignore, "**/.env"), "**/*.env")), /apps\/gateway\/src\/\.env would enter the image build context/);
  assert.match(found(drop(dockerignore, "**")), /\.local\/loadtest\/userlist\.txt would enter the image build context/);
  assert.match(found(`${dockerignore}\n!apps/gateway/src/anything.env\n`), /apps\/gateway\/src\/anything\.env would enter the image build context/);
  assert.match(found("**\n"), /build input but \.dockerignore excludes it/);
});
