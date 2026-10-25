// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { dashboardRouteTree } from "../router";
import { abortRequests, type Session } from "../lib/api";
import { admin, auditor, session, testClient } from "../lib/test-fixtures";
import { webhookPath, webhookTestLimitMessage, type AlertWebhook } from "../lib/alert-webhooks";
/** Exact accessible-name match: ByRoleOptions has no `exact`; anchor an escaped pattern instead. */
const exactly = (name: string) => new RegExp(`^${name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`);

const saved: AlertWebhook = { id: "hook1", label: "Operations", enabled: false, signing_configured: true, overlap_active: false, previous_secret_until: null, waiting: 0, exhausted: 0, last_success_at: null, last_error: null, created_at: "2026-10-22T00:00:00Z", updated_at: "2026-10-22T00:00:00Z", url: "https://receiver.example/hook", current_secret_ref: "env:ALERT_SIGNING", previous_secret_ref: null };
let rows: AlertWebhook[], failure: { method: string; status: number; message: string; reason?: string } | undefined;
let activeClient: ReturnType<typeof testClient> | undefined;
let testResponse: Promise<Response> | undefined;
function deferTestResponse() {
  let resolve!: (response: Response) => void;
  testResponse = new Promise<Response>(done => { resolve = done; });
  return resolve;
}
beforeEach(() => {
  rows = [{ ...saved }]; failure = undefined; testResponse = undefined;
  document.cookie = "omg_csrf=test-csrf; Path=/"; localStorage.clear(); sessionStorage.clear();
  Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
});
afterEach(() => { cleanup(); abortRequests(); activeClient?.clear(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });
function serve(who: Session = admin) {
  const fetch = vi.fn(async (path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    if (path === "/api/v1/me") return Response.json(who);
    if (path.startsWith(webhookPath)) {
      if (failure?.method === method) return Response.json({ error: { code: String(failure.status), message: failure.message, reason: failure.reason } }, { status: failure.status });
      if (path.endsWith("/deliveries")) return Response.json({ destination: rows.find(row => path === `${webhookPath}/${row.id}/deliveries`), counts: { waiting: 1, leased: 0, sent: 0, failed: 0, exhausted: 0, ineligible: 0 }, data: [{ id: "delivery1", state: "waiting", test: true, attempt_count: 0, created_at: saved.created_at, completed_at: null, next_attempt_at: saved.created_at, last_error: null, payload: "PRIVATE_PAYLOAD_CANARY", signature: "PRIVATE_SIGNATURE_CANARY", secret: "PRIVATE_SECRET_CANARY" }] });
      if (path.endsWith("/test")) return testResponse ?? Response.json({ id: "delivery1", state: "waiting" });
      if (method === "POST") { rows.push({ ...saved, id: "hook2", ...JSON.parse(String(init?.body)) }); return Response.json(rows[1]); }
      if (method === "PATCH") { rows[0] = { ...rows[0]!, ...JSON.parse(String(init?.body)) }; return Response.json(rows[0]); }
      if (method === "DELETE") { rows = []; return Response.json({ deleted: true }); }
      return Response.json({ data: rows });
    }
    return Response.json({ data: [], has_more: false });
  });
  vi.stubGlobal("fetch", fetch); return fetch;
}
async function mount(who = admin) {
  const fetch = serve(who), client = testClient(); activeClient = client;
  const router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: ["/admin/settings/alerts?tab=webhooks"] }) });
  await router.load(); render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
  return { fetch, router };
}
const writes = (fetch: ReturnType<typeof vi.fn>, method: string) => fetch.mock.calls.filter(c => c[1]?.method === method).map(c => [c[0], JSON.parse(String(c[1]?.body ?? "null"))]);
async function action(name: string, label = "Operations") {
  const user = userEvent.setup();
  await user.click(within((await screen.findByText(label)).closest("tr")!).getByRole("button", { name: `Actions for ${label}` }));
  await user.click(await screen.findByRole("menuitem", { name: exactly(name) })); return user;
}
async function confirm(name: string) { await userEvent.setup().click(within(await screen.findByRole("dialog")).getByRole("button", { name: exactly(name) })); }

