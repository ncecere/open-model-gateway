import { describe, expect, it } from "vitest";
import { AuditExportSettingsPage, type AuditExportStatus } from "../pages/settings/audit-export";
import { admin, auditor, markup } from "../lib/test-fixtures";

const path = "/api/v1/platform/settings/audit-export";
const status = (s: Partial<AuditExportStatus>): AuditExportStatus => ({
  configured: true, transport: "webhook", enabled: true, enabled_at: "2026-10-14T00:00:00Z", destination_host: "siem.example.org", schema: "omg.audit.v1",
  queued: 0, oldest_queued_at: null, last_success_at: "2026-10-14T01:00:00Z", last_error: null, last_error_at: null, retrying: false, next_attempt_at: null, delivered_total: "1200", next_seq: "1201", last_heartbeat_at: null, ...s,
});

describe("Admin › Settings › Audit export (#106)", () => {
  it("is read-only: the destination is the operator's, with no fields to change", () => {
    const html = markup(<AuditExportSettingsPage session={admin} />, [[path, status({})]]);
    expect(html).toContain("siem.example.org");
    expect(html).toContain("Delivering");
    expect(html).toContain("Send test delivery");
    expect(html).not.toMatch(/<input|<select|<textarea/);
  });
  it("auditors see the status without the test button", () => {
    const html = markup(<AuditExportSettingsPage session={auditor} />, [[path, status({})]]);
    expect(html).toContain("siem.example.org");
    expect(html).not.toContain("Send test delivery");
  });
  it("explains a failing receiver and that nothing is dropped", () => {
    const html = markup(<AuditExportSettingsPage session={admin} />, [[path, status({ retrying: true, last_error: "http_5xx", last_error_at: "2026-10-14T02:00:00Z", queued: 42 })]]);
    expect(html).toContain("Retrying");
    expect(html).toContain("none are dropped");
    expect(html).toContain("42 events");
  });
  it("shows the sequence counter and heartbeat receivers check for gaps (#174)", () => {
    const html = markup(<AuditExportSettingsPage session={admin} />, [[path, status({ last_heartbeat_at: "2026-10-14T03:00:00Z" })]]);
    expect(html).toContain("Next sequence number");
    expect(html).toContain("1,201");
    expect(html).toContain("Last heartbeat");
    expect(markup(<AuditExportSettingsPage session={admin} />, [[path, status({})]])).toContain("None yet");
  });
  it("shows the transport (#107)", () => {
    const webhook = markup(<AuditExportSettingsPage session={admin} />, [[path, status({})]]);
    expect(webhook).toContain("Transport");
    expect(webhook).toContain("HTTPS webhook");
    expect(webhook).toContain("signed HTTPS deliveries or as syslog over TLS");
    const syslog = markup(<AuditExportSettingsPage session={admin} />, [[path, status({ transport: "syslog", retrying: true, last_error: "tls", last_error_at: "2026-10-14T02:00:00Z" })]]);
    expect(syslog).toContain("Syslog over TLS");
    expect(syslog).toContain("TLS handshake with the receiver failed");
  });
  it("names the operator settings when not set up", () => {
    const html = markup(<AuditExportSettingsPage session={admin} />, [[path, status({ configured: false, transport: null, enabled: false, destination_host: null })]]);
    expect(html).toContain("GATEWAY_AUDIT_WEBHOOK_URL");
    expect(html).toContain("GATEWAY_AUDIT_SYSLOG_URL");
    expect(html).not.toContain("Send test delivery");
  });
});
