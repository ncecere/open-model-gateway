import { expect, test, type Page } from "@playwright/test";
import { expectAccessible, readFixture, storageFor, type Persona } from "./helpers";

// axe WCAG 2.1 A/AA over the main pages per persona at desktop and phone widths.
// Serious/critical violations fail; moderate/minor ones are recorded in docs/accessibility.md.
const viewports = [{ name: "1440", width: 1440, height: 900 }, { name: "390", width: 390, height: 844 }];

function pages(persona: Persona): string[] {
  const { product, model, key } = readFixture();
  switch (persona) {
    case "operator": return ["/admin", "/admin/users", "/admin/teams", `/admin/teams/${product}`, "/admin/models", `/admin/models/${model}`, "/admin/models/new", "/admin/connections", "/admin/catalogs", "/admin/sso-groups", "/admin/logs", "/admin/costs", "/admin/key-safety", "/admin/audit", "/admin/settings/general", "/admin/settings/alerts"];
    case "auditor": return ["/admin", "/admin/teams", `/admin/models/${model}`, "/admin/audit", "/home"];
    case "alex": return ["/home", `/workspaces/${product}`, `/workspaces/${product}/models`, `/workspaces/${product}/keys`, `/workspaces/${product}/logs`, `/workspaces/${product}/costs`, `/workspaces/${product}/settings`];
    case "blair": return ["/home", `/workspaces/${product}/keys`, `/workspaces/${product}/keys/${key}`, `/workspaces/${product}/logs`, `/workspaces/${product}/costs`, "/notifications", "/profile"];
    default: return [];
  }
}

async function settle(page: Page, path: string) {
  await page.goto(path);
  await expect(page.locator("main h1").first()).toBeVisible();
  await page.waitForLoadState("networkidle");
}

for (const viewport of viewports) {
  test.describe(`${viewport.name}px`, () => {
    test.use({ viewport: { width: viewport.width, height: viewport.height } });

    test("signed-out sign-in page", async ({ page }) => {
      await settle(page, "/");
      await expectAccessible(page, "sign-in");
    });

    for (const persona of ["operator", "auditor", "alex", "blair"] as const) {
      test(`${persona} main pages`, async ({ browser }) => {
        const context = await browser.newContext({ storageState: storageFor(persona), viewport });
        const page = await context.newPage();
        for (const path of pages(persona)) {
          await settle(page, path);
          await expectAccessible(page, `${persona} ${path} @${viewport.name}`);
        }
        if (persona === "blair") {
          await settle(page, `/workspaces/${readFixture().product}/logs`);
          await page.getByRole("link", { name: /^Open request/ }).first().click();
          await expect(page.locator("main h1").first()).toBeVisible();
          await page.waitForLoadState("networkidle");
          await expectAccessible(page, `blair request detail @${viewport.name}`);
        }
        await context.close();
      });
    }

    test("dialogs and menus: create key, search, account", async ({ browser }) => {
      const { product } = readFixture();
      const context = await browser.newContext({ storageState: storageFor("blair"), viewport });
      const page = await context.newPage();
      await settle(page, `/workspaces/${product}/keys`);
      const opener = page.getByRole("button", { name: "Create key" }).first();
      await opener.click();
      const dialog = page.getByRole("dialog", { name: "Create API key" });
      await expect(dialog).toBeVisible();
      await expectAccessible(page, `create key dialog @${viewport.name}`);
      // Focus stays inside the modal and returns to the opener on Escape.
      for (let i = 0; i < 25; i++) {
        await page.keyboard.press("Tab");
        // Base UI focus guards hand focus back into the popup on the next frame.
        await expect.poll(() => dialog.evaluate(node => node.contains(document.activeElement) ? null : document.activeElement?.outerHTML.slice(0, 160) ?? "none"), { message: `focus after ${i + 1} Tab presses`, timeout: 2000 }).toBeNull();
      }
      await page.keyboard.press("Escape");
      await expect(dialog).toBeHidden();
      await expect(opener).toBeFocused();
      // Header search (command palette) and the account menu.
      await page.getByRole("button", { name: "Search or jump to…" }).click();
      await expect(page.getByRole("dialog")).toBeVisible();
      await expectAccessible(page, `search palette @${viewport.name}`);
      const dangling = await page.locator("[aria-controls]").evaluateAll(els => els.map(e => e.getAttribute("aria-controls")!).filter(ids => ids.split(" ").some(id => !document.getElementById(id))));
      expect(dangling, "aria-controls must reference rendered elements").toEqual([]);
      await page.keyboard.press("Escape");
      await expect(page.getByRole("dialog")).toHaveCount(0);
      if (viewport.width < 600) await page.getByRole("button", { name: /navigation|menu|sidebar/i }).first().click();
      await page.getByRole("button", { name: /^Account:/ }).click();
      await expect(page.getByRole("menu")).toBeVisible();
      await expectAccessible(page, `account menu @${viewport.name}`);
      await page.keyboard.press("Escape");
      await context.close();
    });
  });
}

test("keyboard: the first Tab reaches the skip link, which moves focus to main content", async ({ browser }) => {
  const context = await browser.newContext({ storageState: storageFor("blair") });
  const page = await context.newPage();
  await settle(page, "/home");
  await page.keyboard.press("Tab");
  const skip = page.getByRole("link", { name: "Skip to content" });
  await expect(skip).toBeFocused();
  await expect(skip).toBeInViewport();
  await page.keyboard.press("Enter");
  expect(await page.evaluate(() => !!document.activeElement?.closest("main") || document.activeElement?.id === "main")).toBe(true);
  await context.close();
});

test("reduced motion collapses animations and transitions", async ({ browser }) => {
  const context = await browser.newContext({ storageState: storageFor("blair"), reducedMotion: "reduce" });
  const page = await context.newPage();
  await settle(page, "/home");
  const slowest = await page.evaluate(() => {
    const seconds = (value: string) => Math.max(...value.split(",").map(v => v.trim().endsWith("ms") ? parseFloat(v) / 1000 : parseFloat(v)));
    return Math.max(0, ...[...document.querySelectorAll("*")].map(el => getComputedStyle(el)).flatMap(s => [seconds(s.transitionDuration), s.animationName === "none" ? 0 : seconds(s.animationDuration)]));
  });
  expect(slowest, "longest animation/transition (s) under prefers-reduced-motion").toBeLessThanOrEqual(0.001);
  await context.close();
});

test("dark color scheme: sign-in and member pages", async ({ browser }) => {
  const { product } = readFixture();
  const signedOut = await browser.newContext({ colorScheme: "dark" });
  const outPage = await signedOut.newPage();
  await settle(outPage, "/");
  await expectAccessible(outPage, "sign-in (dark)");
  await signedOut.close();
  const context = await browser.newContext({ storageState: storageFor("blair"), colorScheme: "dark" });
  const page = await context.newPage();
  for (const path of ["/home", `/workspaces/${product}/keys`, `/workspaces/${product}/logs`, `/workspaces/${product}/costs`]) {
    await settle(page, path);
    await expectAccessible(page, `blair ${path} (dark)`);
  }
  await context.close();
});
