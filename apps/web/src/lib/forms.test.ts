import { describe, expect, it } from "vitest";
import { validateFields, expiryField, nameField, roleField, uuidError, slugError, type Field } from "./forms";
import { referenceField } from "../pages/catalog";

describe("form validation", () => {
  it("requires non-whitespace names", () => {
    expect(validateFields([nameField], { name: "   " })).toHaveProperty("name");
    expect(validateFields([nameField], { name: "Production" })).toEqual({});
    expect(validateFields([nameField], { name: "x".repeat(121) })).toHaveProperty("name");
  });
  it.each(["", "0", "366", "1.5", "NaN", "Infinity", "-1"])("rejects invalid expiry %s", (expires_in_days) => {
    expect(validateFields([expiryField], { expires_in_days })).toHaveProperty("expires_in_days");
  });
  it.each(["1", "30", "365"])("accepts bounded whole-day expiry %s", (expires_in_days) => {
    expect(validateFields([expiryField], { expires_in_days })).toEqual({});
  });
  it("rejects unsupported role selections", () => {
    expect(validateFields([roleField], { role: "operator" })).toHaveProperty("role");
    expect(validateFields([roleField], { role: "owner" })).toEqual({});
  });
  it("validates email, slugs, UUIDs and optional values", () => {
    const email: Field = { name: "email", label: "Email", type: "email", required: true };
    expect(validateFields([email], { email: "person@example.org" })).toEqual({});
    expect(validateFields([email], { email: "person@" })).toHaveProperty("email");
    expect(uuidError("00000000-0000-0000-0000-000000000001")).toBeUndefined();
    expect(uuidError("user@example.org")).toBeDefined();
    expect(slugError("my-team-2")).toBeUndefined();
    expect(slugError("My Team")).toBeDefined();
    expect(validateFields([{ name: "optional", label: "Optional", validate: () => "Invalid" }], {})).toEqual({});
  });
  it("accepts only credential variable names, not plaintext keys or credential URLs", () => {
    expect(validateFields([referenceField], { credential_variable: "OPENAI_API_KEY" })).toEqual({});
    for (const credential_variable of ["sk-real-secret", "env:OPENAI_API_KEY", "https://vault/secret", "secret with spaces"]) {
      expect(validateFields([referenceField], { credential_variable })).toHaveProperty("credential_variable");
    }
  });
  it("does not require hidden fields and passes values for provider-specific validation", () => {
    const field: Field = { name: "region", label: "Region", required: true, visibleWhen: (v) => v.provider === "bedrock", validate: (_region, v) => v.provider === "bedrock" ? undefined : "Wrong provider" };
    expect(validateFields([field], { provider: "openai" })).toEqual({});
    expect(validateFields([field], { provider: "bedrock" })).toHaveProperty("region");
    expect(validateFields([field], { provider: "bedrock", region: "us-east-1" })).toEqual({});
  });
});
