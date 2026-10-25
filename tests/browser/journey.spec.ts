import { expect, test, type Page } from "@playwright/test";
import { BASE_URL, ISSUER_URL, UPSTREAM_URL } from "./stack.mjs";
import { MOCK_REPLY } from "./mock-upstream.mjs";
import { api, chat, expectNoHorizontalScroll, expectTableFits, expectTileChartsAligned, readFixture, saveFixture, signIn, expectNoCspViolations } from "./helpers";

test.afterEach(expectNoCspViolations);

// One ordered journey on a fresh database: each step builds on the previous one.
test.describe.configure({ mode: "serial" });

const idFrom = (page: Page) => new URL(page.url()).pathname.split("/").pop()!;
/** A server-searched choice (R06): type part of it, then pick the matching option. */
async function pick(scope: ReturnType<Page["getByRole"]>, page: Page, name: string, text: string, option: string) {
  await scope.getByRole("combobox", { name, exact: true }).fill(text);
  await page.getByRole("option", { name: option, exact: true }).click();
}

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
    await page.getByRole("button", { name: `Create ${kind}` }).first().click();
    const dialog = page.getByRole("dialog", { name: `Create ${kind}` });
    await dialog.getByRole("textbox", { name: "Name", exact: true }).fill(name);
    await pick(dialog, page, "Initial owner", "operator", "operator@demo.invalid");
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
      await pick(dialog, page, "Shared workspace", target.split(" · ")[0]!, target);
      await dialog.getByLabel("Membership role").selectOption({ label: role });
    }
    // #89: a mapping change is reviewed (impact preview) before it is saved.
    await dialog.getByRole("button", { name: "Review changes" }).click();
    await expect(dialog).toBeHidden();
    const review = page.getByRole("dialog", { name: "Review changes" });
    await review.getByRole("button", { name: "Continue to confirmation" }).click();
    const confirm = page.getByRole("dialog", { name: /^Create this SSO group mapping\?$/ });
    await confirm.getByRole("button", { name: "Create mapping" }).click();
    await expect(confirm).toBeHidden();
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
  await expect(form.getByRole("combobox", { name: "Type", exact: true })).toHaveText("Text");
  await page.getByRole("button", { name: /Add price now/ }).click();
  await page.getByLabel("Input tokens price").selectOption({ label: "Priced" });
  await page.getByLabel("$ per M input tokens").fill("2.50");
  await page.getByLabel("Output tokens price").selectOption({ label: "Priced" });
  await page.getByLabel("$ per M output tokens").fill("10");
  for (const meter of ["Cache read", "Cache write (default)", "Cache write (5-minute)", "Cache write (1-hour)"]) await page.getByLabel(`${meter} price`).selectOption({ label: "Not applicable" });
  await page.getByLabel("Requests price").selectOption({ label: "Free · explicit $0" });

  // #18: header submission must not silently leave required price errors below the fold.
  let modelSetupPosts = 0;
  page.on("request", request => {
    if (request.method() === "POST" && new URL(request.url()).pathname === "/api/v1/platform/model-setup") modelSetupPosts += 1;
  });
  const addModel = page.locator("[data-form-actions]").getByRole("button", { name: "Add model", exact: true });
  const inputCeiling = form.getByRole("textbox", { name: "Hard upstream input token ceiling", exact: true });
  const outputCeiling = form.getByRole("textbox", { name: "Hard upstream output token ceiling", exact: true });
  const validation = form.getByRole("alert", { name: "Form validation" });
  await expect(page.locator("[data-form-actions]")).toHaveCount(1);
  for (const ceiling of [inputCeiling, outputCeiling]) await expect(ceiling).toHaveAttribute("aria-required", "true");
  await expect(form.getByText("Required", { exact: true })).toHaveCount(2);
  // Use a short desktop viewport deliberately: at 900px high, the ceiling can
  // still fit on screen after returning to the header on some platforms.
  for (const viewport of [{ width: 1440, height: 720 }, { width: 390, height: 844 }]) {
    await page.setViewportSize(viewport);
    await addModel.scrollIntoViewIfNeeded();
    await expect(inputCeiling).not.toBeInViewport();
    await addModel.click();
    await expect(validation).toBeVisible();
    await expect(validation.getByText("Fix 2 fields", { exact: true })).toBeVisible();
    await expect(validation.getByRole("link")).toHaveCount(2);
    await expect(validation.getByRole("link", { name: "Input token ceiling", exact: true })).toHaveAttribute("href", `#${await inputCeiling.getAttribute("id")}`);
    await expect(validation.getByRole("link", { name: "Output token ceiling", exact: true })).toHaveAttribute("href", `#${await outputCeiling.getAttribute("id")}`);
    expect(await validation.evaluate(node => !!(node.compareDocumentPosition(node.closest("form")!.querySelector("fieldset")!) & Node.DOCUMENT_POSITION_FOLLOWING))).toBe(true);
    await expect(inputCeiling).toHaveAttribute("aria-invalid", "true");
    await expect(inputCeiling).toBeFocused();
    await expect.poll(() => inputCeiling.evaluate(node => document.activeElement === node)).toBe(true);
    await expect(inputCeiling).toBeInViewport({ ratio: 1 });
    expect(await inputCeiling.evaluate(node => {
      const box = node.getBoundingClientRect();
      return document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2) === node;
    }), "the focused ceiling is visible, not covered by the header").toBe(true);
    expect(modelSetupPosts, "invalid header submission must send no create request").toBe(0);
    if (viewport.width === 390) await expectNoHorizontalScroll(page);
  }
  await page.setViewportSize({ width: 1440, height: 900 });
  await inputCeiling.fill("4096");
  await outputCeiling.fill("0");
  await addModel.click();
  await expect(validation.getByText("Fix 1 field", { exact: true })).toBeVisible();
  await expect(validation.getByRole("link", { name: "Output token ceiling", exact: true })).toBeVisible();
  await expect(outputCeiling).toHaveAttribute("aria-invalid", "true");
  await expect(outputCeiling).toBeFocused();
  await expect.poll(() => outputCeiling.evaluate(node => document.activeElement === node)).toBe(true);
  await expect(outputCeiling).toBeInViewport({ ratio: 1 });
  await expect(outputCeiling).toHaveAccessibleDescription(/Must be positive\..*Enter a whole number of tokens from 1 to 2,147,483,647/);
  await expect(form.getByText("Enter a whole number of tokens from 1 to 2,147,483,647, without exponent notation.", { exact: true })).toBeVisible();
  await expect(form.getByText(/Enter a whole number of tokens from 0 to/)).toHaveCount(0);
  expect(modelSetupPosts, "zero output is invalid for text generation").toBe(0);
  await outputCeiling.fill("1024");
  await expect(validation).toHaveCount(0);
  const created = page.waitForResponse(response => response.request().method() === "POST" && new URL(response.url()).pathname === "/api/v1/platform/model-setup");
  await addModel.click();
  expect((await created).status()).toBe(201);
  await expect(page.getByRole("heading", { level: 1, name: "Mock Chat" })).toBeVisible();
  expect(modelSetupPosts, "correcting the ceilings creates the model exactly once").toBe(1);
  await expect(page.getByText("$2.50 / $10.00")).toBeVisible();
  saveFixture({ model: idFrom(page) });

  // Direct assignment from the Team page (no catalog needed).
  await page.goto(`/admin/teams/${readFixture().product}`);
  await page.getByRole("tab", { name: /Models/ }).click();
  await page.getByRole("button", { name: "Assign models" }).first().click();
  const assign = page.getByRole("dialog", { name: "Assign models to this team" });
  await pick(assign, page, "Models", "Mock", "Mock Chat · mock/chat");
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

