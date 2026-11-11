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
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
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
  refused(standIn(join(dir, "slow"), "exec sleep 30"), "no answer in time");
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
  assert.deepEqual(source.match(/process\.env[\w.[\]"']*/g), ["process.env.PATH"], "no environment variable but PATH reaches the preflight");
  assert.doesNotMatch(source, /process\.argv|skip|todo/i);
});

// Every test file that runs Bash must do it through the preflight module.
test("no test file starts bash on its own (#380)", () => {
  const files = readdirSync(join(root, "tests")).filter(name => name.endsWith(".mjs") && !name.startsWith("bash-preflight."));
  assert.ok(files.includes("supply-chain.test.mjs") && files.includes("oci-tool.test.mjs"));
  const users = [];
  for (const name of files) {
    const text = readFileSync(join(root, "tests", name), "utf8");
    assert.doesNotMatch(text, /\b(?:spawn|spawnSync|exec|execSync|execFile|execFileSync|fork)\(\s*["'`][^"'`\n]*\b(?:ba|z|da|k)?sh["'`]/, `${name} starts a shell directly`);
    assert.doesNotMatch(text, /\bshell:\s*true\b/, `${name} uses the shell option`);
    assert.doesNotMatch(text, /\b(?:checkBash|findBash)\b/, `${name} must use bashPath()/runBash(), which take no file`);
    if (/\b(?:runBash|bashPath)\b/.test(text)) users.push(name);
  }
  assert.deepEqual(users.sort(), ["oci-tool.test.mjs", "supply-chain.test.mjs"]);
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
