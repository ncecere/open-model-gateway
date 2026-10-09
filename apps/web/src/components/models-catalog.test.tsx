// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { platformPath, type Model } from "../lib/api";
import type { DashboardSearch } from "../lib/permissions";
import { DashboardNavigationProvider } from "./navigation-link";
import { ActionProvider } from "./ui";
import { Models, WorkspaceModels, catalogQueryPaths } from "../pages/model-catalog";
import { WorkspaceModelPage } from "../pages/workspace-model";
import { admin, auditor, grant, markup, member, model, personal, session, team, testClient } from "../lib/test-fixtures";

afterEach(() => { cleanup(); vi.restoreAllMocks(); });
const readiness = { routes: 1, enabled_routes: 1, priced_enabled_routes: 1, catalogs: 1, direct_workspaces: 0, connections: [{ id: "c1", name: "OpenAI prod" }] };
const rows: (Model & Record<string, unknown>)[] = [
  { ...model, readiness, workload: "generation", min_input_microusd_per_million: "8100", created_at: "2026-01-01T00:00:00Z" },
  { ...model, id: "embed", public_name: "company/embed", display_name: "Embedder", supported_protocols: ["embeddings"], readiness: { ...readiness, connections: [{ id: "c2", name: "Local" }] }, workload: "embeddings", min_input_microusd_per_million: null, created_at: "2026-02-01T00:00:00Z" },
  { ...model, id: "img", public_name: "company/image", display_name: "Imager", supported_protocols: ["images"], enabled: false, readiness, workload: "images", min_input_microusd_per_million: null },
];
function client(search: DashboardSearch = { page: "models" }) {
  const c = testClient();
  for (const path of catalogQueryPaths(search)) c.setQueryData(["api", undefined, path, "choices"], rows);
  c.setQueryData(["api", undefined, `${platformPath}/providers`, "choices"], [{ id: "c1", name: "OpenAI prod", provider: "openai", endpoint: null, region: null, enabled: true, auth_mode: "credential" }, { id: "c2", name: "Local", provider: "vllm", endpoint: null, region: null, enabled: true, auth_mode: "none" }]);
  return c;
}
const page = (search: DashboardSearch, session = admin, navigate = vi.fn()) => markup(<DashboardNavigationProvider search={search} navigate={navigate}><Models session={session} /></DashboardNavigationProvider>, [], client(search));

