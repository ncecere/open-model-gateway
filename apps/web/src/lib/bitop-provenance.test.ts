import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const root = new URL("../../../../", import.meta.url);
const manifest = JSON.parse(readFileSync(new URL("apps/web/bitop-provenance.json", root), "utf8")) as {
  selected: string[];
  files: { source: string; target: string; copiedSha256: string; sourceSha256: string; patches?: string[] }[];
};
describe("Bitop copy-and-own provenance", () => {
  it("records the real checkbox primitives and unchanged upstream license", () => {
    expect(manifest.selected).toContain("checkbox");
    expect(manifest.files.some((file) => file.target.endsWith("checkbox/checkbox.tsx"))).toBe(true);
    expect(manifest.files.some((file) => file.target.endsWith("checkbox/checkbox.module.css"))).toBe(true);
    const license = manifest.files.find((file) => file.source === "LICENSE");
    expect(license).toBeDefined();
    expect(license?.copiedSha256).toBe(license?.sourceSha256);
  });
  it.each(manifest.files)("matches the recorded copied hash: $target", (file) => {
    expect(file.target).toMatch(/^apps\/web\/src\//);
    expect(file.target).not.toContain("..");
    const bytes = readFileSync(fileURLToPath(new URL(file.target, root)));
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(file.copiedSha256);
  });
});
