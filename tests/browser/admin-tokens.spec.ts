/*
 * Admin API tokens (#37 PR3) acceptance journey: create -> copy -> dismiss -> revoke, role
 * 403/deep-link refusals, and membership/privacy refusals exercised with the token itself
 * (never another owner's personal keys/requests, never admin-token CRUD, never inference).
 *
 * Wired into playwright.config.ts `projects` as "admin-tokens". Execution still depends on the
 * route declarations, scope enforcement and `/api/v1/platform/admin-tokens` endpoints (#37 PR2),
 * which land on `main` separately from this UI-only branch: running this project before PR2 is
 * integrated fails at the real gateway, not at discovery. `npx playwright test --list` proves
 * this file is discovered; a real run is the coordinator's to perform once PR2 and a built UI
 * are both in place.
 */
import { expect, request, test, type APIRequestContext, type Page } from "@playwright/test";
import { api, chat, expectNoCspViolations, signIn } from "./helpers";
import { BASE_URL } from "./stack.mjs";

test.afterEach(expectNoCspViolations);
// A real admin token is a privileged management credential: never retain it in traces/images.
test.use({ trace: "off", screenshot: "off", video: "off" });

const ADMIN_TOKENS_PATH = "/admin/settings/admin-tokens";
let number = 0;
const tokenName = () => `Admin token acceptance ${++number}`;
/** Format from the approved spec: `omg_admin_<32-hex>.<64-hex>`. */
const TOKEN_SHAPE = /^omg_admin_[0-9a-f]{32}\.[0-9a-f]{64}$/;

/** Only the clipboard is mocked; the Copy button still runs the real production handler. */
async function stubClipboard(page: Page) {
  await page.evaluate(() => {
    let copied = "";
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: {
      writeText: async (value: string) => { copied = value; },
      readText: async () => copied,
    } });
  });
}
const copiedValue = (page: Page) => page.evaluate(() => navigator.clipboard.readText());

async function dismissNotices(page: Page) {
  const dismiss = page.getByRole("button", { name: "Dismiss notification", exact: true });
  const count = await dismiss.count();
  for (let index = 0; index < count; index++) await dismiss.first().click();
}

/*
 * A raw bearer request, bypassing the browser session entirely: this is what a script using the
 * issued token would send. `page.request` is NOT this: it shares the page's browser context,
 * including its session cookie, so a request built from it is really a mixed cookie+Bearer
 * request (rejected globally - see the dedicated test below) and never proves the bearer-only
 * path. Use a disposable, cookie-free `APIRequestContext` (no storageState) for every bearer-only
 * call instead; the caller creates one per test and disposes it in `finally`.
 */
async function bearerClient(): Promise<APIRequestContext> {
  return request.newContext({ storageState: { cookies: [], origins: [] } });
}
function bearerRequest(client: APIRequestContext, token: string, path: string, init: { method?: string; data?: unknown } = {}) {
  return client.fetch(`${BASE_URL}/api/v1${path}`, { method: init.method ?? "GET", data: init.data, headers: { authorization: `Bearer ${token}` }, failOnStatusCode: false });
}

test.describe("Admin API tokens role authority", () => {
  test("Admin opens the page; Auditor's sidebar hides the link and a direct deep link denies", async ({ browser }) => {
    const operator = await signIn(browser, "operator");
    const auditor = await signIn(browser, "auditor");
    try {
      await operator.page.goto(ADMIN_TOKENS_PATH);
      await expect(operator.page.getByRole("heading", { name: "Admin API tokens" })).toBeVisible();

      await auditor.page.goto("/admin/settings/general");
      await expect(auditor.page.getByRole("link", { name: "Admin API tokens" })).toHaveCount(0);
      await auditor.page.goto(ADMIN_TOKENS_PATH);
      await expect(auditor.page.getByRole("heading", { name: "Admin API tokens" })).toHaveCount(0);
      await expect(auditor.page.getByText("Access not available")).toBeVisible();
      expect((await api(auditor.page, "GET", "/platform/admin-tokens")).status()).toBe(403);
    } finally { await operator.context.close(); await auditor.context.close(); }
  });

  test("a plain workspace member (no platform role) cannot reach the page or the API", async ({ browser }) => {
    const alex = await signIn(browser, "alex");
    try {
      await alex.page.goto(ADMIN_TOKENS_PATH);
      await expect(alex.page.getByText("Access not available")).toBeVisible();
      expect((await api(alex.page, "GET", "/platform/admin-tokens")).status()).toBe(403);
    } finally { await alex.context.close(); }
  });
});

