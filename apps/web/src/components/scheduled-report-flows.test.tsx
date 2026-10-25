// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { ScheduledReports, ReportRunPage } from "../pages/scheduled-reports";
import { ActionProvider } from "./ui";
import { admin, auditor, session, member, personal, testClient } from "../lib/test-fixtures";
import { abortRequests } from "../lib/api";
const id = "11111111-1111-4111-8111-111111111111", me = { ...admin, user: { ...admin.user, id } };
const base = "/api/v1/platform/report-schedules";
const schedule = { id: "schedule", scope_kind: "installation", recipient_user_ids: [id], cadence: "weekly", timezone: "UTC", delivery_time: "09:00", state: "paused", next_run_at: null, cost_center_id: null };
beforeEach(() => { document.cookie = "omg_csrf=csrf; Path=/"; Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] }); vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} }); });
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });
function mount(node: React.ReactNode) { const client = testClient(); render(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>); return client; }
function serve() {
  let saved = { ...schedule };
  const fetch = vi.fn(async (path: string, init?: RequestInit) => {
    if (init?.method === "POST") { saved = { ...saved, ...JSON.parse(String(init.body)) }; return Response.json(saved); }
    if (init?.method === "PATCH") { saved = { ...saved, ...JSON.parse(String(init.body)) }; return Response.json(saved); }
    if (path.includes("/runs?")) return Response.json({ data: [], outcomes: [] });
    return Response.json({ data: [saved], has_more: false, configuration: { smtp_configured: false, public_url_configured: false } });
  });
  vi.stubGlobal("fetch", fetch); return fetch;
}
describe("scheduled report editor and refusals", () => {
  it("validates before sending, creates paused, and requires confirmation to resume or delete", async () => {
    const user = userEvent.setup(), fetch = serve(), client = mount(<ScheduledReports session={me} />);
    await user.click(await screen.findByRole("button", { name: "Create schedule" }));
    const dialog = screen.getByRole("dialog");
    await user.clear(within(dialog).getByRole("textbox", { name: "Local delivery time" }));
    await user.type(within(dialog).getByRole("textbox", { name: "Local delivery time" }), "25:00");
    await user.click(within(dialog).getByRole("button", { name: "Create schedule" }));
    expect(await within(dialog).findByText("Enter HH:MM from 00:00 to 23:59.")).toBeTruthy();
    expect(fetch.mock.calls.some(([, init]) => init?.method === "POST")).toBe(false);
    await user.clear(within(dialog).getByRole("textbox", { name: "Local delivery time" }));
    await user.type(within(dialog).getByRole("textbox", { name: "Local delivery time" }), "09:45");
    await user.selectOptions(within(dialog).getByRole("combobox", { name: "Cadence" }), "monthly");
    await user.click(within(dialog).getByRole("button", { name: "Create schedule" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const post = fetch.mock.calls.find(([, init]) => init?.method === "POST")!;
    expect(post[0]).toBe(base); expect(JSON.parse(String(post[1]?.body))).toMatchObject({ state: "paused", cadence: "monthly", delivery_time: "09:45", recipient_user_ids: [id] });
    await user.click(screen.getByRole("button", { name: "Resume" }));
    expect(fetch.mock.calls.some(([, init]) => init?.method === "PATCH")).toBe(false);
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Resume schedule" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(JSON.parse(String(fetch.mock.calls.find(([, init]) => init?.method === "PATCH")![1]?.body))).toEqual({ state: "active" });
    await user.click(screen.getByRole("button", { name: "Delete" }));
    expect(fetch.mock.calls.some(([, init]) => init?.method === "DELETE")).toBe(false);
    expect(within(screen.getByRole("dialog")).getByText(/Deletion is final/)).toBeTruthy();
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Cancel" }));
    client.clear();
  });
  it("reauthorizes a cached run on entry and never renders it while permission is refused", async () => {
    let complete!: (response: Response) => void;
    const fetch = vi.fn(() => new Promise<Response>(resolve => { complete = resolve; }));
    vi.stubGlobal("fetch",fetch);
    const client = testClient();
    const path = `${base}/schedule/runs/run`;
    client.setQueryData(["api",undefined,path],{ snapshot: { breakdowns: { models: [{ name: "Private allocation" }] } } });
    render(<QueryClientProvider client={client}><ActionProvider><ReportRunPage session={me} schedule="schedule" run="run" /></ActionProvider></QueryClientProvider>);
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(1));
    expect(screen.queryByText("Private allocation")).toBeNull();
    expect(screen.queryByRole("link",{ name: "Download aggregate CSV" })).toBeNull();
    complete(Response.json({ error: { code: "403", message: "Forbidden" } },{ status: 403 }));
    await screen.findByText("Access denied");
    expect(screen.queryByText("Private allocation")).toBeNull();
    client.clear();
  });
  it("Auditor has no write controls, members/foreign personal never fetch report data", async () => {
    const fetch = serve(); let client = mount(<ScheduledReports session={auditor} />);
    await screen.findByText("Read-only · Platform Auditor");
    expect(screen.queryByRole("button", { name: "Create schedule" })).toBeNull();
    client.clear(); cleanup(); fetch.mockClear();
    for (const workspace of [member, { ...personal, owner_user_id: "other" }]) {
      client = mount(<ReportRunPage session={session} workspace={workspace} schedule="schedule" run="run" />);
      expect(screen.getByText("Access not available")).toBeTruthy(); expect(fetch).not.toHaveBeenCalled(); client.clear(); cleanup();
    }
  });
});