describe("Admin Alerts Webhooks", () => {
  it("routes to the Webhooks tab and shows bounded status, not secret material", async () => {
    await mount();
    expect((await screen.findByRole("tab", { name: "Webhooks" })).getAttribute("aria-selected")).toBe("true");
    expect(await screen.findByText("Operations")).toBeTruthy();
    expect(screen.getByText("Disabled")).toBeTruthy();
    expect(screen.getByText(/HMAC-SHA256/)).toBeTruthy();
    await action("Deliveries");
    expect(await screen.findByRole("table", { name: "Webhook deliveries" })).toBeTruthy();
    expect(document.body.textContent).toContain("Waiting: 1");
    expect(document.body.textContent).not.toContain("PRIVATE_PAYLOAD_CANARY");
    expect(document.body.textContent).not.toContain("PRIVATE_SIGNATURE_CANARY");
    expect(document.body.textContent).not.toContain("PRIVATE_SECRET_CANARY");
  });
  it("gives Auditors redacted status with no mutating controls even if extra fields arrive", async () => {
    await mount(auditor);
    await screen.findByText("Operations");
    expect(screen.queryByRole("button", { name: "Create webhook" })).toBeNull();
    const user = await action("Deliveries");
    expect(document.body.textContent).not.toContain(saved.url);
    expect(document.body.textContent).not.toContain(saved.current_secret_ref);
    expect(screen.queryByRole("button", { name: /^Send\ test$/ })).toBeNull();
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Close deliveries" }));
    await user.click(within(screen.getByText("Operations").closest("tr")!).getByRole("button", { name: /Actions/ }));
    for (const name of ["Edit…", "Enable…", "Delete…", "Send test"]) expect(screen.queryByRole("menuitem", { name: exactly(name) })).toBeNull();
  });
  it("does not fetch webhooks for a non-platform user", async () => {
    const { fetch, router } = await mount(session);
    await waitFor(() => expect(router.state.location.pathname).toBe("/home"));
    expect(fetch.mock.calls.some(c => c[0].startsWith(webhookPath))).toBe(false);
  });
  it("validates and creates disabled with only the three supported create fields", async () => {
    const { fetch } = await mount(), user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Create webhook" }));
    await confirm("Create webhook");
    expect(writes(fetch, "POST")).toEqual([]);
    const dialog = within(screen.getByRole("dialog"));
    await user.type(dialog.getByRole("textbox", { name: "Name" }), "New receiver");
    await user.type(dialog.getByRole("textbox", { name: "URL" }), saved.url!);
    await user.type(dialog.getByRole("textbox", { name: "Signing reference" }), saved.current_secret_ref!);
    await confirm("Create webhook");
    await waitFor(() => expect(writes(fetch, "POST")).toEqual([[webhookPath, { label: "New receiver", url: saved.url, current_secret_ref: saved.current_secret_ref }]]));
    expect(await screen.findByText("New receiver")).toBeTruthy();
  });
  it("enables, queues a body-free single test, disables, and confirms deletion", async () => {
    const { fetch } = await mount();
    await action("Enable…"); await confirm("Enable webhook");
    await waitFor(() => expect(writes(fetch, "PATCH")).toEqual([[`${webhookPath}/hook1`, { enabled: true }]]));
    await action("Send test");
    await waitFor(() => expect(writes(fetch, "POST")).toEqual([[`${webhookPath}/hook1/test`, null]]));
    expect(await screen.findByText(/Test queued/)).toBeTruthy();
    await confirm("Close deliveries");
    await action("Disable…"); await confirm("Disable webhook");
    await waitFor(() => expect(writes(fetch, "PATCH")[1]).toEqual([`${webhookPath}/hook1`, { enabled: false }]));
    await action("Delete…"); expect(writes(fetch, "DELETE")).toEqual([]); await confirm("Delete webhook");
    await waitFor(() => expect(writes(fetch, "DELETE")).toEqual([[`${webhookPath}/hook1`, null]]));
  });
  it("does not reopen closed deliveries when a delayed test succeeds", async () => {
    rows[0]!.enabled = true; const { fetch } = await mount(), resolve = deferTestResponse();
    await action("Send test");
    expect(writes(fetch, "POST")).toEqual([[`${webhookPath}/hook1/test`, null]]);
    await action("Deliveries"); await confirm("Close deliveries");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await act(async () => { resolve(Response.json({ id: "delivery1", state: "waiting" })); });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByText("Test queued")).toBeNull();
  });
  it("does not replace another destination's deliveries when a delayed test succeeds", async () => {
    rows = [{ ...saved, enabled: true }, { ...saved, id: "hook2", label: "Security" }];
    const { fetch } = await mount(), resolve = deferTestResponse();
    await action("Send test");
    expect(writes(fetch, "POST")).toEqual([[`${webhookPath}/hook1/test`, null]]);
    await action("Deliveries", "Security");
    await screen.findByRole("dialog", { name: exactly("Deliveries · Security") });
    await act(async () => { resolve(Response.json({ id: "delivery1", state: "waiting" })); });
    expect(screen.getByRole("dialog", { name: exactly("Deliveries · Security") })).toBeTruthy();
    expect(screen.queryByRole("dialog", { name: exactly("Deliveries · Operations") })).toBeNull();
    expect(screen.queryByText("Test queued")).toBeNull();
  });
  it("does not surface a delayed test error after deliveries are opened and closed", async () => {
    rows[0]!.enabled = true; await mount(); const resolve = deferTestResponse();
    await action("Send test"); await action("Deliveries"); await confirm("Close deliveries");
    await act(async () => { resolve(Response.json({ error: { code: "429", message: "Too many webhook tests; try again later" } }, { status: 429 })); });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByText(webhookTestLimitMessage)).toBeNull();
  });
  it.each(["cancel", "save", "discard"] as const)("returns focus to the originating row actions after Edit %s", async mode => {
    await mount();
    const trigger = await screen.findByRole("button", { name: exactly("Actions for Operations") });
    const user = await action("Edit…");
    if (mode === "discard") {
      await user.type(within(screen.getByRole("dialog")).getByRole("textbox", { name: "Name" }), " changed");
      await confirm("Cancel");
      await user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: /Discard/ }));
    } else await confirm(mode === "save" ? "Save webhook" : "Cancel");
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  });
  it("returns Edit focus to Create webhook if the originating row disappears", async () => {
    await mount(); await action("Edit…");
    rows = []; await act(async () => { await activeClient!.invalidateQueries({ queryKey: ["api"] }); });
    await confirm("Cancel");
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("button", { name: exactly("Create webhook") })));
  });
  it("edits references and overlap, never a URL or enablement, and clears overlap explicitly", async () => {
    rows[0] = { ...saved, previous_secret_ref: "env:OLD", previous_secret_until: "2026-10-23T00:00:00Z" };
    const { fetch } = await mount(); const user = await action("Edit…");
    const dialog = within(screen.getByRole("dialog"));
    expect(dialog.getByRole("textbox", { name: "URL" }).hasAttribute("readonly")).toBe(true);
    await user.clear(dialog.getByRole("textbox", { name: "Previous signing reference" }));
    await user.clear(dialog.getByRole("textbox", { name: /Overlap ends/ }));
    await confirm("Save webhook");
    await waitFor(() => expect(writes(fetch, "PATCH")).toEqual([[`${webhookPath}/hook1`, { previous_secret_ref: null, previous_secret_until: null }]]));
  });
  it("saves a live signing rotation with a bounded previous-key overlap", async () => {
    rows[0]!.enabled = true; const { fetch } = await mount(); const user = await action("Edit…");
    const dialog = within(screen.getByRole("dialog")), until = new Date(Date.now() + 23 * 3600000).toISOString();
    await user.clear(dialog.getByRole("textbox", { name: /^Signing\ reference$/ }));
    await user.type(dialog.getByRole("textbox", { name: /^Signing\ reference$/ }), "env:NEW_KEY");
    await user.type(dialog.getByRole("textbox", { name: /^Previous\ signing\ reference$/ }), saved.current_secret_ref!);
    await user.type(dialog.getByRole("textbox", { name: /Overlap ends/ }), until);
    await confirm("Save webhook");
    await waitFor(() => expect(writes(fetch, "PATCH")).toEqual([[`${webhookPath}/hook1`, { current_secret_ref: "env:NEW_KEY", previous_secret_ref: saved.current_secret_ref, previous_secret_until: until }]]));
  });
  it("maps 429, re-enables Send test, and never automatically retries", async () => {
    rows[0]!.enabled = true; const { fetch } = await mount();
    failure = { method: "POST", status: 429, message: "Too many webhook tests; try again later" };
    await action("Send test");
    expect(await screen.findByText(webhookTestLimitMessage)).toBeTruthy();
    expect(writes(fetch, "POST")).toHaveLength(1);
    await action("Send test");
    await waitFor(() => expect(writes(fetch, "POST")).toHaveLength(2));
  });
  it.each<[number, string, RegExp]>([
    [400, "Invalid request", /Check the name, operator-approved URL and signing references/],
    [409, "Maximum ten webhook destinations", /Ten webhooks already exist/],
    [409, "An enabled saved destination is required", /Enable the saved webhook/],
    [404, "Not found", /This webhook was deleted/],
  ])("maps %s API refusal without rendering raw server content", async (status, message, expected) => {
    rows[0]!.enabled = true; await mount(); failure = { method: "POST", status, message };
    await action("Send test"); expect(await screen.findByText(expected)).toBeTruthy();
  });
  it("refreshes live access on a write 403", async () => {
    rows[0]!.enabled = true; await mount(); const refresh = vi.fn(); window.addEventListener("omg:access-refresh", refresh);
    failure = { method: "POST", status: 403, message: "Access denied" };
    try { await action("Send test"); await waitFor(() => expect(refresh).toHaveBeenCalled()); }
    finally { window.removeEventListener("omg:access-refresh", refresh); }
  });
});
