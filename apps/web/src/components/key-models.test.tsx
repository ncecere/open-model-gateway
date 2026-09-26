import { afterEach, describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { Grant, Key, Organization, Session, Workspace } from "../lib/api";
import { keyModelFields } from "../lib/key-models";
import { ActionProvider, CheckboxesField, StatCard } from "./ui";
import { Keys } from "../pages/workspace";

const org: Organization = { id: "org", name: "Acme", slug: "acme", role: "member" };
const workspace: Workspace = { id: "ws", organization_id: "org", name: "Team", kind: "team", role: "member" };
const session: Session = { user: { id: "me", email: "me@example.invalid", platform_admin: false }, organizations: [org], workspaces: [workspace] };
const grant: Grant = { model_id: "00000000-0000-0000-0000-000000000001", display_name: "Test model", public_name: "test/model" };
const key: Key = { id: "key", name: "My key", issued_to_user_id: "me", service_account_id: null, created_at: "2026-01-01T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null };
const clients: QueryClient[] = [];
function renderKeys({ grants, error, fetching = false, keys = [], ws = workspace, organization = org, members, accounts }: { grants?: Grant[]; error?: Error; fetching?: boolean; keys?: Key[]; ws?: Workspace; organization?: Organization; members?: unknown[]; accounts?: unknown[] } = {}) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false, retryOnMount: false, staleTime: Infinity } } });
  clients.push(client);
  const path = "/api/v1/workspaces/ws";
  client.setQueryData(["api", `${path}/keys?limit=100&offset=0`], { data: keys });
  if (grants) client.setQueryData(["api", `${path}/grants`, "choices"], grants);
  if (members) client.setQueryData(["api", `${path}/members`, "choices"], members);
  if (accounts) client.setQueryData(["api", `${path}/service-accounts`, "choices"], accounts);
  if (error || fetching) client.getQueryCache().build(client, { queryKey: ["api", `${path}/grants`, "choices"] }).setState({ ...(error ? { status: "error", error } : {}), fetchStatus: fetching ? "fetching" : "idle" });
  return renderToStaticMarkup(<QueryClientProvider client={client}><ActionProvider><Keys session={session} organization={organization} workspace={ws} /></ActionProvider></QueryClientProvider>);
}
afterEach(() => { for (const client of clients) client.clear(); clients.length = 0; });
const createButton = (html: string) => html.match(/<button[^>]*>Create key<\/button>/)?.[0];

describe("key restriction screen", () => {
  it("disables issuance while grants load or refresh, and on errors even with cached grants", () => {
    for (const html of [renderKeys(), renderKeys({ grants: [grant], fetching: true })]) {
      expect(createButton(html)).toContain('disabled=""');
      expect(html).toContain("Loading effective model access");
      expect(html).toContain('role="status"');
    }
    const failed = renderKeys({ grants: [grant], error: new Error("Later page failed") });
    expect(createButton(failed)).toContain('disabled=""');
    expect(failed).toContain("Effective model access could not be fully loaded");
    expect(failed).toContain("Try again");
    expect(failed).toContain("Later page failed");
  });
  it("allows inherit/deny-all issuance after a successful empty grant result", () => {
    expect(createButton(renderKeys({ grants: [] }))).not.toContain("disabled");
    expect(createButton(renderKeys({ grants: [grant] }))).not.toContain("disabled");
  });
  it("preserves explicit membership and service-account issuance rules for inherited admins", () => {
    const ws = { ...workspace, role: "admin" as const };
    const organization = { ...org, role: "admin" as const };
    expect(createButton(renderKeys({ grants: [], ws, organization, members: [], accounts: [] }))).toContain("disabled");
    expect(createButton(renderKeys({ grants: [], ws, organization, members: [{ user_id: "me", disabled_at: null }], accounts: [] }))).not.toContain("disabled");
    expect(createButton(renderKeys({ grants: [], ws, organization, members: [{ user_id: "me", disabled_at: "2026-01-01" }], accounts: [{ id: "sa", name: "Service", disabled_at: null }] }))).not.toContain("disabled");
    expect(createButton(renderKeys({ grants: [], ws, organization, members: [], accounts: [{ id: "sa", name: "Service", disabled_at: "2026-01-01" }] }))).toContain("disabled");
  });
  it("labels all three modes and preserves revoked-grant UUIDs without an edit control", () => {
    const html = renderKeys({ grants: [grant], keys: [key, { ...key, id: "none", model_ids: [] }, { ...key, id: "selected", model_ids: [grant.model_id, "removed-grant"] }] });
    expect(html).toContain("Model access");
    expect(html).toContain("Inherited");
    expect(html).toContain("No models");
    expect(html).toContain("2 selected");
    expect(html).toContain("Test model (test/model)");
    expect(html).toContain("removed-grant");
    expect(html).toContain("Model access is fixed at creation");
    expect(html).not.toContain("Edit model");
    expect(html).toContain("Rotate</button>");
  });
});

