// @vitest-environment jsdom
/* Wave 2 contract adoption: key create with limits, named policy rejections inline, stacked replacement limits. */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { ActionProvider } from "./ui";
import { ReplacementLimitsDialog, ScopeLimits } from "./scope-limits";
import { CreateKeyDialog } from "../pages/keys";
import { abortRequests } from "../lib/api";
import { policy, session, team, testClient } from "../lib/test-fixtures";

beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

function mount(node: ReactNode, client = testClient()) { return { client, ...render(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>) }; }
type Handler = (method: string, url: string, body: unknown) => Response | undefined;
function serve(seed: [string, unknown][], handler: Handler) {
  const client = testClient();
  for (const [path, data] of seed) client.setQueryData(["api", undefined, path], data);
  const fetch = vi.fn(async (url: string, options: RequestInit = {}) => {
    const method = options.method ?? "GET", body = options.body ? JSON.parse(String(options.body)) : undefined;
    const reply = handler(method, url, body);
    if (reply) return reply;
    if (method === "GET") return Response.json({ data: [], has_more: false });
    throw new Error(`Unexpected ${method} ${url}`);
  });
  vi.stubGlobal("fetch", fetch);
  return { client, fetch, writes: () => fetch.mock.calls.filter(([, o]) => o?.method && o.method !== "GET").map(([url, o]) => ({ method: o!.method, url, body: o!.body ? JSON.parse(String(o!.body)) : undefined })) };
}
const reject = (status: number, reason: string, detail: Record<string, string> = {}) => Response.json({ error: { code: String(status), message: "server text", reason, ...detail } }, { status });
const platform = { ...policy, requests_per_minute: 60, budgets: [{ period: "month" as const, amount_microusd: "100000000" }], monthly_budget_microusd: "100000000", budget_period: "month" as const };
const wsPolicy = { policy, effective: platform, provenance: { platform_source: "type_default", platform, local: policy, key: null } };

describe("Create key sends its limits in the create request", () => {
  it("posts rate limits and stacked budgets with the key, never a follow-up policy PUT, and shows the returned policy", async () => {
    const user = userEvent.setup();
    const api = serve([["/api/v1/workspaces/team/policy", wsPolicy]], (method, url, body) => method === "POST" && url === "/api/v1/workspaces/team/keys"
      ? Response.json({ id: "new-key", token: "omg_secret_value", model_ids: null, policy: { requests_per_minute: (body as { requests_per_minute: number }).requests_per_minute, tokens_per_minute: null, concurrent_requests: null, budgets: (body as { budgets: unknown[] }).budgets, monthly_budget_microusd: "10000000", budget_period: "month" } }) : undefined);
    mount(<CreateKeyDialog session={session} workspace={team} options={[{ value: "model", label: "Smart model" }]} accounts={[]} onClose={() => {}} />, api.client);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "CI");
    await user.click(screen.getByRole("radio", { name: "$10" }));
    await user.click(screen.getByRole("button", { name: /Rate limits/ }));
    await user.type(screen.getByRole("textbox", { name: /^Requests per minute/ }), "30");
    await user.click(screen.getByRole("button", { name: "Create key" }));
    await screen.findByText("Save your API key");
    expect(api.writes()).toEqual([{ method: "POST", url: "/api/v1/workspaces/team/keys", body: { name: "CI", expires_in_days: 30, model_ids: null, requests_per_minute: 30, tokens_per_minute: null, concurrent_requests: null, concurrent_jobs: null, budgets: [{ period: "month", amount_microusd: "10000000" }] } }]);
    expect(screen.getByText(/Key limits: 30 RPM · \$10\.00 monthly/)).toBeTruthy();
    expect(screen.queryByText(/weren't saved/)).toBeNull();
    api.client.clear();
  });
  it("sends no limit fields when none are set (the key inherits)", async () => {
    const user = userEvent.setup();
    const api = serve([["/api/v1/workspaces/team/policy", wsPolicy]], (method, url) => method === "POST" && url === "/api/v1/workspaces/team/keys" ? Response.json({ id: "k", token: "omg_x", model_ids: null, policy: null }) : undefined);
    mount(<CreateKeyDialog session={session} workspace={team} options={[{ value: "model", label: "Smart model" }]} accounts={[]} onClose={() => {}} />, api.client);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "Laptop");
    await user.click(screen.getByRole("button", { name: "Create key" }));
    await screen.findByText("No key limits: workspace and platform limits apply.");
    expect(api.writes()[0]!.body).toEqual({ name: "Laptop", expires_in_days: 30, model_ids: null });
    api.client.clear();
  });
  it("shows a named rejection on the field it concerns and creates nothing", async () => {
    const user = userEvent.setup();
    const api = serve([["/api/v1/workspaces/team/policy", wsPolicy]], (method) => method === "POST" ? reject(400, "exceeds_parent_rate", { limit: "tokens_per_minute" }) : undefined);
    mount(<CreateKeyDialog session={session} workspace={team} options={[{ value: "model", label: "Smart model" }]} accounts={[]} onClose={() => {}} />, api.client);
    await user.type(screen.getByRole("textbox", { name: "Name" }), "CI");
    await user.click(screen.getByRole("button", { name: /Rate limits/ }));
    await user.type(screen.getByRole("textbox", { name: /^Tokens per minute/ }), "5000");
    await user.click(screen.getByRole("button", { name: "Create key" }));
    expect(await screen.findByText("Tokens per minute is higher than an inherited limit. Lower it to at most the inherited value.")).toBeTruthy();
    expect(screen.queryByText("Save your API key")).toBeNull();
    // Editing the field clears the server's message.
    await user.type(screen.getByRole("textbox", { name: /^Tokens per minute/ }), "0");
    expect(screen.queryByText(/Tokens per minute is higher than an inherited limit/)).toBeNull();
    api.client.clear();
  });
});

