// Structure ratchet for the gateway crate (#500, first step of #499): a file
// may not grow past the limit, and a file already past it may not grow at all.
// The rules and the counting live in tests/structure.mjs; the allowance list is
// tests/structure-allowance.json. docs/verification.md describes both.
//
// The first tests exercise each rule on in-memory fixtures. The last ones apply
// the rules to the real tree.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import {
  LIMIT,
  SLACK,
  checkAllowance,
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

  const list = JSON.parse(readFileSync(join(root, "tests/structure-allowance.json"), "utf8"));
  const errors = checkAllowance(list, measured);
  assert.deepEqual(errors, [], `\n${errors.join("\n\n")}\n`);
});
