import { expect, test, type Page } from "@playwright/test";
import { BASE_URL, ISSUER_URL, UPSTREAM_URL } from "./stack.mjs";
import { MOCK_REPLY } from "./mock-upstream.mjs";
import { api, chat, expectNoHorizontalScroll, readFixture, saveFixture, signIn } from "./helpers";

// One ordered journey on a fresh database: each step builds on the previous one.
test.describe.configure({ mode: "serial" });

const idFrom = (page: Page) => new URL(page.url()).pathname.split("/").pop()!;

test("an authenticated but unentitled SSO user is denied", async ({ browser }) => {
  const { context, page } = await signIn(browser, "unentitled");
  await expect(page.getByRole("link", { name: "Sign in with single sign-on" })).toBeVisible();
  expect((await api(page, "GET", "/me")).status()).toBe(401);
  await context.close();
});

test("platform admin creates a team and a project", async ({ browser }) => {
  const { context, page } = await signIn(browser, "operator");
  for (const [section, kind, name] of [["teams", "team", "Product"], ["projects", "project", "Research"]] as const) {
    await page.goto(`/admin/${section}`);
    await page.waitForLoadState("networkidle"); // owner choices load before the dialog opens
    await page.getByRole("button", { name: `Create ${kind}` }).first().click();
    const dialog = page.getByRole("dialog", { name: `Create ${kind}` });
    await dialog.getByRole("textbox", { name: "Name", exact: true }).fill(name);
    await dialog.getByRole("combobox", { name: "Initial owner", exact: true }).selectOption({ label: "operator@demo.invalid" });
    await dialog.getByRole("button", { name: `Create ${kind}` }).click();
    await expect(dialog).toBeHidden();
    await page.getByRole("link", { name, exact: true }).click();
    await expect(page.getByRole("heading", { level: 1, name })).toBeVisible();
    saveFixture(kind === "team" ? { product: idFrom(page) } : { research: idFrom(page) });
  }
  await context.close();
});

test("platform admin maps SSO groups to roles and memberships", async ({ browser }) => {
  const { context, page } = await signIn(browser, "operator");
  await page.goto("/admin/sso-groups");
  const mappings: [string, string, string?][] = [
    ["omg/platform-auditor", "Platform auditor"], ["omg/platform-user", "Platform user"],
    ["omg/product-admin", "Product · team", "Admin"], ["omg/product-member", "Product · team", "Member"], ["omg/research-member", "Research · project", "Member"],
  ];
  for (const [group, target, role] of mappings) {
    await page.getByRole("button", { name: "Create mapping" }).first().click();
    const dialog = page.getByRole("dialog", { name: "Create SSO group mapping" });
    await dialog.getByLabel("OIDC issuer").fill(ISSUER_URL);
    await dialog.getByLabel("Verified group-claim value").fill(group);
    if (!role) await dialog.getByLabel("Platform role").selectOption({ label: target });
    else {
      await dialog.getByLabel("Mapping target").selectOption({ label: "Team or Project membership" });
      await dialog.getByLabel("Shared workspace").selectOption({ label: target });
      await dialog.getByLabel("Membership role").selectOption({ label: role });
    }
    await dialog.getByRole("button", { name: "Create mapping" }).click();
    await expect(dialog).toBeHidden();
  }
  await expect(page.getByRole("table", { name: "SSO group mappings" }).getByRole("row")).toHaveCount(mappings.length + 1);
  await context.close();
});

