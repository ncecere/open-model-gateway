import { afterEach, describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { api, type Model, type Organization, type Session, type Workspace } from "../lib/api";
import { type Action } from "./ui";
import { AssignedModels, DelegatedGrants, PlatformAssignment, PlatformModelAccess, recipientGrantPath } from "../pages/model-access";
import { Models, Providers, Deployments } from "../pages/catalog";
import { PolicyPanel, Governance, Pricing, Routing, PriceVersions } from "../pages/governance";
import { Teams, PlatformTeams } from "../pages/hierarchy";
import { WorkspaceMembers, ServiceAccounts, Grants } from "../pages/workspace";
import { Overview } from "../pages/overview";
import { adminGroups, adminLanding, contextOptions, navigation, organizationLanding, scopeSearch } from "../lib/navigation";
import { canView, permissions } from "../lib/permissions";
import { jumpTargets } from "../lib/search";
import { policyFields } from "../lib/governance";

const captures = vi.hoisted(() => ({ ask: vi.fn(), clicks: new Map<string, () => void>() }));
vi.mock("./ui", async importOriginal => {
  const original = await importOriginal<typeof import("./ui")>();
  return { ...original, useAction: () => captures.ask, Button: (props: React.ComponentProps<typeof original.Button>) => {
    const label = Array.isArray(props.children) ? props.children.join("") : String(props.children);
    if (props.onClick) captures.clicks.set(label, props.onClick as () => void);
    return <original.Button {...props} />;
  } };
});
vi.mock("../lib/api", async importOriginal => ({ ...await importOriginal<typeof import("../lib/api")>(), api: vi.fn().mockResolvedValue({ ok: true }) }));
const org: Organization = { id: "org", name: "Consumer", slug: "consumer", role: "admin" };
const project: Workspace = { id: "project", name: "Research", organization_id: org.id, kind: "project", role: "admin" };
const session: Session = { user: { id: "me", email: "me@example.invalid", platform_admin: false }, organizations: [org], workspaces: [project] };
const operator: Session = { ...session, user: { ...session.user, platform_admin: true } };
const model: Model = { id: "model", public_name: "global/model", display_name: "Global Model", enabled: true, personal_enabled: false };
const policy = { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, monthly_budget_microusd: null };
const clients: QueryClient[] = [];
type Entry = [unknown[], unknown];
const choices = (path: string, data: unknown[]): Entry => [["api", path, "choices"], data];
const rows = (path: string, data: unknown[]): Entry => [["api", `${path}?limit=100&offset=0`], { data }];
function render(node: ReactNode, entries: Entry[] = []) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: Infinity } } }); clients.push(client);
  for (const [key, value] of entries) client.setQueryData(key, value);
  return renderToStaticMarkup(<QueryClientProvider client={client}>{node}</QueryClientProvider>);
}
function action(label: string): Action {
  const click = captures.clicks.get(label); expect(click, `button ${label}`).toBeDefined(); click!();
  return captures.ask.mock.lastCall![0] as Action;
}
afterEach(() => { clients.forEach(client => client.clear()); clients.length = 0; captures.clicks.clear(); vi.clearAllMocks(); });

