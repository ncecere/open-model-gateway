import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

/*
 * Copy-and-own integrity for vendored Bitop files. `bitop-lock.json` is
 * written by the Bitop CLI and records the SHA-256 of every file it
 * installed, so an edit made here (or a partial refresh) fails this test
 * until it is reconciled upstream or re-installed. See docs/bitop-ui.md.
 */
const web = new URL("../../", import.meta.url);
const lock = JSON.parse(readFileSync(new URL("bitop-lock.json", web), "utf8")) as {
  version: number;
  items: Record<string, { files: Record<string, string> }>;
};
const files = Object.entries(lock.items).flatMap(([item, entry]) =>
  Object.entries(entry.files).map(([target, sha256]) => ({ item, target, sha256 })),
);

describe("Bitop copy-and-own lock", () => {
  it("records the real checkbox primitives and the shared core", () => {
    expect(lock.version).toBe(1);
    expect(Object.keys(lock.items)).toEqual(expect.arrayContaining(["core", "checkbox", "command-palette", "stat-card"]));
    expect(files.some((file) => file.target.endsWith("checkbox/checkbox.tsx"))).toBe(true);
    expect(files.some((file) => file.target.endsWith("checkbox/checkbox.module.css"))).toBe(true);
  });

  it("keeps the upstream MIT license notice with the vendored components", () => {
    const license = readFileSync(new URL("src/components/ui/LICENSE", web), "utf8");
    expect(license.startsWith("MIT License\n")).toBe(true);
    expect(license).toContain("Copyright (c) 2026 Nicholas Cecere");
  });

  it.each(files)("$target matches the hash recorded for $item", (file) => {
    expect(file.target).toMatch(/^src\//);
    expect(file.target).not.toContain("..");
    const bytes = readFileSync(fileURLToPath(new URL(file.target, web)));
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(file.sha256);
  });
});