test("team admin chooses who sees files, batches and videos; members see it read-only (#141)", async ({ browser }) => {
  const { product } = readFixture();
  const alex = await signIn(browser, "alex");
  await alex.page.goto(`/workspaces/${product}/settings?tab=overview`);
  const panel = alex.page.getByRole("region", { name: "Files, batches and videos" }).or(alex.page.locator("section", { hasText: "Files, batches and videos" }));
  const select = alex.page.getByLabel("Who sees them");
  await expect(select).toHaveValue("member");
  await expect(panel.first()).toContainText("Only the creator or a workspace admin can delete or cancel.");
  await select.selectOption("workspace");
  await alex.page.getByRole("button", { name: /^Save/ }).last().click();
  await expect(alex.page.getByText("Settings saved.")).toBeVisible();
  expect((await (await api(alex.page, "GET", `/workspaces/${product}`)).json()).object_visibility).toBe("workspace");
  const blair = await signIn(browser, "blair");
  await blair.page.goto(`/workspaces/${product}/settings?tab=overview`);
  await expect(blair.page.getByText("Workspace-wide")).toBeVisible();
  await expect(blair.page.getByLabel("Who sees them")).toHaveCount(0);
  expect((await api(blair.page, "PATCH", `/workspaces/${product}`, { object_visibility: "member" })).status()).toBe(403);
  // Back to the default for the journeys that follow.
  expect((await api(alex.page, "PATCH", `/workspaces/${product}`, { object_visibility: "member" })).status()).toBe(200);
  await blair.context.close();
  await alex.context.close();
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

  // #232: Logs does not poll; Refresh fetches the current tab now and brings in a request made after the page loaded.
  await page.goto(`/workspaces/${product}/logs`);
  await expect(page.getByRole("row", { name: /mock\/chat ci-key/ })).toHaveCount(1);
  const second = await chat(token);
  expect(second.status).toBe(200);
  await expect(page.getByRole("row", { name: /mock\/chat ci-key/ })).toHaveCount(1);
  const refreshed = page.waitForResponse(r => new URL(r.url()).pathname === `/api/v1/workspaces/${product}/requests` && r.ok());
  await page.getByRole("button", { name: "Refresh" }).click();
  await refreshed;
  await expect(page.getByRole("row", { name: /mock\/chat ci-key/ })).toHaveCount(2);

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
  // Two requests exist by now (the earlier journey step made a second one).
  await expect(alex.page.getByRole("row", { name: /mock\/chat ci-key/ })).toHaveCount(2);
  await alex.context.close();
  const blair = await signIn(browser, "blair");
  expect((await api(blair.page, "GET", "/platform/logs/requests")).status()).toBe(403);
  await blair.context.close();
});

