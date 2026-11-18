// Check credential inputs before any step can relay them through a wrapper, action or GITHUB_ENV.
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { posix } from "node:path";
import { test } from "node:test";
import { git } from "./secret-scan-lib.mjs";
import { cargoScanner, indexedReader, wrapperPath } from "./cargo-wrappers.mjs";
import { shellCommands } from "./policy.mjs";
import { parseStrict, repoFiles, root, sourceFiles, toData } from "./lib.mjs";

// GitHub secrets have arbitrary names. Check the source of a value, including
// whole-context and dynamic lookups, instead of guessing provider variable names.
function readsSecret(value, condition) {
  const identifier = /[\w-]+/y;
  let inExpression = condition;
  let quoted = false;
  let afterDot = false;
  let i = 0;
  while (i < value.length) {
    if (!inExpression) {
      const open = value.indexOf("${{", i);
      if (open === -1) break;
      i = open + 3;
      inExpression = true;
      afterDot = false;
      continue;
    }
    const ch = value[i];
    if (ch === "'") {
      if (quoted && value[i + 1] === "'") {
        i += 2;
        continue;
      }
      quoted = !quoted;
      afterDot = false;
    } else if (!quoted) {
      if (!condition && value.startsWith("}}", i)) {
        i += 2;
        inExpression = false;
        continue;
      }
      identifier.lastIndex = i;
      const word = identifier.exec(value);
      if (word) {
        if (!afterDot && word[0].toLowerCase() === "secrets") return true;
        afterDot = false;
        i = identifier.lastIndex;
        continue;
      }
      if (!/\s/.test(ch)) afterDot = ch === ".";
    }
    i++;
  }
  assert.ok(condition || !inExpression, "unterminated expression in workflow input");
  return false;
}

const repoScript = indexedReader(root, sourceFiles());
const documents = new Map(repoFiles().map(({ file, text }) => [file, toData(parseStrict(text, file))]));
const liveNames = new Set(["OPENAI_KEY", "OPEN_ROUTER"]);
const fixtureValues = new Set(["", "fixture", "test-only"]);

function checkInputs(data, file, docs = documents, read = repoScript) {
  function local(ref, ancestors) {
    const path = ref.slice(2);
    const file = [path, path + "/action.yml", path + "/action.yaml"].find(key => docs.has(key));
    assert.ok(file, `unresolved local workflow/action ${ref}`);
    assert.ok(!ancestors.has(file), `recursive workflow/action ${file}`);
    return { node: docs.get(file), file, ancestors: new Set([...ancestors, file]) };
  }

  function runner(block, file, ancestors, action, defaultDir = ".") {
    const steps = block.steps ?? [];
    const conditions = new Set([block, ...steps]);
    const checks = [() => inspect(action ?? block, file, conditions, action?.runs)];
    const shellSources = new Map();
    const scan = cargoScanner(read, shellSources);
    let tests = false;
    for (const step of steps) {
      const cwd = step["working-directory"] ?? defaultDir;
      const source = action && step.run
        ? step.run.replace(/\$\{\{\s*github\.action_path\s*}}|\$\{GITHUB_ACTION_PATH}|\$GITHUB_ACTION_PATH\b/gi, posix.relative(cwd, posix.dirname(file)) || ".")
        : step.run;
      if (scan(source, "shell", cwd)) tests = true;
    }
    if (action) {
      for (const key of ["pre", "main", "post"]) {
        if (block[key] && scan(read(wrapperPath(posix.dirname(file), block[key])), "js", ".")) tests = true;
      }
    }
    for (const step of steps) {
      if (!step.uses?.startsWith("./")) continue;
      const child = local(step.uses, ancestors);
      const nested = runner(child.node.runs, child.file, child.ancestors, child.node);
      tests ||= nested.tests;
      checks.push(...nested.checks);
    }
    for (const [path, script] of shellSources) checks.push(() => checkAssignments(script, path));
    return { tests, checks };
  }

  function workflow(node, file, ancestors) {
    let tests = false;
    for (const [id, job] of Object.entries(node.jobs)) {
      const path = `${file}.jobs.${id}`;
      if (job.uses?.startsWith("./")) {
        const called = local(job.uses, ancestors);
        if (!workflow(called.node, called.file, called.ancestors)) continue;
        assert.notEqual(job.secrets, "inherit", `${path}: inheriting secrets in a test job`);
        inspect(job, path, new Set([job]));
      } else {
        const defaultDir = job.defaults?.run?.["working-directory"] ?? node.defaults?.run?.["working-directory"];
        const result = runner(job, path, ancestors, undefined, defaultDir);
        if (!result.tests) continue;
        // Actions in earlier steps can prepare the environment for later steps.
        result.checks.forEach(check => check());
        inspect({ env: node.env }, file, new Set());
      }
      tests = true;
    }
    return tests;
  }
  const ancestors = new Set([file]);
  if (data.jobs) workflow(data, file, ancestors);
  else {
    const result = runner(data.runs, file, ancestors, data);
    if (result.tests) result.checks.forEach(check => check());
  }
}

