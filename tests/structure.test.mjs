// Structure ratchet for the gateway crate (#500, first step of #499): a file
// may not grow past the limit, and a file already past it may not grow at all.
// The rules and the counting live in tests/structure.mjs; the allowance list is
// tests/structure-allowance.json. docs/verification.md describes both.
//
// The first tests exercise each rule on in-memory fixtures. The last ones apply
// the rules to the real tree.
import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import {
  CRATE_LINTS,
  LIMIT,
  SLACK,
  checkAllowance,
  checkClippyConfig,
  checkLints,
  checkSources,
  duplicateKeys,
  isTestPath,
  measure,
  productionLines,
  readSources,
  testOnlyModules,
} from "./structure.mjs";

const root = fileURLToPath(new URL("..", import.meta.url));
const SRC = "apps/gateway/src";

const lines = (n, text = "let x = 1;") => `${Array(n).fill(text).join("\n")}\n`;
const allowance = (files, baseline = files) => ({ limit: LIMIT, files, baseline });
const sizes = (entries) => new Map(Object.entries(entries));

// --- which files are test files -------------------------------------------

test("test files are recognised by name", () => {
  for (const path of [
    `${SRC}/alerts/tests.rs`,
    `${SRC}/governance/locks_tests.rs`,
    `${SRC}/management/resources/tests/audit_filters.rs`,
    `${SRC}/management/governance/tests/db/reconcile_usage_tests.rs`,
  ]) {
    assert.equal(isTestPath(path), true, path);
  }
  for (const path of [
    `${SRC}/governance.rs`,
    `${SRC}/management/mod.rs`,
    // Names that only contain the word are production files.
    `${SRC}/contest.rs`,
    `${SRC}/attestation/latest.rs`,
    `${SRC}/filestore/testdata/readme.rs`,
    // Not one of the names in use. A test-only module with another name is
    // found through its `#[cfg(test)]` declaration instead (next test).
    `${SRC}/log_capture_test.rs`,
  ]) {
    assert.equal(isTestPath(path), false, path);
  }
});

test("a module declared under #[cfg(test)] is a test file, with its directory", () => {
  const sources = new Map([
    [
      `${SRC}/lib.rs`,
      [
        "pub mod auth;",
        "#[cfg(test)]",
        "mod authz_matrix;",
        "#[cfg(test)]",
        "pub(crate) mod management_openapi;",
        "pub mod providers;",
        "",
      ].join("\n"),
    ],
    [`${SRC}/auth.rs`, lines(3)],
    [`${SRC}/authz_matrix.rs`, lines(3)],
    [`${SRC}/management_openapi/mod.rs`, "pub(crate) mod quality;\n"],
    [`${SRC}/management_openapi/quality.rs`, lines(3)],
    [`${SRC}/management_openapi/quality/deep.rs`, lines(3)],
    [
      `${SRC}/providers/mod.rs`,
      [
        "#[cfg(all(test, feature = \"integration-tests\"))]",
        "#[path = \"shared/fixture.rs\"]",
        "mod fixture;",
        "#[cfg(test)]",
        "pub(crate) mod contract;",
        "#[cfg(any(test, feature = \"integration-tests\"))]",
        "pub mod mock;",
        "#[cfg(feature = \"integration-tests\")]",
        "pub mod harness;",
        "",
      ].join("\n"),
    ],
    [`${SRC}/providers/contract.rs`, lines(3)],
    [`${SRC}/providers/shared/fixture.rs`, lines(3)],
    [`${SRC}/providers/mock.rs`, lines(3)],
    [`${SRC}/providers/harness.rs`, lines(3)],
    [
      `${SRC}/providers/openai.rs`,
      ["#[cfg(test)]", "pub(crate) mod mock;", ""].join("\n"),
    ],
    [`${SRC}/providers/openai/mock.rs`, lines(3)],
    // The real tree mounts management_openapi/ this way (management/mod.rs).
    [
      `${SRC}/management/mod.rs`,
      ["#[cfg(test)]", "#[path = \"../contract_tree/mod.rs\"]", "pub(crate) mod openapi;", ""].join(
        "\n",
      ),
    ],
    [`${SRC}/contract_tree/mod.rs`, "pub(crate) mod vectors;\n"],
    [`${SRC}/contract_tree/vectors.rs`, lines(3)],
  ]);
  const isTestOnly = testOnlyModules(sources);
  const testOnly = [...sources.keys()].filter(isTestOnly).sort();
  assert.deepEqual(testOnly, [
    `${SRC}/authz_matrix.rs`,
    `${SRC}/contract_tree/mod.rs`,
    `${SRC}/contract_tree/vectors.rs`,
    `${SRC}/management_openapi/mod.rs`,
    `${SRC}/management_openapi/quality.rs`,
    `${SRC}/management_openapi/quality/deep.rs`,
    `${SRC}/providers/contract.rs`,
    `${SRC}/providers/openai/mock.rs`,
    `${SRC}/providers/shared/fixture.rs`,
  ]);
  // `any(test, feature)` and a bare feature gate also build without `test`.
  assert.deepEqual([...measure(sources).keys()].sort(), [
    `${SRC}/auth.rs`,
    `${SRC}/lib.rs`,
    `${SRC}/management/mod.rs`,
    `${SRC}/providers/harness.rs`,
    `${SRC}/providers/mock.rs`,
    `${SRC}/providers/mod.rs`,
    `${SRC}/providers/openai.rs`,
  ]);
});