test("create -> copy -> dismiss -> revoke", async ({ browser }) => {
  const { page, context } = await signIn(browser, "operator");
  const bearer = await bearerClient();
  try {
    await page.goto(ADMIN_TOKENS_PATH);
    await stubClipboard(page);
    const name = tokenName();
    await page.getByRole("button", { name: "Create token", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByRole("textbox", { name: "Name" }).fill(name);
    await dialog.getByRole("checkbox", { name: "Catalog" }).check();
    await dialog.locator('button[type="submit"]').click();

    await expect(dialog.getByText("Shown only once")).toBeVisible();
    const disclosure = dialog.getByRole("textbox", { name: "Admin API token" });
    const token = await disclosure.inputValue();
    expect(TOKEN_SHAPE.test(token)).toBe(true);

    await dialog.getByRole("button", { name: "Copy token" }).click();
    await expect(dialog.getByRole("button", { name: "Copied" })).toBeVisible();
    expect((await copiedValue(page)) === token).toBe(true);

    await dialog.getByRole("button", { name: "I have saved it" }).click();
    await expect(dialog).toHaveCount(0);

    const row = page.getByRole("row", { name: new RegExp(name) });
    await expect(row).toBeVisible();
    await expect(row.getByText("Active")).toBeVisible();
    await row.getByRole("button", { name: `Actions for ${name}` }).click();
    await page.getByRole("menuitem", { name: "Revoke token…" }).click();
    await page.getByRole("dialog").locator('button[type="submit"]').click();
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await expect(row.getByText("Revoked")).toBeVisible();

    // Revocation is permanent: the action disappears, and even Rust refuses a revoked token.
    await row.getByRole("button", { name: `Actions for ${name}` }).click();
    await expect(page.getByRole("menuitem", { name: "Revoke token…" })).toHaveCount(0);
    await dismissNotices(page);
    // Revocation is a credential failure for a bearer-only client: always 401, never 403.
    expect((await bearerRequest(bearer, token, "/platform/users")).status()).toBe(401);
  } finally { await bearer.dispose(); await context.close(); }
});

test("a bearer token paired with an active browser session cookie is rejected globally (401), not resolved by cookie precedence", async ({ browser }) => {
  const { page, context } = await signIn(browser, "operator");
  try {
    const created = await api(page, "POST", "/platform/admin-tokens", { name: tokenName(), scopes: ["platform:read"], expires_in_days: 1 });
    expect(created.status()).toBe(200);
    const body = await created.json();
    // page.request shares this page's browser context, including its session cookie: adding a
    // Bearer header on top makes this a mixed cookie+Bearer request, kept as its own explicit
    // case precisely because it looks like `bearerRequest` built on `page.request` used to.
    const csrf = (await page.context().cookies(BASE_URL)).find(c => c.name === "omg_csrf")?.value ?? "";
    const mixed = await page.request.fetch(`${BASE_URL}/api/v1/platform/users`, { headers: { authorization: `Bearer ${body.token}`, origin: BASE_URL, "x-csrf-token": csrf }, failOnStatusCode: false });
    expect(mixed.status()).toBe(401);
    expect((await api(page, "DELETE", `/platform/admin-tokens/${body.id}`)).ok()).toBe(true);
  } finally { await context.close(); }
});

test("an unrevoked token can never manage its own kind, see another owner's personal data, or call inference", async ({ browser }) => {
  const { page, context } = await signIn(browser, "operator");
  const bearer = await bearerClient();
  try {
    const alex = await signIn(browser, "alex");
    let alexPersonal: string;
    try {
      const alexMe = await (await api(alex.page, "GET", "/me")).json();
      alexPersonal = alexMe.workspaces.find((w: { kind: string }) => w.kind === "personal").id;
    } finally { await alex.context.close(); }

    const created = await api(page, "POST", "/platform/admin-tokens", { name: tokenName(), scopes: ["platform:read", "access:write"], expires_in_days: 1 });
    expect(created.status()).toBe(200);
    const body = await created.json();
    expect(typeof body.token === "string" && TOKEN_SHAPE.test(body.token)).toBe(true);

    // BrowserAdminOnly: a valid, unrevoked bearer credential is still authenticated, so this is
    // an authorization refusal (403), never 401 - no scope ever lets a token manage its own kind.
    expect((await bearerRequest(bearer, body.token, "/platform/admin-tokens")).status()).toBe(403);
    expect((await bearerRequest(bearer, body.token, "/platform/admin-tokens", { method: "POST", data: { name: "nested", scopes: ["platform:read"], expires_in_days: 1 } })).status()).toBe(403);
    expect((await bearerRequest(bearer, body.token, `/platform/admin-tokens/${body.id}`, { method: "DELETE" })).status()).toBe(403);

    // platform:read covers this existing PlatformRead route.
    expect((await bearerRequest(bearer, body.token, "/platform/users")).status()).toBe(200);
    // The same scope reaches the workspace list route, but personal-workspace privacy still
    // applies underneath it: never another owner's keys or request details.
    expect((await bearerRequest(bearer, body.token, `/workspaces/${alexPersonal}/keys`)).status()).toBe(403);
    expect((await bearerRequest(bearer, body.token, `/workspaces/${alexPersonal}/requests`)).status()).toBe(403);

    // Never usable for inference or to issue inference keys, whatever scopes it holds.
    expect((await chat(body.token)).status).toBe(401);
    expect((await bearerRequest(bearer, body.token, `/workspaces/${alexPersonal}/keys`, { method: "POST", data: { name: "forged", expires_in_days: 1 } })).status()).toBe(403);

    expect((await api(page, "DELETE", `/platform/admin-tokens/${body.id}`)).ok()).toBe(true);
  } finally { await bearer.dispose(); await context.close(); }
});