function checkAssignments(script, path) {
  for (const { text, variable } of shellCommands(script).flat()) {
    const eq = text.indexOf("=");
    if (eq >= 0 && liveNames.has(text.slice(0, eq))) {
      assert.ok(!variable && fixtureValues.has(text.slice(eq + 1)), `${path}: live credential assignment`);
    }
  }
}

function inspect(parent, path, conditions, runs) {
  if (!parent || typeof parent !== "object") return;
  for (const [key, value] of Object.entries(parent)) {
    const child = `${path}.${key}`;
    if (liveNames.has(key.toUpperCase())) {
      assert.ok(fixtureValues.has(value), `${child}: live credential destination`);
    }
    if (typeof value !== "string") {
      inspect(value, child, conditions, runs);
      continue;
    }
    if (key === "run") checkAssignments(value, child);
    if (readsSecret(value, (key === "if" && conditions.has(parent))
        || (parent === runs && ["pre-if", "post-if"].includes(key)))) {
      assert.ok(["GH_TOKEN", "GITHUB_TOKEN"].includes(key) && /^\$\{\{\s*secrets\.GITHUB_TOKEN\s*}}$/i.test(value),
        `${child}: only the automatic GitHub token may be imported from secrets`);
    }
  }
}

test("Cargo jobs and their local calls cannot import provider secrets", () => {
  for (const [file, data] of documents) if (data.jobs) checkInputs(data, file);
});

test("provider secret references and inheritance are refused regardless of the credential alias", () => {
  const ref = "${{ secrets.SYNTHETIC_PROVIDER }}";
  const job = { steps: [{ run: "cargo test" }] };
  for (const data of [
    { env: { OPEN_ROUTER: ref }, jobs: { tests: job } },
    { jobs: { tests: { ...job, env: { OPENAI_KEY: "${{ SeCrets['synthetic'] }}" } } } },
    { jobs: { tests: { steps: [{ env: { ANY: "${{ toJSON(secrets) }}" }, run: "cargo test" }] } } },
    { jobs: { tests: { steps: [{ run: `echo "ALIAS=${ref}" >> "$GITHUB_ENV"` }, ...job.steps] } } },
    { jobs: { tests: { uses: "./.github/workflows/tests.yml", secrets: "inherit" } } },
    { jobs: { tests: { uses: "./.github/workflows/tests.yml", secrets: { alias: ref } } } },
    { jobs: { tests: { steps: [{ uses: "./.github/actions/test", with: { token: ref } }] } } },
    { jobs: { tests: { steps: [{ run: "KEY=${{ format('}}', secrets.ALIAS) }} cargo test" }] } } },
    { jobs: { tests: { ...job, env: { GH_TOKEN: "${{ secrets.GITHUB_TOKEN_EXTRA }}" } } } },
    { jobs: { tests: { if: "secrets[vars.NAME]", ...job } } },
    { runs: { using: "node24", main: "main.js", post: "post.js", "post-if": "secrets.ALIAS" } },
    { runs: { using: "composite", steps: [{ run: `KEY=${ref} cargo test` }] } },
  ]) assert.throws(() => checkInputs(data, "fixture.yml", new Map([
    [".github/workflows/tests.yml", { jobs: { leg: job } }],
    [".github/actions/test/action.yml", { runs: job }],
  ]), path => path === "main.js" ? 'spawn("cargo", ["test"])' : undefined), /fixture\.yml.*(?:secret|live credential)/i, JSON.stringify(data));

  checkInputs({ jobs: { prose: { steps: [{ run: "echo 'scan for secrets at ${{ github.sha }}'" }] },
    tests: job, publish: { env: { GH_TOKEN: "${{ secrets.GITHUB_TOKEN }}" } } } }, "ok.yml");
});

