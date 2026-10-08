// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { usagePeriod, usageQuery } from "../lib/usage";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { dashboardRouteTree } from "../router";
import { abortRequests } from "../lib/api";
import { platformPath, type Model, type PlatformOverviewData } from "../lib/api";
import { DashboardNavigationProvider } from "./navigation-link";
import { ActionProvider } from "./ui";
import { AddModel, conflictErrors } from "../pages/model-setup";
import { PlatformOverview } from "../pages/start";
import { Models, Providers } from "../pages/catalog";
import { ModelDetail, ProviderDetail, ModelReadinessChecklist } from "../pages/catalog-details";
import { admin, auditor, model, provider, markup, testClient } from "../lib/test-fixtures";

const connections = [{ ...provider, id: "c1", name: "Local vLLM", provider: "vllm", enabled: true }, { ...provider, id: "c2", name: "Cloud", enabled: false }];
const catalogs = [{ id: "cat1", name: "Approved", description: null }];
const readiness = { routes: 2, enabled_routes: 1, priced_enabled_routes: 0, catalogs: 0, direct_workspaces: 0, connections: [{ id: "c1", name: "Local vLLM" }] };
beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, writable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { abortRequests(); cleanup(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

function mountAddModel(fetch: ReturnType<typeof vi.fn>, connection?: string, session = admin) {
  vi.stubGlobal("fetch", fetch);
  const client = testClient(), navigate = vi.fn(), user = userEvent.setup();
  client.setQueryData(["api", undefined, `${platformPath}/providers`, "choices"], connections);
  client.setQueryData(["api", undefined, `${platformPath}/catalogs`, "choices"], catalogs);
  render(<QueryClientProvider client={client}><DashboardNavigationProvider search={{ page: "model-new", connection }} navigate={navigate}><ActionProvider><AddModel session={session} connection={connection} /></ActionProvider></DashboardNavigationProvider></QueryClientProvider>);
  return { client, navigate, user };
}
const created = () => Promise.resolve(Response.json({ model_id: "new-model", deployment_id: "new-route", price_id: null }, { status: 201 }));
const form = () => screen.getByRole("form", { name: "Add model" });

describe("Add model page", () => {
  it("lays out Grounded form sections with a back link, starting from ?connection=", async () => {
    mountAddModel(vi.fn());
    for (const section of ["Source", "Identity", "Availability", "Pricing", "Status"]) expect(screen.getByRole("group", { name: section })).toBeTruthy();
    expect(screen.getByRole("link", { name: "Back to Models" }).getAttribute("href")).toBe("/admin/models");
    cleanup(); mountAddModel(vi.fn(), "c2");
    expect((screen.getByRole("combobox", { name: /Connection/ }) as HTMLSelectElement).value).toBe("c2");
    expect(screen.getByText(/this connection is disabled/)).toBeTruthy();
    expect((screen.getByRole("switch", { name: /Enable the model/ }) as HTMLElement).getAttribute("aria-checked")).toBe("false");
    expect((screen.getByRole("radio", { name: "Leave unpriced" }) as HTMLInputElement).getAttribute("aria-checked")).toBe("true");
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("combobox", { name: /Connection/ })));
  });
  it("derives the API name from the upstream model ID, not the display name, until it is edited", async () => {
    const { user } = mountAddModel(vi.fn());
    const upstream = screen.getByRole("textbox", { name: /Upstream model ID/ }), display = screen.getByRole("textbox", { name: /Display name/ }), apiName = screen.getByRole("textbox", { name: /API model name/ }) as HTMLInputElement;
    // Live acceptance F1: "Claude Haiku 5.5" must not turn claude-haiku-5-5 into claude-haiku-5.5.
    await user.type(upstream, "claude-haiku-5-5");
    await user.type(display, "Claude Haiku 5.5");
    expect(apiName.value).toBe("claude-haiku-5-5");
    await user.clear(upstream); await user.type(upstream, "Meta/Llama-3");
    expect(apiName.value).toBe("meta/llama-3");
    await user.clear(apiName); await user.type(apiName, "company/llama");
    await user.type(upstream, "-chat"); await user.type(display, "!");
    expect(apiName.value).toBe("company/llama");
    expect(form().getAttribute("data-dirty")).toBe("true");
  });
  it("shows field errors, then submits one exact transaction and opens the model", async () => {
    const fetch = vi.fn().mockImplementation(created), { user, navigate } = mountAddModel(fetch);
    await user.click(screen.getByRole("button", { name: "Add model" }));
    expect(fetch).not.toHaveBeenCalled();
    expect(screen.getAllByText(/is required/).length).toBeGreaterThan(0);
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("textbox", { name: /Upstream model ID/ })));
    await user.type(screen.getByRole("textbox", { name: /Upstream model ID/ }), "llama3");
    await user.type(screen.getByRole("textbox", { name: /Display name/ }), "Llama 3");
    await user.click(screen.getByRole("checkbox", { name: "Approved" }));
    await user.click(screen.getByRole("radio", { name: "Set a price now" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "Input tokens price" }), "priced");
    await user.type(screen.getByRole("textbox", { name: "$ per M input tokens" }), "0.123456");
    await user.selectOptions(screen.getByRole("combobox", { name: "Output tokens price" }), "priced");
    await user.type(screen.getByRole("textbox", { name: "$ per M output tokens" }), "9007199254.740993");
    await user.selectOptions(screen.getByRole("combobox", { name: "Requests price" }), "free");
    await user.type(screen.getByRole("textbox", { name: /input token ceiling/ }), "1000");
    await user.type(screen.getByRole("textbox", { name: /output token ceiling/ }), "10");
    await user.click(screen.getByRole("switch", { name: /Enable the model/ }));
    await user.click(screen.getByRole("button", { name: "Add model" }));
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ page: "model-detail", record: "new-model" }));
    const [path, init] = fetch.mock.calls[0];
    expect(path).toBe(`${platformPath}/model-setup`);
    expect(init.method).toBe("POST");
    const body = JSON.parse(init.body);
    expect(body).toMatchObject({ model: { public_name: "llama3", display_name: "Llama 3", description: null, supported_protocols: ["chat_completions"], enabled: true }, route: { provider_connection_id: "c1", upstream_model: "llama3", enabled: true }, catalog_ids: ["cat1"], price: { input_token_limit: 1000, output_token_limit: 10, pricing_version: 3 } });
    expect(body.price.price_lines).toContainEqual({ meter: "input_tokens", microusd_per_batch: "123456", batch: 1000000, unit_label: "/M tokens", sku_label: "Input" });
    expect(body.price.price_lines).toContainEqual({ meter: "output_tokens", microusd_per_batch: "9007199254740993", batch: 1000000, unit_label: "/M tokens", sku_label: "Output" });
    expect(body.price.price_lines).toContainEqual({ meter: "requests", microusd_per_batch: "0", batch: 1, unit_label: "/request", sku_label: "Request" });
    expect(body.price.price_lines.some((l: { meter: string }) => l.meter === "cache_read_tokens")).toBe(false);
    expect(form().getAttribute("data-dirty")).toBeNull();
  });
  it("maps a 409 to API name and connection fields, then retries after a fix", async () => {
    let setups = 0;
    const fetch = vi.fn().mockImplementation((path: string) => path.startsWith(`${platformPath}/providers`) ? Promise.resolve(Response.json({ data: connections, has_more: false })) : setups++ ? created() : Promise.resolve(Response.json({ error: { code: "409", message: "Resource conflicts with existing configuration" } }, { status: 409 })));
    const { user, navigate } = mountAddModel(fetch);
    await user.type(screen.getByRole("textbox", { name: /Upstream model ID/ }), "llama3");
    await user.type(screen.getByRole("textbox", { name: /Display name/ }), "Taken");
    await user.click(screen.getByRole("button", { name: "Add model" }));
    await screen.findByText(conflictErrors.public_name);
    expect(screen.getByText(conflictErrors.provider_connection_id)).toBeTruthy();
    expect(navigate).not.toHaveBeenCalled();
    const apiName = screen.getByRole("textbox", { name: /API model name/ });
    await user.clear(apiName); await user.type(apiName, "taken-2");
    expect(screen.queryByText(conflictErrors.public_name)).toBeNull();
    await user.click(screen.getByRole("button", { name: "Add model" }));
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ page: "model-detail", record: "new-model" }));
    const posts = fetch.mock.calls.filter(call => call[0] === `${platformPath}/model-setup`);
    expect(posts).toHaveLength(2); expect(fetch.mock.calls.some(call => String(call[0]).startsWith(`${platformPath}/providers`))).toBe(true);
    expect(JSON.parse(posts[1][1].body).model.public_name).toBe("taken-2");
    expect(JSON.parse(posts[1][1].body).price).toBeNull();
  });
  it("keeps values after a server error so the same request can be retried", async () => {
    const fetch = vi.fn().mockResolvedValueOnce(Response.json({ error: { code: "503", message: "Management storage unavailable" } }, { status: 503 })).mockImplementationOnce(created);
    const { user, navigate } = mountAddModel(fetch);
    await user.type(screen.getByRole("textbox", { name: /Upstream model ID/ }), "llama3");
    await user.type(screen.getByRole("textbox", { name: /Display name/ }), "Llama");
    await user.click(screen.getByRole("button", { name: "Add model" }));
    await screen.findByText("Management storage unavailable");
    await user.click(screen.getByRole("button", { name: "Add model" }));
    await waitFor(() => expect(navigate).toHaveBeenCalled());
    expect(fetch.mock.calls[0][1].body).toBe(fetch.mock.calls[1][1].body);
  });
  it("gives Auditors no form", () => { mountAddModel(vi.fn(), undefined, auditor); expect(screen.getByRole("heading", { name: "Access not available" })).toBeTruthy(); expect(screen.queryByRole("form")).toBeNull(); expect(screen.queryByRole("button", { name: "Add model" })).toBeNull(); });
});