describe("platform infrastructure boundary", () => {
  it("offers global infrastructure without an organization and drops stale scope", () => {
    const empty = { ...operator, organizations: [], workspaces: [] };
    for (const page of ["models", "providers", "deployments", "routing", "pricing", "platform-audit", "model-access", "platform-projects"] as const) {
      expect(canView(page, empty)).toBe(true);
      expect(scopeSearch(page, "stale-org", "private")).toEqual({ page, org: undefined, ws: undefined });
      expect(jumpTargets(empty).find(target => target.id === `page:${page}`)).toBeDefined();
      expect(canView(page, session, org, project)).toBe(false);
    }
    expect(adminGroups("models", true)).toEqual(["Platform", "Models", "Oversight"]);
    expect(navigation.filter(item => item.group === "Platform").map(item => item.label)).toEqual(["Overview", "Organizations", "Teams", "Projects", "Users"]);
  });
  it("reads infrastructure at platform paths, never the consuming organization", () => {
    expect(render(<Models session={operator} />, [rows("/api/v1/platform/models", [model])])).toContain("Global Model");
    expect(render(<Providers session={operator} />, [rows("/api/v1/platform/providers", [{ id: "provider", name: "Global provider", provider: "openai", enabled: true }])])).toContain("Global provider");
    expect(render(<Deployments session={operator} />, [rows("/api/v1/platform/deployments", [{ id: "d", upstream_model: "global-deployment", enabled: true }])])).toContain("global-deployment");
    expect(render(<Routing session={operator} />, [choices("/api/v1/platform/models", [model])])).toContain("Global Model");
    expect(render(<Pricing session={operator} />, [choices("/api/v1/platform/deployments", [{ id: "d", upstream_model: "global-priced-deployment" }])])).toContain("global-priced-deployment");
  });
  it("publishes dollar-denominated rates as exact integer micro-USD without changing the API contract", async () => {
    const path = "/api/v1/platform/deployments/d/prices";
    render(<PriceVersions path={path} writable />);
    const publish = action("Publish price version");
    expect(publish.fields?.map(field => field.name)).toContain("input_usd_per_million");
    expect(publish.fields?.map(field => field.label).join(" ")).not.toContain("micro-USD");
    await publish.run({ input_usd_per_million: "0.123456", output_usd_per_million: "0.500001", input_token_limit: "1000", output_token_limit: "100" });
    expect(api).toHaveBeenCalledWith(path, { method: "POST", body: { input_microusd_per_million: "123456", output_microusd_per_million: "500001", input_token_limit: 1000, output_token_limit: 100 } });
  });
  it("requires explicit organization selection for assignments, not infrastructure", () => {
    const html = render(<PlatformModelAccess session={operator} />, [choices("/api/v1/orgs", [org])]);
    expect(html).toContain("Select an organization");
    expect(html).toContain("Choose an organization…");
    expect(html).not.toContain("Platform-assigned ceiling");
    expect(html).not.toContain("Delegate model</button>");
  });
  it("does not expose infrastructure actions in the assigned organization catalog", () => {
    const html = render(<AssignedModels organization={org} />, [rows("/api/v1/orgs/org/models", [model])]);
    expect(html).toContain("Global Model"); expect(html).toContain("Allow personal access");
    expect(html).not.toContain("Disable</button>"); expect(html).not.toContain("Create alias");
    expect(html).toContain("Private workspaces are not listed");
  });
});