test("platform admin adds a mock connection and a priced model for Product", async ({ browser }) => {
  const { context, page } = await signIn(browser, "operator");
  await page.goto("/admin/connections");
  await page.getByRole("button", { name: "Add connection" }).first().click();
  const dialog = page.getByRole("dialog", { name: "Add connection" });
  await dialog.getByRole("textbox", { name: "Name", exact: true }).fill("Mock upstream");
  await dialog.getByRole("combobox", { name: "Provider profile" }).click();
  await page.getByRole("option", { name: "OpenAI-compatible" }).click();
  await dialog.getByLabel("Authentication").selectOption({ label: "No authentication" });
  await dialog.getByLabel("Endpoint").fill(UPSTREAM_URL);
  await dialog.getByRole("button", { name: "Add connection" }).click();
  await expect(dialog).toBeHidden();
  await expect(page.getByRole("row", { name: /Mock upstream OpenAI-compatible/ })).toBeVisible();

  await page.goto("/admin/models/new");
  const form = page.getByRole("form", { name: "Add model" });
  await expect(form.getByLabel("Connection")).toHaveValue(/.+/);
  await form.getByLabel("Upstream model ID").fill("mock-chat-1");
  await form.getByLabel("Display name").fill("Mock Chat");
  await form.getByLabel("API model name").fill("mock/chat");
  await form.getByRole("switch", { name: "Enabled" }).click();
  await page.getByRole("button", { name: /Add price now/ }).click();
  await page.getByLabel("Hard upstream input token ceiling").fill("4096");
  await page.getByLabel("Hard upstream output token ceiling").fill("1024");
  await page.getByLabel("Input tokens price").selectOption({ label: "Priced" });
  await page.getByLabel("$ per M input tokens").fill("2.50");
  await page.getByLabel("Output tokens price").selectOption({ label: "Priced" });
  await page.getByLabel("$ per M output tokens").fill("10");
  for (const meter of ["Cache read", "Cache write (default)", "Cache write (5-minute)", "Cache write (1-hour)"]) await page.getByLabel(`${meter} price`).selectOption({ label: "Not applicable" });
  await page.getByLabel("Requests price").selectOption({ label: "Free · explicit $0" });
  await page.getByRole("button", { name: "Add model", exact: true }).click();
  await expect(page.getByRole("heading", { level: 1, name: "Mock Chat" })).toBeVisible();
  await expect(page.getByText("$2.50 / $10.00")).toBeVisible();
  saveFixture({ model: idFrom(page) });

  // Direct assignment from the Team page (no catalog needed).
  await page.goto(`/admin/teams/${readFixture().product}`);
  await page.getByRole("tab", { name: /Models/ }).click();
  await page.getByRole("button", { name: "Assign models" }).first().click();
  const assign = page.getByRole("dialog", { name: "Assign models to this team" });
  await assign.getByRole("checkbox", { name: /Mock Chat/ }).check();
  await assign.getByRole("button", { name: "Assign models" }).click();
  await expect(assign).toBeHidden();
  await expect(page.getByRole("tab", { name: /Models 1/ })).toBeVisible();
  await context.close();
});