test("unrelated properties, environment literals and token whitespace are accepted", () => {
  checkInputs({ jobs: { tests: { steps: [{
    env: { if: "secrets are documented here", secrets: "inherit", ARG: "${{ inputs.secrets }}",
      GH_TOKEN: "${{secrets.GITHUB_TOKEN}}" },
    if: "inputs.secrets && inputs['secrets']",
    run: "cargo test",
  }] } } }, "ok.yml");
});

test("expression boundaries respect Actions strings across interpolations", () => {
  const first = "${{ format('it''s }} and secrets', inputs.name) }}";
  assert.equal(readsSecret(first + " secrets outside ${{ inputs.secrets }}", false), false);
  assert.equal(readsSecret(first + " ${{ secrets.SYNTHETIC }}", false), true);
  assert.throws(() => readsSecret("label: ${{ inputs.name", false), /unterminated expression/);
});

test("notification credentials stay in their job alongside Cargo tests", () => {
  checkInputs({ jobs: {
    tests: { steps: [{ run: "cargo test" }] },
    notify: { env: { HOOK: "${{ secrets.NOTIFICATION_HOOK }}" }, steps: [{ run: "echo cargo test completed" }] },
  } }, "fixture.yml");
});

test("live destinations refuse variable and input values inherited by test wrappers", () => {
  for (const name of ["OPEN_ROUTER", "OPENAI_KEY"]) {
    for (const value of ["${{ inputs.fixture }}", "${{ vars.FIXTURE }}"]) {
      for (const scope of ["workflow", "job", "step"]) {
        const step = { run: "node .github/scripts/rust-shards.mjs --shard 0 --count 4" };
        const job = { steps: [step] };
        const workflow = { jobs: { leg: job } };
        ({ workflow, job, step })[scope].env = { [name]: value };
        assert.throws(() => checkInputs(workflow, "fixture.yml"), /live credential/);
      }
    }
  }
});

test("a called workflow and nested actions keep test credentials out at each boundary", () => {
  const workflow = ".github/workflows/suite.yml";
  const outer = ".github/actions/outer";
  const inner = ".github/actions/inner";
  const wrapper = "scripts/fixture-check";
  const variable = "${{ vars.FIXTURE }}";
  const hook = "${{ secrets.NOTIFICATION_HOOK }}";
  const call = { uses: "./" + workflow };
  const action = { uses: "./" + outer };
  const run = { run: "./" + wrapper, env: { OPEN_ROUTER: "fixture", AUDIO_LIVE: "${{ inputs.enabled }}" } };
  const suite = { jobs: { leg: { steps: [action] }, notify: { steps: [{ env: { HOOK: hook }, run: "notify" }] } } };
  const docs = new Map([
    [workflow, suite],
    [outer + "/action.yml", { runs: { using: "composite", steps: [{ uses: "./" + inner }] } }],
    [inner + "/action.yaml", { runs: { using: "composite", steps: [run] } }],
  ]);
  const source = new Map([[wrapper, "bash scripts/suite.sh"], ["scripts/suite.sh", "cargo test"]]);
  const check = () => checkInputs({ jobs: { call } }, "caller.yml", docs, path => source.get(path));
  check();
  for (const [target, key, input] of [
    [call, "with", { OPENAI_KEY: variable }],
    [call, "secrets", "inherit"],
    [suite, "env", { OUTBOUND: hook }],
    [suite.jobs.leg, "env", { OPEN_ROUTER: variable }],
    [action, "with", { label: hook }],
    [run, "env", { OPENAI_KEY: "${{ inputs.value }}" }],
    [run, "run", 'echo "OPEN_ROUTER=${{ inputs.value }}" >> "$GITHUB_ENV"; ./' + wrapper],
  ]) {
    const before = target[key];
    target[key] = input;
    assert.throws(check, /(?:secret|live credential)/);
    if (before === undefined) delete target[key];
    else target[key] = before;
  }
});

