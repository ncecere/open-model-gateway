import { afterEach, describe, expect, it, vi } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { ApiError, type Session, type Organization, type Workspace } from "../lib/api";
import { ErrorNotice, ActionProvider } from "./ui";
import { Overview, Profile } from "../pages/overview";
import { Keys, Grants, MembersTable } from "../pages/workspace";
import { Providers } from "../pages/catalog";

const org: Organization = { id: "org", name: "Acme", slug: "acme", role: "admin" };
const workspace: Workspace = { id: "ws", organization_id: "org", name: "Team", kind: "team", role: "admin" };
const session: Session = { user: { id: "me", email: "me@example.org", platform_admin: false }, organizations: [org], workspaces: [workspace] };
const clients: QueryClient[] = [];
function render(node: ReactNode, entries: [string, unknown][] = []) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: Infinity } } });
  clients.push(client);
  for (const [path, data] of entries) client.setQueryData(["api", path], data);
  return renderToStaticMarkup(<QueryClientProvider client={client}><ActionProvider>{node}</ActionProvider></QueryClientProvider>);
}
afterEach(() => { clients.forEach((client) => client.clear()); clients.length = 0; vi.unstubAllGlobals(); });
describe("dashboard component rendering", () => {
  it("does not fabricate usage while loading", () => {
    const html = render(<Overview session={session} organization={org} workspace={workspace} />);
    expect(html).toContain("Loading usage");
    expect(html).not.toContain('class="stat"');
    expect(html).not.toContain("$0");
  });
  it("renders real counts, unknown execution usage, and empty-state guidance", () => {
    const html = render(<Overview session={session} organization={org} workspace={workspace} />, [
      ["/api/v1/workspaces/ws/usage", { requests: 12, input_tokens: 140, output_tokens: 60, unknown_usage_requests: 3 }],
      ["/api/v1/workspaces/ws/executions?limit=50&offset=0", { data: [{ id: "ex", public_model: "company/smart", provider: "openai", state: "cancelled", streamed: true, input_tokens: null, output_tokens: null, elapsed_ms: null, started_at: "2026-01-01T00:00:00Z", error_code: null }] }],
    ]);
    expect(html).toContain("140");
    expect(html).toContain("Unknown / Unknown");
    expect(html).toContain("cancelled");
    expect(html).toContain("Accounting, not billing");
    expect(html).toContain("Missing usage is not zero");
  });
  it("never gives org admins operator-only provider write controls", () => {
    const html = render(<Providers session={session} organization={org} />, [["/api/v1/orgs/org/providers?limit=100&offset=0", { data: [{ id: "provider", name: "OpenAI", provider: "openai", enabled: true, region: null, endpoint: null }] }]]);
    expect(html).toContain("Access not available");
    expect(html).not.toContain("Create connection</button>");
    expect(html).not.toContain("Rotate reference</button>");
    expect(html).not.toContain("Disable</button>");
  });
  it("offers revoke but not rotation for another human’s key", () => {
    const html = render(<Keys session={session} organization={org} workspace={workspace} />, [["/api/v1/workspaces/ws/keys?limit=100&offset=0", { data: [{ id: "key", name: "Other human", issued_to_user_id: "other", service_account_id: null, created_at: "2026-01-01T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null }] }]]);
    expect(html).toContain("Revoke</button>");
    expect(html).not.toContain("Rotate</button>");
  });
  it("uses a useful read-only grants empty state for workspace members", () => {
    const html = render(<Grants session={session} organization={{ ...org, role: "member" }} workspace={{ ...workspace, role: "member" }} />, [["/api/v1/workspaces/ws/grants?limit=100&offset=0", { data: [] }]]);
    expect(html).toContain("Ask an organization administrator");
    expect(html).not.toContain("Grant model</button>");
  });
  it("labels denied and missing resources and escapes error text", () => {
    const denied = renderToStaticMarkup(<ErrorNotice error={new ApiError(403, "forbidden", "No access")} />);
    expect(denied).toContain('role="alert"');
    expect(denied).toContain("Access denied");
    const missing = renderToStaticMarkup(<ErrorNotice error={new ApiError(404, "missing", "<script>bad</script>")} />);
    expect(missing).toContain("Not found");
    expect(missing).not.toContain("<script>");
  });
  it("does not offer owner changes to an administrator", () => {
    const html = render(<MembersTable path="/api/v1/orgs/org/members" title="Members" canManageOwners={false} />, [["/api/v1/orgs/org/members?limit=100&offset=0", { data: [{ user_id: "owner", email: "owner@example.org", role: "owner", disabled_at: null }] }]]);
    expect(html).toContain("Owner protected");
    expect(html).not.toContain("Manage</button>");
  });
  it("renders authenticated identity rather than a fake account", () => {
    const html = render(<Profile session={session} />);
    expect(html).toContain("me@example.org");
    expect(html).toContain("Standard user");
    expect(html).toContain("Acme");
    expect(html).toContain("managed by your identity provider");
  });
});
