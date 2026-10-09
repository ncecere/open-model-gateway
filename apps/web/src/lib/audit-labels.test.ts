import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { auditActionLabels } from "./people";

/*
 * Every audit code the gateway can emit has a short human label (the code
 * itself stays in the row's tooltip and "Copy event code"). The codes are read
 * from the Rust sources: the action argument of every `audit(...)` call and the
 * literal action of every direct `INSERT INTO audit_events`.
 */
const gatewaySrc = fileURLToPath(new URL("../../../gateway/src/", import.meta.url));
const CODE = /^[a-z_]+(?:\.[a-z_]+)+$/;

function rustFiles(dir: string): string[] {
  return readdirSync(dir).flatMap(name => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return rustFiles(path);
    return name.endsWith(".rs") && !/tests?\.rs$|_tests\.rs$/.test(name) ? [path] : [];
  });
}
/** The text between `(` at `start` and its matching `)`, skipping string literals. */
function callArgs(source: string, start: number): string {
  let depth = 1, i = start;
  while (depth && i < source.length) {
    const c = source[i];
    if (c === "(") depth++;
    else if (c === ")") depth--;
    else if (c === "\"") { i++; while (i < source.length && source[i] !== "\"") { if (source[i] === "\\") i++; i++; } }
    i++;
  }
  return source.slice(start, i);
}
function emittedAuditCodes(): Set<string> {
  const codes = new Set<string>();
  for (const file of rustFiles(gatewaySrc)) {
    const source = readFileSync(file, "utf8");
    for (const m of source.matchAll(/\baudit\(/g)) {
      if (source.slice(Math.max(0, m.index - 3), m.index) === "fn ") continue;
      for (const lit of callArgs(source, m.index + m[0].length).matchAll(/"([^"\\]*)"/g)) if (CODE.test(lit[1]!)) codes.add(lit[1]!);
    }
    for (const m of source.matchAll(/INSERT INTO audit_events\([^)]*\)\s*(?:VALUES|SELECT)([^"]{0,300})/g)) {
      for (const lit of m[1]!.matchAll(/'([^']*)'/g)) if (CODE.test(lit[1]!)) codes.add(lit[1]!);
    }
  }
  return codes;
}

describe("audit event labels", () => {
  it("labels every audit code the gateway emits", () => {
    const codes = emittedAuditCodes();
    // The scan itself must find the codes (guards against a silently broken extractor).
    for (const known of ["key.created", "file.uploaded", "alert_rule.created", "scim.user.deactivated", "user.bootstrap_role", "usage.reconciled", "settings.storage_test"]) expect(codes).toContain(known);
    expect(codes.size).toBeGreaterThan(60);
    const missing = [...codes].filter(code => !auditActionLabels[code]).sort();
    expect(missing).toEqual([]);
  });
  it("uses short labels, not codes", () => {
    for (const [code, label] of Object.entries(auditActionLabels)) {
      expect(label, code).not.toMatch(/[._]/);
      expect(label.length, code).toBeLessThanOrEqual(40);
    }
  });
});
