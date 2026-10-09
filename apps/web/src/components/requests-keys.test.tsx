import { describe, expect, it } from "vitest";
import type { ReactNode } from "react";
import { DashboardNavigationProvider } from "./navigation-link";
import { EffectiveAccess, AccessCard } from "./effective-access";
import { Requests } from "../pages/requests";
import { RequestDetailPage } from "../pages/request-detail";
import { Keys } from "../pages/workspace";
import { KeyDetail } from "../pages/key-detail";
import type { DashboardSearch } from "../lib/permissions";
import type { RequestDetail, RequestRow } from "../lib/requests";
import type { KeyRow, KeyStats } from "../lib/keys";
import type { AccessResponse } from "../lib/effective-access";
import { grant, markup, member, personal, policy, session, team, testClient } from "../lib/test-fixtures";
import { ApiError } from "../lib/api";

const ws = "/api/v1/workspaces/team";
const nav = (search: DashboardSearch, node: ReactNode) => <DashboardNavigationProvider search={search} navigate={() => {}}>{node}</DashboardNavigationProvider>;
const row: RequestRow = { root_request_id: "1a2b3c4d-0000-0000-0000-000000000001", started_at: "2026-10-08T10:00:00Z", completed_at: "2026-10-08T10:00:02Z", model: "company/smart", key: { id: "key1", name: "CI runner" }, status: "failed", attempts: 2, input_tokens: null, output_tokens: "4", cost_microusd: null, held_microusd: "1900", latency_ms: 2100, cost_center: null, workload_kind: "generation", streamed: false };
const attempt = (n: number, state: string, error: string | null, failover: string | null) => ({ attempt_number: n, execution_id: `exec-${n}-0000-0000-0000-000000000000`, state, error_code: error, started_at: "2026-10-08T10:00:00Z", completed_at: "2026-10-08T10:00:01Z", latency_ms: 900 + n, deployment: { id: `d${n}`, upstream_model: `upstream-${n}` }, connection: { id: `c${n}`, name: n === 1 ? "Primary OpenAI" : "Backup OpenRouter", provider: n === 1 ? "openai" : "openrouter" }, input_tokens: null, output_tokens: n === 2 ? "4" : "0", billing_usage: null, meter_usage: null, cost_microusd: n === 1 ? "0" : null, held_microusd: n === 1 ? "0" : "1900", accounting_state: n === 1 ? "settled" : "unknown", unresolved_reason: n === 1 ? null : "incomplete_usage", price_id: null, pricing_version: null, failover_reason: failover, data_policy: { data_collection: n === 1 ? "unknown" as const : "deny" as const, basis: n === 1 ? "not_configured" : "current_configuration" } });
const detail: RequestDetail = { ...row, workspace_id: "team", attempt_count: 2, attempts: [attempt(1, "failed", "upstream_timeout", null), attempt(2, "indeterminate", null, "upstream_timeout")], prev_id: "newer-0000", next_id: null };