test("action-relative shell wrappers check their own live destinations", () => {
  const action = ".github/actions/local/action.yml";
  const scriptPath = ".github/actions/local/check.sh";
  const data = { jobs: { leg: { steps: [{ uses: "./.github/actions/local" }] } } };
  const docs = new Map([[action, { runs: { using: "composite", steps: [
    { shell: "bash", run: 'bash "$GITHUB_ACTION_PATH/check.sh"' },
  ] } }]]);
  const check = script => checkInputs(data, "caller.yml", docs, path => path === scriptPath ? script : undefined);
  check("OPENAI_KEY=test-only cargo test");
  assert.throws(() => check('OPENAI_KEY="$FIXTURE" cargo test'), /check.sh: live credential/);
});

test("only the root npm wrapper leading to Cargo restricts its job inputs", () => {
  const manifest = JSON.stringify({ scripts: { verify: "node scripts/check.mjs", notify: "notify" } });
  const source = new Map([["package.json", manifest], ["scripts/check.mjs", 'spawn("cargo", ["test"])']]);
  const job = { env: { TOKEN: "${{ secrets.FIXTURE }}" }, steps: [{ run: "npm run notify" }] };
  const check = () => checkInputs({ jobs: { job } }, "fixture.yml", new Map(), path => source.get(path));
  check();
  job.steps[0].run = "npm run verify";
  assert.throws(check, /fixture.yml.jobs.job.env.TOKEN: .*secrets/);
});

function credentialJob(run) {
  return { env: { TOKEN: "${{ secrets.FIXTURE }}" }, steps: [{ run }] };
}

for (const hook of ["preverify", "postverify"]) {
  test(`npm run follows ${hook} when the main script is benign`, () => {
    const scripts = { verify: "echo fixture", [hook]: "bash check.sh" };
    const files = new Map([["package.json", JSON.stringify({ scripts })], ["check.sh", "cargo test"]]);
    const job = credentialJob("npm run verify");
    job.steps[0]["working-directory"] = "suite";
    assert.throws(() => checkInputs({ jobs: { job } }, "fixture.yml", new Map(), p => files.get(p)), /env.TOKEN/);
  });
}

test("shell wrappers use the effective run directory at each step", () => {
  const job = credentialJob("bash check.sh");
  const workflow = { jobs: { job } };
  const check = () => checkInputs(workflow, "fixture.yml", new Map(), path => files.get(path));
  const files = new Map([["check.sh", "echo fixture"], ["suite/check.sh", "bash inner.sh"], ["suite/inner.sh", "cargo test"]]);
  check();
  workflow.defaults = { run: { "working-directory": "suite" } };
  assert.throws(check, /env.TOKEN/);
  job.defaults = { run: { "working-directory": "." } };
  check();
  job.steps.push({ run: "bash check.sh", "working-directory": "suite" });
  assert.throws(check, /env.TOKEN/);
});

test("the container image denylist allows a notification input, the shard runner refuses it", () => {
  const job = credentialJob("npm run test:container");
  const check = () => checkInputs({ jobs: { job } }, "fixture.yml");
  check();
  job.steps[0].run = "node .github/scripts/rust-shards.mjs";
  assert.throws(check, /env.TOKEN/);
});

