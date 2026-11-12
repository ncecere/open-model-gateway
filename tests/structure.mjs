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
//
// Three checks keep a file from leaving the count by accident (`checkSources`,
// `readSources`): a file skipped as a test file may not also be declared by
// production code, a measured file may not switch rustfmt off, and nothing
// under the sources may be a symbolic link. `duplicateKeys` reads the raw
// allowance file, because JSON.parse keeps the last of two equal keys.
//
// The Clippy half of the ratchet (function length and complexity) is enforced
// by Clippy itself. `checkLints` and `checkClippyConfig` only check that its
// switches are still on: the two crate lines, no threshold setting, no mention
// of either lint outside a `reason = "#499"` marker, and the marker count.
//
// docs/verification.md lists what none of this sees.
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
// Either spelling, as an attribute or inside `cfg_attr`.
const RUSTFMT_SKIP = /rustfmt::skip|rustfmt_skip/;

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

// Where a `mod x;` declared at the top level of `path` points: the files that
// can hold the module, and the directory that holds its children.
function moduleTarget(path, module) {
  const dir = posix.dirname(path);
  if (module.path) {
    const target = posix.join(dir, module.path);
    // A `mod.rs` target keeps its children beside it, like any `mod.rs`.
    const own = posix.basename(target) === "mod.rs" ? posix.dirname(target) : target.replace(/\.rs$/, "");
    return { files: [target], dir: `${own}/` };
  }
  const name = posix.basename(path, ".rs");
  // Where this file's child modules live.
  const children = ["mod", "lib", "main"].includes(name) ? dir : posix.join(dir, name);
  const base = posix.join(children, module.name);
  return { files: [`${base}.rs`, `${base}/mod.rs`], dir: `${base}/` };
}

// Returns a predicate: is this path a module declared only for tests (rule 2
// above), or inside one? `sources` is a Map of path -> file text.
export function testOnlyModules(sources) {
  const files = new Set();
  const dirs = [];
  for (const [path, text] of sources) {
    for (const module of testModules(text).found) {
      if (module.inline) continue;
      const target = moduleTarget(path, module);
      target.files.forEach((file) => files.add(file));
      dirs.push(target.dir);
    }
  }
  return (path) => files.has(path) || dirs.some((prefix) => path.startsWith(prefix));
}

// Every top-level `mod x;` of a file that is not a test module:
// {line, name, path}, `line` 0-based.
function productionModules(text) {
  const { lines, found } = testModules(text);
  const inTest = new Set();
  for (const module of found) {
    for (let i = module.start; i <= module.end; i++) inTest.add(i);
  }
  const declared = [];
  lines.forEach((line, i) => {
    if (inTest.has(i)) return;
    const item = line.match(MOD_ITEM);
    if (!item || item[2] !== ";") return;
    let path;
    for (let j = i - 1; j >= 0 && !inTest.has(j) && BETWEEN.test(lines[j]); j--) {
      path ??= lines[j].match(PATH_ATTR)?.[1];
    }
    declared.push({ line: i, name: item[1], path });
  });
  return declared;
}

// Problems in the sources themselves, as messages (empty when there are none):
//   - a file that is skipped as a test file, by name or by declaration, is also
//     declared by a measured file outside `#[cfg(test)]`, so it is built into
//     the gateway and nothing counts it;
//   - a measured file switches rustfmt off, and the counting relies on its layout.
export function checkSources(sources) {
  const errors = [];
  const isTestOnly = testOnlyModules(sources);
  const skipped = (path) => isTestPath(path) || isTestOnly(path);
  for (const path of [...sources.keys()].sort()) {
    if (skipped(path)) continue;
    const text = sources.get(path);
    splitLines(text).forEach((line, i) => {
      if (!RUSTFMT_SKIP.test(line)) return;
      errors.push(
        `${path} line ${i + 1} switches rustfmt off.\n` +
          `  Remove it: the line count relies on rustfmt's layout (top-level items at the start of a line).`,
      );
    });
    let declared;
    try {
      declared = productionModules(text);
    } catch (error) {
      throw new Error(`${path}: ${error.message}`);
    }
    for (const module of declared) {
      for (const file of moduleTarget(path, module).files) {
        if (!sources.has(file) || !skipped(file)) continue;
        errors.push(
          `${file} is declared outside #[cfg(test)] (\`mod ${module.name};\` in ${path} line ${module.line + 1}), ` +
            `so it is production code, but it is not measured.\n` +
            `  Rename it or move it: test names (tests.rs, *_tests.rs, a tests/ directory) and test-only directories are not measured.`,
        );
      }
    }
  }
  return errors;
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
      if (entry.isSymbolicLink()) {
        throw new Error(
          `${path} is a symbolic link. Links under ${src} are not followed, so what they point to would not be counted.\n` +
            `  Replace it with a real file or a \`mod\` declaration.`,
        );
      }
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
      errors.push(`${LIST}: "${section}" must be sorted by path.`);
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

// Keys written more than once in the same object of a JSON text, as
// `"section": "key"` (or `top level: "key"`). JSON.parse keeps the last one
// without complaint, so this reads the raw text. The text must be valid JSON.
export function duplicateKeys(text) {
  const repeats = [];
  // One entry per open object or array: its own key in the parent, and the keys seen in it.
  const open = [];
  let last;
  for (const [token] of text.matchAll(/"(?:[^"\\]|\\.)*"|[{}[\],:]/g)) {
    if (token === "{" || token === "[") {
      open.push({ name: last, keys: new Set(), wantsKey: token === "{", isObject: token === "{" });
      continue;
    }
    const here = open.at(-1);
    if (!here) continue;
    if (token === "}" || token === "]") open.pop();
    else if (token === ",") here.wantsKey = here.isObject;
    else if (token === ":") here.wantsKey = false;
    else if (here.wantsKey) {
      last = JSON.parse(token);
      if (here.keys.has(last)) {
        const where = open.length === 1 ? "top level" : `"${here.name}"`;
        repeats.push(`${where}: "${last}"`);
      }
      here.keys.add(last);
    }
  }
  return repeats;
}

