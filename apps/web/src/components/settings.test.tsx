// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { createMemoryHistory, createRouter, RouterProvider } from "@tanstack/react-router";
import { dashboardRouteTree } from "../router";
import { abortRequests } from "../lib/api";
import { admin, auditor, testClient } from "../lib/test-fixtures";
import { emailErrors, httpsUrlError, isLoopback, storageBody, storageDraft, storageErrorsOf, type EmailDraft, type EmailSettings, type PrivacySettings, type SignInSettings, type StorageGroup, type StorageSettings } from "../lib/settings";

beforeEach(() => { document.cookie = "omg_csrf=test-csrf; Path=/"; localStorage.clear(); sessionStorage.clear(); Object.defineProperty(Element.prototype, "getAnimations", { configurable: true, value: () => [] }); vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} }); Object.defineProperty(window, "matchMedia", { configurable: true, value: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent() { return false; } }) }); });
afterEach(() => { cleanup(); abortRequests(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

const general = { display_name: "Example gateway", support_url: null, logo_url: null, human_key_max_lifetime_days: 365, timezone: "UTC", updated_at: "2026-10-08T00:00:00Z", updated_by: null };
const privacy: PrivacySettings = {
  openrouter_data_collection: { value: "deny", stored: "deny", locked: false, source: "installation", variable: "GATEWAY_OPENROUTER_DATA_COLLECTION" },
  request_log_retention_days: { value: null, stored: null, locked: false, source: "installation", variable: "GATEWAY_EXECUTION_DETAIL_RETENTION_DAYS", minimum: 30, maximum: 3650 },
  prompt_response_storage: "never_stored", updated_at: "2026-10-08T00:00:00Z",
};
const email: EmailSettings = { configured: false, status: "not_configured", host: null, port: null, tls: null, username: null, password_ref: null, password_ref_allowed: null, from_address: null, from_name: null, public_url_configured: true, last_test: null, updated_at: "2026-10-08T00:00:00Z" };
const group = (g: StorageGroup["group"], label: string, days: number | null, toggle: boolean): StorageGroup => ({ group: g, label, purposes: [], holds_customer_content: toggle, toggle, enabled: !toggle, active: false, retention_days: days, retention_editable: days !== null, default_retention_days: days, minimum: 1, maximum: 365, objects: 0, bytes: 0 });
const storageOff: StorageSettings = { backend: "off", location: null, encryption: null, health: null, updated_at: "2026-10-08T00:00:00Z", groups: [group("batch", "Batch files", 7, true), group("video", "Video outputs", 7, true), group("user_files", "User files", 30, true), group("export", "Exports", 1, false), group("branding", "Branding", null, false)] };
const storageOn: StorageSettings = { ...storageOff, backend: "s3", location: { kind: "s3", bucket: "omg-files", region: "us-east-1", endpoint_host: "minio.internal", endpoint_tls: false, path_style: true, prefix_set: true, auth: "static" }, encryption: { key_id: "k2026", decrypt_only_keys: 1 }, health: { checked_at: "2026-10-08T00:00:00Z", ok: true, error: null, current: true }, groups: storageOff.groups.map(g => g.group === "export" ? { ...g, active: true, objects: 2, bytes: 1500 } : g) };
const ready: EmailSettings = { ...email, configured: true, status: "ready", host: "smtp.example.com", port: 587, tls: "starttls", from_address: "gateway@example.com" };

type Handler = (path: string, init?: RequestInit) => unknown;
function serve(session = admin, handler: Handler = () => undefined) {
  const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
    if (path === "/api/v1/me") return Promise.resolve(Response.json(session));
    const custom = handler(path, init);
    if (custom instanceof Response) return Promise.resolve(custom);
    if (custom !== undefined) return Promise.resolve(Response.json(custom));
    if (path.endsWith("/settings/general")) return Promise.resolve(Response.json(general));
    if (path.endsWith("/settings/privacy")) return Promise.resolve(Response.json(privacy));
    if (path.endsWith("/settings/email")) return Promise.resolve(Response.json(email));
    if (path.endsWith("/settings/storage")) return Promise.resolve(Response.json(storageOff));
    return Promise.resolve(Response.json({ data: [], has_more: false }));
  });
  vi.stubGlobal("fetch", fetch); return fetch;
}
async function mount(href: string) {
  const client = testClient(), router = createRouter({ routeTree: dashboardRouteTree, history: createMemoryHistory({ initialEntries: [href] }) });
  await router.load();
  render(<QueryClientProvider client={client}><RouterProvider router={router} /></QueryClientProvider>);
  return { router, client };
}
const puts = (fetch: ReturnType<typeof vi.fn>) => fetch.mock.calls.filter(c => (c[1] as RequestInit | undefined)?.method === "PUT").map(c => [String(c[0]), JSON.parse(String((c[1] as RequestInit).body))]);

describe("Admin › Settings", () => {
  it("adds a Settings sidebar group with its five pages, Limits moved into it", async () => {
    serve();
    const { client } = await mount("/admin/settings/general");
    await screen.findByRole("heading", { name: "General", level: 1 });
    const nav = screen.getByRole("navigation", { name: "Main" });
    for (const [label, href] of [["General", "/admin/settings/general"], ["Defaults & limits", "/admin/settings/limits"], ["Data & privacy", "/admin/settings/privacy"], ["Email", "/admin/settings/email"], ["Sign-in", "/admin/settings/sign-in"]]) expect(within(nav).getByRole("link", { name: label }).getAttribute("href")).toBe(href);
    expect(within(nav).queryByRole("link", { name: "Limits" })).toBeNull();
    client.clear();
  });
  it("validates and saves General, refreshing the session", async () => {
    const user = userEvent.setup(), fetch = serve();
    const { client } = await mount("/admin/settings/general");
    const support = await screen.findByRole("textbox", { name: /Support link/ });
    await user.type(support, "http://help.example.com");
    const days = screen.getByRole("textbox", { name: /Longest lifetime/ });
    await user.clear(days); await user.type(days, "90");
    await user.click(await screen.findByRole("button", { name: "Save settings" }));
    expect(await screen.findByText("Use an https:// address.")).toBeTruthy();
    expect(puts(fetch)).toEqual([]);
    await user.clear(support); await user.type(support, "https://help.example.com");
    await user.click(screen.getByRole("button", { name: "Save settings" }));
    await waitFor(() => expect(puts(fetch)).toEqual([["/api/v1/platform/settings/general", { display_name: "Example gateway", support_url: "https://help.example.com", logo_url: null, human_key_max_lifetime_days: 90 }]]));
    client.clear();
  });
  it("shows Auditors read-only values, never a form", async () => {
    serve(auditor);
    const { client } = await mount("/admin/settings/general");
    expect(await screen.findByText("Longest lifetime for a new personal key")).toBeTruthy();
    expect(screen.getByText("365 days")).toBeTruthy();
    expect(document.querySelector("main input")).toBeNull();
    expect(screen.queryByRole("button", { name: "Save settings" })).toBeNull();
    client.clear();
  });
  it("shows an environment-locked privacy setting with its variable and states prompts are never stored", async () => {
    const user = userEvent.setup(), fetch = serve(admin, path => path.endsWith("/settings/privacy") ? { ...privacy, openrouter_data_collection: { ...privacy.openrouter_data_collection, value: "allow", locked: true, source: "environment" } } : undefined);
    const { client } = await mount("/admin/settings/privacy");
    expect(await screen.findByText("GATEWAY_OPENROUTER_DATA_COLLECTION")).toBeTruthy();
    expect(screen.getByText("Allow")).toBeTruthy();
    expect(screen.queryByRole("radiogroup")).toBeNull();
    expect(screen.getByText("Never stored")).toBeTruthy();
    // Retention stays editable and the save resends the locked value unchanged.
    await user.type(screen.getByRole("textbox", { name: /Compact details after/ }), "90");
    await user.click(await screen.findByRole("button", { name: "Save settings" }));
    await waitFor(() => expect(puts(fetch)).toEqual([["/api/v1/platform/settings/privacy", { openrouter_data_collection: "deny", request_log_retention_days: 90 }]]));
    client.clear();
  });
  it("sets up email with a password reference and places server reasons on the field", async () => {
    const user = userEvent.setup(), fetch = serve(admin, (path, init) => init?.method === "PUT" && path.endsWith("/settings/email") ? Response.json({ error: { code: "400", message: "The password reference is not on the server allowlist", reason: "credential_reference_not_allowed" } }, { status: 400 }) : undefined);
    const { client } = await mount("/admin/settings/email");
    // Without a relay the disabled test button says why.
    expect(await screen.findByText("Turn on Send email below to test delivery.")).toBeTruthy();
    await user.click(await screen.findByRole("switch", { name: /Send email/ }));
    await user.type(screen.getByRole("textbox", { name: "Host" }), "smtp.example.com");
    await user.type(screen.getByRole("textbox", { name: /^Username/ }), "relay");
    await user.type(screen.getByRole("textbox", { name: /Password reference/ }), "env:SMTP_PASSWORD");
    await user.type(screen.getByRole("textbox", { name: "From address" }), "gateway@example.com");
    await user.click(screen.getByRole("button", { name: "Save email settings" }));
    await waitFor(() => expect(puts(fetch)).toEqual([["/api/v1/platform/settings/email", { host: "smtp.example.com", port: 587, tls: "starttls", username: "relay", password_ref: "env:SMTP_PASSWORD", from_address: "gateway@example.com", from_name: null }]]));
    expect(await screen.findByText(/isn't on the server allowlist/)).toBeTruthy();
    // The password itself is never asked for.
    expect(document.querySelector('input[type="password"]')).toBeNull();
    client.clear();
  });
  it("sends a test email to the signed-in admin and reports the outcome", async () => {
    const user = userEvent.setup(), fetch = serve(admin, (path, init) => path.endsWith("/email/test") && init?.method === "POST" ? { ok: false, error: "authentication", recipient: admin.user.email } : path.endsWith("/settings/email") ? ready : undefined);
    const { client } = await mount("/admin/settings/email");
    expect(await screen.findByText(`Sends a short test message to ${admin.user.email}. At most 2 a minute.`)).toBeTruthy();
    await user.click(screen.getByRole("button", { name: /Send test email/ }));
    expect(await screen.findByText("The relay rejected the username or password.")).toBeTruthy();
    expect(fetch.mock.calls.some(c => String(c[0]) === "/api/v1/platform/settings/email/test" && (c[1] as RequestInit).method === "POST" && new Headers((c[1] as RequestInit).headers).get("X-CSRF-Token") === "test-csrf")).toBe(true);
    client.clear();
  });
  it("shows the read-only sign-in configuration and links to SSO groups", async () => {
    serve(auditor, path => path.endsWith("/settings/sign-in") ? { enabled: true, issuer: "https://login.example.com", client_id: "gateway", client_type: "confidential", groups_claim: "groups", public_url: "https://gateway.example.com", callback_url: "https://gateway.example.com/api/v1/auth/callback", secure_cookies: true, enabled_group_mappings: 3 } : undefined);
    const { client } = await mount("/admin/settings/sign-in");
    expect(await screen.findByText("https://login.example.com")).toBeTruthy();
    expect(document.querySelector("main")!.innerHTML).toContain("https://gateway.example.com/api/v1/auth/callback");
    expect(document.querySelector("main")!.textContent).not.toMatch(/secret value/i);
    expect(screen.getByRole("link", { name: /SSO groups/ }).getAttribute("href")).toBe("/admin/sso-groups");
    expect(screen.getByText("3")).toBeTruthy();
    // Older servers without SCIM/JWKS fields: provisioning shows as off.
    expect(screen.getByRole("heading", { name: "Provisioning (SCIM)" })).toBeTruthy();
    expect(screen.getByText("Off")).toBeTruthy();
    expect(screen.getByText("GATEWAY_SCIM_TOKEN_ENV")).toBeTruthy();
    client.clear();
  });
  it("shows SCIM status and the cached signing keys read-only, never a token", async () => {
    const signIn: SignInSettings = {
      enabled: true, issuer: "https://login.example.com", client_id: "gateway", client_type: "public", groups_claim: "groups", public_url: "https://gateway.example.com", callback_url: "https://gateway.example.com/api/v1/auth/callback", enabled_group_mappings: 2,
      jwks: { keys: 2, refreshed_at: new Date(Date.now() - 5 * 60_000).toISOString(), fresh_until: new Date(Date.now() + 55 * 60_000).toISOString(), last_failure_at: null, state: "fresh" },
      scim: { enabled: true, base_url: "https://gateway.example.com/scim/v2", users: 12, active_users: 10, groups: 3, memberships: 1, last_sync_at: new Date(Date.now() - 60 * 60_000).toISOString() },
    };
    serve(auditor, path => path.endsWith("/settings/sign-in") ? signIn : undefined);
    const { client } = await mount("/admin/settings/sign-in");
    expect(await screen.findByText("Provisioning (SCIM)")).toBeTruthy();
    const main = document.querySelector("main")!;
    expect(main.innerHTML).toContain("https://gateway.example.com/scim/v2");
    expect(screen.getByRole("button", { name: /Copy SCIM base URL/i })).toBeTruthy();
    expect(main.textContent).toContain("10 active of 12");
    expect(main.textContent).toContain("3 groups · 1 membership");
    expect(main.textContent).toMatch(/2 keys · refreshed/);
    expect(screen.getByText("Current")).toBeTruthy();
    expect(main.querySelectorAll("time").length).toBeGreaterThanOrEqual(2);
    // Read-only: no inputs other than copy fields, no edits.
    expect(main.querySelectorAll("input:not([readonly])").length).toBe(0);
    expect(main.textContent).not.toMatch(/token value|secret value/i);
    client.clear();
  });
  it("flags stale or unavailable signing keys and a never-synced SCIM endpoint", async () => {
    const signIn: SignInSettings = {
      enabled: true, issuer: "https://login.example.com", client_id: "gateway", client_type: "public", groups_claim: "groups", public_url: "https://gateway.example.com", callback_url: "https://gateway.example.com/api/v1/auth/callback", enabled_group_mappings: 0,
      jwks: { keys: 1, refreshed_at: "2026-10-08T00:00:00Z", fresh_until: "2026-10-08T01:00:00Z", last_failure_at: "2026-10-08T02:00:00Z", state: "unavailable" },
      scim: { enabled: true, base_url: "https://gateway.example.com/scim/v2", users: 0, active_users: 0, groups: 0, memberships: 0, last_sync_at: null },
    };
    serve(admin, path => path.endsWith("/settings/sign-in") ? signIn : undefined);
    const { client } = await mount("/admin/settings/sign-in");
    expect(await screen.findByText("Unavailable")).toBeTruthy();
    expect(screen.getByText("Never")).toBeTruthy();
    expect(document.querySelector("main")!.textContent).toContain("1 key · refreshed");
    client.clear();
  });
});

describe("Admin › Settings › Data & privacy › Storage", () => {
  it("shows the store as off and keeps customer-content toggles unavailable", async () => {
    serve();
    const { client } = await mount("/admin/settings/privacy");
    expect(await screen.findByRole("heading", { name: "Storage" })).toBeTruthy();
    expect(await screen.findByText("GATEWAY_FILE_STORE")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Test storage/ })).toBeNull();
    const batch = screen.getByRole("switch", { name: /Allow batch files/ });
    expect(batch.getAttribute("aria-disabled") === "true" || batch.hasAttribute("disabled") || batch.getAttribute("data-disabled") !== null).toBe(true);
    expect(screen.getByText("No expiry")).toBeTruthy();
    client.clear();
  });
  it("lets admins allow batch storage, edit retention and test the store", async () => {
    const user = userEvent.setup(), fetch = serve(admin, (path, init) => path.endsWith("/storage/test") && init?.method === "POST" ? { ok: false, error: "denied", backend: "s3", round_trip_ms: null } : path.endsWith("/settings/storage") && (init?.method ?? "GET") === "GET" ? storageOn : undefined);
    const { client } = await mount("/admin/settings/privacy");
    expect(await screen.findByText("omg-files · minio.internal (HTTP)")).toBeTruthy();
    expect(screen.getByText("k2026")).toBeTruthy();
    expect(screen.getByText("+1 decrypt-only")).toBeTruthy();
    expect(screen.getByText("Healthy")).toBeTruthy();
    expect(screen.getByText("1.5 KB")).toBeTruthy(); // same binary units as Files and quotas (1 KB = 1024 bytes)
    expect(screen.getByText("1.5 KB").getAttribute("title")).toBe("2 files · 1,500 bytes");
    await user.click(screen.getByRole("switch", { name: /Allow batch files/ }));
    const days = screen.getByRole("textbox", { name: "Exports retention in days" });
    await user.clear(days); await user.type(days, "400");
    await user.click(screen.getByRole("button", { name: "Save storage" }));
    expect(await screen.findByText("Enter 1–365 days.")).toBeTruthy();
    expect(puts(fetch).filter(([p]) => p.endsWith("/storage"))).toEqual([]);
    await user.clear(days); await user.type(days, "3");
    await user.click(screen.getByRole("button", { name: "Save storage" }));
    await waitFor(() => expect(puts(fetch).filter(([p]) => p.endsWith("/storage"))).toEqual([["/api/v1/platform/settings/storage", { batch: { enabled: true, retention_days: 7 }, video: { enabled: false, retention_days: 7 }, user_files: { enabled: false, retention_days: 30 }, export: { retention_days: 3 } }]]));
    await user.click(screen.getByRole("button", { name: /Test storage/ }));
    expect(await screen.findByText("The store refused the credentials or permissions.")).toBeTruthy();
    client.clear();
  });
  it("shows Auditors storage values read-only", async () => {
    serve(auditor, path => path.endsWith("/settings/storage") ? storageOn : undefined);
    const { client } = await mount("/admin/settings/privacy");
    expect(await screen.findByText("omg-files · minio.internal (HTTP)")).toBeTruthy();
    expect(screen.queryByRole("switch")).toBeNull();
    expect(screen.queryByRole("button", { name: /Test storage/ })).toBeNull();
    expect(screen.getByText("30 days")).toBeTruthy();
    client.clear();
  });
  it("validates storage drafts like the server", () => {
    const draft = storageDraft(storageOn);
    expect(storageErrorsOf(storageOn, draft)).toEqual({});
    expect(Object.keys(storageErrorsOf(storageOn, { ...draft, batch: { enabled: true, days: "0" } }))).toEqual(["batch"]);
    expect(storageBody(draft).export).toEqual({ retention_days: 1 });
  });
});

describe("settings validation", () => {
  it("mirrors the server's checks", () => {
    expect(httpsUrlError("")).toBeUndefined();
    expect(httpsUrlError("https://help.example.com/x")).toBeUndefined();
    for (const bad of ["http://x.example", "javascript:alert(1)", "https://u:p@x.example", "https://x.example/#frag", "not a url"]) expect(httpsUrlError(bad)).toBeTruthy();
    expect(isLoopback("localhost") && isLoopback("127.0.0.1") && isLoopback("::1")).toBe(true);
    expect(isLoopback("10.0.0.1")).toBe(false);
    const draft: EmailDraft = { host: "smtp.example.com", port: "587", tls: "starttls", username: "", password_ref: "", from_address: "gateway@example.com", from_name: "" };
    expect(emailErrors(draft)).toEqual({});
    expect(Object.keys(emailErrors({ ...draft, tls: "none" }))).toEqual(["tls"]);
    expect(emailErrors({ ...draft, tls: "none", host: "localhost" })).toEqual({});
    expect(Object.keys(emailErrors({ ...draft, username: "relay" }))).toEqual(["password_ref"]);
    expect(Object.keys(emailErrors({ ...draft, username: "relay", password_ref: "hunter2" }))).toEqual(["password_ref"]);
    expect(Object.keys(emailErrors({ ...draft, host: "smtp.example.com:587", port: "0", from_address: "nope" })).sort()).toEqual(["from_address", "host", "port"]);
  });
});