describe("assignment and delegation forms", () => {
  it("shows individual personal grants without offering the wrong workspace removal action", () => {
    const personal: Workspace = { ...project, id: "personal", kind: "personal", role: "owner" };
    const html = render(<Grants session={session} organization={org} workspace={personal} />, [rows("/api/v1/workspaces/personal/grants", [{ model_id: "model", public_name: "alias", display_name: "Individual model", workspace_granted: false, individual_granted: true }])]);
    expect(html).toContain("Individual model");
    expect(html).toContain("Individual · organization-managed");
    expect(html).toContain("Manage individual access under Assigned models");
    expect(html).not.toContain("Remove grant</button>");
  });
  it("assigns a global model using PUT with optional organization alias", async () => {
    render(<PlatformAssignment organization={org} />, [choices("/api/v1/platform/models", [model]), choices("/api/v1/orgs/org/models", [])]);
    const assign = action("Assign model");
    expect(assign.fields?.find(field => field.name === "model_id")?.options).toEqual([{ value: "model", label: "Global Model (global/model)" }]);
    await assign.run({ model_id: "model", public_name: "consumer/alias" });
    expect(api).toHaveBeenLastCalledWith("/api/v1/platform/orgs/org/models/model", { method: "PUT", body: { public_name: "consumer/alias" } });
    await assign.run({ model_id: "model", public_name: "" });
    expect(api).toHaveBeenLastCalledWith("/api/v1/platform/orgs/org/models/model", { method: "PUT", body: {} });
  });
  it("revokes the parent assignment with clear child-grant removal warning", async () => {
    render(<PlatformAssignment organization={org} />, [rows("/api/v1/orgs/org/models", [model])]);
    const revoke = action("Revoke assignment"); expect(revoke.description).toContain("all child workspace and individual grants");
    await revoke.run({}); expect(api).toHaveBeenCalledWith("/api/v1/platform/orgs/org/models/model", { method: "DELETE" });
  });
  it("uses only assigned catalog choices for individual and shared delegation", async () => {
    for (const kind of ["team", "project", "user"] as const) {
      const path = recipientGrantPath("org", kind, "recipient");
      expect(path).toBe(kind === "user" ? "/api/v1/orgs/org/users/recipient/grants" : "/api/v1/workspaces/recipient/grants");
      render(<DelegatedGrants organization={org} path={path} label="Recipient" personal={kind === "user"} />, [choices("/api/v1/platform/models", [{ ...model, id: "not-assigned" }]), choices("/api/v1/orgs/org/models", [model]), choices(path, [])]);
      const delegate = action("Delegate model");
      expect(delegate.fields?.[0].options?.map(option => option.value)).toEqual(["model"]);
      await delegate.run({ model_id: "model" }); expect(api).toHaveBeenLastCalledWith(path, { method: "POST", body: { model_id: "model" } });
    }
  });
  it("removes individual grants without enumerating personal workspaces", async () => {
    const path = recipientGrantPath("org", "user", "user");
    render(<DelegatedGrants organization={org} path={path} label="Person" personal />, [rows(path, [{ model_id: "model", public_name: "alias", display_name: "Model" }])]);
    await action("Remove grant").run({}); expect(api).toHaveBeenLastCalledWith(`${path}/model`, { method: "DELETE" });
  });
});