describe("Requests list", () => {
  it("scopes copy by role, links rows to their page with the current filters, and shows unknown cost with holds", () => {
    const search: DashboardSearch = { page: "requests", ws: "team", status: "failed", model: "company/smart" };
    const html = markup(nav(search, <Requests session={session} workspace={member} />), [[`${ws}/requests?model=company%2Fsmart&status=failed&limit=50`, { data: [row], next_cursor: "1759917600000000_1a2b3c4d-0000-0000-0000-000000000001" }], [`${ws}/models`, undefined]]);
    expect(html).toContain("Requests made with your keys in Product. Workspace admins see everyone&#x27;s.");
    expect(html).toContain('href="/workspaces/team/requests/1a2b3c4d-0000-0000-0000-000000000001?model=company%2Fsmart&amp;status=failed"');
    expect(html).toContain("Unknown · $0.0019 on hold");
    expect(html).toContain("2 (1 fallback)");
    expect(html).not.toContain("Unknown in · 4 out");
    expect(html).not.toContain("$0.00<");
    expect(markup(<Requests session={session} workspace={personal} />)).toContain("Only you can see them.");
    expect(markup(<Requests session={session} workspace={team} />)).toContain("Every request made with Product&#x27;s keys.");
  });
  it("does not send an incomplete request ID search and says what to type", () => {
    const client = testClient(), html = markup(nav({ page: "requests", ws: "team", q: "ab" }, <Requests session={session} workspace={team} />), [], client);
    expect(html).toContain("at least 4 characters");
    expect(html).toContain("Filters not applied");
    expect(client.getQueryCache().getAll().filter(q => String(q.queryKey[2]).includes("/requests?")).every(q => q.state.data === undefined && q.state.fetchStatus === "idle")).toBe(true);
    client.clear();
  });
});
describe("Request page", () => {
  const search: DashboardSearch = { page: "request-detail", ws: "team", record: row.root_request_id, status: "failed", cols: "none" };
  const html = () => markup(nav(search, <RequestDetailPage session={session} workspace={team} id={row.root_request_id} />), [[`${ws}/requests/${row.root_request_id}?status=failed`, detail]]);
  it("shows tiles, copyable IDs, the current data policy setting and the attempt timeline with failover", () => {
    const page = html();
    expect(page).toContain("Back to Logs");
    expect(page).toContain('href="/workspaces/team/logs?status=failed&amp;cols=none"');
    expect(page).toContain("$0.0019 on hold until the cost is known");
    expect(page).toContain("Unknown");
    expect(page).toContain("1 fallback");
    expect(page).toContain("Copy request ID");
    expect(page).toContain("Doesn&#x27;t keep data");
    expect(page).toContain("Current setting of the route that served it, not a record of this request.");
    // No model was reported, so each attempt shows its configured route id, marked as such.
    const text = page.replace(/<[^>]+>/g, "");
    expect(text).toContain("Primary OpenAI · upstream-1 (configured)");
    expect(text).toContain("Backup OpenRouter · upstream-2 (configured)");
    expect(page).toContain("upstream_timeout");
    expect(page).toContain("Fallback");
    expect(page).toContain("Tried after attempt 1 (Primary OpenAI) ended with");
    expect(page).toContain("The provider didn&#x27;t report complete usage");
    expect(page).toContain('href="/workspaces/team/keys/key1"');
  });
  it("steps to newer/older requests inside the same filters and view", () => {
    const page = html();
    expect(page).toContain('aria-label="Previous request: newer-0000"');
    expect(page).toContain('href="/workspaces/team/requests/newer-0000?status=failed&amp;cols=none"');
    expect(page).toContain('aria-label="No next request"');
  });
});