describe("Policy rejections in the limits editor", () => {
  it("places exceeds_parent_budget on that period's budget row", async () => {
    const user = userEvent.setup(), path = "/api/v1/workspaces/team/policy";
    const api = serve([[path, wsPolicy]], (method, url) => method === "PUT" && url === path ? reject(400, "exceeds_parent_budget", { period: "day" }) : undefined);
    mount(<ScopeLimits mode="local" path={path} writable />, api.client);
    await user.click(screen.getByRole("button", { name: "Add budget" }));
    await user.type(screen.getByRole("textbox", { name: /^Monthly budget \(USD\)/ }), "50");
    await user.click(screen.getByRole("button", { name: "Add budget" }));
    await user.type(screen.getByRole("textbox", { name: /^Daily budget \(USD\)/ }), "5");
    await user.click(screen.getByRole("button", { name: "Save limits" }));
    expect(await screen.findByText("The daily budget is higher than an inherited daily budget for the same period. Lower it to at most the inherited amount.")).toBeTruthy();
    expect(screen.getByText("Not saved: see the highlighted limit")).toBeTruthy();
    api.client.clear();
  });
  it("explains period_change_not_allowed and stored_rate_loosen_not_allowed", async () => {
    const user = userEvent.setup(), path = "/api/v1/workspaces/team/keys/k/policy";
    let reply = reject(403, "stored_rate_loosen_not_allowed", { limit: "requests_per_minute" });
    const api = serve([[path, { ...wsPolicy, provenance: { ...wsPolicy.provenance, key: policy } }]], (method) => method === "PUT" ? reply : undefined);
    mount(<ScopeLimits mode="key" path={path} writable />, api.client);
    await user.type(screen.getByRole("textbox", { name: /^Requests per minute/ }), "10");
    await user.click(screen.getByRole("button", { name: "Save limits" }));
    expect(await screen.findByText("The saved requests per minute limit can only be lowered, never raised or removed.")).toBeTruthy();
    reply = reject(403, "period_change_not_allowed", { period: "week" });
    await user.clear(screen.getByRole("textbox", { name: /^Requests per minute/ })); await user.type(screen.getByRole("textbox", { name: /^Requests per minute/ }), "9");
    await user.click(screen.getByRole("button", { name: "Save limits" }));
    expect(within(await screen.findByRole("alert")).getByText("The saved weekly budget can't be removed or moved to another period. Keep it; you can lower its amount.")).toBeTruthy();
    api.client.clear();
  });
});

describe("Admin › Costs › Set custom limits", () => {
  it("edits the workspace override with stacked budgets, prefilled from the live type defaults", async () => {
    const user = userEvent.setup(), path = "/api/v1/platform/workspaces/w1/policy", onClose = vi.fn();
    const typeDefault = { ...policy, requests_per_minute: 60, budgets: [{ period: "month" as const, amount_microusd: "100000000" }], monthly_budget_microusd: "100000000", budget_period: "month" as const };
    const current = { policy, effective: typeDefault, mode: "inherit", provenance: { platform_source: "type_default", platform: typeDefault, local: policy, key: null, type_default: typeDefault } };
    const api = serve([[path, current]], (method, url) => method === "PUT" && url === path ? Response.json({ ok: true }) : method === "GET" && url === path ? Response.json(current) : undefined);
    mount(<ReplacementLimitsDialog workspaceId="w1" name="Data science" onClose={onClose} />, api.client);
    expect(screen.getByText("Set custom limits · Data science")).toBeTruthy();
    expect((screen.getByRole("textbox", { name: /^Requests per minute/ }) as HTMLInputElement).value).toBe("60");
    expect((screen.getByRole("textbox", { name: /^Monthly budget \(USD\)/ }) as HTMLInputElement).value).toBe("100.00");
    await user.click(screen.getByRole("button", { name: "Add budget" }));
    await user.type(screen.getByRole("textbox", { name: /^Daily budget \(USD\)/ }), "2.5");
    await user.click(screen.getByRole("button", { name: "Save custom limits" }));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(api.writes()).toEqual([{ method: "PUT", url: path, body: { requests_per_minute: 60, tokens_per_minute: null, concurrent_requests: null, concurrent_jobs: null, budgets: [{ period: "day", amount_microusd: "2500000" }, { period: "month", amount_microusd: "100000000" }] } }]);
    api.client.clear();
  });
});