describe("Admin Models catalog", () => {
  it("lets a long connection name wrap (two lines, full names as the tooltip) instead of truncating", () => {
    const search: DashboardSearch = { page: "models" }, c = client(search), long = "Local compatible — disabled demo server in the basement";
    for (const path of catalogQueryPaths(search)) c.setQueryData(["api", undefined, path, "choices"], [{ ...rows[0], readiness: { ...readiness, connections: [{ id: "c2", name: long }] } }]);
    document.body.innerHTML = markup(<DashboardNavigationProvider search={search} navigate={vi.fn()}><Models session={admin} /></DashboardNavigationProvider>, [], c);
    const link = screen.getByRole("link", { name: long }), wrap = link.parentElement!;
    expect(wrap.getAttribute("title")).toBe(long); expect(wrap.className).toMatch(/sourceWrap/);
    expect(link.closest("li")!.className).toMatch(/adminRow/);
    c.clear(); document.body.innerHTML = "";
  });
  it("shows no price (not 'Unpriced') for a model without an enabled route", () => {
    const search: DashboardSearch = { page: "models" }, c = client(search);
    const idle = { ...model, id: "idle", public_name: "demo/idle", display_name: "demo/idle", readiness: { ...readiness, enabled_routes: 0, priced_enabled_routes: 0, connections: [] }, workload: "generation", min_input_microusd_per_million: null };
    for (const path of catalogQueryPaths(search)) c.setQueryData(["api", undefined, path, "choices"], [idle]);
    const html = markup(<DashboardNavigationProvider search={search} navigate={vi.fn()}><Models session={admin} /></DashboardNavigationProvider>, [], c);
    expect(html).toContain("No enabled route, so no price yet"); expect(html).not.toContain(">Unpriced<");
    c.clear();
  });
  it("shows type tabs with counts, sort, list cards with exact prices and the Add model action", () => {
    const html = page({ page: "models" });
    for (const tab of ["All", "Text", "Embeddings", "Images"]) expect(html).toContain(`>${tab}<`);
    // Types with no models at all are hidden (All and the selected type stay).
    for (const tab of ["Speech to text", "Text to speech", "Rerank", "System One"]) expect(html).not.toContain(`>${tab}<`);
    expect(page({ page: "models", type: "rerank" })).toContain(">Rerank<");
    expect(html).toContain('aria-label="Model type"'); expect(html).toContain("Input price: low to high");
    // One toolbar row: Sort and View carry no visible labels of their own; the count sits after the tabs.
    expect(html).toContain('aria-label="Sort by"'); expect(html).not.toMatch(/>Sort<|>View</); expect(html).toMatch(/aria-label="List"[^>]*title="List"/);
    expect(html.indexOf('role="status"')).toBeLessThan(html.indexOf('aria-label="Filters"'));
    // Rows are compact: no descriptions or route/date prose, the why lives in tooltips.
    expect(html).not.toMatch(/route[s]? enabled|Added <|No enabled route<\/p>/);
    expect(html).toContain("$0.0081"); expect(html).not.toContain("$0.01 "); // sub-cent never rounded
    expect(html).toContain("Input price unknown · not free"); // embeddings with no price
    expect(html).toContain("Priced per image");
    expect(html).toContain("Add model"); expect(html).toMatch(/role="status"[^>]*>3 models</);
    expect(page({ page: "models" }, auditor)).not.toContain("Add model");
  });
  it("applies URL facets: type, connection and status, with server data policy and price ceiling in the query", () => {
    expect(catalogQueryPaths({ page: "models", q: "smart", sort: "price", max_price: "0.15", policy: "deny,unknown", deprecated: "hide" })).toEqual([
      `${platformPath}/models?q=smart&sort=price&max_input_price=150000&include_deprecated=false&data_policy=deny`,
      `${platformPath}/models?q=smart&sort=price&max_input_price=150000&include_deprecated=false&data_policy=unknown`,
    ]);
    const embeddings = page({ page: "models", type: "embeddings" });
    expect(embeddings).toContain("Embedder"); expect(embeddings).not.toContain("Imager");
    const c1 = page({ page: "models", connections: "c1" });
    expect(c1).toContain("Smart model"); expect(c1).not.toContain("Embedder"); expect(c1).toContain('href="/admin/models/new?connection=c1"');
    const disabled = page({ page: "models", enabled: "false" });
    expect(disabled).toContain("Imager"); expect(disabled).not.toContain("Embedder");
    expect(page({ page: "models", type: "rerank" })).toContain("No models match");
  });
  it("renders the table view with Columns at the end of the toolbar row (density inside its menu)", () => {
    const html = page({ page: "models", layout: "table" });
    expect(html).toContain("<table"); expect(html).toContain("Columns"); expect(html).not.toContain('aria-label="Row density"');
    expect(html.indexOf("Columns")).toBeLessThan(html.indexOf("<table"));
    expect(page({ page: "models", layout: "table", cols: "connections" })).not.toContain(">Connections<");
  });
  it("writes tab and filter changes to the URL", async () => {
    const navigate = vi.fn(), user = userEvent.setup();
    render(<QueryClientProvider client={client()}><ActionProvider><DashboardNavigationProvider search={{ page: "models" }} navigate={navigate}><Models session={admin} /></DashboardNavigationProvider></ActionProvider></QueryClientProvider>);
    await user.click(screen.getByRole("tab", { name: /Embeddings/ }));
    expect(navigate).toHaveBeenLastCalledWith(expect.objectContaining({ page: "models", type: "embeddings" }));
    // Status is one of the least-used facets: behind "More filters" so the row fits one line.
    await user.click(screen.getByRole("button", { name: "More filters" }));
    await user.click(await screen.findByRole("button", { name: /^Disabled/ }));
    expect(navigate).toHaveBeenLastCalledWith(expect.objectContaining({ enabled: "false" }));
    await user.click(screen.getByRole("button", { name: "Table" }));
    expect(navigate).toHaveBeenLastCalledWith(expect.objectContaining({ layout: "table" }));
  });
});

