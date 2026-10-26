import { describe, expect, it } from "vitest";
import { ScheduledReports, ReportRunPage } from "../pages/scheduled-reports";
import { admin, auditor, markup, member, session, team } from "../lib/test-fixtures";
import type { ReportRun, Schedule } from "../lib/scheduled-reports";
const base = "/api/v1/platform/report-schedules";
const schedule: Schedule = { id: "schedule", scope_kind: "installation", workspace_id: null, cost_center_id: null, cadence: "weekly", timezone: "UTC", delivery_time: "09:30", state: "paused", recipient_user_ids: ["me"], next_run_at: "2026-11-02T09:30:00Z", configuration_revision: "1" };
const run: ReportRun = { id: "run", schedule_id: "schedule", period_start: "2026-10-19T00:00:00Z", period_end: "2026-10-26T00:00:00Z", observed_at: "2026-10-26T09:30:00Z", expires_at: "2027-01-24T09:30:00Z", expired: false, configuration_revision: "1", delivery: { status: "partial", recipients: 2, sent: 1, failed: 1, waiting: 0 }, snapshot: { basis: "configured_rate_estimate", currency: "USD", totals: { known_cost_microusd: "9007199254740993", held_microusd: "1", attempts: "2", unresolved_attempts: "1", root_requests: null }, coverage: { priced_attempts: null, total_attempts: "2" }, breakdowns: { models: [{ id: "model", name: "Example", totals: { known_cost_microusd: "2", held_microusd: "1", attempts: "2", unresolved_attempts: "1", root_requests: null } }] }, breakdowns_truncated: true, truncated_dimensions: ["models"], unavailable: { pricing_coverage: "not_in_rollups", provider_reported_cost_microusd: "not_in_rollups" } } };
describe("scheduled report pages", () => {
  const entries: [string, unknown][] = [[`${base}?limit=100`, { data: [schedule], has_more: false, configuration: { smtp_configured: false, public_url_configured: false } }], [`${base}/schedule/runs?limit=20`, { data: [run] }]];
  it("lists next occurrence, recent runs, delivery counts and configuration help", () => {
    const html = markup(<ScheduledReports session={admin} />, entries);
    expect(html).toContain("Next occurrence"); expect(html).toContain("UTC"); expect(html).toContain("Recent runs");
    expect(html).toContain("1 sent"); expect(html).toContain("1 failed"); expect(html).toContain('href="/admin/settings/email"');
    expect(html).toContain("Create schedule"); expect(html).toContain("Resume"); expect(html).toContain("Delete");
    expect(html).toContain("GATEWAY_PUBLIC_URL");
    // Delivery status is a human StatusBadge, and recipients pluralize; no raw status code.
    expect(html).toContain("Partially delivered"); expect(html).toContain("2 recipients"); expect(html).not.toContain(">partial<");
  });
  it("keeps Auditors read-only and members out", () => {
    const html = markup(<ScheduledReports session={auditor} />, entries);
    for (const action of ["Create schedule", "Resume", "Edit schedule", "Delete schedule"]) expect(html).not.toContain(`>${action}<`);
    expect(html).toContain("Read-only");
    expect(markup(<ScheduledReports session={session} workspace={member} />)).toContain("Access not available");
    expect(markup(<ScheduledReports session={session} workspace={team} />, [["/api/v1/workspaces/team/report-schedules?limit=100", { data: [], configuration: { smtp_configured: true, public_url_configured: true } }]])).toContain("Create schedule");
  });
  it("keeps configuration details and the settings link exclusive to platform writers", () => {
    const unavailable = { data: [], configuration: { smtp_configured: false, public_url_configured: false } };
    for (const viewer of [session, auditor]) {
      const workspace = viewer === session ? { ...team, role: "owner" as const } : undefined;
      const html = markup(<ScheduledReports session={viewer} workspace={workspace} />, [[workspace ? "/api/v1/workspaces/team/report-schedules?limit=100" : `${base}?limit=100`, unavailable]]);
      expect(html).toContain("Email delivery isn&#x27;t configured yet");
      expect(html).toContain("Ask a Platform Admin");
      expect(html).not.toContain("GATEWAY_PUBLIC_URL");
      expect(html).not.toContain("SMTP");
      expect(html).not.toContain('href="/admin/settings/email"');
    }
    const denied = markup(<ScheduledReports session={session} workspace={member} />, [["/api/v1/workspaces/team/report-schedules?limit=100", unavailable]]);
    expect(denied).toContain("Access not available"); expect(denied).not.toContain("Email delivery");
  });
  it("renders immutable exact totals, unavailable values, truncation and authenticated CSV link", () => {
    const html = markup(<ReportRunPage session={admin} schedule="schedule" run="run" />, [[`${base}/schedule/runs/run`, run]]);
    expect(html).toContain("Configured-rate estimates, not invoices"); expect(html).toContain("9,007,199,254.740993");
    expect(html).toContain("Unavailable"); expect(html).toContain("Models: first 100 entries"); expect(html).not.toContain("models: first 100 entries");
    // Human measure, dimension, coverage and unavailable labels; never raw snake_case or a bare "microusd".
    expect(html).toContain("Upstream attempts"); expect(html).toContain("Unresolved attempts"); expect(html).toContain("Root requests");
    expect(html).toContain("Priced attempts"); expect(html).toContain("Total attempts");
    expect(html).toContain("Pricing coverage"); expect(html).toContain("Not kept in historical rollups"); expect(html).toContain("Provider-reported cost");
    expect(html).not.toContain("microusd"); expect(html).not.toContain("priced_attempts"); expect(html).not.toContain("total_attempts"); expect(html).not.toContain("pricing_coverage"); expect(html).not.toContain("provider_reported_cost");
    expect(html).toContain(`href="${base}/schedule/runs/run/csv"`); expect(html).toContain("Download aggregate CSV");
  });
  it("never downloads or renders an expired snapshot", () => {
    const html = markup(<ReportRunPage session={admin} schedule="schedule" run="run" />, [[`${base}/schedule/runs/run`, { ...run, expired: true, snapshot: null }]]);
    expect(html).toContain("Report expired"); expect(html).not.toContain("Download aggregate CSV"); expect(html).not.toContain("9007199254");
  });
});
