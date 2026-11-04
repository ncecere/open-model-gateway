// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { BulkDisableDialog } from "./deployment-bulk";
import { testClient } from "../lib/test-fixtures";

const targets = [{ id: "a", name: "route-a" }, { id: "b", name: "route-b" }];
const preview = (id: string, revision: string, enabled = true) => ({ target: { kind: "deployment", id }, expected_revision: revision, diff: enabled ? [{ field: "enabled", before: true, after: false }] : [], serving_routes_gained: [], serving_routes_lost: enabled ? [id] : [], models_losing_last_serving_route: id === "b" ? [{ id: "m", public_name: "company/smart" }] : [], affected_workspaces: { data: [], total: 0, limit: 100, truncated: false }, observed_at: "2026-11-04T00:00:00Z", dependency_versions: {}, warnings: [] });
const operation = { operation_id: "op", idempotency_key: "k", completed: true, replayed: false, items: [
  { id: "a", expected_revision: "3", outcome: "committed", reason: null, revision: "4", change_id: "c" },
  { id: "b", expected_revision: "1", outcome: "conflict", reason: "revision_conflict", revision: "2", change_id: null },
] };
beforeEach(() => { document.cookie = "omg_csrf=test; path=/"; });
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
function view(onDone = vi.fn()) {
  render(<QueryClientProvider client={testClient()}><BulkDisableDialog targets={targets} onClose={() => {}} onDone={onDone} /></QueryClientProvider>);
  return userEvent.setup();
}
const previewFetch = (url: string) => url.includes("/deployments/a/") ? preview("a", "3") : preview("b", "1");

