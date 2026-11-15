// Secret scanning (#596): the CI job's structure, the scanner pin, the config and its allowlist,
// the project's own rules, and the ignore rules that keep generated secrets out of git and the
// image build context. What the scan step and the local hook DO is tested by executing them in
// secret-scan-run.test.mjs. Every fixture is built from pieces at run time, so this file
// contains no string that a scanner (or GitHub's own scanning) would take for a credential.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { readLocalFromRepo, root, stepsOf } from "./lib.mjs";
import { CONFIG_FILE, FETCH, copy, fakes, installStep, jobOf, pinOf, readConfig, scanStep } from "./secret-scan-lib.mjs";

// ---- the CI job ----

const EXPECTED_ENV = {
  EVENT_NAME: "${{ github.event_name }}",
  BASE_SHA: "${{ github.event.pull_request.base.sha }}",
  HEAD_SHA: "${{ github.event.pull_request.head.sha }}",
  BEFORE_SHA: "${{ github.event.before }}",
  AFTER_SHA: "${{ github.sha }}",
};
const COMMON = 'common=(--config .gitleaks.toml --redact --no-banner --no-color --ignore-gitleaks-allow --log-level info --report-format template --report-template "${work}/report.tmpl" --report-path -)';
// Every line that runs gitleaks on a repository or a directory, exactly. The last one is the
// scan of the checked-out tree; the first two are the canaries.
const CALLS = [
  'if report="$(gitleaks dir "${common[@]}" "${work}/dir-canary" 2>/dev/null)"; then',
  'if report="$(gitleaks git "${common[@]}" --log-opts="-m --text HEAD" "${work}/git-canary" 2>/dev/null)"; then',
  'gitleaks git "${common[@]}" --log-opts="-m --text ${range}" . 2>"${work}/gitleaks.log" || status=$?',
  'gitleaks dir "${common[@]}" .',
];

