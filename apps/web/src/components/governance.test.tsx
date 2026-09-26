import { afterEach, describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { ApiError, type Session, type Organization, type Workspace } from "../lib/api";
import { ActionProvider, Button, FormField, Input, Panel, Heading, StatCard, Table } from "./ui";
import { Costs, Governance, PriceVersions, ModelRoutingPanel, DeploymentRoutingPanel } from "../pages/governance";
import { AppShell, Main, Sidebar, SidebarContent, SidebarItem, SidebarNav, SidebarSection, TopBar } from "./ui/app-shell/app-shell";
import { TooltipProvider } from "./ui/tooltip/tooltip";

const org: Organization = { id: "org", name: "Acme", slug: "acme", role: "member" };
const workspace: Workspace = { id: "ws", organization_id: "org", name: "Team", kind: "team", role: "member" };
const session: Session = { user: { id: "me", email: "me@example.org", platform_admin: false }, organizations: [org], workspaces: [workspace] };
const clients: QueryClient[] = [];
function render(node: ReactNode, entries: [string, unknown][] = [], errors: [string, Error][] = []) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, retryOnMount: false, staleTime: Infinity } } }); clients.push(client);
  for (const [path, data] of entries) client.setQueryData(["api", path], data);
  for (const [path, error] of errors) client.getQueryCache().build(client, { queryKey: ["api", path] }).setState({ status: "error", error, fetchStatus: "idle" });
  return renderToStaticMarkup(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>);
}
afterEach(() => { for (const client of clients) client.clear(); clients.length = 0; });
describe("governance screen rendering", () => {
  it("shows a member's workspace policy read-only without reading organization policy", () => {
    const html = render(<Governance session={session} organization={org} workspace={workspace} />, [["/api/v1/workspaces/ws/policy", { policy: { requests_per_minute: 20, tokens_per_minute: null, concurrent_requests: null, monthly_budget_microusd: "9007199254740993" }, ceiling: { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, monthly_budget_microusd: null } }]]);
    expect(html).toContain("Read-only"); expect(html).toContain("$9,007,199,254.740993");
    expect(html).not.toContain("Edit workspace policy"); expect(html).not.toContain("Organization limits</h2>");
    expect(html).toContain("full upstream input ceiling");
  });
  it("offers organization policy edits even without a selected workspace", () => {
    const html = render(<Governance session={session} organization={{ ...org, role: "admin" }} />, [["/api/v1/orgs/org/policy", { policy: { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, monthly_budget_microusd: null } }]]);
    expect(html).toContain("Edit additional organization limits"); expect(html).not.toContain("No workspace selected");
    expect(html).toContain("Clearing these fields never removes a platform maximum");
  });
  it("uses pill tabs and opens only workspace limits, not a stack of organization and key panels", () => {
    const html = render(<Governance session={session} organization={{ ...org, role: "admin" }} workspace={workspace} />, [["/api/v1/workspaces/ws/policy", { policy: { requests_per_minute: 20, tokens_per_minute: null, concurrent_requests: null, monthly_budget_microusd: "10000000" }, ceiling: { requests_per_minute: 120, tokens_per_minute: null, concurrent_requests: null, monthly_budget_microusd: "50000000" } }]]);
    expect(html).toContain('aria-label="Limit scope"'); expect(html).toContain('data-variant="pills"');
    expect(html).toContain('aria-label="Workspace policy details"');
    expect(html).toContain("Organization</button>"); expect(html).toContain("API keys</button>");
    expect(html).toContain("Effective limits"); expect(html).toContain("$10.00");
    expect(html).not.toContain("$50.00"); expect(html).not.toContain("Key policy selection");
    expect(html).not.toContain("Edit additional organization limits");
    const member = render(<Governance session={session} organization={org} workspace={workspace} />);
    expect(member).not.toContain("Organization</button>");
  });
  it("does not invent effective caps when inherited limits are unavailable", () => {
    const html = render(<Governance session={session} organization={org} workspace={workspace} />, [["/api/v1/workspaces/ws/policy", { policy: { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, monthly_budget_microusd: null } }]]);
    expect(html).toContain("Effective limits unavailable"); expect(html).not.toContain("No configured cap");
  });
  it("does not fabricate zero costs while loading or on forbidden reads", () => {
    const props = { session, organization: org, workspace };
    const loading = render(<Costs {...props} />);
    expect(loading).toContain("Loading cost summary"); expect(loading).not.toContain("$0");
    const denied = render(<Costs {...props} />, [], [["/api/v1/workspaces/ws/cost-summary", new ApiError(403, "forbidden", "No access")]]);
    expect(denied).toContain("Access denied"); expect(denied).not.toContain("$0");
  });
  it("separates known estimates from held and unknown usage without member reconcile controls", () => {
    const entries: [string, unknown][] = [
      ["/api/v1/workspaces/ws/cost-summary", { currency: "USD", known_cost_microusd: "1234567", held_microusd: "9999999", unknown_cost_requests: 2, requests: 10 }],
      ["/api/v1/workspaces/ws/costs?limit=100&offset=0", { data: [{ id: "execution", state: "failed", public_model: "model", provider: "openai", started_at: "2026-01-01T00:00:00Z", input_tokens: null, output_tokens: null, price_id: "price", cost_microusd: null, reserved_microusd: "9999999", cost_status: "unknown" }] }],
    ];
    const html = render(<Costs session={session} organization={org} workspace={workspace} />, entries);
    expect(html).toContain("Only activity from your own human keys"); expect(html).toContain("Estimates, not vendor invoices");
    expect(html).toContain("$1.234567"); expect(html).toContain("$9.999999"); expect(html).toContain("Unknown / Unknown");
    expect(html).not.toContain("Reconcile</button>");
    const operator = render(<Costs session={{ ...session, user: { ...session.user, platform_admin: true } }} organization={org} workspace={workspace} />, entries);
    expect(operator).toContain("Reconcile</button>");
  });
  it("shows immutable paginated price history with explicit publication capability", () => {
    const path = "/api/v1/platform/deployments/deployment/prices";
    const entries: [string, unknown][] = [[`${path}?limit=100&offset=0`, { data: [{ id: "price", input_microusd_per_million: "1000000", output_microusd_per_million: "2000000", input_token_limit: 128000, output_token_limit: 4096, created_at: "2026-01-01T00:00:00Z" }] }]];
    const html = render(<PriceVersions path={path} writable={false} />, entries);
    expect(html).toContain("Read-only access"); expect(html).toContain("$1.00"); expect(html).toContain("Page 1");
    expect(html).not.toContain("Publish price version</button>"); expect(html).not.toContain("Delete");
    expect(render(<PriceVersions path={path} writable />, entries)).toContain("Publish price version</button>");
  });
  it("renders required residency, read-only assertions and unknown passive health honestly", () => {
    const path = "/api/v1/platform/deployments/d/routing";
    const html = render(<DeploymentRoutingPanel path={path} operator={false} />, [[path, { routing: { priority: 0, weight: 1, residency: "us-east" }, health: { consecutive_failures: 0, open_until: null } }]]);
    expect(html).toContain("operator-managed (read-only)"); expect(html).toContain("Unknown · no recorded observation");
    expect(html).not.toContain("Healthy");
    const modelPath = "/api/v1/platform/models/m/routing";
    const model = render(<ModelRoutingPanel path={modelPath} />, [[modelPath, { policy: { strategy: "weighted", max_attempts: 3, allow_ambiguous_failover: true, failure_threshold: 3, cooldown_seconds: 30, required_residency: "us-east" } }]]);
    expect(model).toContain("duplicate charges possible"); expect(model).toContain("Required residency"); expect(model).toContain("us-east");
  });
});
describe("vendored Bitop composition", () => {
  it("marks rendered links disabled while keeping native button semantics", () => {
    const link = renderToStaticMarkup(<Button render={<a href="#target" />} loading>Open</Button>);
    expect(link).toContain('aria-disabled="true"'); expect(link).toContain('aria-busy="true"');
    expect(link).not.toContain(' disabled=""');
    const native = renderToStaticMarkup(<Button disabled>Save</Button>);
    expect(native).toContain(' disabled=""'); expect(native).toContain('type="button"');
  });
  it("uses real accessible table/card/header/stat/form primitives", () => {
    const html = renderToStaticMarkup(<><Heading title="Example" /><Panel title="Card"><StatCard label="Known" value="Unknown" /><Table label="Rows" rows={["row"]} rowKey={row => row} columns={[{ title: "Name", render: row => row }]} /><FormField label="Budget" description="US dollars" error="Invalid amount"><Input name="budget" defaultValue="bad" /></FormField></Panel></>);
    expect(html).toContain("<caption"); expect(html).toContain('scope="col"');
    // Bitop adds a keyboard-scroll region after measuring actual overflow in the browser.
    expect(html).toContain('data-scroll=""'); expect(html).not.toContain('role="region"');
    expect(html).toContain("<dl"); expect(html).toContain("<h1"); expect(html).toContain('aria-invalid="true"'); expect(html).toContain("Invalid amount");
  });
  it("renders a collapsible sidebar with skip target and labelled toggle", () => {
    const html = renderToStaticMarkup(<TooltipProvider><AppShell sidebar={<Sidebar><SidebarContent><SidebarNav aria-label="Dashboard"><SidebarSection label="People"><SidebarItem label="Members" href="#members" current /></SidebarSection></SidebarNav></SidebarContent></Sidebar>} topbar={<TopBar />}><Main>Real content</Main></AppShell></TooltipProvider>);
    expect(html).toContain('href="#main"'); expect(html).toContain('id="main"'); expect(html).toContain('aria-label="Collapse sidebar"'); expect(html).toContain('aria-current="page"'); expect(html).toContain("People");
  });
});
