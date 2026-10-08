// @vitest-environment jsdom
/*
 * Catalog clarity (one list with who gets it, one Defaults matrix, catalog
 * pages with Models / Who gets it / Settings), Pricing folded into the Models
 * table, and read-only views of disabled workspaces for platform readers.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { abortRequests, platformPath, type Model } from "../lib/api";
import type { DashboardSearch } from "../lib/permissions";
import { canonicalSearch, dashboardHref, parseDashboardLocation } from "../lib/locations";
import { navigation } from "../lib/navigation";
import { reasonText } from "../lib/effective-access";
import { DashboardNavigationProvider } from "./navigation-link";
import { ActionProvider } from "./ui";
import { CatalogDetail, Catalogs, DefaultsMatrix, catalogDefaultsPath, type CatalogSummary } from "../pages/model-access";
import { Models, catalogQueryPaths, isUnpricedModel } from "../pages/model-catalog";
import { WorkspaceDetail } from "../pages/resource-details";
import { admin, auditor, markup, model, testClient } from "../lib/test-fixtures";

beforeEach(() => {
  document.cookie = "omg_csrf=test-csrf; Path=/";
  vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) });
});
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const catalogs: CatalogSummary[] = [
  { id: "approved", name: "Approved cloud", description: "Vetted cloud models", model_count: 2, models: [{ id: "m1", public_name: "openai/gpt-x", display_name: "GPT X", enabled: true }, { id: "m2", public_name: "anthropic/claude-y", display_name: "Claude Y", enabled: true }], default_for: ["personal", "team"], own_choice_count: 3 },
  { id: "local", name: "Local datacenter", description: null, model_count: 0, models: [], default_for: [], own_choice_count: 0 },
];
const defaults = { personal: ["approved"], team: ["approved"], project: [] as string[] };
function mount(node: ReactNode, client = testClient()) {
  return render(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>);
}
/** Serves GETs from fixtures and records writes. */
function serve(fixtures: Record<string, unknown>, choices: Record<string, unknown[]> = {}) {
  const client = testClient();
  for (const [path, data] of Object.entries(fixtures)) client.setQueryData(["api", undefined, path], data);
  for (const [path, data] of Object.entries(choices)) client.setQueryData(["api", undefined, path, "choices"], data);
  const fetch = vi.fn(async (url: string, options: RequestInit = {}) => {
    if (options.method && options.method !== "GET") return Response.json({ ok: true });
    const path = url.split("?")[0]!;
    if (path in choices) return Response.json({ data: choices[path] });
    if (path in fixtures) return Response.json(fixtures[path]);
    throw new Error(`Unexpected request: ${url}`);
  });
  vi.stubGlobal("fetch", fetch);
  return { client, writes: () => fetch.mock.calls.filter(([, o]) => o?.method && o.method !== "GET").map(([url, o]) => ({ url, method: o!.method, body: o!.body ? JSON.parse(String(o!.body)) : undefined })) };
}

describe("Catalogs: one list with who gets it", () => {
  it("shows each catalog with its description, model count and icons, and who-gets-it chips", () => {
    const client = testClient();
    client.setQueryData(["api", undefined, `${platformPath}/catalogs`, "choices"], catalogs);
    const html = markup(<Catalogs session={auditor} />, [], client);
    for (const text of ["A catalog is a set of approved models", "Approved cloud", "Vetted cloud models", "2 models", "Personal default", "Team default", "3 workspaces by own choice", "Local datacenter", "No models yet", "No one yet", 'href="/admin/catalogs/approved"']) expect(html).toContain(text);
    expect(html).not.toContain("Project default<");
    // One Defaults tab replaces the three per-type tabs; Auditors can't create catalogs.
    expect(html).toContain(">Defaults<"); for (const old of ["Personal defaults", "Team defaults", "Project defaults"]) expect(html).not.toContain(old);
    expect(html).not.toContain("Create catalog");
  });
  it("old per-type defaults links open the Defaults matrix", () => {
    const html = markup(<DashboardNavigationProvider search={{ page: "catalogs", tab: "team" }} navigate={vi.fn()}><Catalogs session={admin} /></DashboardNavigationProvider>, [[catalogDefaultsPath, defaults]], (() => { const c = testClient(); c.setQueryData(["api", undefined, `${platformPath}/catalogs`, "choices"], catalogs); return c; })());
    expect(html).toContain("Workspaces get the checked catalogs unless they choose their own.");
    expect(html).toContain("Default catalogs by workspace type");
  });
});