function scriptViolations(run) {
  const out = [];
  const bad = (cond, message) => { if (cond) out.push(message); };
  const lines = run.split("\n").map(l => l.trim()).filter(l => l && !l.startsWith("#"));
  bad(lines[0] !== "set -euo pipefail", "the script must start with set -euo pipefail");
  bad(/set \+[eo]|set \+o errexit|\|\|\s*(true|:)(\s|$)|;\s*true(\s|$)|\btrap\b.*\bERR\b/.test(run), "nothing may switch off errexit or swallow a failure");
  bad(JSON.stringify(lines.filter(l => l.startsWith("common="))) !== JSON.stringify([COMMON]), "common must be exactly the reviewed option list");
  bad((run.match(/--config/g) ?? []).length !== 1, "--config must appear once, in common");
  const calls = lines.filter(l => /(^|[\s(])gitleaks (git|dir)\b/.test(l));
  bad(JSON.stringify(calls) !== JSON.stringify(CALLS), `every gitleaks call must be exactly one of the reviewed forms, got ${JSON.stringify(calls)}`);
  bad(lines.some(l => /(^|[\s(])gitleaks\s+(?!git |dir )[a-z]/.test(l) && !/echo "/.test(l)), "no other gitleaks command");
  bad(lines.at(-1) !== CALLS[3], "the last command must be the scan of the whole checked-out tree");
  bad(/(^|[\s;])(BASE_SHA|HEAD_SHA|BEFORE_SHA|AFTER_SHA|EVENT_NAME)=/.test(run), "the event values must not be reassigned in the script");
  bad(/--first-parent|--exit-code|--baseline|-b\s|--no-git|--max-target|--enable-rule|--redact=|--verbose|\s-v\s|--gitleaks-ignore-path|-i\s|--max-decode|--diagnostics/.test(run), "an option that narrows or weakens the scan");
  bad(!lines.includes('echo "scanning ${commits} commits of ${range}"'), "the scan must say what it scans");
  return out;
}

// Everything the job must keep.
function jobViolations(data) {
  const job = jobOf(data);
  if (!job) return ["ci.yml has no secret-scan job"];
  const out = [];
  const bad = (cond, message) => { if (cond) out.push(message); };
  bad(JSON.stringify(job.permissions) !== JSON.stringify({ contents: "read" }), "the job must set exactly permissions: contents: read");
  bad(job.if !== undefined || job["continue-on-error"] !== undefined || job.defaults !== undefined || job.container !== undefined || job.services !== undefined || job.needs !== undefined || job.strategy !== undefined, "the job must always run as written (no if, continue-on-error, defaults, container, services, needs or strategy)");
  bad(!(Number(job["timeout-minutes"]) >= 5 && Number(job["timeout-minutes"]) <= 60), "timeout-minutes must be between 5 and 60 (a full-history scan takes about a minute)");
  bad(JSON.stringify(job.env) !== JSON.stringify(EXPECTED_ENV), `the job env must be exactly ${JSON.stringify(EXPECTED_ENV)}`);
  const steps = stepsOf(job);
  bad(steps.length !== 3, "the job has exactly three steps: checkout, install, scan");
  for (const step of steps) {
    bad(step.if !== undefined || step["continue-on-error"] !== undefined || step.shell !== undefined || step["working-directory"] !== undefined || step.env !== undefined || step["timeout-minutes"] !== undefined, `step '${step.name ?? step.uses ?? "run"}' must always run as written, with the job env only`);
    bad(step.uses !== undefined && !/^actions\/checkout@[0-9a-f]{40}$/.test(String(step.uses)), `step uses ${step.uses}: only actions/checkout may run here`);
  }
  const checkout = steps[0];
  bad(!String(checkout?.uses ?? "").startsWith("actions/checkout@"), "the first step is actions/checkout");
  bad(JSON.stringify(checkout?.with) !== JSON.stringify({ "fetch-depth": 0, "persist-credentials": false }), "checkout takes exactly fetch-depth: 0 and persist-credentials: false (no ref, sparse-checkout, filter or fetch-tags)");
  const pin = pinOf(job);
  bad(!pin, `the scanner must be fetched by one ${FETCH} call with an exact-version gitleaks linux_x64 release URL and a 64-digit checksum`);
  bad(steps[1] !== installStep(data) || steps[2] !== scanStep(data), "the steps are install then scan, with the reviewed names");
  bad(typeof scanStep(data)?.run !== "string", "the scan step runs a script");
  out.push(...scriptViolations(scanStep(data)?.run ?? ""));
  bad(JSON.stringify(data.on?.pull_request ?? {}) !== "{}" || !data.on?.push, "pull_request is unrestricted and push is on");
  return out;
}

test("ci.yml scans every commit of a pull request and every push in a read-only job (#596)", () => {
  assert.deepEqual(jobViolations(copy()), []);
  const data = copy();
  assert.equal(data.permissions.contents, "read");
  assert.deepEqual(data.on.push.branches, ["main"]);
});

test("the scanner pin is an exact version with the checksum of the upstream checksums file (#596)", () => {
  const pin = pinOf(jobOf(copy()));
  assert.ok(pin, "the pin is readable");
  const docs = readLocalFromRepo("docs/verification.md");
  assert.ok(docs.includes(`gitleaks ${pin.version}`), "docs/verification.md names the pinned version");
  assert.ok(docs.includes(pin.sha256), "docs/verification.md records the checksum");
});

const script = d => scanStep(d).run;
const set = (d, f) => { scanStep(d).run = f(script(d)); };
const MUTATIONS = [
  ["no job", d => { delete d.jobs["secret-scan"]; }, /no secret-scan job/],
  ["write permission", d => { jobOf(d).permissions = { contents: "write" }; }, /exactly permissions: contents: read/],
  ["extra permission", d => { jobOf(d).permissions = { contents: "read", "id-token": "write" }; }, /exactly permissions/],
  ["no job permissions", d => { delete jobOf(d).permissions; }, /exactly permissions/],
  ["job skipped on pull requests", d => { jobOf(d).if = "github.event_name == 'push'"; }, /always run as written/],
  ["job allowed to fail", d => { jobOf(d)["continue-on-error"] = true; }, /always run as written/],
  ["a job dependency", d => { jobOf(d).needs = ["web"]; }, /always run as written/],
  ["step allowed to fail", d => { scanStep(d)["continue-on-error"] = true; }, /always run as written/],
  ["step skipped", d => { scanStep(d).if = "false"; }, /always run as written/],
  ["no-op step interpreter", d => { scanStep(d)["working-directory"] = "/tmp"; }, /always run as written/],
  ["step env that shadows the event values", d => { scanStep(d).env = { BASE_SHA: "x" }; }, /job env only/],
  ["timeout of one minute", d => { jobOf(d)["timeout-minutes"] = 1; }, /timeout-minutes/],
  ["no timeout", d => { delete jobOf(d)["timeout-minutes"]; }, /timeout-minutes/],
  ["shallow checkout", d => { jobOf(d).steps[0].with = { "persist-credentials": false }; }, /fetch-depth: 0/],
  ["depth one", d => { jobOf(d).steps[0].with = { "fetch-depth": 1, "persist-credentials": false }; }, /fetch-depth: 0/],
  ["persisted credentials", d => { jobOf(d).steps[0].with = { "fetch-depth": 0 }; }, /persist-credentials/],
  ["a checkout ref", d => { jobOf(d).steps[0].with.ref = "main"; }, /no ref, sparse-checkout/],
  ["a sparse checkout", d => { jobOf(d).steps[0].with["sparse-checkout"] = "docs"; }, /no ref, sparse-checkout/],
  ["a partial clone filter", d => { jobOf(d).steps[0].with.filter = "blob:none"; }, /no ref, sparse-checkout/],
  ["a third-party action", d => { jobOf(d).steps.push({ uses: `gitleaks/gitleaks-action@${"1".repeat(40)}` }); }, /only actions\/checkout may run here/],
  ["a local action", d => { jobOf(d).steps.push({ uses: "./.github/actions/setup-node" }); }, /only actions\/checkout may run here/],
  ["an unpinned checkout", d => { jobOf(d).steps[0].uses = "actions/checkout@v4"; }, /only actions\/checkout may run here/],
  ["an extra step", d => { jobOf(d).steps.push({ run: "true" }); }, /exactly three steps/],
  ["a download by curl", d => { installStep(d).run = installStep(d).run.replace(FETCH, "curl -fsSL -o x"); }, /fetched by one/],
  ["a floating version", d => { installStep(d).run = installStep(d).run.replace(/download\/v[\d.]+\/gitleaks_[\d.]+_/, "download/latest/gitleaks_"); }, /fetched by one/],
  ["a short checksum", d => { installStep(d).run = installStep(d).run.replace(/ [0-9a-f]{64} /, " abc123 "); }, /fetched by one/],
  ["another release host", d => { installStep(d).run = installStep(d).run.replace("https://github.com/gitleaks/", "https://example.org/gitleaks/"); }, /fetched by one/],
  ["BASE_SHA from the wrong field", d => { jobOf(d).env.BASE_SHA = "${{ github.sha }}"; }, /job env must be exactly/],
  ["HEAD_SHA from the wrong field", d => { jobOf(d).env.HEAD_SHA = "${{ github.event.pull_request.base.sha }}"; }, /job env must be exactly/],
  ["BEFORE_SHA from the wrong field", d => { jobOf(d).env.BEFORE_SHA = "${{ github.sha }}"; }, /job env must be exactly/],
  ["AFTER_SHA from the wrong field", d => { jobOf(d).env.AFTER_SHA = "${{ github.event.before }}"; }, /job env must be exactly/],
  ["EVENT_NAME from the wrong field", d => { jobOf(d).env.EVENT_NAME = "pull_request"; }, /job env must be exactly/],
  ["a sixth env variable", d => { jobOf(d).env.GITLEAKS_CONFIG = "/dev/null"; }, /job env must be exactly/],
  ["pull requests limited to branches", d => { d.on.pull_request = { branches: ["main"] }; }, /pull_request is unrestricted/],
  // The script.
  ["no strict mode", d => set(d, t => t.replace("set -euo pipefail", "set +e")), /set -euo pipefail/],
  ["errexit switched off later", d => set(d, t => t.replace("zeros=", "set +e\n          zeros=")), /switch off errexit/],
  ["the final scan swallowed", d => set(d, t => `${t.trimEnd()} || true\n`), /swallow a failure/],
  ["the final scan swallowed with a colon", d => set(d, t => `${t.trimEnd()} || :\n`), /swallow a failure/],
  ["a failure followed by true", d => set(d, t => `${t.trimEnd()}; true\n`), /swallow a failure/],
  ["a second config on the tree scan", d => set(d, t => t.replace(/gitleaks dir "\$\{common\[@\]\}" \.\s*$/, 'gitleaks dir "${common[@]}" --config /dev/null .\n')), /--config must appear once|reviewed forms/],
  ["a second config on the range scan", d => set(d, t => t.replace('--log-opts="-m --text ${range}"', '--config /dev/null --log-opts="-m --text ${range}"')), /--config must appear once|reviewed forms/],
  ["a second config in common", d => set(d, t => t.replace("--config .gitleaks.toml ", "--config .gitleaks.toml --config /dev/null ")), /common must be exactly|--config must appear once/],
  ["another config in common", d => set(d, t => t.replace("--config .gitleaks.toml", "--config /dev/null")), /common must be exactly/],
  ["the tree scan dropped", d => set(d, t => t.replace(/\n\s*gitleaks dir "\$\{common\[@\]\}" \.\s*$/, "\n")), /reviewed forms|last command/],
  ["the tree scan of a subdirectory", d => set(d, t => t.replace(/gitleaks dir "\$\{common\[@\]\}" \.\s*$/, 'gitleaks dir "${common[@]}" apps\n')), /reviewed forms|last command/],
  ["the tree scan before something else", d => set(d, t => `${t.trimEnd()}\n          echo done\n`), /last command/],
  ["--first-parent on the range", d => set(d, t => t.replace("-m --text ${range}", "--first-parent -m --text ${range}")), /reviewed forms|narrows/],
  ["no -m on the range", d => set(d, t => t.replace('--log-opts="-m --text ${range}"', '--log-opts="--text ${range}"')), /reviewed forms/],
  ["no --text on the range", d => set(d, t => t.replace('--log-opts="-m --text ${range}"', '--log-opts="-m ${range}"')), /reviewed forms/],
  ["no --text in the git canary", d => set(d, t => t.replace('--log-opts="-m --text HEAD"', '--log-opts="-m HEAD"')), /reviewed forms/],
  ["the git canary reading a different range", d => set(d, t => t.replace('--log-opts="-m --text HEAD"', '--log-opts="-m --text HEAD~0 --max-count=0"')), /reviewed forms/],
  ["the range scan of the last commit only", d => set(d, t => t.replace('--log-opts="-m --text ${range}"', '--log-opts="-m --text -1 ${range}"')), /reviewed forms/],
  ["a fifth gitleaks call", d => set(d, t => t.replace("commits=", 'gitleaks detect --no-git .\n          commits=')), /reviewed forms|other gitleaks/],
  ["BASE_SHA reassigned", d => set(d, t => t.replace("case ", 'BASE_SHA="${HEAD_SHA}"\n          case ')), /reassigned/],
  ["HEAD_SHA reassigned", d => set(d, t => t.replace("case ", 'HEAD_SHA="${BASE_SHA}"\n          case ')), /reassigned/],
  ["BEFORE_SHA reassigned", d => set(d, t => t.replace("case ", 'BEFORE_SHA="${zeros}"\n          case ')), /reassigned/],
  ["AFTER_SHA reassigned", d => set(d, t => t.replace("case ", 'AFTER_SHA="${BEFORE_SHA}"\n          case ')), /reassigned/],
  ["EVENT_NAME reassigned", d => set(d, t => t.replace("case ", 'EVENT_NAME=push\n          case ')), /reassigned/],
  ["an exit-code override", d => set(d, t => t.replace("--redact ", "--redact --exit-code 0 ")), /common must be exactly|narrows/],
  ["a baseline", d => set(d, t => t.replace("--redact ", "--redact --baseline-path /tmp/b.json ")), /common must be exactly|narrows/],
  ["partial redaction", d => set(d, t => t.replace("--redact ", "--redact=20 ")), /common must be exactly|narrows/],
  ["no redaction", d => set(d, t => t.replace("--redact ", "")), /common must be exactly/],
  ["inline allow comments honoured", d => set(d, t => t.replace("--ignore-gitleaks-allow ", "")), /common must be exactly/],
  ["verbose findings", d => set(d, t => t.replace("--log-level info ", "--log-level info --verbose ")), /common must be exactly|narrows/],
  ["the message of what is scanned removed", d => set(d, t => t.replace('echo "scanning ${commits} commits of ${range}"', "true")), /say what it scans/],
];
for (const [label, mutate, reason] of MUTATIONS) {
  test(`the secret-scan job check refuses: ${label}`, () => {
    const data = copy();
    mutate(data);
    assert.match(jobViolations(data).join("\n"), reason);
  });
}

// ---- the config and its allowlist ----

const EXACT_PATH = /^\^[A-Za-z0-9_\-/]+(?:\\\.[A-Za-z0-9_-]+)+\$$/; // ^dir/file\.ext$ : one file, no wildcard

// The longest run of characters that a regex matches literally: an escaped character counts,
// a wildcard `.`, a class, a group, a quantified character and an anchor do not.
function longestLiteralRun(regex) {
  let run = 0;
  let best = 0;
  const end = () => { best = Math.max(best, run); run = 0; };
  for (let i = 0; i < regex.length;) {
    const c = regex[i];
    if (c === "\\") {
      if (/[dDwWsSbB]/.test(regex[i + 1] ?? "")) end(); else run++;
      i += 2;
    } else if (c === "[") {
      let j = i + 1;
      if (regex[j] === "^") j++;
      if (regex[j] === "]") j++;
      while (j < regex.length && regex[j] !== "]") j += regex[j] === "\\" ? 2 : 1;
      end();
      i = j + 1;
    } else if (c === "{") {
      run = Math.max(0, run - 1);
      end();
      i = regex.indexOf("}", i) + 1 || regex.length;
    } else if (c === "*" || c === "+" || c === "?") {
      run = Math.max(0, run - 1);
      end();
      i++;
    } else if (c === "(" && regex.startsWith("(?", i)) {
      end();
      i += regex[i + 2] === ":" ? 3 : 2;
    } else if (c === "(" || c === ")" || c === "|" || c === "." || c === "^" || c === "$") {
      end();
      i++;
    } else {
      run++;
      i++;
    }
  }
  end();
  return best;
}

function allowlistViolations(config) {
  const out = [];
  const lists = [
    ...(config.allowlists ?? []).map((a, i) => ({ where: `allowlists[${i}]`, a })),
    ...(config.rules ?? []).flatMap(r => (r.allowlists ?? []).map((a, i) => ({ where: `rule ${r.id} allowlists[${i}]`, a }))),
  ];
  if (config.allowlist) out.push("the legacy [allowlist] table is not used; use [[allowlists]]");
  for (const { where, a } of lists) {
    const known = new Set(["description", "condition", "paths", "regexes", "targetRules", "regexTarget"]);
    for (const key of Object.keys(a)) if (!known.has(key)) out.push(`${where}: '${key}' is not allowed (stopwords, commits and other switches are refused)`);
    if (typeof a.description !== "string" || a.description.trim().length < 20) out.push(`${where}: needs a description that gives the reason`);
    const paths = a.paths ?? [];
    const regexes = a.regexes ?? [];
    if (paths.length === 0) out.push(`${where}: needs the exact file it applies to (paths)`);
    if (regexes.length === 0) out.push(`${where}: a path alone would allow the whole file; add the exact value (regexes)`);
    if (a.condition !== "AND") out.push(`${where}: condition must be "AND" (the default OR makes every criterion allow on its own)`);
    if (a.regexTarget !== undefined && !["match", "secret"].includes(a.regexTarget)) out.push(`${where}: regexTarget must be match or secret, not ${a.regexTarget}`);
    for (const p of paths) if (!EXACT_PATH.test(p)) out.push(`${where}: path ${p} must be one anchored file (^dir/file\\.ext$), not a directory or a pattern`);
    for (const r of regexes) {
      if (!r.startsWith("^") && !r.includes("(?m)") || !r.endsWith("$") || r.endsWith("\\$")) out.push(`${where}: regex ${JSON.stringify(r.slice(0, 40))} must be anchored ^...$ to the whole matched text`);
      if (longestLiteralRun(r) < 8) out.push(`${where}: regex ${JSON.stringify(r.slice(0, 40))} is too broad (it needs a literal run of 8 or more characters; an unescaped . is a wildcard)`);
    }
    if ((a.targetRules ?? []).length !== 1) out.push(`${where}: name the one rule it silences (targetRules)`);
  }
  return out;
}

const LOCAL_RULES = ["omg-gateway-key", "omg-session-cookie", "omg-file-encryption-key", "omg-url-credentials"];
const config = () => readConfig(readLocalFromRepo(CONFIG_FILE));

test("the gitleaks config extends the default rules unchanged and has a narrow allowlist (#596)", () => {
  const c = config();
  assert.deepEqual(c.extend, { useDefault: true }, "[extend] has useDefault = true and nothing else (no path, url, disabledRules)");
  assert.equal(c.disabledRules, undefined);
  assert.deepEqual(Object.keys(c).sort(), ["allowlists", "extend", "rules", "title"], "no other top-level table (no [allowlist], no disabled rules)");
  assert.deepEqual(allowlistViolations(c), []);
  assert.deepEqual((c.rules ?? []).map(r => r.id).sort(), [...LOCAL_RULES].sort(), "the only local rules are this repository's own, so none shadows a default rule id");
  for (const rule of c.rules) {
    assert.ok(rule.id.startsWith("omg-"), rule.id);
    assert.ok(rule.keywords?.length > 0, `${rule.id} has keywords`);
    assert.equal(rule.allowlists, undefined, `${rule.id}: allowlists belong in the reviewed list at the end of the file`);
    assert.equal(rule.path, undefined);
  }
  assert.equal(existsSync(join(root, ".gitleaksignore")), false, "no .gitleaksignore (a fingerprint list would silence findings outside the config)");
  const tracked = spawnSync("git", ["ls-files"], { cwd: root, encoding: "utf8" }).stdout.split("\n");
  assert.deepEqual(tracked.filter(f => /(^|\/)\.gitleaksignore$/.test(f)), []);
  assert.deepEqual(tracked.filter(f => /(^|\/)\.gitleaks\.toml$/.test(f) && f !== CONFIG_FILE), [], "one config only");
});

test("every allowlist entry in the real config has a unique reason", () => {
  const reasons = config().allowlists.map(a => a.description);
  assert.equal(new Set(reasons).size, reasons.length);
});

const FIXTURE_LIST = { description: "Test-only value in one named file; see the test module.", condition: "AND", paths: ["^apps/gateway/src/example\\.rs$"], regexes: ["^exact-test-value-1234$"], targetRules: ["generic-api-key"] };
const LIST_MUTATIONS = [
  ["a whole directory", { paths: ["^tests/.*$"] }, /one anchored file/],
  ["a directory prefix", { paths: ["^tests/"] }, /one anchored file/],
  ["every rust file", { paths: ["^.*\\.rs$"] }, /one anchored file/],
  ["a glob", { paths: ["tests/**"] }, /one anchored file/],
  ["an unanchored path", { paths: ["example\\.rs"] }, /one anchored file/],
  ["no path", { paths: [] }, /exact file/],
  ["no value", { regexes: [] }, /whole file/],
  ["a match-all regex", { regexes: ["^.*$"] }, /too broad/],
  ["only wildcards", { regexes: ["^........$"] }, /too broad/],
  ["an unescaped dot standing for a character", { regexes: ["^a.b.c.d.e$"] }, /too broad/],
  ["a short regex", { regexes: ["^secret$"] }, /too broad/],
  ["a class-only regex", { regexes: ["^[A-Za-z0-9]+$"] }, /too broad/],
  ["a quantified regex", { regexes: ["^abcdefgh+$"] }, /too broad/],
  ["a substring regex (no anchors)", { regexes: ["exact-test-value-1234"] }, /anchored/],
  ["a prefix regex", { regexes: ["^exact-test-value-1234"] }, /anchored/],
  ["a suffix regex", { regexes: ["exact-test-value-1234$"] }, /anchored/],
  ["OR semantics", { condition: "OR" }, /condition must be "AND"/],
  ["default semantics", { condition: undefined }, /condition must be "AND"/],
  ["no reason", { description: "" }, /description/],
  ["a thin reason", { description: "test" }, /description/],
  ["stopwords", { stopwords: ["example"] }, /stopwords/],
  ["a commit scope", { commits: ["a".repeat(40)] }, /commits/],
  ["no rule named", { targetRules: [] }, /name the one rule/],
  ["two rules named", { targetRules: ["generic-api-key", "private-key"] }, /name the one rule/],
  ["a line regexTarget", { regexTarget: "line" }, /regexTarget/],
];
test("a fixture allowlist entry passes the narrowness check", () => {
  assert.deepEqual(allowlistViolations({ allowlists: [FIXTURE_LIST] }), []);
  assert.deepEqual(allowlistViolations({ allowlists: [{ ...FIXTURE_LIST, regexTarget: "match", regexes: ['^tokens\\.css": "[0-9a-f]{64}"$'] }] }), []);
  assert.deepEqual(allowlistViolations({ allowlists: [{ ...FIXTURE_LIST, regexes: ["^(?:first-fixture-marker|second-fixture-marker)$"] }] }), []);
});
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

// ---- the project's own rules ----

const rule = id => config().rules.find(r => r.id === id);
const jsRegExp = r => new RegExp(r.regex.replace(/^\(\?i\)/, ""), r.regex.startsWith("(?i)") ? "i" : "");

test("omg-gateway-key matches inference keys and admin tokens, also fused to other text, and nothing looser", () => {
  const re = jsRegExp(rule("omg-gateway-key"));
  const uuid = "a1".repeat(16);
  const secret = "c3".repeat(32);
  for (const token of [`omg_${uuid}.${secret}`, `omg_admin_${uuid}.${secret}`]) {
    for (const text of [token, `Authorization: Bearer ${token}`, `"${token}"`, `pre_${token}`, `x${token}`, `${token}_suffix`]) assert.ok(re.test(text), text.slice(0, 20));
  }
  for (const text of [`omg_${uuid}.${"c3".repeat(31)}`, `omg_${"a1".repeat(15)}.${secret}`, `omg_${uuid}:${secret}`, "omg_bad.secret", `omg_admin_${"0".repeat(32)}.${"z".repeat(64)}`, "omg_admin_"]) assert.equal(re.test(text), false, text.slice(0, 16));
  assert.ok(rule("omg-gateway-key").keywords.includes("omg_"));
  assert.ok(re.test(fakes.gateway()));
});

test("omg-session-cookie matches a session or CSRF cookie value of 64 hex characters", () => {
  const re = jsRegExp(rule("omg-session-cookie"));
  const hex = "9f".repeat(32);
  for (const text of [`omg_session=${hex}`, `Cookie: a=b; omg_csrf=${hex}`, `Set-Cookie: omg_session=${hex}; Path=/; HttpOnly`, `pre_omg_session=${hex}`]) assert.ok(re.test(text), text.slice(0, 20));
  for (const text of [`omg_session=${"9f".repeat(31)}`, "omg_session=test-session", "omg_csrf=test-csrf", `omg_session: ${hex}`, `omg_other=${hex}`, "omg_session={session}"]) assert.equal(re.test(text), false, text);
  assert.ok(re.test(fakes.session()));
  assert.deepEqual(rule("omg-session-cookie").keywords.sort(), ["omg_csrf", "omg_session"]);
});

test("omg-file-encryption-key matches a key id and 32 base64 bytes held in a FILE...KEY variable", () => {
  const re = jsRegExp(rule("omg-file-encryption-key"));
  const b64 = "Qr7Xm2Zk9Lp4Wn8Tv3Bc6Yd1Fg5Hj0Sa2Ue4Io7Pq9A";
  assert.equal(b64.length, 43);
  for (const text of [`GATEWAY_FILE_KEYS=k2026a:${b64}=`, `export GATEWAY_FILE_KEYS="k2026a:${b64}="`, `OMG_TEST_FILE_KEYS: k1.b-2_c:${b64}=`, `file_keys = 'id:${b64}='`, `GATEWAY_FILE_KEYS=k1:${b64}=,k2:${b64}=`]) assert.ok(re.test(text), text.slice(0, 30));
  for (const text of [`GATEWAY_FILE_KEYS=k2026a:${b64.slice(1)}=`, "GATEWAY_FILE_KEYS=k2026a:<base64 32 bytes>", "GATEWAY_FILE_ENCRYPTION_KEYS_ENV=GATEWAY_FILE_KEYS", `OTHER_SETTING=k2026a:${b64}=`, `GATEWAY_FILE_KEYS=${b64}=`, `GATEWAY_FILE_KEYS=k2026a:${b64}`]) assert.equal(re.test(text), false, text.slice(0, 30));
  assert.ok(re.test(fakes.fileKey()));
  assert.equal(rule("omg-file-encryption-key").secretGroup, 1);
});

test("omg-url-credentials matches a long password in a URL and leaves placeholders alone", () => {
  const re = jsRegExp(rule("omg-url-credentials"));
  const url = (scheme, user, password, host) => `${scheme}://${user}:${password}@${host}`; // pieces: no credential-shaped literal in this file
  for (const text of [fakes.url(), url("https", "operator", "Zq8Lm2Xv5Rt9Kp", "api.vendor.example/v1"), `url = "${url("amqps", "svc", "Wt6Yb1Nc5Hd8Jf3Gs", "broker.internal:5671")}"`]) assert.ok(re.test(text), text.slice(0, 24));
  for (const text of [
    "postgres://postgres:postgres@db:5432/gateway", "postgres://gateway:gateway@127.0.0.1:5432/gateway", "postgres://user:pass@host/db", "postgres://u:password@localhost/db",
    "postgres://u:${DB_PASSWORD_VALUE}@h/db", "postgres://u:$DB_PASSWORD_VALUE@h/db", "postgres://u:<your-password-here>@h/db", "postgres://u:*************@h/db", "https://example.com/path:with@at",
    "postgres://u:{{ .Values.password }}@h/db", "postgres://u:%(password)s_long_name@h/db".replace("%", "$"),
  ]) assert.equal(re.test(text), false, text);
  assert.equal(rule("omg-url-credentials").entropy, 3);
  assert.equal(rule("omg-url-credentials").secretGroup, 1);
});

// ---- ignored secret locations ----

// Where this repository's tools write secrets or where people keep them. A file here must stay
// out of git and out of the image build context. Inside the trees the image does build from
// (apps/gateway/src, apps/web/public, tools/*/src) the context is deny-by-default turned
// allow-by-directory, so these paths are the ones the explicit exclusions have to catch.
const SECRET_PLACES = [
  ".env", ".env.local", ".env.production", "apps/web/.env.local", "apps/web/public/.env", "apps/web/public/.env.local", "apps/gateway/src/.env",
  "ai.env", "boat.env", "deploy/anything.env", "apps/gateway/src/anything.env", "apps/web/public/anything.env", "tools/loadgen/src/anything.env",
  "anything.env.local", "apps/web/public/anything.env.local", "apps/gateway/src/anything.env.production",
  ".envrc", "apps/gateway/src/.envrc", ".netrc", "apps/web/public/.netrc", ".pgpass", "apps/web/public/.pgpass", ".git-credentials", "tools/loadgen/src/.git-credentials",
  ".local/staging/staging.env", ".local/staging/local-ca.key", ".local/loadtest/stack.env", ".local/loadtest/userlist.txt", ".local/enterprise-rebuild/demo-runtime.env",
  "apps/web/.local/state.json", "tools/loadgen/src/.local/state",
];
// Excluded from the image build context only (git does not ignore them): key material and the like.
const DOCKER_ONLY_PLACES = [
  "apps/web/public/server.pem", "apps/gateway/src/server.key", "tools/loadgen/src/store.p12", "apps/web/public/store.pfx",
  "apps/gateway/src/secrets/token", "apps/web/public/secrets/token", "apps/web/public/backups/dump.sql", "apps/gateway/src/state.bak",
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
  for (const p of [...SECRET_PLACES, ...DOCKER_ONLY_PLACES]) if (inBuildContext(dockerignore, p)) out.push(`${p} would enter the image build context`);
  for (const p of ["Dockerfile", "apps/gateway/src/main.rs", "apps/web/src/main.tsx", "Cargo.toml"]) if (!inBuildContext(dockerignore, p)) out.push(`${p} is a build input but .dockerignore excludes it (the matcher or the file is wrong)`);
  return out;
}

const realIgnores = () => [readLocalFromRepo(".gitignore"), readLocalFromRepo(".dockerignore")];
const drop = (text, ...patterns) => text.split("\n").filter(l => !patterns.includes(l.trim())).join("\n");

test("generated-secret locations are ignored by git and absent from the image build context (#596)", () => {
  assert.deepEqual(secretPlaceViolations(...realIgnores()), []);
});

test("the tracked example files are tracked and no tracked file is matched by an ignore rule", () => {
  const tracked = new Set(spawnSync("git", ["ls-files"], { cwd: root, encoding: "utf8" }).stdout.split("\n"));
  for (const p of TRACKED_EXAMPLES) assert.ok(tracked.has(p), p);
  assert.deepEqual([...tracked].filter(f => /\.env$/.test(f)), [], "no tracked file ends in .env");
  const ignoredButTracked = spawnSync("git", ["-c", "core.excludesFile=/dev/null", "ls-files", "-ci", "--exclude-standard"], { cwd: root, encoding: "utf8" });
  assert.equal(ignoredButTracked.status, 0, ignoredButTracked.stderr);
  assert.equal(ignoredButTracked.stdout.trim(), "", "a tracked file is also ignored");
});

test("the secret location list matches what the tools write", () => {
  for (const [file, needle] of WRITERS) assert.ok(readLocalFromRepo(file).includes(needle), `${file} no longer mentions ${needle}`);
});

// Each group of equivalent lines: dropping all of them must un-ignore the place.
const GIT_GROUPS = [
  [["*.env"], "boat.env"], [["*.env"], "apps/web/public/anything.env"], [[".env", "*.env"], ".env"], [["ai.env", "*.env"], "ai.env"],
  [[".env.*", "*.env.*"], ".env.local"], [["*.env.*"], "anything.env.local"], [[".env.*", "*.env.*"], "apps/web/.env.local"],
  [[".envrc"], ".envrc"], [[".netrc"], ".netrc"], [[".pgpass"], "apps/web/public/.pgpass"], [[".git-credentials"], ".git-credentials"],
  [[".local/"], ".local/loadtest/userlist.txt"], [[".local/"], "apps/web/.local/state.json"],
];
for (const [patterns, place] of GIT_GROUPS) {
  test(`dropping ${patterns.join(" and ")} from .gitignore is noticed (${place})`, () => {
    const [gitignore, dockerignore] = realIgnores();
    assert.ok(patterns.every(p => gitignore.split("\n").some(l => l.trim() === p)), "the pattern is in .gitignore");
    assert.match(secretPlaceViolations(drop(gitignore, ...patterns), dockerignore).join("\n"), new RegExp(`${place.replaceAll(".", "\\.")} is not ignored by git`));
  });
}
test("re-including a secret file in .gitignore, or ignoring a tracked example, is noticed", () => {
  const [gitignore, dockerignore] = realIgnores();
  const found = text => secretPlaceViolations(text, dockerignore).join("\n");
  assert.match(found(`${gitignore}\n!ai.env\n`), /ai\.env is not ignored by git/);
  assert.match(found(`${gitignore}\n!*.env\n`), /boat\.env is not ignored by git/);
  assert.match(found(`${gitignore}\n!.envrc\n`), /\.envrc is not ignored by git/);
  assert.match(found(`${gitignore}\n.env.example\n`), /\.env\.example is a tracked example/);
  assert.match(found(`${gitignore}\n*.example\n`), /is a tracked example/);
});

const DOCKER_GROUPS = [
  [["**/*.env"], "apps/web/public/anything.env"], [["**/.env", "**/*.env"], "apps/gateway/src/.env"], [["**/.env.*", "**/*.env.*"], "apps/web/public/.env.local"],
  [["**/*.env.*"], "apps/gateway/src/anything.env.production"], [["**/.envrc"], "apps/gateway/src/.envrc"], [["**/.netrc"], "apps/web/public/.netrc"],
  [["**/.pgpass"], "apps/web/public/.pgpass"], [["**/.git-credentials"], "tools/loadgen/src/.git-credentials"],
  [["**/.local", "**/.local/**"], "tools/loadgen/src/.local/state"],
  [["**/*.pem"], "apps/web/public/server.pem"], [["**/*.key"], "apps/gateway/src/server.key"], [["**/*.p12"], "tools/loadgen/src/store.p12"], [["**/*.pfx"], "apps/web/public/store.pfx"],
  [["**/secrets", "**/secrets/**"], "apps/gateway/src/secrets/token"], [["**/backups", "**/backups/**"], "apps/web/public/backups/dump.sql"], [["**/*.bak"], "apps/gateway/src/state.bak"],
];
for (const [patterns, place] of DOCKER_GROUPS) {
  test(`dropping ${patterns.join(" and ")} from .dockerignore is noticed (${place})`, () => {
    const [gitignore, dockerignore] = realIgnores();
    assert.ok(patterns.every(p => dockerignore.split("\n").some(l => l.trim() === p)), "the pattern is in .dockerignore");
    assert.match(secretPlaceViolations(gitignore, drop(dockerignore, ...patterns)).join("\n"), new RegExp(`${place.replaceAll(".", "\\.")} would enter the image build context`));
  });
}
test("re-including a secret path in .dockerignore, or excluding the whole context, is noticed", () => {
  const [gitignore, dockerignore] = realIgnores();
  const found = text => secretPlaceViolations(gitignore, text).join("\n");
  for (const path of ["apps/gateway/src/anything.env", "apps/web/public/server.pem", "apps/gateway/src/secrets/token", "apps/web/public/.netrc"]) {
    assert.match(found(`${dockerignore}\n!${path}\n`), new RegExp(`${path.replaceAll(".", "\\.")} would enter`), path);
  }
  assert.match(found("**\n"), /build input but \.dockerignore excludes it/);
});

test("the docs state what the ignore files do not protect", () => {
  const docs = readLocalFromRepo("docs/verification.md");
  assert.match(docs, /apps\/gateway\/src/);
  assert.match(docs, /re-include/i);
});

test("the made-up credential generators produce the shapes the rules look for", () => {
  assert.match(fakes.aws(), /^AKIA[A-Z2-7]{16}$/);
  assert.match(fakes.gateway(), /^omg_[0-9a-f]{32}\.[0-9a-f]{64}$/);
  assert.match(fakes.session(), /^omg_session=[0-9a-f]{64}$/);
  assert.match(fakes.fileKey(), /^GATEWAY_FILE_KEYS=k\d{4}:[A-Za-z0-9]{43}=$/);
});
