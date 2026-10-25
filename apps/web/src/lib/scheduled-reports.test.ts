import { describe, expect, it } from "vitest";
import { scheduleFields, scheduleBody, reportsPath } from "./scheduled-reports";
import { validateFields } from "./forms";
import { admin, personal, session, team } from "./test-fixtures";
import { canView } from "./permissions";
import { dashboardHref, parseDashboardLocation } from "./locations";
const user = "11111111-1111-4111-8111-111111111111";
const valid = { cadence: "weekly", timezone: "America/New_York", delivery_time: "09:30", recipient_user_ids: user };
describe("scheduled reports", () => {
  it("validates cadence, IANA zone, exact HH:MM and 1–50 unique internal IDs", () => {
    const fields = scheduleFields(admin);
    expect(validateFields(fields, valid)).toEqual({});
    for (const [key, value] of Object.entries({ cadence: "daily", timezone: "Not/AZone", delivery_time: "9:30", recipient_user_ids: "external@example.test" })) expect(validateFields(fields, { ...valid, [key]: value })[key]).toBeTruthy();
    expect(validateFields(fields, { ...valid, delivery_time: "24:00" }).delivery_time).toBeTruthy();
    expect(validateFields(fields, { ...valid, recipient_user_ids: `${user},${user}` }).recipient_user_ids).toBeTruthy();
    expect(validateFields(fields, { ...valid, recipient_user_ids: Array.from({ length: 51 }, (_, i) => `11111111-1111-4111-8111-${String(i).padStart(12, "0")}`).join(",") }).recipient_user_ids).toBeTruthy();
    expect(scheduleBody(valid)).toEqual({ ...valid, recipient_user_ids: [user], state: "paused" });
  });
  it("fixes personal recipients to the owner and never offers a personal cost-center filter", () => {
    const fields = scheduleFields(session, personal);
    expect(fields.some(f => f.name === "recipient_user_ids" || f.name === "cost_center_id")).toBe(false);
    expect(scheduleBody(valid, personal, session).recipient_user_ids).toEqual([session.user.id]);
    expect(scheduleFields(admin, team).some(f => f.name === "cost_center_id")).toBe(true);
  });
  it("round trips the exact email routes and uses same-origin API paths", () => {
    for (const ws of [undefined, "workspace"]) {
      const search = { page: ws ? "workspace-report-run" as const : "platform-report-run" as const, ws, record: "schedule", run: "run" };
      const href = ws ? `/workspaces/${ws}/report-schedules/schedule/runs/run` : "/admin/report-schedules/schedule/runs/run";
      expect(dashboardHref(search)).toBe(href);
      expect(parseDashboardLocation(href)).toMatchObject(search);
      expect(reportsPath(ws && { ...team, id: ws })).toBe(ws ? `/api/v1/workspaces/${ws}/report-schedules` : "/api/v1/platform/report-schedules");
    }
  });
  it("denies members and foreign personal owners before fetching", () => {
    const member = { ...team, role: "member" as const };
    expect(canView("workspace-report-run", session, member)).toBe(false);
    expect(canView("workspace-report-run", admin, { ...personal, owner_user_id: "other" })).toBe(false);
    expect(canView("platform-report-run", admin)).toBe(true);
  });
});