describe("Add model route", () => {
  it("guards unsaved input on the back link and keeps the page until confirmed", async () => {
    Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] });
    const fetch = vi.fn().mockImplementation((path: string) => path === "/api/v1/me" ? Promise.resolve(Response.json(admin)) : path.startsWith(`${platformPath}/providers`) ? Promise.resolve(Response.json({ data: connections, has_more: false })) : path.startsWith(`${platformPath}/catalogs`) ? Promise.resolve(Response.json({ data: catalogs, has_more: false })) : Promise.resolve(Response.json({ data: [], has_more: false })));
    vi.stubGlobal("fetch", fetch);
    const router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: ["/admin/models/new?connection=c2"] }) }), client = testClient(), user = userEvent.setup();
    await router.load(); render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
    await screen.findByRole("heading", { name: "Add model", level: 1 });
    expect(document.title).toContain("Add model");
    await user.type(await screen.findByRole("textbox", { name: /Display name/ }), "Draft");
    await user.click(screen.getByRole("link", { name: "Back to Models" }));
    await screen.findByRole("alertdialog", { name: "Leave without saving?" });
    expect(router.state.location.pathname).toBe("/admin/models/new");
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    expect((screen.getByRole("textbox", { name: /Display name/ }) as HTMLInputElement).value).toBe("Draft");
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    await user.click(await screen.findByRole("button", { name: "Leave without saving" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/admin/models"));
    client.clear();
  });
});

