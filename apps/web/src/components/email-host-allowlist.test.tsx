import { describe, expect, it } from "vitest";
import { EmailSettingsPage } from "../pages/settings/email";
import { deliveryErrors, settingsPath, type EmailSettings } from "../lib/settings";
import { admin, markup } from "../lib/test-fixtures";

const settings = (status: EmailSettings["status"], host_allowed: boolean): EmailSettings => ({
  configured: true, status, host: "smtp.example.org", port: 587, tls: "starttls", username: null, password_ref: null, password_ref_allowed: null, host_allowed,
  from_address: "gateway@example.org", from_name: null, public_url_configured: true, last_test: null, updated_at: "2026-10-14T00:00:00Z",
});

describe("SMTP relay host allowlist (#168)", () => {
  it("explains an unapproved relay host and names the server setting", () => {
    const html = markup(<EmailSettingsPage session={admin} />, [[`${settingsPath}/email`, settings("host_not_allowed", false)]]);
    expect(html).toContain("Host not approved");
    expect(html).toContain("GATEWAY_SMTP_HOST_ALLOWLIST");
    expect(html).toContain("smtp.example.org");
    expect(deliveryErrors.host_not_allowed).toContain("GATEWAY_SMTP_HOST_ALLOWLIST");
  });
  it("shows nothing about it once the host is approved", () => {
    const html = markup(<EmailSettingsPage session={admin} />, [[`${settingsPath}/email`, settings("ready", true)]]);
    expect(html).not.toContain("Host not approved");
    expect(html).not.toContain("GATEWAY_SMTP_HOST_ALLOWLIST");
  });
});