describe("Models load errors (unknown is not zero)", () => {
  it("shows one error and no fabricated counts in tabs or the summary", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json({ error: { code: "503", message: "Unavailable" } }, { status: 503 })));
    const c = testClient();
    render(<QueryClientProvider client={c}><ActionProvider><DashboardNavigationProvider search={{ page: "models" }} navigate={vi.fn()}><Models session={admin} /></DashboardNavigationProvider></ActionProvider></QueryClientProvider>);
    await screen.findAllByRole("button", { name: "Try again" });
    for (const tab of screen.getAllByRole("tab")) expect(tab.textContent).not.toMatch(/\d/);
    expect(screen.queryByText(/\b0 models\b/)).toBeNull();
    vi.unstubAllGlobals();
  });
});

describe("Workspace Models (eligible catalog)", () => {
  const catalog = [
    { model_id: model.id, public_name: model.public_name, display_name: model.display_name, description: null, protocols: ["chat_completions"], workload: "generation", eligibility: "selected", reason: "Added to this workspace from an available catalog", min_input_microusd_per_million: "100000", min_output_microusd_per_million: "400000", routes: 1 },
    { model_id: "avail", public_name: "company/avail", display_name: "Available one", description: null, protocols: ["chat_completions"], workload: "generation", eligibility: "available_from_catalog", reason: "In a catalog available to this workspace; not added yet", min_input_microusd_per_million: null, min_output_microusd_per_million: null, routes: 1 },
    { model_id: "direct", public_name: "company/direct", display_name: "Assigned one", description: null, protocols: ["embeddings"], workload: "embeddings", eligibility: "direct", reason: "Assigned to this workspace by a Platform Admin", min_input_microusd_per_million: "20000", min_output_microusd_per_million: null, routes: 1 },
  ];
  const ws = (workspace = team, who = session, search: DashboardSearch = { page: "grants", ws: workspace.id }) => { const c = testClient(); c.setQueryData(["api", undefined, `/api/v1/workspaces/${workspace.id}/catalog`, "choices"], catalog); c.setQueryData(["api", undefined, `/api/v1/workspaces/${workspace.id}/models`, "choices"], [grant]); return markup(<DashboardNavigationProvider search={search} navigate={() => {}}><WorkspaceModels session={who} workspace={workspace} /></DashboardNavigationProvider>, [], c); };
  it("shows \"—\" (not \"Unpriced\") for a model that isn't serving, in the list and the table, as Admin's list does", () => {
    const idle = { ...catalog[0]!, model_id: "idle", display_name: "Idle one", min_input_microusd_per_million: null, min_output_microusd_per_million: null, routes: 0 };
    for (const layout of [undefined, "table" as const]) {
      const c = testClient(); c.setQueryData(["api", undefined, `/api/v1/workspaces/${team.id}/catalog`, "choices"], [idle]); c.setQueryData(["api", undefined, `/api/v1/workspaces/${team.id}/models`, "choices"], []);
      const html = markup(<DashboardNavigationProvider search={{ page: "grants", ws: team.id, layout }} navigate={() => {}}><WorkspaceModels session={session} workspace={team} /></DashboardNavigationProvider>, [], c);
      expect(html).toContain("No enabled route, so no price yet"); expect(html).not.toContain(">Unpriced<"); expect(html).not.toContain("Input price unknown · not free</span>");
      c.clear();
    }
  });
  it("shows the model's lab logo left of the title on the workspace model page", () => {
    const c = testClient(); c.setQueryData(["api", undefined, `/api/v1/workspaces/${team.id}/catalog`, "choices"], [{ ...catalog[0]!, public_name: "claude-haiku-5.5", display_name: "Claude Haiku 5.5" }]);
    const html = markup(<DashboardNavigationProvider search={{ page: "workspace-model", ws: team.id, record: model.id }} navigate={() => {}}><WorkspaceModelPage session={session} workspace={member} id={model.id} /></DashboardNavigationProvider>, [], c);
    expect(html).toMatch(/<h1[^>]*><span[^>]*data-title-icon[^>]*><span aria-hidden="true"[^>]*data-brand="claude"/);
    expect(html).toMatch(/Claude Haiku 5\.5<\/span><\/span><\/h1>/);
    c.clear();
  });
  it("lists only eligible models with eligibility badges, reasons, prices, Add and a row menu with Remove for admins", () => {
    const html = ws();
    for (const text of ["Added", "Available to add", "Assigned by admin", "Assigned to this workspace by a Platform Admin", "$0.10", "$0.40", "Add</button>", "Add models"]) expect(html).toContain(text);
    expect(html).not.toContain("Select</button>");
    expect(html.match(/aria-label="Actions for [^"]+"/g)).toHaveLength(1); // direct-only has no catalog selection to remove
    // Names open the read-only model page.
    expect(html).toContain('href="/workspaces/team/models/');
    expect(html).not.toContain("$0.00");
  });
  it("opens a read-only model page with how to call it, prices and Previous/Next (finding #8)", () => {
    const page = (id: string) => { const c = testClient(); c.setQueryData(["api", undefined, `/api/v1/workspaces/${team.id}/catalog`, "choices"], catalog); return markup(<DashboardNavigationProvider search={{ page: "workspace-model", ws: team.id, record: id }} navigate={() => {}}><WorkspaceModelPage session={session} workspace={member} id={id} /></DashboardNavigationProvider>, [], c); };
    const html = page(model.id);
    for (const text of [model.display_name, model.public_name, "How to call it", "/v1/chat/completions", "$0.10", "per M input tokens", "$0.40", "per M output tokens", "Back to Models", "Requests with this model"]) expect(html).toContain(text);
    expect(html).not.toMatch(/Routes|Connection|Routing policy/); // admin-only sections stay in Admin
    expect(html).toMatch(/Previous model|Next model/);
    const embed = page("direct");
    expect(embed).toContain("Not applicable"); expect(embed).toContain("/v1/embeddings");
    const missing = page("nope");
    expect(missing).toContain("Model not found"); expect(missing).toContain("doesn&#x27;t exist or you can&#x27;t see it");
  });
  it("is read-only for ordinary members and filters by eligibility from the URL", () => {
    const html = ws(member);
    expect(html).toContain("Available one"); expect(html).not.toContain("Add</button>"); expect(html).not.toContain('aria-label="Actions for');
    const added = ws(team, session, { page: "grants", ws: team.id, eligibility: "selected" });
    expect(added).toContain("Smart model"); expect(added).not.toContain("Available one");
  });
  it("describes the personal variant in one short line", () => { expect(ws(personal)).toContain("Models you can call from your personal workspace."); });
  it("offers Newest and orders by the catalog's created_at, showing when each model was added", () => {
    const dated = (id: string, created_at: string) => catalog.find(r => r.model_id === id) && { ...catalog.find(r => r.model_id === id)!, created_at };
    const rows = [dated(model.id, "2026-01-01T00:00:00Z"), dated("avail", "2026-09-01T00:00:00Z"), dated("direct", "2026-05-01T00:00:00Z")];
    const c = testClient(); c.setQueryData(["api", undefined, "/api/v1/workspaces/team/catalog", "choices"], rows); c.setQueryData(["api", undefined, "/api/v1/workspaces/team/models", "choices"], [grant]);
    const html = markup(<DashboardNavigationProvider search={{ page: "grants", ws: "team", sort: "newest" }} navigate={() => {}}><WorkspaceModels session={session} workspace={team} /></DashboardNavigationProvider>, [], c);
    expect(html).toMatch(/<option value="newest" selected="">Newest<\/option>/);
    expect(html.indexOf("Available one")).toBeLessThan(html.indexOf("Assigned one")); expect(html.indexOf("Assigned one")).toBeLessThan(html.indexOf("Smart model"));
    // The date is in the Table view's Added column, not the list row.
    const table = markup(<DashboardNavigationProvider search={{ page: "grants", ws: "team", sort: "newest", layout: "table" }} navigate={() => {}}><WorkspaceModels session={session} workspace={team} /></DashboardNavigationProvider>, [], c);
    expect(table).toContain(">Added<");
  });
});
