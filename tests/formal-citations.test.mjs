import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const readme = new URL("../spec/formal/README.md", import.meta.url);

// The README is the inventory. Code-labelled links quote the source, and links
// into the gateway or deployment tree must carry a code label.
function checkCitations(markdown, readSource) {
  let count = 0;
  for (const [, label, target] of markdown.matchAll(/\[([^\]\n]*)\]\(([^)\n]*)\)/g)) {
    if (!label.startsWith("`") && !/^\.\.\/\.\.\/(apps|deploy)\//.test(target)) continue;
    const token = label.match(/^`([^`\n]+)`$/)?.[1];
    const anchor = target.match(/^(\.\.\/\.\.\/(?:[\w-]+\/)+[\w-]+\.(?:rs|sql))#L([1-9]\d*)$/);
    assert.ok(token && anchor, `invalid citation: [${label}](${target})`);
    const [, path, number] = anchor;
    const line = readSource(path).split(/\r?\n/)[Number(number) - 1];
    assert.ok(line?.includes(token), `${target} should contain ${token}`);
    count++;
  }
  assert.ok(count, "no citations in formal README");
}

const fixture = "[`FOR UPDATE OF r`](../../apps/gateway/src/governance/settlement.rs#L2)";

test("formal citation checks the exact line even when the guard occurs elsewhere", () => {
  checkCitations(fixture, () => "\nSELECT ... FOR UPDATE OF r");
  assert.throws(
    () => checkCitations(fixture, () => "SELECT ... FOR UPDATE OF r\nSELECT ..."),
    /#L2 should contain FOR UPDATE OF r/,
  );
});

test("empty inventories and malformed source citations fail closed", () => {
  assert.throws(() => checkCitations("", () => ""), /no citations/);
  for (const malformed of [fixture.replace("#L2", "#2"), fixture.replaceAll("`", "")]) {
    assert.throws(
      () => checkCitations(fixture + "\n" + malformed, () => "\nFOR UPDATE OF r"),
      /invalid citation/,
    );
  }
});

test("formal model README cites current implementation guards", () => {
  checkCitations(readFileSync(readme, "utf8"), path => readFileSync(new URL(path, readme), "utf8"));
});