test("Logs deep links filter and fit at 1440; usage tiles line up their charts at 1440 and 390", async ({ browser }) => {
  const { product } = readFixture();
  const alex = await signIn(browser, "alex");
  const page = alex.page;
  // ?model= applies the Model filter (URL → filter state): the request carries it and the control shows it.
  const filtered = page.waitForRequest(r => r.url().includes(`/workspaces/${product}/requests?`) && r.url().includes("model=mock%2Fchat"));
  await page.goto(`/workspaces/${product}/logs?model=mock%2Fchat`);
  await filtered;
  await expect(page.getByRole("combobox", { name: "Model" })).toHaveValue(/mock\/chat/);
  // Two requests exist by now (the earlier journey step made a second one).
  await expect(page.getByRole("row", { name: /mock\/chat ci-key/ })).toHaveCount(2);
  await expectTableFits(page, "Requests");
  await page.goto(`/workspaces/${product}/logs?model=nothing%2Fhere`);
  await expect(page.getByRole("row", { name: /mock\/chat ci-key/ })).toHaveCount(0);
  // Usage overview tiles: sparklines at one height whatever the hint lines (review #12).
  await page.goto(`/workspaces/${product}/costs`);
  await expectTileChartsAligned(page, "Usage summary");
  await page.setViewportSize({ width: 390, height: 844 });
  // A viewport resize fires matchMedia before React removes the desktop sidebar.
  // Await the rendered mobile shell; keep the exact no-overflow assertion below.
  await expect(page.getByRole("button", { name: "Open navigation", exact: true })).toBeVisible();
  await expect.poll(
    () => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth),
    { message: "mobile document must fit after the responsive shell settles" },
  ).toBe(0);
  await expectTileChartsAligned(page, "Usage summary");
  await expectNoHorizontalScroll(page);
  await alex.context.close();

  const operator = await signIn(browser, "operator");
  // Admin › Logs has the extra Workspace column and must still fit at 1440.
  const platform = operator.page.waitForRequest(r => r.url().includes("/platform/logs/requests?") && r.url().includes("model=mock%2Fchat"));
  await operator.page.goto("/admin/logs?model=mock%2Fchat");
  await platform;
  await expect(operator.page.getByRole("combobox", { name: "Model" })).toHaveValue(/mock\/chat/);
  await expectTableFits(operator.page, "Requests");
  // Capture-like content (long names, large counts, an unknown cost with a hold, a failed request with a fallback)
  // must still fit with the default columns at 1440: Status is never clipped (review #7).
  const wide = { root_request_id: "1a2b3c4d-0000-4000-8000-000000000001", workspace: { id: readFixture().product, name: "Chemistry Teaching and Learning", kind: "team" }, started_at: new Date().toISOString(), completed_at: new Date().toISOString(), model: "example/chat-fast-extended", upstream_model: "chat-small", key: { id: "k", name: "Priya's laptop (course tools)" }, app: "Course assistant", status: "failed", finish_reason: "length", attempts: 2, input_tokens: "1234567", output_tokens: "765432", cached_input_tokens: null, reasoning_tokens: null, cost_microusd: null, held_microusd: "1234567", latency_ms: 123456, time_to_first_token_ms: 1200, generation_ms: 4000, tokens_per_second: "123.45", cost_center: null, workload_kind: "generation", streamed: true, session_id: null };
  await operator.page.route("**/api/v1/platform/logs/requests?*", route => route.fulfill({ json: { data: [wide, { ...wide, root_request_id: "1a2b3c4d-0000-4000-8000-000000000002", status: "succeeded", finish_reason: "tool_calls", attempts: 1, cost_microusd: "8271628", held_microusd: "0" }], next_cursor: null } }));
  await operator.page.goto("/admin/logs");
  await expect(operator.page.getByRole("row", { name: /example\/chat-fast-extended/ }).first()).toBeVisible();
  await expectTableFits(operator.page, "Requests");
  await operator.page.unroute("**/api/v1/platform/logs/requests?*");
  await operator.page.goto("/admin/costs");
  await expectTileChartsAligned(operator.page, "Usage summary");
  await operator.context.close();
});