describe("bulk disable (#88)", () => {
  it("reviews every target, then disables each at its reviewed revision with one key", async () => {
    const fetch = vi.fn().mockImplementation((url: string) => Promise.resolve(Response.json(url.endsWith("/deployment-bulk-operations") ? operation : previewFetch(url))));
    vi.stubGlobal("fetch", fetch);
    const onDone = vi.fn();
    const user = view(onDone);
    await screen.findByText(/route-a · Enabled → Disabled · revision/);
    expect(screen.getByText(/company\/smart/)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Disable 2 routes" }));
    await screen.findByText("route-a: Disabled");
    expect(screen.getByText("route-b: Changed since review: not disabled (review it again)")).toBeTruthy();
    const call = fetch.mock.calls.find(([url]) => String(url).endsWith("/deployment-bulk-operations"))!;
    const sent = JSON.parse(call[1].body);
    expect(sent.action).toBe("disable");
    expect(sent.items).toEqual([{ id: "a", expected_revision: "3" }, { id: "b", expected_revision: "1" }]);
    expect(sent.idempotency_key).toMatch(/^[A-Za-z0-9._:-]{1,128}$/);
    expect(onDone).toHaveBeenCalled();
  });
  it("an interrupted request is read through its status, with the same key, never replayed blindly", async () => {
    let sentKey = "";
    const fetch = vi.fn().mockImplementation((url: string, init: RequestInit) => {
      if (String(url).endsWith("/deployment-bulk-operations")) { sentKey = JSON.parse(String(init.body)).idempotency_key; return Promise.reject(new TypeError("network down")); }
      if (String(url).includes("/deployment-bulk-operations/")) return Promise.resolve(Response.json({ ...operation, replayed: true }));
      return Promise.resolve(Response.json(previewFetch(url)));
    });
    vi.stubGlobal("fetch", fetch);
    const user = view();
    await user.click(await screen.findByRole("button", { name: "Disable 2 routes" }));
    await screen.findByText("Outcome unknown");
    await user.click(screen.getByRole("button", { name: "Check outcome" }));
    await screen.findByText("route-a: Disabled");
    const status = fetch.mock.calls.find(([url]) => String(url).includes("/deployment-bulk-operations/"))!;
    expect(String(status[0])).toBe(`/api/v1/platform/deployment-bulk-operations/${encodeURIComponent(sentKey)}`);
    expect(status[1].method ?? "GET").toBe("GET");
    expect(fetch.mock.calls.filter(([url, init]) => String(url).endsWith("/deployment-bulk-operations") && init.method === "POST")).toHaveLength(1);
  });
  it("after an unknown outcome only the status is read: no new review or post under the key", async () => {
    let statusCalls = 0;
    const pending = { ...operation, completed: false, replayed: true, items: [{ ...operation.items[0] }, { id: "b", expected_revision: "1", outcome: null, reason: null, revision: null, change_id: null }] };
    const done = { ...operation, replayed: true, items: [{ ...operation.items[0] }, { id: "b", expected_revision: "1", outcome: "unattempted", reason: null, revision: null, change_id: null }] };
    const fetch = vi.fn().mockImplementation((url: string) => {
      if (String(url).endsWith("/deployment-bulk-operations")) return Promise.resolve(Response.json({ error: { code: "503", message: "Storage unavailable" } }, { status: 503 }));
      if (String(url).includes("/deployment-bulk-operations/")) return Promise.resolve(Response.json(statusCalls++ === 0 ? pending : done));
      return Promise.resolve(Response.json(previewFetch(url)));
    });
    vi.stubGlobal("fetch", fetch);
    const user = view();
    await user.click(await screen.findByRole("button", { name: "Disable 2 routes" }));
    await screen.findByText("Outcome unknown");
    expect(screen.getByRole("button", { name: "Disable 2 routes" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Refresh review" }).hasAttribute("disabled")).toBe(true);
    await user.click(screen.getByRole("button", { name: "Check outcome" }));
    await screen.findByText("Still in progress");
    expect(screen.getByText("route-b: No outcome yet")).toBeTruthy();
    await user.click(screen.getByRole("button", { name: "Check outcome" }));
    await screen.findByText("route-b: Not attempted: not disabled");
    expect(screen.queryByText("Still in progress")).toBeNull();
    expect(fetch.mock.calls.filter(([url]) => String(url).endsWith("/deployment-bulk-operations"))).toHaveLength(1);
  });
  it("a reused key is an unknown outcome; a key the gateway never recorded may be sent again", async () => {
    let posts = 0;
    const fetch = vi.fn().mockImplementation((url: string) => {
      if (String(url).endsWith("/deployment-bulk-operations")) return Promise.resolve(posts++ === 0 ? Response.json({ error: { code: "409", reason: "idempotency_key_reused", message: "This idempotency key was used for different targets; use a new key." } }, { status: 409 }) : Response.json(operation));
      if (String(url).includes("/deployment-bulk-operations/")) return Promise.resolve(Response.json({ error: { code: "404", message: "Not found" } }, { status: 404 }));
      return Promise.resolve(Response.json(previewFetch(url)));
    });
    vi.stubGlobal("fetch", fetch);
    const user = view();
    await user.click(await screen.findByRole("button", { name: "Disable 2 routes" }));
    await screen.findByText("Outcome unknown");
    await user.click(screen.getByRole("button", { name: "Check outcome" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Disable 2 routes" }).hasAttribute("disabled")).toBe(false));
    await user.click(screen.getByRole("button", { name: "Disable 2 routes" }));
    await screen.findByText("route-a: Disabled");
  });
  it("warns when routes of one model are selected together", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation((url: string) => Promise.resolve(Response.json(previewFetch(url)))));
    render(<QueryClientProvider client={testClient()}><BulkDisableDialog targets={targets.map(t => ({ ...t, model: { id: "m2", name: "company/fast" } }))} onClose={() => {}} onDone={() => {}} /></QueryClientProvider>);
    await screen.findByText("Several routes of one model");
    expect(screen.getByText(/company\/fast/)).toBeTruthy();
  });
  it("cannot submit while the review is missing or refused", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => Promise.resolve(Response.json({ error: { code: "403", message: "Not allowed" } }, { status: 403 }))));
    view();
    await screen.findByText("Access denied");
    await waitFor(() => expect(screen.getByRole("button", { name: "Disable 2 routes" }).hasAttribute("disabled")).toBe(true));
  });
});