const overview: PlatformOverviewData = { setup: { connections: 1, enabled_connections: 1, models: 3, ready_models: 0, enabled_routes: 2, priced_enabled_routes: 2, catalogs: 1, type_defaults: { personal: 0, team: 0, project: 0 }, entitled_users: 1, oidc_mappings: 0 }, glance: { entitled_users: 1, teams: 2, projects: 1, ready_models: 0, attempts_7d: "12345678901234567890", known_cost_7d_microusd: "9007199254740993" } };
describe("Admin overview", () => {
  const path = `${platformPath}/overview`;
  it("shows the setup checklist with progress and per-step actions, then server-only glance numbers", () => {
    // Spend and requests cover this month, like Usage & costs (review #25), from the platform usage overview.
    const month = `${platformPath}/usage/overview?${usageQuery(usagePeriod({}))}`, usage = { tiles: { spend: { value: "9007199254740993" }, requests: { attempts: "12345678901234567890" } } };
    const html = markup(<PlatformOverview session={admin} />, [[path, overview], [month, usage]]);
    expect(html).toContain("Spend, this month"); expect(html).toContain("Requests (incl. retries), this month");
    expect(markup(<PlatformOverview session={admin} />, [[path, overview]])).not.toContain("$0.00");
    expect(html).toContain("Set up this install"); expect(html).toContain("3 of 6 done"); expect(html).toContain('aria-valuenow="3"');
    expect(html).toContain('href="/admin/catalogs"'); expect(html).toContain('href="/admin/sso-groups"');
    expect(html).toContain("Platform at a glance"); expect(html).toContain("$9,007,199,254.740993"); expect(html).toContain("12,345,678,901,234,567,890");
  });
  it("shows installation budgets with amount, used, spent and on hold per period", () => {
    const html = markup(<PlatformOverview session={admin} />, [[path, { ...overview, installation_budgets: [{ period: "month", amount_microusd: "9007199254740993", used_microusd: "8100", settled_microusd: "8000", held_microusd: "100", unresolved_usage: false, exhausted: false, window_start: "2026-10-01T00:00:00Z", window_end: "2026-11-01T00:00:00Z" }, { period: "day", amount_microusd: "5000000", used_microusd: "5000000", settled_microusd: "5000000", held_microusd: "0", unresolved_usage: false, exhausted: true, window_start: "2026-10-08T00:00:00Z", window_end: "2026-10-09T00:00:00Z" }] }]]);
    for (const text of ["Installation budgets", "Installation daily budget (used up)", "$0.0081 / $9,007,199,254.740993 · Monthly", "Spent $0.008 · on hold $0.0001 · resets Nov 1, 2026"]) expect(html).toContain(text);
    expect(html.indexOf("Installation daily budget")).toBeLessThan(html.indexOf("Installation monthly budget"));
    expect(markup(<PlatformOverview session={admin} />, [[path, overview]])).not.toContain("Installation budgets");
  });
  it("is read-only for Auditors: progress without step actions", () => { const html = markup(<PlatformOverview session={auditor} />, [[path, overview]]); expect(html).toContain("3 of 6 done"); expect(html).not.toContain(">Catalog defaults<"); expect(html).not.toContain(">SSO groups<"); expect(html).toContain("read-only"); });
  it("keeps a visible done state when setup is complete, with no primary next step", () => { const done = { ...overview, setup: { ...overview.setup, ready_models: 1, type_defaults: { personal: 1, team: 1, project: 1 }, oidc_mappings: 1 } }; const html = markup(<PlatformOverview session={admin} />, [[path, done]]); expect(html).not.toContain("Set up this install"); expect(html).toContain("Setup complete"); expect(html).toContain("Platform at a glance"); expect(html).not.toContain('data-variant="primary"'); });
  it("makes the next setup step the page's one primary button", () => { const html = markup(<PlatformOverview session={admin} />, [[path, overview]]); expect(html.match(/data-variant="primary"/g)).toHaveLength(1); expect(html).toMatch(/<h1[^>]*>Overview<\/h1>/); });
  it("shows an error instead of zeros when the overview is unavailable", () => { const client = testClient(); client.getQueryCache().build(client, { queryKey: ["api", undefined, path] }).setState({ status: "error", error: new Error("Overview unavailable"), fetchStatus: "idle" }); const html = markup(<PlatformOverview session={admin} />, [], client); expect(html).toContain("Overview unavailable"); expect(html).not.toContain("$0.00"); client.clear(); });
});