describe("Defaults matrix", () => {
  it("saves every type in one PUT, warning that unchecked catalogs retire models without resurrection", async () => {
    const user = userEvent.setup(), api = serve({ [catalogDefaultsPath]: defaults }, { [`${platformPath}/catalogs`]: catalogs });
    mount(<DefaultsMatrix session={admin} />, api.client);
    expect(screen.getByText("Workspaces get the checked catalogs unless they choose their own.")).toBeDefined();
    const box = (name: string) => screen.getByRole("checkbox", { name });
    expect(box("Personal default: Approved cloud").getAttribute("aria-checked")).toBe("true");
    expect(box("Project default: Local datacenter").getAttribute("aria-checked")).toBe("false");
    await user.click(box("Project default: Local datacenter"));
    await user.click(box("Team default: Approved cloud"));
    expect(screen.getByText(/Team › Approved cloud/)).toBeDefined();
    expect(screen.getByText(/aren't restored if you check them again/)).toBeDefined();
    await user.click(screen.getByRole("button", { name: "Save defaults" }));
    await waitFor(() => expect(api.writes()).toHaveLength(1));
    expect(api.writes()[0]).toEqual({ url: catalogDefaultsPath, method: "PUT", body: { personal: ["approved"], team: [], project: ["local"] } });
  });
  it("is read-only for Auditors", () => {
    const api = serve({ [catalogDefaultsPath]: defaults }, { [`${platformPath}/catalogs`]: catalogs });
    mount(<DefaultsMatrix session={auditor} />, api.client);
    expect(screen.getByRole("checkbox", { name: "Team default: Approved cloud" }).getAttribute("aria-disabled")).toBe("true");
    expect(screen.queryByRole("button", { name: "Save defaults" })).toBeNull();
  });
});

describe("Catalog page", () => {
  const detail: CatalogSummary = { ...catalogs[0]!, own_choice: { workspaces: [{ id: "t1", name: "Research team", kind: "team", disabled: false }, { id: "p1", name: "Old project", kind: "project", disabled: true }], personal_count: 2 } };
  const other: Model = { ...model, id: "m3", public_name: "company/new", display_name: "New model" };
  const linked: Model[] = [{ ...model, id: "m1", public_name: "openai/gpt-x", display_name: "GPT X" }];
  const fixtures = { [`${platformPath}/catalogs/approved`]: detail, [catalogDefaultsPath]: defaults };
  const choices = { [`${platformPath}/catalogs/approved/models`]: linked, [`${platformPath}/models`]: [...linked, other] };
  it("lists models and adds more through a picker (keeping the existing ones)", async () => {
    const user = userEvent.setup(), api = serve(fixtures, choices);
    mount(<CatalogDetail session={admin} id="approved" onTabChange={vi.fn()} />, api.client);
    expect(screen.getByRole("link", { name: "GPT X" }).getAttribute("href")).toBe("/admin/models/m1");
    await user.click(screen.getByRole("button", { name: "Add models" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).queryByText(/GPT X/)).toBeNull();
    await user.click(within(dialog).getByRole("checkbox", { name: /New model/ }));
    await user.click(within(dialog).getByRole("button", { name: "Add models" }));
    await waitFor(() => expect(api.writes()).toEqual([{ url: `${platformPath}/catalogs/approved/models`, method: "PUT", body: { model_ids: ["m1", "m3"] } }]));
  });
  it("removes a model only after the retirement warning", async () => {
    const user = userEvent.setup(), api = serve(fixtures, choices);
    mount(<CatalogDetail session={admin} id="approved" onTabChange={vi.fn()} />, api.client);
    await user.click(screen.getByRole("button", { name: "Remove" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(/Adding it back doesn't restore those choices/)).toBeDefined();
    await user.click(within(dialog).getByRole("button", { name: "Remove model" }));
    await waitFor(() => expect(api.writes()).toEqual([{ url: `${platformPath}/catalogs/approved/models`, method: "PUT", body: { model_ids: [] } }]));
  });
  it("Who gets it: type defaults saved through the matrix, and own-choice workspaces linked (personal only counted)", async () => {
    const user = userEvent.setup(), api = serve(fixtures, choices);
    mount(<CatalogDetail session={admin} id="approved" tab="workspaces" onTabChange={vi.fn()} />, api.client);
    expect(screen.getByRole("link", { name: "Research team" }).getAttribute("href")).toBe("/admin/teams/t1?tab=model-access");
    expect(screen.getByRole("link", { name: "Old project" }).getAttribute("href")).toBe("/admin/projects/p1?tab=model-access");
    expect(screen.getByText(/Plus 2 personal workspaces/)).toBeDefined();
    await user.click(screen.getByRole("checkbox", { name: "Project workspaces" }));
    await user.click(screen.getByRole("checkbox", { name: "Personal workspaces" }));
    expect(screen.getByText(/Personal workspaces using the defaults lose this catalog/)).toBeDefined();
    await user.click(screen.getByRole("button", { name: "Save defaults" }));
    await waitFor(() => expect(api.writes()).toEqual([{ url: catalogDefaultsPath, method: "PUT", body: { personal: [], team: ["approved"], project: ["approved"] } }]));
  });
  it("is read-only for Auditors", () => {
    const html = markup(<CatalogDetail session={auditor} id="approved" onTabChange={vi.fn()} />, [[`${platformPath}/catalogs/approved`, detail]], (() => { const c = testClient(); c.setQueryData(["api", undefined, `${platformPath}/catalogs/approved/models`, "choices"], linked); return c; })());
    expect(html).toContain("GPT X"); for (const control of ["Add models", "Remove</button>", "Delete catalog"]) expect(html).not.toContain(control);
  });
});

describe("Pricing lives in the Models table", () => {
  it("leaves the sidebar, and /admin/pricing redirects to the Models table with Unpriced only", () => {
    expect(navigation.some(item => (item.page as string) === "pricing")).toBe(false);
    const target = { page: "models", layout: "table", pricing: "unpriced" };
    expect(parseDashboardLocation("/admin/pricing")).toEqual(target);
    expect(parseDashboardLocation("/admin/pricing?status=unknown")).toEqual(target);
    expect(canonicalSearch({ page: "pricing" })).toEqual(target);
    expect(dashboardHref({ page: "pricing" })).toBe("/admin/models?layout=table&pricing=unpriced");
    expect(parseDashboardLocation("/admin/models?layout=table&pricing=unpriced")).toMatchObject(target);
  });
  const readiness = { routes: 2, enabled_routes: 2, priced_enabled_routes: 2, catalogs: 1, direct_workspaces: 0, connections: [{ id: "c1", name: "OpenAI prod" }] };
  const rows = [
    { ...model, readiness, workload: "generation", min_input_microusd_per_million: "8100" },
    { ...model, id: "bare", public_name: "company/bare", display_name: "Bare model", readiness: { ...readiness, priced_enabled_routes: 1 }, workload: "generation", min_input_microusd_per_million: null },
  ];
  function page(search: DashboardSearch, prices = true) {
    const c = testClient();
    for (const path of catalogQueryPaths(search)) c.setQueryData(["api", undefined, path, "choices"], rows);
    c.setQueryData(["api", undefined, `${platformPath}/providers`, "choices"], []);
    if (prices) {
      c.setQueryData(["api", undefined, `${platformPath}/deployments`, "choices"], [{ id: "d1", model_id: model.id, upstream_model: "gpt-x", enabled: true }, { id: "d2", model_id: model.id, upstream_model: "gpt-x-eu", enabled: true }, { id: "d3", model_id: "bare", upstream_model: "bare-up", enabled: true }, { id: "d4", model_id: "bare", upstream_model: "bare-up-2", enabled: true }]);
      c.setQueryData(["api", undefined, `${platformPath}/deployments/d1/prices?limit=1&offset=0`], { data: [{ id: "p1", pricing_version: 1, created_at: "2026-10-01T00:00:00Z", input_microusd_per_million: "8100", output_microusd_per_million: "500000", cache_pricing: null, input_token_limit: 1, output_token_limit: 1 }] });
      c.setQueryData(["api", undefined, `${platformPath}/deployments/d2/prices?limit=1&offset=0`], { data: [{ id: "p2", pricing_version: 1, created_at: "2026-10-01T00:00:00Z", input_microusd_per_million: "9000", output_microusd_per_million: "600000", cache_pricing: null, input_token_limit: 1, output_token_limit: 1 }] });
      c.setQueryData(["api", undefined, `${platformPath}/deployments/d3/prices?limit=1&offset=0`], { data: [] });
      c.setQueryData(["api", undefined, `${platformPath}/deployments/d4/prices?limit=1&offset=0`], { data: [{ id: "p4", pricing_version: 1, created_at: "2026-10-01T00:00:00Z", input_microusd_per_million: "1000000", output_microusd_per_million: "2000000", cache_pricing: null, input_token_limit: 1, output_token_limit: 1 }] });
    }
    return markup(<DashboardNavigationProvider search={search} navigate={vi.fn()}><Models session={admin} /></DashboardNavigationProvider>, [], c);
  }
  it("Table view shows exact input/output prices of the cheapest priced route and an Unpriced badge", () => {
    const html = page({ page: "models", layout: "table" });
    for (const text of [">Price<", ">Priced routes<", "$0.0081", "$0.50", "Cheapest of 2 priced routes", 'href="/admin/routes/d1"', "1 of 2 enabled", "Unpriced", "$1.00", "$2.00"]) expect(html).toContain(text);
    expect(html).not.toContain("$0.009"); // the dearer route of the same model isn't the headline
  });
  it("Unpriced only keeps models with an enabled route that has no price", () => {
    expect(isUnpricedModel(rows[1]!)).toBe(true); expect(isUnpricedModel(rows[0]!)).toBe(false); expect(isUnpricedModel({ readiness: undefined })).toBe(false);
    const html = page({ page: "models", layout: "table", pricing: "unpriced" }, false);
    expect(html).toContain("Bare model"); expect(html).not.toContain("Smart model");
    expect(html).toContain("Unpriced only"); expect(html).toContain("usage is recorded with unknown cost");
  });
});

describe("Disabled workspaces: read-only for platform readers", () => {
  const disabled = { id: "acc", name: "Acceptance Project", kind: "project", owner_user_id: null, cost_center_id: null, cost_center: null, member_count: 1, disabled_at: "2026-10-01T00:00:00Z", created_at: "2026-09-01T00:00:00Z" };
  const policy = { policy: { requests_per_minute: 10, tokens_per_minute: null, concurrent_requests: null, budgets: [] }, effective: { requests_per_minute: 10, tokens_per_minute: null, concurrent_requests: null, budgets: [] }, mode: "inherit", provenance: { platform_source: "type_default", platform: { requests_per_minute: 10, tokens_per_minute: null, concurrent_requests: null, budgets: [] }, local: { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, budgets: [] }, key: null, type_default: { requests_per_minute: 10, tokens_per_minute: null, concurrent_requests: null, budgets: [] } }, budgets: [] };
  const access = { workspace_id: "acc", key_id: null, workspace_disabled: true, truncated: false, summary: { available: 0, partial: 0, unavailable: 1 }, layers: [], models: [{ model_id: "m", public_name: "company/smart", display_name: "Smart model", status: "unavailable", reasons: [{ code: "workspace_disabled", layer: "platform" }] }] };
  const fixtures: [string, unknown][] = [[`${platformPath}/workspaces/acc`, disabled], [`${platformPath}/workspaces/acc/policy`, policy], ["/api/v1/workspaces/acc/access", access], [`${platformPath}/workspaces/acc/catalogs`, { mode: "inherit", catalog_ids: [], effective_catalog_ids: ["approved"] }], [`${platformPath}/workspace-types/project/catalogs`, { kind: "project", catalog_ids: ["approved"] }]];
  it("Limits shows the banner, read-only limits and effective access, never Not found", () => {
    const html = markup(<WorkspaceDetail session={admin} id="acc" kind="project" tab="limits" onTabChange={vi.fn()} />, fixtures);
    for (const text of ["Disabled", "read-only until the project is enabled", "Disabled: no models can be used", "Smart model"]) expect(html).toContain(text);
    expect(html).not.toContain("Not found"); expect(html).not.toContain("Save limits"); expect(html).not.toContain("Disable workspace…");
    expect(html).toMatch(/role="radiogroup" aria-disabled="true"/);
  });
  it("Models tab shows catalogs read-only; reasons explain the disabled workspace", () => {
    const client = testClient(); client.setQueryData(["api", undefined, `${platformPath}/catalogs`, "choices"], catalogs); client.setQueryData(["api", undefined, "/api/v1/workspaces/acc/models", "choices"], []);
    const html = markup(<WorkspaceDetail session={admin} id="acc" kind="project" tab="model-access" onTabChange={vi.fn()} />, fixtures, client);
    expect(html).toContain("Use Project defaults"); expect(html).not.toContain("Assign models"); expect(html).not.toContain("Not found");
    expect(html).toMatch(/role="radio"[^>]*aria-disabled="true"|aria-disabled="true"[^>]*role="radio"|data-disabled/);
    expect(reasonText({ code: "workspace_disabled", layer: "platform" }, "project")).toBe("This project is disabled, so no model can be used until a Platform Admin enables it.");
  });
  it("General keeps only the Enable action writable", () => {
    const client = testClient(); client.setQueryData(["api", undefined, `${platformPath}/cost-centers`, "choices"], []);
    const html = markup(<WorkspaceDetail session={admin} id="acc" kind="project" tab="settings" onTabChange={vi.fn()} />, fixtures, client);
    expect(html).toContain("Enable workspace"); expect(html).not.toContain(">Save<");
  });
});
