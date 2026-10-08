import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { AuthRequired, loginHref, takeSignedOutNotice } from "../pages/home";

describe("verified browser access denial", () => {
  it("explains lack of access without identity or callback details and allows another sign-in", () => {
    const html = renderToStaticMarkup(<AuthRequired denied refresh={() => {}} />);
    expect(html).toContain("No access yet");
    expect(html).toContain("Your account doesn&#x27;t have access yet. Ask a platform admin to add you.");
    expect(html).not.toContain("entitlement");
    expect(html).toContain('href="/api/v1/auth/login"');
    expect(html).not.toContain("Request verification failed");
    expect(html).not.toContain("oauth_state");
  });
  it("does not imply a denial on an ordinary signed-out landing", () => {
    expect(renderToStaticMarkup(<AuthRequired refresh={() => {}} />)).not.toContain("No access yet");
  });
  it("returns to the page you started from after signing in (deep link)", () => {
    expect(loginHref({ pathname: "/workspaces/w1/keys", search: "?status=all" })).toBe("/api/v1/auth/login?return_to=%2Fworkspaces%2Fw1%2Fkeys%3Fstatus%3Dall");
    expect(loginHref({ pathname: "/", search: "?auth_error=access_denied" })).toBe("/api/v1/auth/login");
    expect(loginHref({ pathname: "/home", search: "?auth_error=access_denied" })).toBe("/api/v1/auth/login?return_to=%2Fhome");
    expect(loginHref({ pathname: "//evil.example", search: "" })).toBe("/api/v1/auth/login");
  });
  it("shows \"You're signed out\" once after signing out (review #49)", () => {
    const store = new Map<string, string>([["omg.enterprise.signedOut", "1"]]), storage = { getItem: (k: string) => store.get(k) ?? null, removeItem: (k: string) => { store.delete(k); } };
    expect(takeSignedOutNotice(storage)).toBe(true); expect(takeSignedOutNotice(storage)).toBe(false);
  });
});