test("measure skips test files", () => {
  const sources = new Map([
    [`${SRC}/a.rs`, lines(7)],
    [`${SRC}/a/tests.rs`, lines(5000)],
    [`${SRC}/a_tests.rs`, lines(5000)],
  ]);
  assert.deepEqual([...measure(sources)], [[`${SRC}/a.rs`, 7]]);
});

// --- counting production lines --------------------------------------------

test("a file without a test module counts whole", () => {
  assert.equal(productionLines(lines(12)), 12);
  assert.equal(productionLines("fn a() {}"), 1, "no trailing newline");
  assert.equal(productionLines(""), 0);
});

test("an inline #[cfg(test)] module is not counted", () => {
  const text = [
    "fn production() {}", // 1
    "", // 2
    "#[cfg(test)]",
    "mod tests {",
    "    #[test]",
    "    fn works() {",
    "        assert!(true);",
    "    }",
    "}",
    "",
  ].join("\n");
  assert.equal(productionLines(text), 2);
});

test("attributes and comments between the cfg and the mod do not hide the module", () => {
  const text = [
    "fn production() {}",
    "#[cfg(all(test, feature = \"integration-tests\"))]",
    "// database tests",
    "#[allow(clippy::unwrap_used)]",
    "pub(crate) mod db_tests {",
    "    fn helper() {}",
    "}",
    "",
  ].join("\n");
  assert.equal(productionLines(text), 1);
});

test("production code after a test module is still counted", () => {
  // Several files declare their test modules at the top (metrics.rs,
  // management/directory.rs). Cutting the file at the first one would hide
  // everything below it.
  const text = [
    "use std::fmt;", // 1
    "#[cfg(test)]",
    "mod webhook_tests;",
    "#[cfg(test)]",
    "mod semantic {",
    "    fn helper() {}",
    "}",
    lines(900).trimEnd(), // 900 more
    "#[cfg(test)]",
    "mod tests {",
    "    fn helper() {}",
    "}",
    "fn last() {}", // 1 more
    "",
  ].join("\n");
  assert.equal(productionLines(text), 902);
});

test("a #[cfg(test)] on something that is not a module does not cut", () => {
  const text = [
    "fn production() {}",
    "#[cfg(test)]",
    "fn test_helper() {",
    "    let x = 1;",
    "}",
    "#[cfg(test)]",
    "use std::fmt;",
    "#[cfg(test)]",
    "impl Thing {",
    "    fn only_in_tests(&self) {}",
    "}",
    "#[cfg(not(test))]",
    "mod real {",
    "    fn f() {}",
    "}",
    "#[cfg(any(test, feature = \"integration-tests\"))]",
    "mod shared {",
    "    fn f() {}",
    "}",
    "",
  ].join("\n");
  assert.equal(productionLines(text), 19);
});

