import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import type { Grant, Key, Workspace } from "../lib/api";
import { keyModelFields } from "../lib/key-models";
import { CheckboxesField, StatCard } from "./ui";
import { Keys } from "../pages/workspace";
import { admin, session, member, team, none, grant, markup, testClient } from "../lib/test-fixtures";
const key: Key = { id: "key", name: "My key", issued_to_user_id: "me", service_account_id: null, created_at: "2026-01-01T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null };
function renderKeys({ grants, error, fetching = false, keys = [], ws = member, accounts = [] }: { grants?: Grant[]; error?: Error; fetching?: boolean; keys?: Key[]; ws?: Workspace; accounts?: unknown[] } = {}) {
  const client = testClient(), path = "/api/v1/workspaces/team"; client.setQueryData(["api", undefined, `${path}/keys`, "choices"], keys);
  if (grants) client.setQueryData(["api", undefined, `${path}/models`, "choices"], grants); client.setQueryData(["api", undefined, `${path}/service-accounts`, "choices"], accounts);
  if (error || fetching) client.getQueryCache().build(client, { queryKey: ["api", undefined, `${path}/models`, "choices"] }).setState({ ...(error ? { status: "error", error } : {}), fetchStatus: fetching ? "fetching" : "idle" });
  const html = markup(<Keys session={admin} workspace={ws} />, [], client); client.clear(); return html;
}
const create = (html: string) => html.match(/<button[^>]*>Create key<\/button>/)?.[0];
describe("key restriction screen", () => {
  it("disables issuance while grants load/refresh and on cached refresh failures", () => { for (const html of [renderKeys(), renderKeys({ grants: [grant], fetching: true })]) { expect(create(html)).toContain("disabled"); expect(html).toContain("Loading this workspace&#x27;s models"); } const failed = renderKeys({ grants: [grant], error: new Error("Later page failed") }); expect(create(failed)).toContain("disabled"); expect(failed).toContain("Later page failed"); });
  it("asks for a model first when the workspace has none (a key couldn't call anything)", () => { const empty = renderKeys({ grants: [] }); expect(create(empty)).toContain("disabled"); expect(empty).toContain("Add a model first"); expect(create(renderKeys({ grants: [grant] }))).not.toContain("disabled"); });
  it("uses actual issuance capability, never inferred membership", () => { const staff = { ...team, role: null, membership_source: null, capabilities: { ...none, manage_members: true, manage_service_accounts: true } }; expect(create(renderKeys({ grants: [grant], ws: staff, accounts: [] }))).toContain("disabled"); expect(create(renderKeys({ grants: [grant], ws: { ...staff, role: "member", capabilities: { ...staff.capabilities, issue_own_key: true } } }))).not.toContain("disabled"); expect(create(renderKeys({ grants: [grant], ws: staff, accounts: [{ id: "sa", name: "Service", disabled_at: null }] }))).not.toContain("disabled"); expect(create(renderKeys({ grants: [grant], ws: staff, accounts: [{ id: "sa", name: "Service", disabled_at: "now" }] }))).toContain("disabled"); });
  it("labels inherit/deny-all/selection and preserves retired UUIDs without edit controls", () => { const html = renderKeys({ grants: [grant], keys: [key, { ...key, id: "none", model_ids: [] }, { ...key, id: "selected", model_ids: [grant.model_id, "removed-grant"] }] }); expect(html).toContain("All workspace models"); expect(html).toContain("No models"); expect(html).toContain("2 models"); expect(html).toContain("removed-grant"); expect(html).not.toContain("change a key&#x27;s models later"); expect(html).not.toContain("Edit model"); });
});
describe("upstream accessible checkbox composition", () => {
  const field = keyModelFields([{ value: grant.model_id, label: "Smart model" }])[1];
  it("renders fieldset legend, option labels, checked state and validation", () => { const html = renderToStaticMarkup(<CheckboxesField field={field} id="models" value={JSON.stringify([grant.model_id])} error="Choose granted models" onChange={() => {}} />); expect(html).toContain('role="group"'); expect(html).toContain("Models"); expect(html).toContain("Smart model"); expect(html).toContain('aria-checked="true"'); expect(html).toContain("Choose granted models"); expect(html).toContain('tabindex="-1"'); });
  it("disables checkbox controls during saves", () => { const html = renderToStaticMarkup(<CheckboxesField field={field} id="models" value="[]" disabled onChange={() => {}} />); expect(html).toContain("data-disabled"); expect(html).toContain('aria-checked="false"'); });
  it("keeps malformed/empty selections renderable with useful feedback", () => { const html = renderToStaticMarkup(<CheckboxesField field={keyModelFields([])[1]} id="models" value="invalid" error="Select at least one model" onChange={() => {}} />); expect(html).toContain("no models yet"); expect(html).toContain("Select at least one model"); expect(html).toContain('id="models"'); });
});
describe("compatible refreshed stat-card semantics", () => {
  it("preserves unlinked metrics and optional labelled links/details", () => { const plain = renderToStaticMarkup(<StatCard label="Known cost" value="$1.00" hint="Estimated" />); expect(plain).toContain("<dl"); expect(plain).toContain("Known cost</dt>"); expect(plain).toContain("$1.00</dd>"); expect(plain).not.toContain("<a"); const linked = renderToStaticMarkup(<StatCard label="Known cost" value="$1.00" href="#costs" details="Not an invoice" />); expect(linked).toContain('href="#costs"'); expect(linked).toContain("Not an invoice"); });
});
