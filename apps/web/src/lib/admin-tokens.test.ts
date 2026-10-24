import { describe, expect, it } from "vitest";
import {
  adminTokenCidrError, adminTokenExpiryDefault, adminTokenExpiryError, adminTokenNameError,
  adminTokenPill, adminTokenScopes, adminTokenScopesError, adminTokenStatus, canRevokeAdminToken,
  parseCidrAllowlist, scopeLabel,
} from "./admin-tokens";

const now = Date.parse("2026-10-23T00:00:00Z");

describe("admin token status", () => {
  it("is active while unrevoked and unexpired", () => {
    expect(adminTokenStatus({ revoked_at: null, expires_at: "2026-11-01T00:00:00Z" }, now)).toBe("active");
  });
  it("is expired once expires_at has passed, even without revocation", () => {
    expect(adminTokenStatus({ revoked_at: null, expires_at: "2026-10-22T00:00:00Z" }, now)).toBe("expired");
  });
  it("is revoked when revoked, even if still unexpired, and revocation outranks expiry", () => {
    expect(adminTokenStatus({ revoked_at: "2026-10-20T00:00:00Z", expires_at: "2026-11-01T00:00:00Z" }, now)).toBe("revoked");
    expect(adminTokenStatus({ revoked_at: "2026-10-20T00:00:00Z", expires_at: "2026-10-01T00:00:00Z" }, now)).toBe("revoked");
  });
  it("treats the exact expiry instant as already expired", () => {
    expect(adminTokenStatus({ revoked_at: null, expires_at: "2026-10-23T00:00:00Z" }, now)).toBe("expired");
  });
  it("maps expired to the permanent (revoked) pill tone with its own label", () => {
    expect(adminTokenPill("expired")).toEqual({ status: "revoked", label: "Expired" });
    expect(adminTokenPill("active")).toEqual({ status: "active" });
    expect(adminTokenPill("revoked")).toEqual({ status: "revoked" });
  });
  it("never offers revoke for an already-revoked token, but does for active and expired", () => {
    expect(canRevokeAdminToken("revoked")).toBe(false);
    expect(canRevokeAdminToken("active")).toBe(true);
    expect(canRevokeAdminToken("expired")).toBe(true);
  });
});

describe("admin token scopes", () => {
  it("lists exactly the five approved write-does-not-imply-read scopes", () => {
    expect(adminTokenScopes.map(s => s.value)).toEqual(["platform:read", "catalog:write", "access:write", "governance:write", "settings:write"]);
  });
  it("names access and settings as highly privileged in their help text", () => {
    const access = adminTokenScopes.find(s => s.value === "access:write")!, settings = adminTokenScopes.find(s => s.value === "settings:write")!;
    expect(access.description.toLowerCase()).toContain("privileged");
    expect(settings.description.toLowerCase()).toContain("privileged");
  });
  it("resolves a short label for a known scope and echoes an unknown one back", () => {
    expect(scopeLabel("catalog:write")).toBe("Catalog");
    expect(scopeLabel("unknown:scope")).toBe("unknown:scope");
  });
  it("requires at least one scope but accepts any non-empty selection", () => {
    expect(adminTokenScopesError([])).toBe("Select at least one scope.");
    expect(adminTokenScopesError(["platform:read"])).toBeUndefined();
  });
});

describe("admin token creation field validation", () => {
  it("defaults the suggested expiry to 30 days", () => { expect(adminTokenExpiryDefault).toBe(30); });
  it.each(["1", "30", "90"])("accepts %s days", value => expect(adminTokenExpiryError(value)).toBeUndefined());
  it.each(["0", "91", "-1", "", "abc", "30.5", "1000"])("rejects %s days", value => expect(adminTokenExpiryError(value)).toBeDefined());
  it("rejects a missing name", () => { expect(adminTokenNameError("")).toBeDefined(); expect(adminTokenNameError("   ")).toBeDefined(); });
  it("rejects a name over 120 bytes but accepts exactly 120", () => {
    expect(adminTokenNameError("x".repeat(120))).toBeUndefined();
    expect(adminTokenNameError("x".repeat(121))).toBeDefined();
  });
  it("trims a valid name", () => { expect(adminTokenNameError("  CI publisher  ")).toBeUndefined(); });
});

describe("CIDR allowlist textarea (no second network parser; the server is authoritative)", () => {
  it("omits the allowlist entirely when the field is blank", () => {
    expect(parseCidrAllowlist("")).toBeUndefined();
    expect(parseCidrAllowlist("   \n  \n")).toBeUndefined();
  });
  it("splits on newlines and commas and trims each entry", () => {
    expect(parseCidrAllowlist("10.0.0.0/8\n192.168.1.0/24")).toEqual(["10.0.0.0/8", "192.168.1.0/24"]);
    expect(parseCidrAllowlist(" 10.0.0.0/8 , 192.168.1.0/24 ")).toEqual(["10.0.0.0/8", "192.168.1.0/24"]);
  });
  it("deduplicates repeated entries without reordering first occurrences", () => {
    expect(parseCidrAllowlist("10.0.0.0/8\n10.0.0.0/8\n2001:db8::/32")).toEqual(["10.0.0.0/8", "2001:db8::/32"]);
  });
  it("drops blank lines from mixed separators", () => {
    expect(parseCidrAllowlist("10.0.0.0/8,,\n\n192.168.1.0/24")).toEqual(["10.0.0.0/8", "192.168.1.0/24"]);
  });
  it("does not validate CIDR syntax itself (the server rejects malformed entries)", () => {
    expect(parseCidrAllowlist("not-a-cidr")).toEqual(["not-a-cidr"]);
  });
  it("flags more than 32 entries without re-implementing network validation", () => {
    const many = Array.from({ length: 33 }, (_, i) => `10.0.${i}.0/24`).join("\n");
    expect(adminTokenCidrError(many)).toBeDefined();
    const ok = Array.from({ length: 32 }, (_, i) => `10.0.${i}.0/24`).join("\n");
    expect(adminTokenCidrError(ok)).toBeUndefined();
    expect(adminTokenCidrError("")).toBeUndefined();
  });
});
