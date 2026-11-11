// The supported-shell preflight for every test that runs a Bash fixture (#380).
//
// The fixtures run workflow `run:` scripts and helper scripts under Bash, and need 4.4:
// - Their guards are bare `[[ ... ]]` lines under `set -e`. Bash 4.1 is the first release
//   in which a failing `[[ ]]` ends such a script (its NEWS, item j: "The [[ and ((
//   commands are now subject to the setting of `set -e' and the ERR trap"). Under 3.2
//   (macOS's /bin/bash) and 4.0 the script carries on and a refusal test sees exit 0.
// - Under 4.1 to 4.3 .github/scripts/build-oras.sh exits 1, not the failed build's own
//   status, and one tests/oci-tool.test.mjs test fails.
// Measured for #380 with the official Bash builds: the guard scripts under 3.2 and 4.0,
// both suites under 4.1, 4.2 and 4.3 (that one failure each) and under 4.4, 5.0, 5.1, 5.2
// and 5.3 (all pass). CI's Ubuntu runners have Bash 5.2.
//
// A suite calls `bashPath()` once at the top of the file, so an unsupported shell fails
// the whole file before any test or fixture runs, and starts Bash only through
// `runBash()`, which runs the file that was checked and makes every `bash` the fixture
// looks up itself (`#!/usr/bin/env bash`, `bash file`) that same file. Neither takes a
// file to check and only PATH selects the shell; `findBash`/`checkBash` take arguments so
// that tests/bash-preflight.test.mjs can exercise them, and no other test file may use them.
import { spawnSync } from "node:child_process";
import { accessSync, constants, mkdtempSync, rmSync, statSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, isAbsolute, join } from "node:path";

export const MINIMUM_BASH = [4, 4];
const NEED = `These tests run shell fixtures and need Bash ${MINIMUM_BASH.join(".")} or newer`;
const HINT = "Put a supported bash first on PATH (macOS ships /bin/bash 3.2; Homebrew's bash is newer). See docs/verification.md.";

// The first executable regular file named `bash` in PATH's absolute directories.
export function findBash(path) {
  for (const dir of String(path ?? "").split(delimiter)) {
    if (!isAbsolute(dir)) continue;
    const file = join(dir, "bash");
    try {
      if (!statSync(file).isFile()) continue;
      accessSync(file, constants.X_OK);
      return file;
    } catch { /* not here; try the next directory */ }
  }
  throw new Error(`${NEED}; there is no executable 'bash' on PATH. ${HINT}`);
}

// Asks `file` itself for its version, then for the behaviour the fixtures rely on. Each
// answer must come within `limit` milliseconds; only the test of the limit passes one.
export function checkBash(file, limit = 10_000) {
  const ask = script => spawnSync(file, ["--noprofile", "--norc", "-e", "-c", script], { encoding: "utf8", timeout: limit });
  const answer = ask('echo "${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}"');
  if (answer.error) throw new Error(`${NEED}; '${file}' could not be run (${answer.error.code ?? answer.error.message}). ${HINT}`);
  const version = /^(\d{1,3})\.(\d{1,3})\n$/.exec(answer.stdout);
  if (answer.status !== 0 || !version) throw new Error(`${NEED}; '${file}' did not report a Bash version (exit status ${answer.status}, output ${JSON.stringify(answer.stdout.slice(0, 80))}). ${HINT}`);
  const found = `${version[1]}.${version[2]}`;
  const [major, minor] = [Number(version[1]), Number(version[2])];
  if (major < MINIMUM_BASH[0] || (major === MINIMUM_BASH[0] && minor < MINIMUM_BASH[1])) throw new Error(`${NEED}; '${file}' is Bash ${found}. ${HINT}`);
  const errexit = ask("[[ a = b ]]; exit 0");
  if (errexit.error || errexit.status !== 1) throw new Error(`${NEED}; '${file}' reports Bash ${found} but does not stop a 'set -e' script at a failing [[ ]] (exit status ${errexit.status}). ${HINT}`);
  return { file, version: found };
}

let checked;
export function bashPath() {
  checked ??= checkBash(findBash(process.env.PATH)).file;
  return checked;
}

// A private directory holding only `bash`, a link to the checked file. Made once per
// process and removed when it ends.
let linked;
function bashDir() {
  if (!linked) {
    const dir = mkdtempSync(join(tmpdir(), "checked-bash-"));
    process.on("exit", () => rmSync(dir, { recursive: true, force: true }));
    symlinkSync(bashPath(), join(dir, "bash"));
    linked = dir;
  }
  return linked;
}

// spawnSync for the checked Bash; `args` and `options` are spawnSync's. The fixture's PATH
// (the caller's `env.PATH`, or else the inherited one) gets the private directory first,
// so that a `bash` looked up inside the fixture is the checked file too, and loses its
// relative and empty entries: those name whatever directory the fixture happens to be
// in, which the preflight did not look at.
export function runBash(args, options = {}) {
  const { PATH = "", ...rest } = options.env ?? process.env;
  const path = [bashDir(), ...String(PATH).split(delimiter).filter(isAbsolute)].join(delimiter);
  return spawnSync(bashPath(), args, { ...options, env: { ...rest, PATH: path } });
}