test("a member sees where they're signed in and signs out their other sessions (#79)", async ({ browser }) => {
  // A second browser, signed in without saving its state (later specs reuse
  // the saved one, which this test keeps signed in).
  const elsewhere = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const away = await elsewhere.newPage();
  await away.goto("/");
  await away.getByRole("link", { name: "Sign in with single sign-on" }).click();
  await away.locator('button[name="account"][value="blair"]').click();
  await expect(away.getByRole("button", { name: /^Account:/ })).toBeVisible();

  const { context, page } = await signIn(browser, "blair");
  await page.goto("/profile");
  const sessions = page.getByRole("table", { name: "Your dashboard sessions" });
  await expect(sessions.getByText("This browser")).toBeVisible();
  await expect.poll(async () => (await sessions.getByRole("row").count()) - 1).toBeGreaterThanOrEqual(2);
  await expectTableFits(page, "Your dashboard sessions");
  await page.getByRole("button", { name: "Sign out others" }).click();
  const dialog = page.getByRole("dialog", { name: "Sign out your other sessions?" });
  await dialog.getByRole("button", { name: "Sign out others" }).click();
  await expect(page.getByRole("button", { name: "Sign out others" })).toHaveCount(0);
  await expect(sessions.getByRole("row")).toHaveCount(2);

  // The other browser's cookie stops working on its next request.
  await away.reload();
  await expect(away.getByRole("link", { name: "Sign in with single sign-on" })).toBeVisible();
  // This one stays signed in.
  await page.reload();
  await expect(page.getByRole("button", { name: /^Account:/ })).toBeVisible();
  await elsewhere.close();
  await context.close();
});