test("an indexed wrapper cannot be read through a symlinked ancestor", t => {
  const temp = mkdtempSync(posix.join(tmpdir(), "wrapper-"));
  t.after(() => rmSync(temp, { recursive: true, force: true }));
  const checkout = posix.join(temp, "repo");
  mkdirSync(checkout);
  writeFileSync(posix.join(temp, "check.sh"), "echo fixture");
  symlinkSync(temp, posix.join(checkout, "scripts"));
  const read = indexedReader(checkout, ["scripts/check.sh"]);
  assert.throws(() => read("scripts/check.sh"), /symlink/);
});

test("Git-inventoried symlinks are refused before wrapper paths are normalized", t => {
  const checkout = mkdtempSync(posix.join(tmpdir(), "indexed-wrapper-"));
  t.after(() => rmSync(checkout, { recursive: true, force: true }));
  git(checkout, "init", "-q");
  mkdirSync(posix.join(checkout, "wrappers"));
  symlinkSync("wrappers", posix.join(checkout, "linked"));
  git(checkout, "add", "linked");
  const files = sourceFiles(checkout);
  assert.deepEqual(files, ["linked"]);
  const read = indexedReader(checkout, files);
  for (const [dir, run] of [[".", "./linked/check.sh"], [".", "./linked/../check.sh"],
    ["linked/..", "./check.sh"], ["linked/..", "npm run verify"]]) {
    const job = credentialJob(run);
    job.steps[0]["working-directory"] = dir;
    assert.throws(() => checkInputs({ jobs: { job } }, "fixture.yml", new Map(), read), /symlink/);
  }
});

test("JavaScript npm calls traverse lifecycle hooks", () => {
  const scripts = { verify: "echo fixture", postverify: "cargo test" };
  for (const api of ["spawn", "spawnSync", "execFile", "execFileSync"]) {
    const files = new Map([["package.json", JSON.stringify({ scripts })],
      ["check.js", `runner.${api}("npm", ["run", "verify"])`]]);
    assert.throws(() => checkInputs({ jobs: { job: credentialJob("node check.js") } },
      "fixture.yml", new Map(), path => files.get(path)), /live credential|env.TOKEN/);
  }
});

test("computed shell words do not resolve to their literal fragments", () => {
  const files = new Map([["check.sh", "cargo test"],
    ["package.json", JSON.stringify({ scripts: { verify: "cargo test" } })]]);
  for (const run of ["bash check$(printf other).sh", "npm run ver$(printf other)ify"]) {
    checkInputs({ jobs: { job: credentialJob(run) } }, "fixture.yml", new Map(), path => files.get(path));
  }
});

test("literal JavaScript process calls distinguish executable names from messages and arguments", () => {
  const job = credentialJob("node check.js");
  const check = source => checkInputs({ jobs: { job } }, "fixture.yml", new Map(),
    path => path === "check.js" ? source : undefined);
  check('console.log("cargo"); runner.spawnSync("tar", ["cargo"]); // spawn("cargo")');
  assert.throws(() => check('runner.execFileSync("cargo", args)'), /env.TOKEN/);
});

test("JavaScript interpreter calls follow extensionless script operands", () => {
  for (const path of ["scripts/fixture-check", "fixture-check"]) {
    const job = credentialJob("node wrapper.js");
    const files = new Map([["wrapper.js", `runner.execFile("bash", ["${path}"])`], [path, "cargo test"]]);
    assert.throws(() => checkInputs({ jobs: { job } }, "fixture.yml", new Map(), file => files.get(file)), /env.TOKEN/);
  }
});

test("JavaScript process calls resolve wrappers from the child working directory", () => {
  const job = credentialJob("node wrapper.js");
  job.steps[0]["working-directory"] = "jobs";
  const files = new Map([["jobs/check.sh", "echo fixture"], ["jobs/suite/check.sh", "cargo test"]]);
  const check = source => {
    files.set("jobs/wrapper.js", source);
    checkInputs({ jobs: { job } }, "fixture.yml", new Map(), path => files.get(path));
  };
  check('runner.spawn("bash", ["check.sh"])');
  for (const call of [
    'runner.spawnSync("bash", ["check.sh"], { cwd: "suite" })',
    'runner.execFile("bash", ["check.sh"], { "cwd": "suite" }, callback)',
    'runner.exec("bash check.sh", { cwd: "suite" }, callback)',
    'runner.execFileSync("./check.sh", { cwd: "suite" })',
  ]) assert.throws(() => check(call), /env.TOKEN/);
});