describe("project parity and inherited limits", () => {
  it("lands project-only admins on Projects and groups their context/search correctly", () => {
    const memberOrg = { ...org, role: "member" as const };
    const manager = { ...session, organizations: [memberOrg] };
    expect(adminLanding(manager, memberOrg)).toBe("workspace-settings");
    expect(organizationLanding(manager, memberOrg)).toBe("organization-settings");
    expect(contextOptions(manager, memberOrg).find(group => group.label === "Projects")?.items[0].ws).toBe(project.id);
    const targets = jumpTargets(manager, memberOrg, project);
    expect(targets.find(target => target.id === "org:org")?.search.page).toBe("overview");
    expect(targets.find(target => target.id === "workspace:project")?.group).toBe("Projects");
    expect(targets.find(target => target.id === "page:workspace-settings")?.label).toBe("Workspace settings");
    expect(permissions(manager, memberOrg, project)).toMatchObject({ manageTeam: true, manageServiceAccounts: true, managePolicy: true, manageGrants: false });
    expect(permissions(manager, memberOrg, { ...project, role: "member" })).toMatchObject({ managePolicy: false, manageTeam: false });
  });
  it("creates project siblings with kind=project and uses project directory endpoints", async () => {
    const go = vi.fn();
    expect(render(<Teams session={session} organization={org} kind="project" go={go} />, [rows("/api/v1/orgs/org/projects", [project])])).toContain("Research");
    await action("Create project").run({ name: "New project" });
    expect(api).toHaveBeenCalledWith("/api/v1/orgs/org/workspaces", { method: "POST", body: { name: "New project", kind: "project" } });
    expect(render(<PlatformTeams session={operator} kind="project" go={go} />, [rows("/api/v1/platform/projects", [{ ...project, organization_name: "Consumer" }])])).toContain("Research");
  });
  it("uses project labels for overview, membership and service accounts", () => {
    const props = { session, organization: org, workspace: project };
    expect(render(<Overview {...props} />)).toContain("Project workspace");
    expect(render(<WorkspaceMembers {...props} />)).toContain("Project members");
    expect(render(<ServiceAccounts {...props} />)).toContain("project workloads");
  });
  it("separates read-only inherited ceiling from editable shared scope allowance", async () => {
    const path = "/api/v1/workspaces/project/policy";
    const ceiling = { ...policy, requests_per_minute: 120, monthly_budget_microusd: "50000000" };
    const html = render(<PolicyPanel title="Project policy" path={path} writable />, [[["api", path], { policy, ceiling }]]);
    expect(html).toContain('data-variant="pills"'); expect(html).toContain("Effective limits"); expect(html).toContain("$50.00"); expect(html).toContain("Local limits"); expect(html).toContain("Inherited");
    expect(html).not.toContain("Inherited platform / organization limits · read-only");
    const edit = action("Edit project policy"); expect(edit.description).toContain("not reserved allocations");
    await edit.run({ requests_per_minute: "", tokens_per_minute: "", concurrent_requests: "", monthly_budget_usd: "" });
    expect(api).toHaveBeenCalledWith(path, { method: "PUT", body: policy });
    expect(policyFields(policy, ceiling)[0].validate?.("121", {})).toContain("cannot exceed");
    expect(policyFields(policy, ceiling)[3].validate?.("50.000001", {})).toContain("cannot exceed");
  });
  it("edits the platform organization ceiling through its operator-only endpoint", async () => {
    const path = "/api/v1/platform/orgs/org/policy";
    const html = render(<PolicyPanel title="Platform organization ceiling" path={path} writable platform />, [[["api", path], { policy }]]);
    expect(html).toContain("Platform-assigned ceiling");
    const edit = action("Edit platform organization ceiling");
    expect(edit.fields?.find(field => field.name === "monthly_budget_usd")?.label).toBe("Monthly budget (USD)");
    await edit.run({ requests_per_minute: "120", tokens_per_minute: "", concurrent_requests: "", monthly_budget_usd: "50.00" });
    expect(api).toHaveBeenCalledWith(path, { method: "PUT", body: { ...policy, requests_per_minute: 120, monthly_budget_microusd: "50000000" } });
    const local = render(<Governance session={session} organization={org} />, [[["api", "/api/v1/orgs/org/policy"], { policy, ceiling: { ...policy, requests_per_minute: 120 } }]]);
    expect(local).toContain('aria-label="Organization limits details"');
    expect(local).toContain("Additional"); expect(local).toContain("Platform");
    expect(local).not.toContain("Platform maximums (cannot be overridden) · read-only");
    expect(local).toContain("Effective organization caps");
    expect(local).not.toContain("No workspace selected");
    await action("Edit additional organization limits").run({ requests_per_minute: "100", tokens_per_minute: "", concurrent_requests: "", monthly_budget_usd: "" });
    expect(api).toHaveBeenLastCalledWith("/api/v1/orgs/org/policy", { method: "PUT", body: { ...policy, requests_per_minute: 100 } });
  });
  it("lets project admins tighten their scope, not organization policy; members read only", () => {
    const memberOrg = { ...org, role: "member" as const };
    const entries: Entry[] = [[["api", "/api/v1/workspaces/project/policy"], { policy, ceiling: policy }]];
    const adminHtml = render(<Governance session={session} organization={memberOrg} workspace={project} />, entries);
    expect(adminHtml).toContain("Edit workspace policy"); expect(adminHtml).not.toContain("Edit additional organization limits");
    const memberHtml = render(<Governance session={session} organization={memberOrg} workspace={{ ...project, role: "member" }} />, entries);
    expect(memberHtml).not.toContain("Edit workspace policy"); expect(memberHtml).toContain("Read-only");
  });
});