test("auditor reads Admin without mutation controls or workspace secrets", async ({ browser }) => {
  const { context, page } = await signIn(browser, "auditor");
  const { product, model } = readFixture();
  await page.goto("/admin/teams");
  await expect(page.getByRole("row", { name: /Product/ })).toBeVisible();
  await expect(page.getByRole("button", { name: "Create team" })).toHaveCount(0);
  await page.goto("/admin/connections");
  await expect(page.getByRole("row", { name: /Mock upstream/ })).toBeVisible();
  await expect(page.getByRole("button", { name: "Add connection" })).toHaveCount(0);
  await page.goto(`/admin/models/${model}`);
  await expect(page.getByRole("heading", { level: 1, name: "Mock Chat" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Disable" })).toHaveCount(0);
  await page.goto("/admin/sso-groups");
  await expect(page.getByRole("button", { name: "Create mapping" })).toHaveCount(0);
  // The server, not hidden controls, enforces read-only access.
  expect((await api(page, "POST", "/platform/workspaces", { name: "Nope", kind: "team", owner_user_id: "00000000-0000-4000-8000-000000000000" })).status()).toBe(403);
  expect((await api(page, "PATCH", `/platform/models/${model}`, { enabled: false })).status()).toBe(403);
  expect((await api(page, "GET", `/workspaces/${product}/keys`)).status()).toBe(403);
  await context.close();
});

test("team admin Alex administers Product but has no platform Admin", async ({ browser }) => {
  const { context, page } = await signIn(browser, "alex");
  const { product } = readFixture();
  await expect(page.getByRole("link", { name: "Admin", exact: true })).toHaveCount(0);
  await page.goto(`/workspaces/${product}/keys`);
  await expect(page.getByRole("heading", { level: 1, name: "API keys" })).toBeVisible();
  expect((await api(page, "GET", "/platform/users")).status()).toBe(403);
  const me = await (await api(page, "GET", "/me")).json();
  const ws = me.workspaces.find((w: { id: string }) => w.id === product);
  expect(ws.role).toBe("admin");
  expect(ws.capabilities.view_all_activity).toBe(true);
  await context.close();
});

test("member creates a key, calls the mock model, sees Logs and Usage, then revokes it", async ({ browser }) => {
  const { context, page } = await signIn(browser, "blair");
  const { product } = readFixture();
  await expect(page.getByRole("link", { name: "Admin", exact: true })).toHaveCount(0);
  await page.getByRole("link", { name: "Product: API keys" }).click();
  await expect(page).toHaveURL(new RegExp(`/workspaces/${product}/keys`));
  await page.getByRole("button", { name: "Create key" }).first().click();
  const create = page.getByRole("dialog", { name: "Create API key" });
  await create.getByRole("textbox", { name: "Name" }).fill("ci-key");
  await create.getByRole("button", { name: "Create key" }).click();
  const reveal = page.getByRole("dialog", { name: "Save your API key" });
  const token = await reveal.getByRole("textbox", { name: "API key" }).inputValue();
  expect(token).toMatch(/^omg_/);
  await reveal.getByRole("button", { name: "I have saved it" }).click();
  await expect(reveal).toBeHidden();
  await expect(page.getByRole("textbox", { name: "API key" })).toHaveCount(0);

  // Inference keys never authorize management; the mock upstream answers inference.
  const response = await chat(token);
  expect(response.status).toBe(200);
  expect((await response.json()).choices[0].message.content).toBe(MOCK_REPLY);
  expect((await fetch(`${BASE_URL}/api/v1/me`, { headers: { authorization: `Bearer ${token}` } })).status).toBe(401);

  await page.goto(`/workspaces/${product}/logs`);
  const row = page.getByRole("row", { name: /mock\/chat ci-key 12 in · 4 out/ });
  await expect(row).toBeVisible();
  await expect(row).toContainText("Succeeded");
  await expect(row).not.toContainText("Unknown");

  await page.goto(`/workspaces/${product}/costs`);
  await expect(page.getByRole("heading", { level: 1, name: "Usage & costs" })).toBeVisible();
  await expect(page.getByRole("table", { name: "Top API keys" }).getByRole("row", { name: /ci-key 1 request/ })).toBeVisible();

  const keys = await (await api(page, "GET", `/workspaces/${product}/keys`)).json();
  saveFixture({ key: keys.data.find((k: { name: string }) => k.name === "ci-key").id });

  // 390px: key pages must not scroll sideways.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto(`/workspaces/${product}/keys`);
  await expect(page.getByRole("heading", { level: 1, name: "API keys" })).toBeVisible();
  await expectNoHorizontalScroll(page);
  await page.goto(`/workspaces/${product}/keys/${readFixture().key}`);
  await expect(page.getByRole("heading", { level: 1, name: "ci-key" })).toBeVisible();
  await expectNoHorizontalScroll(page);
  await page.goto(`/workspaces/${product}/keys`);
  await page.getByRole("button", { name: "Create key" }).first().click();
  await expect(page.getByRole("dialog", { name: "Create API key" })).toBeVisible();
  await expectNoHorizontalScroll(page);
  await page.keyboard.press("Escape");
  await page.setViewportSize({ width: 1440, height: 900 });

  await page.goto(`/workspaces/${product}/keys`);
  await page.getByRole("button", { name: "Actions for ci-key" }).click();
  await page.getByRole("menuitem", { name: "Revoke key…" }).click();
  const revoke = page.getByRole("dialog", { name: "Revoke ci-key?" });
  await revoke.getByRole("button", { name: "Revoke key" }).click();
  await expect(revoke).toBeHidden();
  await page.getByRole("group", { name: "Status" }).getByRole("button", { name: "Revoked" }).click();
  await expect(page.getByRole("row", { name: /ci-key/ })).toContainText("Revoked");
  expect((await chat(token)).status).toBe(401);
  await context.close();
});

test("team admin sees the member's request workspace-wide; the member cannot see Admin logs", async ({ browser }) => {
  const { product } = readFixture();
  const alex = await signIn(browser, "alex");
  await alex.page.goto(`/workspaces/${product}/logs`);
  await expect(alex.page.getByRole("row", { name: /mock\/chat ci-key/ })).toBeVisible();
  await alex.context.close();
  const blair = await signIn(browser, "blair");
  expect((await api(blair.page, "GET", "/platform/logs/requests")).status()).toBe(403);
  await blair.context.close();
});