// --- the Clippy side ---------------------------------------------------------

// The line that turns both lints on, in lib.rs and in main.rs.
export const CRATE_LINTS =
  "#![cfg_attr(not(test), warn(clippy::too_many_lines, clippy::cognitive_complexity))]";
const CRATE_ROOTS = ["lib.rs", "main.rs"];
const MARKER_REASON = 'reason = "#499"';
// A lint is always named with its `clippy::` prefix. The bare words also occur
// as data (the batch error code "too_many_lines").
const LINT = "clippy::(?:too_many_lines|cognitive_complexity)";
const NAMES_LINT = new RegExp(`${LINT}\\b`);
const EXPECT = `expect\\(${LINT}(?:, ${LINT})?, reason = "#499"\\)`;
// The two forms rustfmt writes for a marker: on one line, or over four.
const MARKER_LINE = new RegExp(`^\\s*#\\[cfg_attr\\(not\\(test\\), ${EXPECT}\\)\\]$`);
const MARKER_SPLIT = [/^\s*#\[cfg_attr\($/, /^\s*not\(test\),$/, new RegExp(`^\\s*${EXPECT}$`), /^\s*\)\]$/];

const isCount = (value) => Number.isInteger(value) && value >= 0;

// Checks that the Clippy half of the ratchet is still switched on. `sources` is
// every .rs file of the crate (test files included); `allowance` is the parsed
// allowance list. Returns messages; an empty array means it is.
export function checkLints(sources, allowance) {
  const errors = [];
  const src = posix.dirname([...sources.keys()].find((path) => path.endsWith("/lib.rs")) ?? "apps/gateway/src/lib.rs");
  const roots = CRATE_ROOTS.map((name) => `${src}/${name}`);
  for (const path of roots) {
    if (splitLines(sources.get(path) ?? "").includes(CRATE_LINTS)) continue;
    errors.push(
      `${path} must contain the line\n  ${CRATE_LINTS}\n` +
        `  It turns on the function-length and complexity checks for the gateway (#499). Put it back as written.`,
    );
  }

  let markers = 0;
  const holders = new Map();
  for (const path of [...sources.keys()].sort()) {
    const lines = splitLines(sources.get(path));
    lines.forEach((line, i) => {
      if (!NAMES_LINT.test(line) && !line.includes(MARKER_REASON)) return;
      if (line === CRATE_LINTS && roots.includes(path)) return;
      const split = MARKER_SPLIT.every((part, k) => part.test(lines[i - 2 + k] ?? ""));
      if (MARKER_LINE.test(line) || split) {
        markers += 1;
        holders.set(path, (holders.get(path) ?? 0) + 1);
        return;
      }
      errors.push(
        `${path} line ${i + 1} names clippy::too_many_lines, clippy::cognitive_complexity or ${MARKER_REASON} ` +
          `outside the two crate lines and the existing markers:\n  ${line.trim()}\n` +
          `  Remove it. The lints are never switched off for new code: never \`allow\`, no other level, ` +
          `no other reason, no new marker. Shorten or split the function instead (#499).`,
      );
    });
  }

  if (!isTable(allowance) || !isCount(allowance.markers) || !isCount(allowance.markersBaseline)) {
    errors.push(`${LIST}: "markers" and "markersBaseline" must be whole numbers.`);
    return errors;
  }
  const where = [...holders].map(([path, count]) => `    ${count} ${path}`).join("\n");
  if (markers > allowance.markers) {
    errors.push(
      `${markers} markers (${MARKER_REASON}) found under ${src}, but "markers" is ${allowance.markers} in ${LIST}.\n` +
        `  Never add a marker: shorten or split the new function instead. Do not raise "markers".\n` +
        `  Markers per file:\n${where}`,
    );
  } else if (markers < allowance.markers) {
    errors.push(
      `${markers} markers (${MARKER_REASON}) found under ${src}, but "markers" is ${allowance.markers} in ${LIST}.\n` +
        `  Lower "markers" to ${markers} in the same commit that removed the marker. Leave "markersBaseline" alone.`,
    );
  } else if (allowance.markers > allowance.markersBaseline) {
    errors.push(
      `${LIST}: "markers" (${allowance.markers}) is above "markersBaseline" (${allowance.markersBaseline}).\n` +
        `  The count only goes down. Never add a marker, and never raise either number: remove the new marker and shorten the function.`,
    );
  }
  return errors;
}

// A threshold setting in a Clippy configuration file would move the limits of
// both lints for every function at once.
export function checkClippyConfig(path, text) {
  const errors = [];
  splitLines(text).forEach((line, i) => {
    if (!/(?:too[-_]many[-_]lines|cognitive[-_]complexity)[-_]threshold/.test(line)) return;
    errors.push(
      `${path} line ${i + 1} sets a threshold for too_many_lines or cognitive_complexity.\n` +
        `  Remove it: the ratchet uses Clippy's default limits (#499).`,
    );
  });
  return errors;
}