test("npm forwards literal script operands to the main lifecycle event", () => {
  const files = new Map([["check.sh", "cargo test"], ["package.json", JSON.stringify({
    scripts: { script: "bash", echo: "echo fixture", preecho: "bash", postecho: "bash" },
  })]]);
  const check = run => checkInputs({ jobs: { job: credentialJob(run) } }, "fixture.yml", new Map(), p => files.get(p));
  check("npm run echo -- check.sh");
  assert.throws(() => check("npm run script -- check.sh"), /env.TOKEN/);
  files.set("runner.js", 'spawn("npm", ["run", "script", "--", "check.sh"])');
  assert.throws(() => check("node runner.js"), /env.TOKEN/);
});

test("removing the parent of an indexed wrapper still reports the missing file", t => {
  const checkout = mkdtempSync(posix.join(tmpdir(), "missing-wrapper-"));
  t.after(() => rmSync(checkout, { recursive: true, force: true }));
  git(checkout, "init", "-q");
  mkdirSync(posix.join(checkout, "scripts"));
  writeFileSync(posix.join(checkout, "scripts/check.sh"), "echo fixture");
  git(checkout, "add", "scripts/check.sh");
  rmSync(posix.join(checkout, "scripts"), { recursive: true });
  const read = indexedReader(checkout, sourceFiles(checkout));
  assert.equal(read("scripts/absent.sh"), undefined);
  assert.throws(() => read("scripts/check.sh"), /indexed wrapper path is missing/);
});

test("shell script flags and end-of-options preserve the script operand", () => {
  const files = new Map([["-check", "cargo test"]]);
  for (const shell of ["sh", "bash"]) {
    files.set("runner.js", `spawn("${shell}", ["-e", "--", "-check"])`);
    assert.throws(() => checkInputs({ jobs: { job: credentialJob("node runner.js") } },
      "fixture.yml", new Map(), path => files.get(path)), /env.TOKEN/);
  }
});

test("npm forwards mixed positional arguments without treating data as executable", () => {
  const files = new Map([["check.sh", "cargo test"], ["package.json", JSON.stringify({
    scripts: { script: "bash", data: "cat", node: "node print.js" },
  })], ["print.js", "console.log(process.argv)"]]);
  const check = run => checkInputs({ jobs: { job: credentialJob(run) } }, "fixture.yml", new Map(), p => files.get(p));
  check("npm run data -- cargo");
  check("npm run node -- check.sh");
  assert.throws(() => check("npm run script check.sh -- --verbose"), /env.TOKEN/);
});

test("an npm launch script inspects the forwarded executable and its script operand", () => {
  const files = new Map([["check", "cargo test"]]);
  const check = run => checkInputs({ jobs: { job: credentialJob(run) } }, "fixture.yml", new Map(), p => files.get(p));
  for (const launcher of ["exec", "env", "exec env"]) {
    files.set("package.json", JSON.stringify({ scripts: { launch: launcher } }));
    check("npm run launch -- cat cargo");
    assert.throws(() => check("npm run launch -- cargo test"), /env.TOKEN/);
    assert.throws(() => check("npm run launch -- bash check"), /env.TOKEN/);
  }
});

test("shell single dash followed by a literal filename executes that script", () => {
  for (const shell of ["bash", "sh"]) {
    const files = new Map([["runner.js", `spawn("${shell}", ["-", "check"])`], ["check", "cargo test"]]);
    assert.throws(() => checkInputs({ jobs: { job: credentialJob("node runner.js") } },
      "fixture.yml", new Map(), p => files.get(p)), /env.TOKEN/);
  }
});
