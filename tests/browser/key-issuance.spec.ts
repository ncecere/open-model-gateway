import { expect, test, type Page } from "@playwright/test";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, readFixture, signIn } from "./helpers";

// Creating a credential is setup only: never retain its response in traces or images.
test.use({ trace: "off", screenshot: "off", video: "off" });
test.afterEach(expectNoCspViolations);
test.describe.configure({ mode: "serial" });

// #253: bounded key issuance. A fresh Team with a low platform override, so the shared defaults other specs rely on are untouched.
const override = (holder: number, workspace: number) => ({ requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, budgets: [], key_issuances_per_holder: holder, key_issuances_per_workspace: workspace });
let workspace: string;
let operator: Awaited<ReturnType<typeof signIn>>, alex: Awaited<ReturnType<typeof signIn>>;
const keysPath = () => `/workspaces/${workspace}/keys`;
const usage = (page: Page) => page.getByTestId("key-issuance-usage");

test.beforeAll(async ({ browser }) => {
  operator = await signIn(browser, "operator");
  alex = await signIn(browser, "alex");
  const owner = await (await api(operator.page, "GET", "/me")).json();
  const member = await (await api(alex.page, "GET", "/me")).json();
  const created = await api(operator.page, "POST", "/platform/workspaces", { kind: "team", name: "Key issuance acceptance", owner_user_id: owner.user.id });
  expect(created.status()).toBe(200);
  workspace = (await created.json()).id;
  expect((await api(operator.page, "POST", `/platform/workspaces/${workspace}/members`, { user_id: member.user.id, role: "member" })).ok()).toBe(true);
  const model = readFixture().model;
  expect(model).toBeDefined();
  expect((await api(operator.page, "POST", `/platform/workspaces/${workspace}/models`, { model_id: model })).ok()).toBe(true);
  expect((await api(operator.page, "PUT", `/platform/workspaces/${workspace}/policy`, override(2, 3))).status()).toBe(200);
});
test.afterAll(async () => { await operator?.context.close(); await alex?.context.close(); });

