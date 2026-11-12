// Structure ratchet for the gateway crate (#500, first step of #499).
//
// Counts the production lines of every Rust file under apps/gateway/src and
// checks them against tests/structure-allowance.json. Pure functions here; the
// tests and the run against the real tree are in tests/structure.test.mjs.
//
// What is a test file (not measured at all):
//   1. by name, the three forms this repository uses: a file called `tests.rs`,
//      a file whose name ends in `_tests.rs`, or any file under a directory
//      called `tests`;
//   2. by declaration: a file module declared at the top level of its parent
//      under `#[cfg(test)]` or `#[cfg(all(test, ...))]` (`mod x;`, with or
//      without `#[path]`), and everything in that module's directory. This
//      covers the test-only trees with other names (management_openapi/,
//      inference_openapi/, authz_matrix.rs, providers/contract.rs, ...).
//
// What is a production line, in any other file: every line except the top-level
// test modules. A top-level test module is a `#[cfg(test)]` or
// `#[cfg(all(test, ...))]` attribute at column 0 followed (after other
// attributes or comments) by a `mod` item. For `mod x { ... }` the lines from the
// attribute to the closing `}` at column 0 are left out; for `mod x;` only the
// declaration is. Code after a test module is counted: several files declare
// their test modules at the top. Blank lines and comments count. A
// `#[cfg(test)]` on anything other than a module (a function, an impl, a use)
// is counted, and so is `#[cfg(any(test, ...))]`, which also builds without
// `test`. The rule relies on rustfmt: top-level items start at column 0.
import { readFileSync, readdirSync } from "node:fs";
import { posix, join } from "node:path";

// A file may have this many production lines before it must be on the list.
export const LIMIT = 800;
// A ceiling may sit at most this far above the file's real size.
export const SLACK = 50;

const LIST = "tests/structure-allowance.json";

