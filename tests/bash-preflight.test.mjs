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

// Process authority is checked by syntax, independently of call spelling.
import { processViolations } from "./process-policy.mjs";

import { promisify } from "node:util";
import { processArguments, spawnSync as safeSpawnSync, execFile as safeExecFile } from "./process.mjs";

test("the process API validates inert arguments before execution (#597)", () => {
  for (const file of ["bash", "/bin/sh", "Bash", "rbash", "mksh", "pwsh", "elvish", "env", "busybox", "npm", "npx", "tool.cmd"]) {
    assert.throws(() => processArguments(file, []), Error, file);
  }
  for (const options of [{ shell: true }, { shell: false }, { shell: "/bin/sh" }, Object.create({ shell: true })]) {
    assert.throws(() => processArguments("git", [], options), /shell options/);
  }
  for (const args of [["helper.yaml"], ["helper.py"], ["-e", "inert"], ["--require", "helper.yaml"], ["--eval=inert.mjs"]]) {
    assert.throws(() => processArguments(process.execPath, args), /JavaScript file/);
  }
  const args = ["--version"];
  assert.deepEqual(processArguments("git", args, { encoding: "utf8" }), ["git", args, { encoding: "utf8", shell: false }]);
  assert.deepEqual(processArguments("node", ["tests/ci.test.mjs"])[1], ["tests/ci.test.mjs"]);
  assert.doesNotThrow(() => processArguments("npm", ["run", "build", "--workspace", "@omg/web", "--", "--outDir", "/tmp/inert-output", "--emptyOutDir"]));
  // Real normal path through the wrapper, without interpreter input or fixtures.
  const version = safeSpawnSync("git", ["--version"], { encoding: "utf8" });
  assert.equal(version.status, 0, version.stderr);
  assert.match(version.stdout, /^git version /);
});

test("the asynchronous process API preserves Node's promisify contract (#597)", async () => {
  const result = await promisify(safeExecFile)("git", ["--version"], { encoding: "utf8" });
  assert.match(result.stdout, /^git version /);
  assert.equal(result.stderr, "");
});

test("process imports are refused regardless of their local call spelling (#597)", () => {
  for (const source of [
    'import { execSync as run } from "node:child_process"; run("inert");',
    'import * as cp from "child_process"; cp["execSync"]?.("inert");',
    'const cp = require("node:child_process");',
    'const cp = import("node:child_process");',
    'const cp = import("node:" + "child_process");',
    'import cp from "node:child_\\x70rocess";',
    'import cp = require("child_process");',
    'import { spawnSync } from "node:child_process"; spawnSync /* inert */ ("inert");',
    'export { spawn } from "node:child_process";',
    'import { createRequire as load } from "node:module";',
    'process["binding"]("inert");',
    'import { checkBash as check } from "./bash-preflight.mjs";',
    'import * as preflight from "./bash-preflight.mjs";',
    'const broken = ;',
  ]) assert.ok(processViolations("fixture.ts", source).length, source);
  for (const source of [
    'const fish = "fish"; const re = /#!\\/bin\\/bash/;',
    'type Options = { shell: string }; const f = (shell: string) => shell;',
    'const value = ok ? shell : other;',
    'import { spawnSync } from "./process.mjs";',
    'const fixture = `import cp from "node:child_process";`;',
  ]) assert.deepEqual(processViolations("fixture.ts", source), [], source);
});

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
  for (const name of files) {
    const text = readFileSync(join(root, "tests", name), "utf8");
    assert.deepEqual(processViolations(name, text), [], name);
  }
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
