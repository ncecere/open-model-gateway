import { describe, expect, it, vi } from "vitest";
import { ApiError } from "./api";
import { dashboardHref, parseDashboardLocation } from "./locations";
import { deliveryErrorText, webhookBody, webhookDraft, webhookDraftErrors, webhookError, webhookTestLimitMessage, type AlertWebhook } from "./alert-webhooks";
const saved: AlertWebhook = { id: "hook", label: "Operations", url: "https://receiver.example/hook", current_secret_ref: "env:CURRENT", previous_secret_ref: null, previous_secret_until: null, enabled: true, signing_configured: true, overlap_active: false, waiting: 0, exhausted: 0, last_success_at: null, last_error: null, created_at: "2026-01-01T00:00:00Z", updated_at: "2026-01-01T00:00:00Z" };

describe("webhook contract helpers", () => {
  it("round trips the Webhooks tab within the existing platform-only Alerts route", () => {
    const search = { page: "settings-alerts", tab: "webhooks" } as const;
    expect(dashboardHref(search)).toBe("/admin/settings/alerts?tab=webhooks");
    expect(parseDashboardLocation(dashboardHref(search))).toMatchObject(search);
  });
  it("sends only label, URL and current reference on create, never enablement or overlap", () => {
    expect(webhookBody({ ...webhookDraft(saved), previous: "env:OLD", until: "2026-01-02T00:00:00Z" })).toEqual({ label: saved.label, url: saved.url, current_secret_ref: saved.current_secret_ref });
  });
  it("never patches a URL or enabled flag; preserves untouched expired overlaps", () => {
    const old = { ...saved, previous_secret_ref: "env:OLD", previous_secret_until: "2020-01-01T00:00:00Z" };
    const draft = { ...webhookDraft(old), label: "Renamed", url: "https://different.example" };
    expect(webhookDraftErrors(draft, old)).toEqual({});
    expect(webhookBody(draft, old)).toEqual({ label: "Renamed" });
    expect(webhookBody({ ...draft, previous: "", until: "" }, old)).toEqual({ label: "Renamed", previous_secret_ref: null, previous_secret_until: null });
  });
  it("requires a paired, distinct, RFC3339 overlap ending within 24 hours", () => {
    const clock = vi.spyOn(Date, "now").mockReturnValue(Date.parse("2026-11-01T00:00:00Z"));
    try {
      const draft = { ...webhookDraft(saved), current: "env:NEW", previous: "env:CURRENT", until: "2026-11-01T23:00:00Z" };
      expect(webhookDraftErrors(draft, saved)).toEqual({});
      expect(webhookBody(draft, saved)).toEqual({ current_secret_ref: "env:NEW", previous_secret_ref: "env:CURRENT", previous_secret_until: draft.until });
      for (const until of ["", "tomorrow", "2026-11-01T23:00:00", "2026-10-31T23:00:00Z", "2026-11-02T00:00:01Z"]) expect(webhookDraftErrors({ ...draft, until }, saved).until).toBeTruthy();
      expect(webhookDraftErrors({ ...draft, previous: "env:NEW" }, saved).previous).toBeTruthy();
    } finally { clock.mockRestore(); }
  });
  it.each(["https://user:password@receiver.example/hook", "https://receiver.example/hook?key=secret", "https://receiver.example/hook#fragment", "file:///tmp/hook"])("rejects URL shape %s before sending", url => {
    expect(webhookDraftErrors({ ...webhookDraft(saved), url }).url).toBeTruthy();
  });
  it.each(["file:/secret", "plaintext-secret", "env:lowercase", "env:1BAD"])("accepts only env reference syntax, never a key value (%s)", current => {
    expect(webhookDraftErrors({ ...webhookDraft(saved), current }).current).toBeTruthy();
  });
  it("maps 429 without claiming a countdown; hides unexpected server/receiver text", () => {
    expect(webhookError(new ApiError(429, "429", "raw")).message).toBe(webhookTestLimitMessage);
    expect(webhookError(new ApiError(500, "500", "PRIVATE_SECRET_CANARY")).message).not.toContain("PRIVATE_SECRET_CANARY");
    expect(deliveryErrorText("PRIVATE_SECRET_CANARY")).not.toContain("PRIVATE_SECRET_CANARY");
  });
});