// `#[cfg(test)]` or `#[cfg(all(test, ...))]`, alone on its line at column 0.
const TEST_CFG = /^#\[cfg\((?:test|all\(test\b.*\))\)\]\s*$/;
// Another attribute or a comment between the cfg and the item.
const BETWEEN = /^(?:#\[|\/\/)/;
const MOD_ITEM = /^(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*([;{])/;
const PATH_ATTR = /^#\[path\s*=\s*"([^"]+)"\]/;

export function isTestPath(path) {
  const parts = path.split("/");
  const name = parts.pop();
  return name === "tests.rs" || name.endsWith("_tests.rs") || parts.includes("tests");
}

function splitLines(text) {
  const lines = text.split("\n");
  if (lines.at(-1) === "") lines.pop();
  return lines;
}

// Every top-level test module in a file: {start, end, name, inline, path}.
// `start`..`end` are the 0-based lines it occupies, both included.
function testModules(text) {
  const lines = splitLines(text);
  const found = [];
  for (let i = 0; i < lines.length; i++) {
    if (!TEST_CFG.test(lines[i])) continue;
    let j = i + 1;
    while (j < lines.length && BETWEEN.test(lines[j])) j++;
    const item = (lines[j] ?? "").match(MOD_ITEM);
    if (!item) continue;
    const [, name, opener] = item;
    let end = j;
    const inline = opener === "{";
    if (inline && !lines[j].trimEnd().endsWith("}")) {
      end = lines.findIndex((line, k) => k > j && line.startsWith("}"));
      if (end === -1) {
        throw new Error(`test module \`${name}\` opened on line ${j + 1} never closes at column 0`);
      }
    }
    const pathAttr = lines.slice(i, j).map((line) => line.match(PATH_ATTR)).find(Boolean);
    found.push({ start: i, end, name, inline, path: pathAttr?.[1] });
    i = end;
  }
  return { lines, found };
}

export function productionLines(text) {
  const { lines, found } = testModules(text);
  return found.reduce((count, module) => count - (module.end - module.start + 1), lines.length);
}

// Returns a predicate: is this path a module declared only for tests (rule 2
// above), or inside one? `sources` is a Map of path -> file text.
export function testOnlyModules(sources) {
  const files = new Set();
  const dirs = [];
  for (const [path, text] of sources) {
    const dir = posix.dirname(path);
    const name = posix.basename(path, ".rs");
    // Where this file's child modules live.
    const children = ["mod", "lib", "main"].includes(name) ? dir : posix.join(dir, name);
    for (const module of testModules(text).found) {
      if (module.inline) continue;
      if (module.path) {
        const target = posix.join(dir, module.path);
        files.add(target);
        // A `mod.rs` target keeps its children beside it, like any `mod.rs`.
        const own = posix.basename(target) === "mod.rs" ? posix.dirname(target) : target.replace(/\.rs$/, "");
        dirs.push(`${own}/`);
      } else {
        const base = posix.join(children, module.name);
        files.add(`${base}.rs`);
        dirs.push(`${base}/`);
      }
    }
  }
  return (path) => files.has(path) || dirs.some((prefix) => path.startsWith(prefix));
}

// Map of path -> production lines for every file that is not a test file.
export function measure(sources) {
  const isTestOnly = testOnlyModules(sources);
  const sizes = new Map();
  for (const path of [...sources.keys()].sort()) {
    if (isTestPath(path) || isTestOnly(path)) continue;
    try {
      sizes.set(path, productionLines(sources.get(path)));
    } catch (error) {
      throw new Error(`${path}: ${error.message}`);
    }
  }
  return sizes;
}

// Map of repository-relative path -> text for every .rs file under `src`.
export function readSources(root, src) {
  const sources = new Map();
  const walk = (dir) => {
    for (const entry of readdirSync(join(root, dir), { withFileTypes: true })) {
      const path = `${dir}/${entry.name}`;
      if (entry.isDirectory()) walk(path);
      else if (entry.isFile() && entry.name.endsWith(".rs")) {
        sources.set(path, readFileSync(join(root, path), "utf8"));
      }
    }
  };
  walk(src);
  return sources;
}

const isSorted = (keys) => keys.every((key, i) => i === 0 || keys[i - 1] < key);
const isTable = (value) => typeof value === "object" && value !== null && !Array.isArray(value);

// Checks the allowance list against the measured sizes (a Map from `measure`).
// Returns every problem as a message; an empty array means the tree passes.
//
// `baseline` is the list as it was when the ratchet landed. It is never edited
// except to delete entries. Rule 4 compares `files` with it, so a ceiling
// cannot be raised and a file cannot be added by editing `files` alone.
export function checkAllowance(allowance, sizes) {
  const errors = [];
  if (!isTable(allowance) || allowance.limit !== LIMIT) {
    errors.push(`${LIST}: "limit" must stay ${LIMIT}. Raising it is not how a file gets smaller.`);
  }
  for (const section of ["files", "baseline"]) {
    const table = allowance?.[section];
    if (!isTable(table)) {
      errors.push(`${LIST}: "${section}" must be an object of path -> ceiling.`);
      continue;
    }
    if (!isSorted(Object.keys(table))) {
      errors.push(`${LIST}: "${section}" must be sorted by path, without duplicates.`);
    }
    for (const [path, ceiling] of Object.entries(table)) {
      if (!Number.isInteger(ceiling) || ceiling <= 0) {
        errors.push(`${LIST}: "${section}" entry ${path} must be a whole number of lines.`);
      }
    }
  }
  if (errors.length > 0) return errors;

  const { files, baseline } = allowance;

  for (const [path, size] of sizes) {
    if (size <= LIMIT || Object.hasOwn(files, path)) continue;
    // Rule 1, a large file that is not listed.
    errors.push(
      `${path} has ${size} production lines; the limit is ${LIMIT}.\n` +
        `  Split the file, or move code out of it, until it is at or under ${LIMIT}.\n` +
        `  Do not add it to ${LIST}: the list only shrinks (#499).`,
    );
  }

  for (const [path, ceiling] of Object.entries(files)) {
    const size = sizes.get(path);
    if (size === undefined) {
      // Rule 3, the file is gone.
      errors.push(
        `${path} is listed in ${LIST} but no longer exists as a production file.\n` +
          `  Remove it from "files" and from "baseline".`,
      );
      continue;
    }
    if (size <= LIMIT) {
      // Rule 3, the file is small enough now.
      errors.push(
        `${path} is listed in ${LIST} but has ${size} production lines, within the limit of ${LIMIT}.\n` +
          `  Remove it from "files" and from "baseline".`,
      );
      continue;
    }
    if (!Object.hasOwn(baseline, path)) {
      // Rule 4, a new entry.
      errors.push(
        `${path} is in "files" but not in "baseline" in ${LIST}: the list can only shrink.\n` +
          `  Split the file until it is at or under ${LIMIT} lines and remove the entry.`,
      );
      continue;
    }
    if (ceiling > baseline[path]) {
      // Rule 4, a raised ceiling.
      errors.push(
        `${path}: ceiling ${ceiling} is above its baseline ${baseline[path]} in ${LIST}.\n` +
          `  The list can only shrink. Set the ceiling back and move code out of the file instead.`,
      );
      continue;
    }
    if (size > ceiling) {
      // Rule 1, a listed file that grew.
      errors.push(
        `${path} has ${size} production lines; its ceiling is ${ceiling}.\n` +
          `  Move code out of the file. Do not raise the ceiling in ${LIST}.`,
      );
      continue;
    }
    if (ceiling > size + SLACK) {
      // Rule 2, a loose ceiling.
      errors.push(
        `${path}: ceiling ${ceiling} is more than ${SLACK} above the file's ${size} production lines.\n` +
          `  Lower the ceiling to ${size} in "files" in ${LIST}.`,
      );
    }
  }

  for (const path of Object.keys(baseline)) {
    if (Object.hasOwn(files, path)) continue;
    // Rule 4, a leftover that would let the file back onto the list.
    errors.push(
      `${path} is in "baseline" but not in "files" in ${LIST}.\n` +
        `  Delete it from "baseline": a file taken off the list stays off.`,
    );
  }

  return errors;
}
