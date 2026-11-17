import { expect, test, type Page } from "@playwright/test";
import { api, expectNoCspViolations, signIn } from "./helpers";
import { UPSTREAM_URL } from "./stack.mjs";

// Real signed OIDC, gateway, disposable database and built SPA. Only delay real
// GET responses so dismissal and the refreshed render are independently checked.
test.afterEach(expectNoCspViolations);

async function create(page: Page, path: string, body: unknown) {
  const response = await api(page, "POST", path, body);
  expect(response.status(), `${path}: ${await response.text()}`).toBe(200);
  return (await response.json()).id as string;
}

async function seed(page: Page, name: string) {
  const connection = await create(page, "/platform/providers", { name, provider: "openai_compatible", credential_ref: "none", endpoint: UPSTREAM_URL, enabled: true });
  const model = await create(page, "/platform/models", { public_name: `focus/${name}`, display_name: name, enabled: true, supported_protocols: ["chat_completions"] });
  const deployment = await create(page, "/platform/deployments", { model_id: model, provider_connection_id: connection, upstream_model: name, enabled: true });
  return { model, deployment };
}

async function activate(page: Page, button: ReturnType<Page["getByRole"]>) {
  await expect(button).toBeEnabled();
  await button.focus();
  await expect(button).toBeFocused();
  await page.keyboard.press("Enter");
}

async function select(page: Page, item: ReturnType<Page["getByRole"]>) {
  await expect.poll(() => page.getByRole("menu").evaluate(node => node.contains(document.activeElement))).toBe(true);
  await activate(page, item);
}

for (const width of [1440, 390]) {
  for (const moveFocus of [false, true]) {
    test(`route regrouping preserves the mounted origin and ${moveFocus ? "user-moved" : "returned"} focus at ${width}px`, async ({ browser }) => {
      const { context, page } = await signIn(browser, "operator");
      try {
        // signIn verifies the desktop sidebar's Account control. Resize only
        // after that signed-OIDC check; mobile hides it in navigation.
        await page.setViewportSize({ width, height: 900 });
        const name = `route-${width}-${moveFocus}`, fixture = await seed(page, name);
        await page.goto(`/admin/models/${fixture.model}?tab=routes`);
        const table = page.getByRole("table", { name: `Routes for ${name}` });
        const origin = table.getByRole("button", { name: `Actions for route ${name}`, exact: true });
        // Wait for initial routing/detail reads before retaining the origin.
        await expect(origin).toBeVisible();
        await expect(table.getByText("Routing order unknown", { exact: true })).toHaveCount(0);
        const mountedOrigin = await origin.elementHandle();
        for (const enabled of [false, true]) {
          let release!: () => void;
          const held = new Promise<void>(resolve => { release = resolve; });
          let holdReads = false;
          await page.route(url => url.pathname.startsWith("/api/v1/platform/deployments"), async route => {
            if (!holdReads || route.request().method() !== "GET") { await route.continue(); return; }
            const response = await route.fetch();
            await held;
            await route.fulfill({ response });
          });
          try {
            await activate(page, origin);
            const toggle = page.getByRole("menuitem", { name: enabled ? "Enable route…" : "Disable route…", exact: true });
            await select(page, toggle);
            const review = page.getByRole("dialog", { name: "Review changes", exact: true });
            await activate(page, review.getByRole("button", { name: "Continue to confirmation", exact: true }));
            const confirm = page.getByRole("dialog", { name: `${enabled ? "Enable" : "Disable"} route ${name}?`, exact: true });
            holdReads = true;
            await activate(page, confirm.getByRole("button", { name: enabled ? "Enable" : "Disable", exact: true }));
            await expect(confirm).toBeHidden();
            await expect(origin).toBeFocused();
            await expect(table.getByText(enabled ? "Disabled" : "Enabled", { exact: true })).toBeVisible();
            const other = page.getByRole("button", { name: "Add route", exact: true });
            if (moveFocus) await other.focus();
            release();
            await expect(table.getByText(enabled ? "Enabled" : "Disabled", { exact: true })).toBeVisible();
            // Cross the restoration frame after the refreshed DOM, including
            // browser focus fixup if the focused node was wrongly removed.
            await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
            expect(await mountedOrigin!.evaluate(node => node.isConnected)).toBe(true);
            expect(await origin.evaluate((node, original) => node === original, mountedOrigin)).toBe(true);
            await expect(moveFocus ? other : origin).toBeFocused();
          } finally {
            holdReads = false;
            release();
            await page.unrouteAll({ behavior: "wait" });
          }
        }
      } finally { await context.close(); }
    });
  }

  test(`retiring restores the connected header fallback after refresh at ${width}px`, async ({ browser }) => {
    const { context, page } = await signIn(browser, "operator");
    try {
      await page.setViewportSize({ width, height: 900 });
      const name = `header-${width}`, fixture = await seed(page, name);
      await page.goto(`/admin/deployments/${fixture.deployment}`);
      const reviewButton = page.getByRole("button", { name: "Review changes", exact: true });
      await expect(reviewButton).toBeVisible();
      const mountedFallback = await reviewButton.elementHandle();
      const menu = page.getByRole("button", { name: width === 390 ? "More actions" : /^Actions for /, exact: true });
      await activate(page, menu);
      const retire = page.getByRole("menuitem", { name: "Retire resource…", exact: true });
      await select(page, retire);
      await activate(page, page.getByRole("dialog", { name: "Review changes", exact: true }).getByRole("button", { name: "Continue to confirmation", exact: true }));
      const confirm = page.getByRole("dialog", { name: /^Retire / });
      await activate(page, confirm.getByRole("button", { name: "Retire resource", exact: true }));
      await expect(confirm).toBeHidden();
      await expect(menu).toHaveCount(0);
      await expect(reviewButton).toBeFocused();
      expect(await mountedFallback!.evaluate(node => node.isConnected)).toBe(true);
    } finally { await context.close(); }
  });
}
