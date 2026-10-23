// Committed merge-conflict markers (#235): #225's merge (df01e7a) landed raw
// `<<<<<<<`/`=======`/`>>>>>>>` markers in docs/releases/unreleased.md on `main`,
// and every required check passed, because nothing parses Markdown/config. `git
// grep` alone misses this too: it skips unmerged paths, which is exactly how the
// bad merge got through locally.
//
// The pattern below is chosen against this repository's actual tracked text,
// not in the abstract: a bare `git grep -nE "^=+$"` over the whole tree (see the
// PR) turns up nothing but the known markers, so Markdown here never uses an
// unguarded `=======` line (no setext headings, no tables built from `=`). Even
// so, the checker does not rely on that staying true forever: a bare `=======`
// line only counts as a marker when it sits between an open `<<<<<<< ` and the
// matching `>>>>>>> `, so a future setext heading or divider that happens to be
// exactly seven `=` characters on its own line still passes.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const root = fileURLToPath(new URL("..", import.meta.url));

// A line starting with these seven characters and a space is unambiguous: real
// conflict markers always carry a ref/label after the space (`<<<<<<< HEAD`,
// `>>>>>>> origin/main`), so this cannot collide with Markdown's `---`/`===`
// rule-of-thumb or with code showing a raw `<<<` operator.
const START = /^<<<<<<< /;
const END = /^>>>>>>> /;
const SEP = "=======";

// Every violation in one file's text, as {line, marker}. A bare `=======` line
// is only a violation while between an open start marker and its end marker;
// start/end markers themselves are always violations, open or not.
export function findMarkerViolations(text) {
  const violations = [];
  let inConflict = false;
  text.split("\n").forEach((line, i) => {
    if (START.test(line)) {
      violations.push({ line: i + 1, marker: line });
      inConflict = true;
    } else if (END.test(line)) {
      violations.push({ line: i + 1, marker: line });
      inConflict = false;
    } else if (inConflict && line === SEP) {
      violations.push({ line: i + 1, marker: line });
    }
  });
  return violations;
}

// git's own binary heuristic (a NUL byte in the first chunk of the file); good
// enough to skip the repo's images and filestore test fixtures without a
// dependency on file extensions.
export function looksBinary(buffer) {
  const len = Math.min(buffer.length, 8000);
  for (let i = 0; i < len; i++) if (buffer[i] === 0) return true;
  return false;
}

const EXCLUDED = /(^|\/)(node_modules|target)\//;

// Tracked files only: untracked/ignored build output (node_modules/, target/)
// never needs this check. This runs in CI on what actually landed, so it does
// not depend on the local `git grep` check that skipped unmerged paths.
export function trackedTextFiles(cwd = root) {
  const out = execFileSync("git", ["ls-files"], { cwd, encoding: "utf8" });
  return out
    .split("\n")
    .filter(Boolean)
    .filter(f => !EXCLUDED.test(f))
    .filter(f => {
      let buf;
      try {
        buf = readFileSync(join(cwd, f));
      } catch {
        return false; // e.g. a broken symlink; nothing to check
      }
      return !looksBinary(buf);
    });
}

test("no tracked text file contains a merge-conflict marker", () => {
  // #225's merge (df01e7a) left raw markers in docs/releases/unreleased.md;
  // #234 repaired that file. This keeps it from happening again (#235).
  const found = [];
  for (const file of trackedTextFiles()) {
    const text = readFileSync(join(root, file), "utf8");
    for (const v of findMarkerViolations(text)) found.push(`${file}:${v.line}: ${v.marker}`);
  }
  assert.deepEqual(found, []);
});

// ---- fixtures: each marker form, a clean file, and a legitimate '===' line ----

test("fixtures: each conflict-marker form is detected", () => {
  assert.deepEqual(findMarkerViolations("a\n<<<<<<< HEAD\nb\n"), [{ line: 2, marker: "<<<<<<< HEAD" }]);
  assert.deepEqual(findMarkerViolations("a\n>>>>>>> origin/main\nb\n"), [{ line: 2, marker: ">>>>>>> origin/main" }]);
  assert.deepEqual(
    findMarkerViolations("<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> origin/main\n"),
    [
      { line: 1, marker: "<<<<<<< HEAD" },
      { line: 3, marker: "=======" },
      { line: 5, marker: ">>>>>>> origin/main" },
    ],
  );
});

test("fixtures: a clean file has no violations", () => {
  const clean = "# Title\n\nSome prose with no markers: < and > appear, just not seven in a row.\n";
  assert.deepEqual(findMarkerViolations(clean), []);
});

test("fixtures: a legitimate Markdown '===' heading underline is not flagged", () => {
  // A setext heading whose underline happens to be exactly seven '=' characters,
  // with no open conflict marker before it: must pass.
  const setext = "Title\n=======\n\nBody text.\n";
  assert.deepEqual(findMarkerViolations(setext), []);
  // Longer or shorter underlines are just as legitimate and must also pass.
  assert.deepEqual(findMarkerViolations("Title\n=====\n"), []);
  assert.deepEqual(findMarkerViolations("Title\n==========\n"), []);
});

test("fixtures: an unterminated start marker is still caught even with a later legitimate '===' line", () => {
  // A broken/partial merge (no closing marker) must still fail; a plain '==='
  // heading further down must not add a second, misleading violation beyond
  // the one genuine '=======' that sits inside the open conflict.
  const text = "<<<<<<< HEAD\nours\n=======\ntheirs\n\nAnother section\n=======\n";
  const violations = findMarkerViolations(text);
  assert.deepEqual(violations, [
    { line: 1, marker: "<<<<<<< HEAD" },
    { line: 3, marker: "=======" },
    { line: 7, marker: "=======" },
  ]);
});

test("fixtures: binary files are skipped by the NUL-byte heuristic", () => {
  assert.equal(looksBinary(Buffer.from("plain text\n")), false);
  assert.equal(looksBinary(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x00, 0x0d, 0x0a])), true);
});

test("fixtures: node_modules and target paths are excluded from the tracked-file list", () => {
  const files = ["src/lib.rs", "node_modules/pkg/index.js", "apps/web/node_modules/x.ts", "target/debug/build", "docs/readme.md"];
  const kept = files.filter(f => !/(^|\/)(node_modules|target)\//.test(f));
  assert.deepEqual(kept, ["src/lib.rs", "docs/readme.md"]);
});