const key: KeyRow = { id: "key1", name: "CI runner", issued_to_user_id: "me", service_account_id: null, created_at: "2026-01-01T00:00:00Z", expires_at: "2099-01-01T00:00:00Z", revoked_at: null, status: "active", last_used_at: null, usage: { period: "day", used_microusd: "8100", limit_microusd: "5000000", window_start: "2026-10-08T00:00:00Z", window_end: "2026-10-09T00:00:00Z", unresolved_usage: false } };
const keys: KeyRow[] = [key, { ...key, id: "key2", name: "Laptop", status: "disabled", disabled_at: "2026-10-01T00:00:00Z", usage: { ...key.usage!, limit_microusd: null, period: "month" } }, { ...key, id: "key3", name: "Old token", status: "revoked", revoked_at: "2026-09-01T00:00:00Z" }];
const seedKeys = (client = testClient()) => { client.setQueryData(["api", undefined, `${ws}/keys`, "choices"], keys); client.setQueryData(["api", undefined, `${ws}/models`, "choices"], [grant]); client.setQueryData(["api", undefined, `${ws}/service-accounts`, "choices"], []); return client; };
describe("API keys list", () => {
  it("shows usage against the key budget with its period and status pills (a pill, never strikethrough)", () => {
    const html = markup(nav({ page: "keys", ws: "team", status: "all" }, <Keys session={session} workspace={team} />), [], seedKeys());
    expect(html).toContain("$0.0081 / $5.00");
    expect(html).toContain("Daily");
    expect(html).toContain("$0.0081 / ∞");
    expect(html).toContain(">Active<");
    expect(html).toContain(">Disabled<");
    expect(html).toContain(">Revoked<");
    expect(html).not.toContain("<s>Old token</s>"); expect(html).toContain("Old token"); expect(html).not.toMatch(/Revoked <time/); // Expires shows the expiry date
    expect(html).toContain("Never");
    expect(html).toContain('href="/workspaces/team/keys/key1?status=all"');
    expect(html).toContain("Search keys");
    expect(html).toContain(">Spent<"); expect(html).not.toContain("Spent + on hold");
  });
  it("shows active keys by default so revoked keys don't fill the list; All is explicit", () => {
    const html = markup(nav({ page: "keys", ws: "team" }, <Keys session={session} workspace={team} />), [], seedKeys());
    expect(html).toContain("CI runner"); expect(html).not.toContain("Old token"); expect(html).not.toContain("Laptop");
    expect(html).toMatch(/aria-pressed="true"[^>]*>Active</);
  });
  it("locks selection for revoked keys and shows no selection footer without selectable rows", () => {
    const html = markup(nav({ page: "keys", ws: "team", status: "all" }, <Keys session={session} workspace={team} />), [], seedKeys());
    expect(html).toContain("Select CI runner"); expect(html).toContain("Select Laptop"); expect(html).not.toContain("Select Old token");
    expect(html).not.toMatch(/\d+ of \d+ selected/);
    const revokedOnly = markup(nav({ page: "keys", ws: "team", status: "revoked" }, <Keys session={session} workspace={team} />), [], seedKeys());
    expect(revokedOnly).toContain("Old token"); expect(revokedOnly).not.toContain("Select all keys");
    const empty = seedKeys(); empty.setQueryData(["api", undefined, `${ws}/keys`, "choices"], [keys[2]]);
    const none = markup(nav({ page: "keys", ws: "team" }, <Keys session={session} workspace={team} />), [], empty);
    expect(none).toContain("No active keys"); expect(none).toContain("Show all keys (1)"); expect(none).not.toContain("selected"); expect(none).not.toContain("rows match");
  });
  it("names other members' keys by email for workspace admins", () => {
    const client = seedKeys(); client.setQueryData(["api", undefined, `${ws}/members`, "choices"], [{ user_id: "them", email: "blair@example.invalid", role: "member" }]);
    client.setQueryData(["api", undefined, `${ws}/keys`, "choices"], [{ ...key, issued_to_user_id: "them" }]);
    expect(markup(<Keys session={session} workspace={team} />, [], client)).toContain("blair@example.invalid");
  });
});
const stats: KeyStats = { key_id: "key2", lineage_id: "key2", status: "disabled", last_used_at: null, daily: [{ date: "2026-10-08", spend_microusd: "8100", held_microusd: "0", requests: "3", unresolved_attempts: "0" }], totals: { today: { spend_microusd: "8100", held_microusd: "0", requests: "3", unresolved_attempts: "0" }, week: { spend_microusd: "8100", held_microusd: "100", requests: "3", unresolved_attempts: "1" }, month: { spend_microusd: "8100", held_microusd: "0", requests: "3", unresolved_attempts: "0" } }, budgets: [{ layer: "key", period: "day", amount_microusd: "5000000", monthly_budget_microusd: "5000000", budget_period: "day", window_start: "2026-10-08T00:00:00Z", window_end: "2026-10-09T00:00:00Z", usage_visible: true, used_microusd: "8100", unresolved_usage: false, exhausted: false }, { layer: "platform", period: "month", amount_microusd: "100000000", monthly_budget_microusd: "100000000", budget_period: "month", window_start: "2026-10-01T00:00:00Z", window_end: "2026-11-01T00:00:00Z", usage_visible: false, used_microusd: null, unresolved_usage: null }] };
const access: AccessResponse = { workspace_id: "team", key_id: "key2", truncated: false, summary: { available: 1, partial: 0, unavailable: 1 }, layers: [
  { layer: "platform", source: "installation", applies: true, visible: false, limits: null, budgets: null, catalogs: null, models: { available: 2, partial: 0, unavailable: 0 } },
  { layer: "type_default", source: "type_default", applies: true, catalogs_apply: true, limits: { requests_per_minute: 60, tokens_per_minute: null, concurrent_requests: null, concurrent_jobs: null }, budgets: [{ period: "month", amount_microusd: "100000000" }], catalogs: [{ id: "c", name: "Approved cloud" }], models: { available: 2, partial: 0, unavailable: 0 } },
  { layer: "workspace_override", source: "workspace_override", applies: false, catalogs_apply: false, limits: null, budgets: null, catalogs: null, models: { available: 2, partial: 0, unavailable: 0 } },
  { layer: "workspace", source: "local", applies: true, limits: { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, concurrent_jobs: null }, budgets: [], catalogs: null, selections: { catalog: 2, direct: 0 }, models: { available: 2, partial: 0, unavailable: 0 } },
  { layer: "key", source: "key", applies: true, limits: { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, concurrent_jobs: null }, budgets: [{ period: "day", amount_microusd: "5000000" }], catalogs: null, restriction: { mode: "restricted", model_ids: ["model"] }, models: { available: 1, partial: 0, unavailable: 1 } },
], models: [{ model_id: "model", public_name: "company/smart", display_name: "Smart model", status: "available", reasons: [] }, { model_id: "other", public_name: "company/other", display_name: "Other model", status: "unavailable", reasons: [{ code: "key_restriction", layer: "key" }, { code: "budget_exhausted", layer: "workspace", period: "week" }] }] };
const keyPolicy = { policy: { ...policy, budgets: [{ period: "day", amount_microusd: "5000000" }] }, effective: policy, provenance: { platform_source: "type_default", platform: policy, local: policy, key: policy }, budgets: [] };
describe("Key page", () => {
  const render = (id: string, workspace = team, tab?: string) => { const client = seedKeys(); for (const [p, d] of [[`${ws}/keys/${id}`, keys.find(x => x.id === id)], [`${ws}/keys/${id}/stats`, { ...stats, key_id: id }], [`${ws}/keys/${id}/access`, access], [`${ws}/keys/${id}/policy`, keyPolicy]] as const) client.setQueryData(["api", undefined, p], d); return markup(nav({ page: "key-detail", ws: "team", record: id, tab }, <KeyDetail session={session} workspace={workspace} id={id} />), [], client); };
  it("shows settings and totals on Overview, limits and budget rings on Limits, effective access on Access", () => {
    const html = render("key2");
    expect(html).toContain("Back to API keys");
    expect(html).toContain("Rotate the key to set a new expiry");
    // Pill tabs, not one long stacked page (ui-principles 7, 8).
    for (const name of ["Overview", "Limits", "Access"]) expect(html).toMatch(new RegExp(`role="tab"[^>]*>(<[^>]*>)*[^<]*${name}`));
    expect(html).not.toContain("Key limits"); expect(html).not.toContain("Effective access");
    expect(html).toContain("This week");
    expect(html).toContain("$0.0001 on hold");
    expect(html).toContain("1 cost unknown");
    expect(html).toContain("Spending, last 30 days");
    const limits = render("key2", team, "limits");
    expect(limits).toContain("Key limits");
    expect(limits).toContain("Daily budget");
    expect(limits).toContain('aria-valuetext="$0.0081 of $5.00 · Daily, &lt;1% used"');
    expect(limits).toContain("Only workspace admins see how much of it is used");
    expect(limits).not.toContain("Budget used this period"); // one budgets card, not two
    expect(limits).not.toContain("Spending, last 30 days");
    const accessHtml = render("key2", team, "access");
    expect(accessHtml).toContain("Effective access");
    expect(accessHtml).toContain("Team defaults");
    expect(accessHtml).toContain("Approved cloud");
    expect(accessHtml).toContain("Only 1 selected model");
    expect(accessHtml).toContain("Why can&#x27;t I use this model?");
    expect(accessHtml).toContain("Details for Other model");
    expect(html).toContain('href="/workspaces/team/logs?key_id=key2"');
  });
  it("offers Enable only for a disabled key; a revoked key can never be enabled", () => {
    const disabled = render("key2");
    expect(disabled).toMatch(/<button[^>]*>Enable key<\/button>/);
    expect(disabled.match(/<button[^>]*>Enable key<\/button>/)![0]).not.toContain("disabled");
    expect(disabled).not.toContain(">Disable key<");
    expect(disabled).not.toContain(">Rotate key<");
    // A revoked key is final: no danger zone, read-only copy, "View requests" in the header.
    const revoked = render("key3");
    expect(revoked).not.toContain("Danger zone");
    expect(revoked).not.toContain(">Enable key<");
    expect(revoked).not.toContain(">Revoke key<");
    expect(revoked).toContain("This key is revoked and can&#x27;t be used or changed.");
    expect(render("key3", team, "limits")).toContain("This key is revoked, so its limits no longer change.");
    expect(render("key3", team, "limits")).not.toContain("Blank inherits");
    expect(revoked).not.toContain("change its limits below");
    expect(revoked).not.toContain("Rotate the key to set a new expiry");
    expect(revoked).toMatch(/<a[^>]*href="\/workspaces\/team\/logs\?key_id=key3"[^>]*>View requests<\/a>/);
    const active = render("key1");
    expect(active).toContain(">Disable key<");
    expect(active).toContain(">Rotate key<");
    expect(active).not.toContain(">Enable key<");
  });
  it("steps through keys in the list's order", () => {
    const html = render("key2");
    expect(html).toContain('aria-label="Previous key: CI runner"');
    expect(html).toContain('aria-label="Next key: Old token"');
    expect(html).toContain("2 / 3");
  });
  it("reads the key itself from GET …/keys/{id}, not the list", () => {
    // The list doesn't have it (yet); the key's own endpoint does.
    const client = testClient(); client.setQueryData(["api", undefined, `${ws}/keys`, "choices"], []); client.setQueryData(["api", undefined, `${ws}/models`, "choices"], [grant]);
    client.setQueryData(["api", undefined, `${ws}/keys/key1`], key);
    const html = markup(nav({ page: "key-detail", ws: "team", record: "key1" }, <KeyDetail session={session} workspace={team} id="key1" />), [], client);
    expect(html).toContain("CI runner"); expect(html).toContain("Rotate the key to set a new expiry"); expect(html).not.toContain("Key not found");
  });
  it("doesn't show other members' keys to a member (the server answers 404) and asks for nothing else about them", () => {
    const client = testClient(); client.setQueryData(["api", undefined, `${ws}/keys`, "choices"], [key]); client.setQueryData(["api", undefined, `${ws}/models`, "choices"], []);
    client.getQueryCache().build(client, { queryKey: ["api", undefined, `${ws}/keys/someone-elses`] }).setState({ status: "error", error: new ApiError(404, "404", "Not found"), fetchStatus: "idle" });
    const html = markup(<KeyDetail session={session} workspace={member} id="someone-elses" />, [], client);
    expect(html).toContain("Members see only their own keys.");
    expect(client.getQueryCache().getAll().map(q => String(q.queryKey[2])).filter(p => p.includes("someone-elses"))).toEqual([`${ws}/keys/someone-elses`]);
  });
});
describe("Effective access", () => {
  it("explains why a model is unavailable in plain words, per layer", () => {
    const client = testClient(); client.setQueryData(["api", undefined, `${ws}/access`, "choices"], undefined);
    const html = markup(<EffectiveAccess workspace={team} />, [[`${ws}/access`, { ...access, key_id: null }]], client);
    expect(html).toContain("Not available (1)");
    expect(html).toContain("Unavailable");
    expect(html).not.toContain(">This key<");
    const card = markup(<AccessCard workspace={team} />, [[`${ws}/access`, access]]);
    expect(card).toContain("1 available · 1 unavailable"); expect(card).not.toContain("partly");
    expect(card).toMatch(/<a[^>]*href="\/workspaces\/team\/settings\?tab=access"[^>]*>See why/); expect(card).not.toContain("see why"); // one header link (review #41), to the Access tab
    // Layers are collapsed under "How limits combine", after the per-model reasons.
    expect(html.indexOf("Why can&#x27;t I use this model?")).toBeLessThan(html.indexOf("How limits combine"));
  });
});
