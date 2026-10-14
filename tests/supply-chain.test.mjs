// Supply-chain pins (#143, SECURITY.md): every GitHub Action is pinned to a full
// commit SHA with a version comment, and every Dockerfile base image to a digest.
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const root = fileURLToPath(new URL("..", import.meta.url));
const workflows = join(root, ".github/workflows");

test("every workflow action is pinned to a commit SHA with a version comment", () => {
  const unpinned = [];
  for (const file of readdirSync(workflows).filter(f => /\.ya?ml$/.test(f))) {
    readFileSync(join(workflows, file), "utf8").split("\n").forEach((line, i) => {
      const m = line.match(/^\s*(?:-\s*)?uses:\s*(\S+)(.*)$/);
      if (!m || m[1].startsWith("./")) return; // local reusable workflows
      if (!/@[0-9a-f]{40}$/.test(m[1]) || !/#\s*\S+/.test(m[2])) unpinned.push(`${file}:${i + 1} ${m[1]}`);
    });
  }
  assert.deepEqual(unpinned, []);
});

test("every Dockerfile base image is pinned by digest", () => {
  const unpinned = [];
  for (const file of ["Dockerfile", "deploy/loadtest/Dockerfile"]) {
    readFileSync(join(root, file), "utf8").split("\n").forEach((line, i) => {
      const m = line.match(/^FROM\s+(\S+)/i);
      if (m && !/@sha256:[0-9a-f]{64}$/.test(m[1])) unpinned.push(`${file}:${i + 1} ${m[1]}`);
    });
  }
  assert.deepEqual(unpinned, []);
});