describe("Models and connections lists", () => {
  const withReadiness: Model = { ...model, readiness };
  it("shows connections, readiness warnings and the Add model link, filtered by connection", () => {
    const client = testClient(); client.setQueryData(["api", undefined, `${platformPath}/models?sort=name`, "choices"], [withReadiness, { ...withReadiness, id: "elsewhere", display_name: "Elsewhere model", readiness: { ...readiness, connections: [{ id: "c2", name: "Other" }] } }]);
    const html = markup(<DashboardNavigationProvider search={{ page: "models", connection: "c1" }} navigate={() => {}}><Models session={admin} /></DashboardNavigationProvider>, [], client);
    expect(html).not.toContain("Elsewhere model"); expect(html).toMatch(/role="status"[^>]*>1 model</);
    expect(html).toContain("Needs setup"); expect(html).toContain("Unpriced route · Not offered"); expect(html).toContain('href="/admin/connections/c1"'); expect(html).toContain('href="/admin/models/new?connection=c1"');
    expect(markup(<Models session={auditor} />)).not.toContain("Add model");
  });
  it("names Connections and counts their models without inventing zero", () => { const html = markup(<Providers session={admin} />, [[`${platformPath}/providers?limit=50&offset=0`, { data: [{ ...provider, model_count: 3 }, { ...provider, id: "old" }] }]]); expect(html).toContain(">Connections<"); expect(html).toContain('href="/admin/models?connections=provider"'); expect(html).toContain("Unknown"); expect(html).toContain("Add connection"); });
});