describe("real Bitop checkbox composition", () => {
  const field = keyModelFields([{ value: grant.model_id, label: "Test model (test/model)" }])[1];
  it("renders a labelled group, individual labels, checked state, descriptions and focusable validation target", () => {
    const html = renderToStaticMarkup(<CheckboxesField field={field} id="models" value={JSON.stringify([grant.model_id])} error="Choose granted models" onChange={() => {}} />);
    expect(html).toContain('role="group"');
    expect(html).toContain('aria-labelledby="models-label"');
    expect(html).toContain('id="models-label">Models (required)</span>');
    expect(html).toContain('aria-labelledby="models-option-0"');
    expect(html).toContain('id="models-option-0">Test model (test/model)</span>');
    expect(html).not.toContain('aria-required');
    expect(html).toContain('id="models"');
    expect(html).toContain('tabindex="-1"');
    expect(html).toContain('aria-invalid="true"');
    expect(html).toContain('aria-describedby="models-description models-error"');
    expect(html).toContain('id="models-description"');
    expect(html).toContain('id="models-error"');
    expect(html).toContain("Choose granted models");
    expect(html).toContain("<label");
    expect(html).toContain("Test model (test/model)");
    expect(html).toContain('aria-checked="true"');
  });
  it("disables checkbox controls while saving", () => {
    const html = renderToStaticMarkup(<CheckboxesField field={field} id="models" value="[]" disabled onChange={() => {}} />);
    expect(html).toContain('disabled=""');
    expect(html).toContain("data-disabled");
    expect(html).toContain('aria-checked="false"');
  });
  it("keeps empty and malformed selections renderable with explicit feedback and an error focus target", () => {
    const html = renderToStaticMarkup(<CheckboxesField field={keyModelFields([])[1]} id="models" value="invalid" error="Select at least one model" onChange={() => {}} />);
    expect(html).toContain('id="models"');
    expect(html).toContain('tabindex="-1"');
    expect(html).toContain("No effective model grants are available");
    expect(html).toContain("Select at least one model");
  });
});

describe("compatible upstream stat-card refresh", () => {
  it("preserves unlinked metric semantics and supports an optional labelled link/details", () => {
    const plain = renderToStaticMarkup(<StatCard label="Known cost" value="$1.00" hint="Estimated" />);
    expect(plain).toContain("<dl");
    expect(plain).toContain("Known cost</dt>");
    expect(plain).toContain("$1.00</dd>");
    expect(plain).not.toContain("<a");
    const linked = renderToStaticMarkup(<StatCard label="Known cost" value="$1.00" href="#costs" details="Estimated, not invoices" />);
    expect(linked).toContain('href="#costs"');
    expect(linked).toContain("Known cost</a>");
    expect(linked).toContain("Estimated, not invoices</dd>");
  });
});
