import { describe, expect, it } from "vitest";
import { LegalHolds, holdsPath, referenceField, type LegalHold } from "../pages/legal-holds";
import { UserDetail } from "../pages/hierarchy";
import { admin, auditor, markup } from "../lib/test-fixtures";

const hold = (h: Partial<LegalHold>): LegalHold => ({ id: "h1", target: { kind: "user", id: "u1", label: "Pat Doe" }, reference: "CASE-2026-0042", created_at: "2026-10-14T00:00:00Z", created_by: "Admin", released_at: null, ...h });

describe("Legal holds (#75)", () => {
  it("lists active holds with their target and lets only admins release them", () => {
    const data = { data: [hold({}), hold({ id: "h2", target: { kind: "workspace", id: "w1", label: "Product" }, reference: "LEGAL-7" })] };
    const html = markup(<LegalHolds session={admin} />, [[holdsPath, data]]);
    expect(html).toContain("CASE-2026-0042");
    expect(html).toContain("Pat Doe");
    expect(html).toContain("Product");
    expect(html).toContain("Release CASE-2026-0042");
    const read = markup(<LegalHolds session={auditor} />, [[holdsPath, data]]);
    expect(read).toContain("CASE-2026-0042");
    expect(read).not.toContain("Release CASE-2026-0042");
  });
  it("accepts reference codes only, never names or descriptions", () => {
    expect(referenceField.validate?.("CASE-2026-0042", {})).toBeUndefined();
    expect(referenceField.validate?.("jira:LEGAL/17", {})).toBeUndefined();
    expect(referenceField.validate?.("Pat Doe harassment complaint", {})).toBeTruthy();
  });
  it("shows a person's active holds on their record", () => {
    const uid = "11111111-1111-4111-8111-111111111111";
    const detail = { id: uid, email: "pat@example.invalid", display_name: "Pat Doe", platform_role: "user", disabled_at: null, cleaned_at: null, cleanup_due_at: null, created_at: "2026-10-01T00:00:00Z", last_sign_in_at: null, first_sign_in_at: null, role_grants: [], shared_memberships: [], legal_holds: 2 };
    const html = markup(<UserDetail session={admin} id={uid} onTabChange={() => {}} />, [[`/api/v1/platform/users/${uid}`, detail]]);
    expect(html).toContain("Legal holds");
    expect(html).toContain("2 active");
  });
});