describe("Model and connection record pages", () => {
  it("connection page lists its models and offers Add model from it", () => {
    const client = testClient(); client.setQueryData(["api", undefined, `${platformPath}/models?provider_connection_id=${provider.id}`, "choices"], [{ ...model, readiness }]);
    const html = markup(<ProviderDetail session={admin} id={provider.id} onTabChange={() => {}} />, [[`${platformPath}/providers/${provider.id}`, { ...provider, model_count: 1 }]], client);
    for (const tab of ["Overview", "Models", "Settings"]) expect(html).toMatch(new RegExp(`role="tab"[^>]*>(?:(?!</button>).)*${tab}`)); expect(html).toContain(`href="/admin/models/new?connection=${provider.id}"`);
    const models = markup(<ProviderDetail session={admin} id={provider.id} tab="models" onTabChange={() => {}} />, [[`${platformPath}/providers/${provider.id}`, { ...provider, model_count: 1 }]], client);
    expect(models).toContain("Models on this connection"); expect(models).toContain(`href="/admin/models/${model.id}"`);
    const read = markup(<ProviderDetail session={auditor} id={provider.id} onTabChange={() => {}} />, [[`${platformPath}/providers/${provider.id}`, provider]], client);
    expect(read).not.toContain("Add model"); expect(read).not.toContain("Rotate credential reference"); client.clear();
  });
  it("model page is one long page: sticky section nav, header tiles, sections with in-place fixes", () => {
    const html = markup(<ModelDetail session={admin} id={model.id} onTabChange={() => {}} />, [[`${platformPath}/models/${model.id}`, { ...model, readiness }]]);
    for (const card of ["Overview", "Routes", "Pricing", "Routing policy", "Availability", "Protocols", "Usage"]) expect(html).toContain(`>${card}</h2>`);
    for (const id of ["overview", "routes", "pricing", "routing", "availability", "protocols", "usage"]) expect(html).toContain(`href="#${id}"`);
    expect(html).not.toContain('role="tab"'); expect(html).toContain('aria-label="On this page"');
    for (const tile of ["Type", "Input / output price", "Context ceilings", "Routes"]) expect(html).toContain(tile); expect(html).toContain('data-columns="4"'); // "Created" moved to Overview (review #47)
    expect(html).toContain("Text → Text"); expect(html).toContain("1 of 2 enabled"); expect(html).toContain("POST"); expect(html).toContain("/v1/chat/completions"); expect(html).toContain("Not enabled for this model: Responses, Messages");
    expect(html).toContain("Set prices"); expect(html).toContain("2 of 4 done"); expect(html).toContain("Back to Models");
    // Sections read as description lists with their own Edit; no inline forms (review #22).
    for (const label of ["Edit model details", "Edit routing policy", "Edit availability"]) expect(html).toContain(`aria-label="${label}"`);
    expect(html).not.toContain("<select"); expect(html).not.toContain("Save settings");
    const routes = markup(<ModelDetail session={admin} id={model.id} tab="routes" onTabChange={() => {}} />, [[`${platformPath}/models/${model.id}`, { ...model, readiness }]]);
    expect(routes).toContain('id="routes"'); expect(routes).toContain(">Routes</h2>");
  });
  it("readiness checklist reports unknown readiness rather than ready", () => { expect(markup(<ModelReadinessChecklist model={model} writable />)).toContain("unknown rather than ready"); const ready = markup(<ModelReadinessChecklist model={{ ...model, readiness: { ...readiness, priced_enabled_routes: 1, catalogs: 1 } }} writable />); expect(ready).toContain("Ready: enabled, routed, priced and offered."); });
});