async function createKey(page: Page, name: string) {
  await page.getByRole("button", { name: "Create key", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("textbox", { name: /^Name/ }).fill(name);
  await dialog.locator('button[type="submit"]').click();
  await expect(dialog.locator("textarea")).toBeVisible();
  await dialog.getByRole("button", { name: "I have saved it", exact: true }).click();
  await expect(dialog).toBeHidden();
}

test("a member creates keys until the budget is spent; Create explains the wait instead of failing", async () => {
  const { page } = alex;
  await page.goto(keysPath());
  await expect(usage(page)).toHaveText("0 of 2 issued in the last 24 h");
  await expect(page.getByRole("button", { name: "Create key", exact: true })).toBeEnabled();
  await createKey(page, "Issued one");
  await expect(usage(page)).toHaveText("1 of 2 issued in the last 24 h");
  await createKey(page, "Issued two");
  await expect(usage(page)).toHaveText("2 of 2 issued in the last 24 h");
  await expect(page.getByRole("button", { name: "Create key", exact: true })).toBeDisabled();
  // Warning-tone alerts are polite status regions (role="status"), not role="alert".
  const notice = page.getByRole("status").filter({ hasText: "Key issuance limit reached" });
  await expect(notice).toContainText("You have created or rotated 2 keys in the last 24 hours");
  await expect(notice).toContainText(/You can issue another in \d+ (hours?|minutes?)/);
  // The API agrees: a refusal is a 429 with Retry-After and no amounts, and it left no key behind.
  const refused = await api(page, "POST", keysPath(), { name: "Refused", expires_in_days: 1, model_ids: null });
  expect(refused.status()).toBe(429);
  expect(Number(refused.headers()["retry-after"])).toBeGreaterThan(0);
  const error = (await refused.json()).error;
  expect(error).toMatchObject({ reason: "key_issuance_limit_exceeded", scope: "holder", operation: "create", window_seconds: 86400 });
  expect(JSON.stringify(error)).not.toMatch(/"limit"|"used"/);
  const rows = await (await api(page, "GET", keysPath())).json();
  expect(rows.data.map((k: { name: string }) => k.name).sort()).toEqual(["Issued one", "Issued two"]);
  // Members never see the workspace-wide block.
  const view = await (await api(page, "GET", `/workspaces/${workspace}/key-issuance`)).json();
  expect(view.workspace).toBeUndefined();
  await expectNoHorizontalScroll(page);
  await expectAccessible(page, "keys page with a spent issuance budget");
  await page.setViewportSize({ width: 390, height: 844 });
  await expectNoHorizontalScroll(page);
  await page.setViewportSize({ width: 1440, height: 900 });
});

test("a request that loses the race shows a named refusal with the retry time in the dialog", async () => {
  const { page } = alex;
  // Room for one more; the dialog opens while there is.
  expect((await api(operator.page, "PUT", `/platform/workspaces/${workspace}/policy`, override(3, 3))).status()).toBe(200);
  await page.goto(keysPath());
  await expect(usage(page)).toHaveText("2 of 3 issued in the last 24 h");
  await page.getByRole("button", { name: "Create key", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("textbox", { name: /^Name/ }).fill("Loses the race");
  // Another client takes the last slot first.
  expect((await api(page, "POST", keysPath(), { name: "Takes the slot", expires_in_days: 1, model_ids: null })).status()).toBe(200);
  await dialog.locator('button[type="submit"]').click();
  const notice = dialog.getByRole("status").filter({ hasText: "Key issuance limit reached" });
  await expect(notice).toBeVisible();
  await expect(notice).toContainText(/You've reached this workspace's limit on creating or rotating keys\. You can try again in /);
  await expect(dialog.locator("textarea")).toHaveCount(0);
  await expectAccessible(page, "create key dialog after an issuance refusal");
  await dialog.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(usage(page)).toHaveText("3 of 3 issued in the last 24 h");
  await expect(page.getByRole("button", { name: "Create key", exact: true })).toBeDisabled();
});

test("raising the limit admits at once, and the limits editor validates and keeps tighten-only", async () => {
  const { page } = alex;
  expect((await api(operator.page, "PUT", `/platform/workspaces/${workspace}/policy`, override(5, 20))).status()).toBe(200);
  await page.goto(keysPath());
  await expect(usage(page)).toHaveText("3 of 5 issued in the last 24 h");
  await expect(page.getByRole("button", { name: "Create key", exact: true })).toBeEnabled();
  // The workspace owner edits the workspace limits (local, tighten-only).
  const admin = operator.page;
  await admin.goto(`/workspaces/${workspace}/settings?tab=limits`);
  const holder = admin.getByRole("textbox", { name: /^Keys per person per 24 h/ });
  const total = admin.getByRole("textbox", { name: /^Keys per workspace per 24 h/ });
  await expect(holder).toHaveAttribute("placeholder", "Inherited: 5");
  await expect(admin.getByRole("row", { name: /Keys per person per 24 h/ })).toContainText("5");
  await holder.fill("6");
  await expect(admin.getByText("Can't be higher than the platform override limit (5).")).toBeVisible();
  await expect(admin.getByRole("button", { name: "Save limits", exact: true })).toBeDisabled();
  await holder.fill("10001");
  await expect(admin.getByText("Enter 1 to 10,000, or leave blank to inherit.")).toBeVisible();
  await holder.fill("0");
  await expect(admin.getByText("Enter 1 to 10,000, or leave blank to inherit.")).toBeVisible();
  await holder.fill("4");
  await total.fill("20");
  await expect(admin.getByRole("button", { name: "Save limits", exact: true })).toBeEnabled();
  await expectAccessible(admin, "workspace limits with key issuance rows");
  await admin.getByRole("button", { name: "Save limits", exact: true }).click();
  await expect(admin.getByText("Limits saved", { exact: true })).toBeVisible();
  // The saved cap composes: 3 of 4 used, effective 4.
  await page.reload();
  await expect(usage(page)).toHaveText("3 of 4 issued in the last 24 h");
  // A saved cap can't be raised or removed (client check, and the API refuses too).
  await admin.reload();
  await holder.fill("5");
  await expect(admin.getByText("A saved limit can only be lowered (now 4).")).toBeVisible();
  const raised = await api(admin, "PUT", `/workspaces/${workspace}/policy`, { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, budgets: [], key_issuances_per_holder: 5 });
  expect(raised.status()).toBe(403);
  expect((await raised.json()).error).toMatchObject({ reason: "stored_rate_loosen_not_allowed", limit: "key_issuances_per_holder" });
  const above = await api(admin, "PUT", `/workspaces/${workspace}/policy`, { requests_per_minute: null, tokens_per_minute: null, concurrent_requests: null, budgets: [], key_issuances_per_holder: 3, key_issuances_per_workspace: 21 });
  expect(above.status()).toBe(400);
  expect((await above.json()).error).toMatchObject({ reason: "exceeds_parent_rate", limit: "key_issuances_per_workspace" });
});

test("the workspace owner sees the workspace's budget; members and unrelated people don't", async () => {
  const { page } = operator;
  const view = await (await api(page, "GET", `/workspaces/${workspace}/key-issuance`)).json();
  expect(view.holder).toMatchObject({ used: 0 });
  expect(view.workspace).toMatchObject({ used: 3, limit: 20 });
  await page.goto(keysPath());
  await expect(usage(page)).toContainText("This workspace: 3 of 20 issued in the last 24 h");
  const auditor = await signIn(page.context().browser()!, "auditor");
  try {
    expect((await api(auditor.page, "GET", `/workspaces/${workspace}/key-issuance`)).status()).toBe(403);
  } finally { await auditor.context.close(); }
});