test("a nested #[cfg(test)] is not a top-level test module", () => {
  const text = [
    "mod outer {",
    "    #[cfg(test)]",
    "    mod tests {",
    "        fn helper() {}",
    "    }",
    "}",
    "",
  ].join("\n");
  assert.equal(productionLines(text), 6);
});

test("a test module that never closes is refused, not counted as zero", () => {
  const text = ["fn production() {}", "#[cfg(test)]", "mod tests {", "    fn helper() {}", ""].join(
    "\n",
  );
  assert.throws(() => productionLines(text), /never closes/);
});

// --- files that escape the count -------------------------------------------

test("a file skipped by its name but declared by production code is refused", () => {
  const sources = new Map([
    [`${SRC}/lib.rs`, ["pub mod adv_tests;", "pub mod tests;", "pub mod real;", ""].join("\n")],
    [`${SRC}/adv_tests.rs`, lines(1650)],
    [`${SRC}/tests.rs`, lines(3)],
    [`${SRC}/real.rs`, ["#[cfg(test)]", "mod tests;", "#[cfg(test)]", "mod real_tests;", ""].join("\n")],
    [`${SRC}/real/tests.rs`, lines(3)],
    [`${SRC}/real/real_tests.rs`, lines(3)],
  ]);
  const errors = checkSources(sources);
  assert.equal(errors.length, 2, errors.join("\n"));
  assert.match(errors[0], /adv_tests\.rs/);
  assert.match(errors[0], /declared outside #\[cfg\(test\)\]/);
  assert.match(errors[0], /lib\.rs line 1/);
  assert.match(errors[0], /[Rr]ename it or move it/);
  assert.match(errors[0], /not measured/);
  assert.match(errors[1], /src\/tests\.rs/);
  assert.match(errors[1], /lib\.rs line 2/);
});

test("a production #[path] into a test-only directory is refused", () => {
  const sources = new Map([
    [
      `${SRC}/lib.rs`,
      [
        "#[cfg(test)]",
        "mod management_openapi;",
        "// Not a test: built into the binary.",
        "#[path = \"management_openapi/grown.rs\"]",
        "pub mod grown;",
        "pub mod dir;",
        "",
      ].join("\n"),
    ],
    [`${SRC}/management_openapi/mod.rs`, "mod grown;\n"],
    [`${SRC}/management_openapi/grown.rs`, lines(5000)],
    // A production `mod dir;` that resolves to a `mod.rs` under a `tests` directory.
    [`${SRC}/dir/mod.rs`, "#[path = \"../tests/helpers/mod.rs\"]\nmod helpers;\n"],
    [`${SRC}/tests/helpers/mod.rs`, lines(3)],
  ]);
  const errors = checkSources(sources);
  assert.equal(errors.length, 2, errors.join("\n"));
  assert.match(errors[0], /tests\/helpers\/mod\.rs/);
  assert.match(errors[0], /dir\/mod\.rs line 2/);
  assert.match(errors[1], /management_openapi\/grown\.rs/);
  assert.match(errors[1], /lib\.rs line 5/);
  assert.match(errors[1], /declared outside #\[cfg\(test\)\]/);
});

test("declarations inside test files and test modules are not production declarations", () => {
  const sources = new Map([
    [
      `${SRC}/lib.rs`,
      [
        "pub mod a;",
        "#[cfg(test)]",
        "mod tests {",
        "mod helper_tests;",
        "}",
        "#[cfg(any(test, feature = \"integration-tests\"))]",
        "pub mod mock;",
        "",
      ].join("\n"),
    ],
    [`${SRC}/a.rs`, "#[cfg(test)]\nmod tests;\n"],
    // A test file declares its own children without a cfg.
    [`${SRC}/a/tests.rs`, "mod db_tests;\nmod more;\n"],
    [`${SRC}/a/tests/db_tests.rs`, lines(3)],
    [`${SRC}/a/tests/more.rs`, lines(3)],
    [`${SRC}/helper_tests.rs`, lines(3)],
    [`${SRC}/mock.rs`, lines(3)],
  ]);
  assert.deepEqual(checkSources(sources), []);
});

test("rustfmt::skip in a measured file is refused", () => {
  // The counting relies on rustfmt's layout (top-level items at column 0).
  const sources = new Map([
    [`${SRC}/a.rs`, ["fn a() {}", "#[rustfmt::skip]", "fn b() {}", ""].join("\n")],
    [`${SRC}/b.rs`, "#![cfg_attr(rustfmt, rustfmt_skip)]\nfn b() {}\n"],
    [`${SRC}/c.rs`, ["fn c() {}", "#[cfg(test)]", "mod tests {", "    #[rustfmt::skip]", "    fn t() {}", "}", ""].join("\n")],
    [`${SRC}/ok.rs`, "fn ok() {}\n"],
    // Test files are not measured, so their layout does not matter.
    [`${SRC}/a/tests.rs`, "#[rustfmt::skip]\nfn t() {}\n"],
  ]);
  const errors = checkSources(sources);
  assert.equal(errors.length, 3, errors.join("\n"));
  assert.match(errors[0], /a\.rs line 2/);
  assert.match(errors[0], /rustfmt/);
  assert.match(errors[0], /Remove/);
  assert.match(errors[1], /b\.rs line 1/);
  assert.match(errors[2], /c\.rs line 4/);
});

test("a symlink under the sources is refused", (t) => {
  const dir = mkdtempSync(join(tmpdir(), "structure-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  mkdirSync(join(dir, "src/real"), { recursive: true });
  writeFileSync(join(dir, "src/lib.rs"), "pub mod real;\n");
  writeFileSync(join(dir, "src/real/mod.rs"), lines(3));
  assert.deepEqual([...readSources(dir, "src").keys()].sort(), ["src/lib.rs", "src/real/mod.rs"]);

  symlinkSync(join(dir, "src/real/mod.rs"), join(dir, "src/linked.rs"));
  assert.throws(() => readSources(dir, "src"), /src\/linked\.rs is a symbolic link.*not followed/s);
  rmSync(join(dir, "src/linked.rs"));
  symlinkSync(join(dir, "src/real"), join(dir, "src/alias"));
  assert.throws(() => readSources(dir, "src"), /src\/alias is a symbolic link/);
});

// --- the allowance file's raw text ------------------------------------------

test("duplicate keys in the allowance file are found in its raw text", () => {
  const clean = JSON.stringify({ limit: 800, files: { "a.rs": 900, "b.rs": 900 }, baseline: { "a.rs": 900, "b.rs": 900 } }, null, 2);
  assert.deepEqual(duplicateKeys(clean), []);
  // JSON.parse keeps the last of two equal keys and says nothing.
  const twice = '{\n  "limit": 800,\n  "files": {\n    "a.rs": 900,\n    "b.rs": 950,\n    "a.rs": 2000\n  },\n  "baseline": { "a.rs": 2000 }\n}\n';
  assert.equal(JSON.parse(twice).files["a.rs"], 2000);
  assert.deepEqual(duplicateKeys(twice), ['"files": "a.rs"']);
  // A whole section, or a top-level number, written twice.
  assert.deepEqual(
    duplicateKeys('{"markers": 3, "files": {"a.rs": 1}, "files": {"b.rs": 1}, "markers": 9}'),
    ['top level: "files"', 'top level: "markers"'],
  );
  // The same key in two sections is the normal case, and an escape does not hide a repeat.
  assert.deepEqual(duplicateKeys('{"baseline": {"a.rs": 1, "\\u0061.rs": 2}}'), ['"baseline": "a.rs"']);
  // Text inside a string is not a key.
  assert.deepEqual(duplicateKeys('{"comment": "\\"files\\": {\\"x\\": 1, \\"x\\": 2}", "files": {"x": 1}}'), []);
});

// --- the Clippy side ---------------------------------------------------------

const ONE = (lint, indent = "") => `${indent}#[cfg_attr(not(test), expect(clippy::${lint}, reason = "#499"))]`;
const BOTH = (indent = "") => [
  `${indent}#[cfg_attr(`,
  `${indent}    not(test),`,
  `${indent}    expect(clippy::too_many_lines, clippy::cognitive_complexity, reason = "#499")`,
  `${indent})]`,
];
// lib.rs and main.rs with the crate lines, and `n` markers in other files.
const lintSources = (extra = {}) =>
  new Map(
    Object.entries({
      [`${SRC}/lib.rs`]: ["#![deny(unsafe_code)]", CRATE_LINTS, "pub mod a;", ""].join("\n"),
      [`${SRC}/main.rs`]: [CRATE_LINTS, ONE("too_many_lines"), "fn main() {}", ""].join("\n"),
      [`${SRC}/a.rs`]: [
        "impl A {",
        ONE("cognitive_complexity", "    "),
        "    fn a() {}",
        ...BOTH("    "),
        "    fn b() {}",
        "}",
        ...BOTH(),
        "fn c() {}",
        // An error code that happens to share the lint's name is not a mention of the lint.
        "const CODE: &str = \"too_many_lines\";",
        "",
      ].join("\n"),
      ...extra,
    }),
  );
const marked = (markers, markersBaseline = markers) => ({ markers, markersBaseline });

test("the crate lint lines and the #499 markers pass as they landed", () => {
  assert.equal(
    CRATE_LINTS,
    "#![cfg_attr(not(test), warn(clippy::too_many_lines, clippy::cognitive_complexity))]",
  );
  assert.deepEqual(checkLints(lintSources(), marked(4)), []);
  // Fewer markers than the baseline is the normal path.
  assert.deepEqual(checkLints(lintSources(), marked(4, 58)), []);
});

test("both crate roots must turn the lints on", () => {
  for (const root of ["lib.rs", "main.rs"]) {
    for (const text of [
      "pub mod a;\n",
      // Commented out, weakened to one lint, or not at the start of a line.
      `// ${CRATE_LINTS}\n`,
      "#![cfg_attr(not(test), warn(clippy::too_many_lines))]\n",
      ` ${CRATE_LINTS}\n`,
    ]) {
      const errors = checkLints(lintSources({ [`${SRC}/${root}`]: text }), marked(root === "main.rs" ? 3 : 4));
      assert.ok(errors.length >= 1, text);
      assert.ok(errors.some((e) => e.includes(root) && /must contain the line/.test(e)), `${root}: ${errors.join("\n")}`);
    }
  }
  const missing = lintSources();
  missing.delete(`${SRC}/main.rs`);
  assert.match(checkLints(missing, marked(3)).join("\n"), /main\.rs.*must contain the line/);
});

test("a lint named anywhere but the crate lines and the markers is refused", () => {
  for (const [line, why] of [
    ["#[allow(clippy::too_many_lines)]", "allow"],
    ["#![allow(clippy::cognitive_complexity)]", "crate allow"],
    ["#[expect(clippy::too_many_lines)]", "no reason"],
    ["#[cfg_attr(not(test), expect(clippy::too_many_lines, reason = \"long\"))]", "another reason"],
    ["#[cfg_attr(not(test), allow(clippy::too_many_lines, reason = \"#499\"))]", "allow with the reason"],
    ["#[cfg_attr(not(test), expect(clippy::too_many_lines, clippy::unwrap_used, reason = \"#499\"))]", "another lint rides along"],
    ["#![warn(clippy::too_many_lines)]", "a second level"],
    [CRATE_LINTS, "the crate line outside lib.rs and main.rs"],
    ["// see clippy::cognitive_complexity", "a comment"],
  ]) {
    const errors = checkLints(lintSources({ [`${SRC}/b.rs`]: `${line}\nfn b() {}\n` }), marked(4));
    assert.equal(errors.length, 1, `${why}: ${errors.join("\n")}`);
    assert.match(errors[0], /b\.rs line 1/, why);
    assert.match(errors[0], /never `allow`/, why);
  }
});

test("the multi-line marker must be the whole attribute rustfmt writes", () => {
  const broken = (edit) => {
    const attr = BOTH();
    edit(attr);
    return checkLints(lintSources({ [`${SRC}/b.rs`]: [...attr, "fn b() {}", ""].join("\n") }), marked(4));
  };
  assert.deepEqual(checkLints(lintSources({ [`${SRC}/b.rs`]: [...BOTH(), "fn b() {}", ""].join("\n") }), marked(5)), []);
  for (const edit of [
    (attr) => (attr[0] = "#[allow("),
    (attr) => (attr[1] = "    test,"),
    (attr) => (attr[1] = "    all(),"),
    (attr) => (attr[3] = ", allow(clippy::too_many_lines))]"),
    (attr) => (attr[2] = "    allow(clippy::too_many_lines, clippy::cognitive_complexity, reason = \"#499\")"),
  ]) {
    const errors = broken(edit);
    assert.equal(errors.length >= 1, true);
    assert.match(errors[0], /b\.rs line/);
  }
});

test("a #499 reason on anything else is not a marker", () => {
  const errors = checkLints(
    lintSources({ [`${SRC}/b.rs`]: "#[expect(clippy::too_many_arguments, reason = \"#499\")]\nfn b() {}\n" }),
    marked(4),
  );
  assert.equal(errors.length, 1, errors.join("\n"));
  assert.match(errors[0], /b\.rs line 1/);
  assert.match(errors[0], /reason = "#499"/);
});

test("the marker count must equal \"markers\", which only goes down", () => {
  const added = checkLints(lintSources({ [`${SRC}/b.rs`]: `${ONE("too_many_lines")}\nfn b() {}\n` }), marked(4));
  assert.equal(added.length, 1, added.join("\n"));
  assert.match(added[0], /5 markers/);
  assert.match(added[0], /"markers" is 4/);
  assert.match(added[0], /Never add a marker/);
  assert.match(added[0], /b\.rs/, "names the files that hold markers");

  const removed = checkLints(lintSources(), marked(5));
  assert.equal(removed.length, 1, removed.join("\n"));
  assert.match(removed[0], /4 markers/);
  assert.match(removed[0], /Lower "markers" to 4/);

  // Raising both the code and the number is refused by the baseline.
  const raised = checkLints(lintSources(), marked(4, 3));
  assert.equal(raised.length, 1, raised.join("\n"));
  assert.match(raised[0], /"markers" \(4\) is above "markersBaseline" \(3\)/);
  assert.match(raised[0], /Never add a marker/);

  for (const bad of [{}, { markers: "4", markersBaseline: 4 }, { markers: 4 }, { markers: -1, markersBaseline: 4 }, null]) {
    assert.match(checkLints(lintSources(), bad)[0], /must be whole numbers/);
  }
});

test("a Clippy threshold setting is refused", () => {
  assert.deepEqual(checkClippyConfig("apps/gateway/clippy.toml", "disallowed-methods = []\n"), []);
  for (const line of [
    "too-many-lines-threshold = 1000",
    "cognitive-complexity-threshold = 99",
    "too_many_lines_threshold = 1000",
    "cognitive_complexity_threshold=99",
  ]) {
    const errors = checkClippyConfig("apps/gateway/clippy.toml", `disallowed-methods = []\n${line}\n`);
    assert.equal(errors.length, 1, line);
    assert.match(errors[0], /apps\/gateway\/clippy\.toml line 2/);
    assert.match(errors[0], /Remove it/);
  }
});

// --- the rules --------------------------------------------------------------

const BIG = `${SRC}/big.rs`;
const OTHER = `${SRC}/other.rs`;

test("a clean list passes", () => {
  const errors = checkAllowance(
    allowance({ [BIG]: 1200, [OTHER]: 900 }),
    sizes({ [BIG]: 1200, [OTHER]: 860, [`${SRC}/small.rs`]: 800 }),
  );
  assert.deepEqual(errors, []);
});

test("rule 1: a new large file that is not listed fails", () => {
  const errors = checkAllowance(allowance({}), sizes({ [BIG]: LIMIT + 1 }));
  assert.equal(errors.length, 1);
  assert.match(errors[0], /big\.rs/);
  assert.match(errors[0], /801 production lines/);
  assert.match(errors[0], /limit is 800/);
  assert.match(errors[0], /Split/);
  // Exactly at the limit is allowed.
  assert.deepEqual(checkAllowance(allowance({}), sizes({ [BIG]: LIMIT })), []);
});

test("rule 1: a listed file that grows past its ceiling fails", () => {
  const errors = checkAllowance(allowance({ [BIG]: 1200 }), sizes({ [BIG]: 1201 }));
  assert.equal(errors.length, 1);
  assert.match(errors[0], /big\.rs/);
  assert.match(errors[0], /1201 production lines/);
  assert.match(errors[0], /ceiling is 1200/);
  assert.match(errors[0], /Do not raise the ceiling/);
});

test("rule 2: a ceiling more than the slack above the real size fails", () => {
  const ok = checkAllowance(allowance({ [BIG]: 1200 }), sizes({ [BIG]: 1200 - SLACK }));
  assert.deepEqual(ok, []);
  const errors = checkAllowance(allowance({ [BIG]: 1200 }), sizes({ [BIG]: 1200 - SLACK - 1 }));
  assert.equal(errors.length, 1);
  assert.match(errors[0], /big\.rs/);
  assert.match(errors[0], /ceiling 1200/);
  assert.match(errors[0], /1149 production lines/);
  assert.match(errors[0], /Lower the ceiling to 1149/);
});

test("rule 3: a listed file at or under the limit must be removed", () => {
  const errors = checkAllowance(allowance({ [BIG]: 820 }), sizes({ [BIG]: LIMIT }));
  assert.equal(errors.length, 1);
  assert.match(errors[0], /big\.rs/);
  assert.match(errors[0], /800 production lines/);
  assert.match(errors[0], /Remove it from "files" and from "baseline"/);
});

test("rule 3: a listed file that no longer exists must be removed", () => {
  const errors = checkAllowance(allowance({ [BIG]: 1200 }), sizes({}));
  assert.equal(errors.length, 1);
  assert.match(errors[0], /big\.rs/);
  assert.match(errors[0], /no longer exists/);
  assert.match(errors[0], /Remove it from "files" and from "baseline"/);
});

test("rule 4: a ceiling above the baseline fails", () => {
  const errors = checkAllowance(
    allowance({ [BIG]: 1250 }, { [BIG]: 1200 }),
    sizes({ [BIG]: 1250 }),
  );
  assert.equal(errors.length, 1);
  assert.match(errors[0], /big\.rs/);
  assert.match(errors[0], /ceiling 1250/);
  assert.match(errors[0], /baseline 1200/);
  assert.match(errors[0], /only shrink/);
});

test("rule 4: a path that is not in the baseline cannot be added", () => {
  const errors = checkAllowance(
    allowance({ [BIG]: 1200, [OTHER]: 900 }, { [BIG]: 1200 }),
    sizes({ [BIG]: 1200, [OTHER]: 900 }),
  );
  assert.equal(errors.length, 1);
  assert.match(errors[0], /other\.rs/);
  assert.match(errors[0], /not in "baseline"/);
  assert.match(errors[0], /Split/);
});

test("rule 4: a baseline entry without a files entry must be deleted", () => {
  // Otherwise a file taken off the list could be put back later.
  const errors = checkAllowance(
    allowance({}, { [BIG]: 1200 }),
    sizes({ [BIG]: 700 }),
  );
  assert.equal(errors.length, 1);
  assert.match(errors[0], /big\.rs/);
  assert.match(errors[0], /Delete it from "baseline"/);
});

test("lowering a ceiling below the baseline is the normal path", () => {
  const errors = checkAllowance(
    allowance({ [BIG]: 1000 }, { [BIG]: 1200 }),
    sizes({ [BIG]: 990 }),
  );
  assert.deepEqual(errors, []);
});

test("the limit, the shape and the order of the list are fixed", () => {
  const good = sizes({ [BIG]: 1200, [OTHER]: 900 });
  assert.match(
    checkAllowance({ limit: 900, files: {}, baseline: {} }, sizes({}))[0],
    /"limit" must stay 800/,
  );
  assert.match(
    checkAllowance(allowance({ [OTHER]: 900, [BIG]: 1200 }, { [BIG]: 1200, [OTHER]: 900 }), good)[0],
    /"files" must be sorted by path/,
  );
  assert.match(
    checkAllowance(allowance({ [BIG]: 1200, [OTHER]: 900 }, { [OTHER]: 900, [BIG]: 1200 }), good)[0],
    /"baseline" must be sorted by path/,
  );
  assert.match(
    checkAllowance(allowance({ [BIG]: "1200" }), sizes({ [BIG]: 1200 }))[0],
    /must be a whole number/,
  );
  assert.match(checkAllowance({ limit: LIMIT, files: {} }, sizes({}))[0], /"baseline"/);
});

test("every problem is reported, not only the first", () => {
  const errors = checkAllowance(
    allowance({ [BIG]: 1200 }),
    sizes({ [BIG]: 1300, [OTHER]: 2000 }),
  );
  assert.equal(errors.length, 2);
});

// --- the real tree ----------------------------------------------------------

test("constants match the documented rule", () => {
  assert.equal(LIMIT, 800);
  assert.equal(SLACK, 50);
});

test("the gateway sources satisfy the allowance list", () => {
  const sources = readSources(root, SRC);
  assert.ok(sources.size > 100, `only ${sources.size} files found under ${SRC}`);
  assert.ok(sources.has(`${SRC}/lib.rs`) && sources.has(`${SRC}/main.rs`));
  const measured = measure(sources);
  assert.ok(measured.size > 50, `only ${measured.size} production files found`);
  // Known test-only trees must not be measured as production.
  assert.equal(measured.has(`${SRC}/management_openapi/http_vectors.rs`), false);
  assert.equal(measured.has(`${SRC}/management/tests.rs`), false);

  const raw = readFileSync(join(root, "tests/structure-allowance.json"), "utf8");
  assert.deepEqual(duplicateKeys(raw), [], "tests/structure-allowance.json repeats a key; JSON keeps only the last one. Delete the repeat.");
  const list = JSON.parse(raw);
  const errors = [...checkSources(sources), ...checkAllowance(list, measured)];
  assert.deepEqual(errors, [], `\n${errors.join("\n\n")}\n`);
});

test("the gateway's Clippy ratchet is switched on and its markers are counted", () => {
  const list = JSON.parse(readFileSync(join(root, "tests/structure-allowance.json"), "utf8"));
  const errors = checkLints(readSources(root, SRC), list);
  // A threshold setting would move the lints' limits. Clippy reads the file nearest the
  // crate, so the workspace root is looked at too, should one ever appear there.
  const configs = ["apps/gateway/clippy.toml", "apps/gateway/.clippy.toml", "clippy.toml", ".clippy.toml"];
  assert.ok(existsSync(join(root, configs[0])), `${configs[0]} is missing`);
  for (const config of configs) {
    if (existsSync(join(root, config))) {
      errors.push(...checkClippyConfig(config, readFileSync(join(root, config), "utf8")));
    }
  }
  assert.deepEqual(errors, [], `\n${errors.join("\n\n")}\n`);
});
