import { expect, test } from "@playwright/test";
import { api, expectNoCspViolations, signIn } from "./helpers";

// Real gateway, database and OIDC session. Only the resource-specific refusal is
// intercepted; /me must reach Rust and return unchanged authority throughout.
// No keys/tokens are issued, and screenshots include only the main content.
test.use({ trace: "off", screenshot: "off", video: "off" });
test.afterEach(expectNoCspViolations);

test("resource 403 with unchanged authority refreshes once, settles denied and recovers by navigation", async ({ browser }, info) => {
  const { context, page } = await signIn(browser, "operator");
  const resource = "/api/v1/platform/legal-holds";
  let blocked = true, refusals = 0, probes = 0;
  const snapshots: Promise<unknown>[] = [];
  try {
    const authority = await (await api(page, "GET", "/me")).json();
    page.on("request", request => { if (new URL(request.url()).pathname === "/api/v1/me") probes++; });
    page.on("response", response => {
      if (new URL(response.url()).pathname === "/api/v1/me") snapshots.push(response.json());
    });
    await page.route(url => url.pathname === resource, async route => {
      if (route.request().method() !== "GET" || !blocked) { await route.continue(); return; }
      refusals++;
      // A regressed browser must fail boundedly, not keep hammering the gateway.
      // These remain real 403 responses; do not conceal extra /me requests.
      if (refusals > 4) { await route.abort("blockedbyclient"); return; }
      await route.fulfill({ status: 403, json: { error: { code: "403", message: "This resource is not permitted." } } });
    });
    await page.goto("/admin/legal-holds");
    await expect(page.getByRole("heading", { name: "Access not available", exact: true })).toBeVisible();
    expect(probes).toBe(2); // document's initial /me + ONE automatic refresh
    expect(refusals).toBeGreaterThan(0);
    expect(refusals).toBeLessThanOrEqual(2);
    expect(await Promise.all(snapshots)).toEqual([authority, authority]);
    const settled = { probes, refusals };
    // Deliberate quiet window: a visible transient notice must not pass while
    // the next remount/refetch cycle is already scheduled.
    await page.waitForTimeout(300);
    expect({ probes, refusals }).toEqual(settled);
    await expect(page.getByText("Checking current access…", { exact: true })).toHaveCount(0);
    await expect(page.getByRole("link", { name: "Sign in with single sign-on" })).toHaveCount(0);
    await page.locator("main").screenshot({ path: info.outputPath("access-refresh-denied.png") });

    // Actual SPA links, not a document reload or a mocked /me response. Leaving
    // the refused entry must remove its denied fence; returning reads Rust again.
    await page.evaluate(() => Reflect.set(window, "__omg291Document", "same-document"));
    blocked = false;
    await page.getByRole("link", { name: "Audit log", exact: true }).click();
    await expect(page.getByRole("heading", { name: "Audit log", exact: true })).toBeVisible();
    const recoveredRead = page.waitForResponse(response => new URL(response.url()).pathname === resource && response.request().method() === "GET" && response.status() === 200);
    await page.getByRole("link", { name: "Legal holds", exact: true }).click();
    await recoveredRead;
    await expect(page.getByRole("heading", { name: "Legal holds", exact: true })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Access not available", exact: true })).toHaveCount(0);
    expect(await page.evaluate(() => Reflect.get(window, "__omg291Document"))).toBe("same-document");
    expect(refusals).toBe(settled.refusals);
    for (const snapshot of await Promise.all(snapshots)) expect(snapshot).toEqual(authority);
    await page.locator("main").screenshot({ path: info.outputPath("access-refresh-recovered.png") });
  } finally { await context.close(); }
});
