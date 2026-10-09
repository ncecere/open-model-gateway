import { expect, type Browser, type BrowserContext, type Page } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
import { appendFileSync, mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { BASE_URL } from "./stack.mjs";

export type Persona = "operator" | "auditor" | "alex" | "blair" | "unentitled";
const stateDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../target/browser-tests/state");
mkdirSync(stateDir, { recursive: true });
export const storageFor = (persona: Persona) => path.join(stateDir, `${persona}.json`);

/** Shared identifiers created by the journey and reused by the accessibility pass. */
export type Fixture = { product?: string; research?: string; model?: string; key?: string };
const fixtureFile = path.join(stateDir, "fixture.json");
export const readFixture = (): Fixture => existsSync(fixtureFile) ? JSON.parse(readFileSync(fixtureFile, "utf8")) : {};
export const saveFixture = (next: Fixture) => writeFileSync(fixtureFile, JSON.stringify({ ...readFixture(), ...next }));

/** Real signed-OIDC sign-in through the local issuer's account picker. */
export async function signIn(browser: Browser, persona: Persona, viewport = { width: 1440, height: 900 }): Promise<{ context: BrowserContext; page: Page }> {
  const context = await browser.newContext({ viewport });
  const page = await context.newPage();
  await page.goto("/");
  await page.getByRole("link", { name: "Sign in with single sign-on" }).click();
  await page.locator(`button[name="account"][value="${persona}"]`).click();
  await page.waitForURL(url => url.origin === BASE_URL);
  if (persona === "unentitled") await expect(page.getByText("No access yet")).toBeVisible();
  else await expect(page.getByRole("button", { name: /^Account:/ })).toBeVisible();
  await context.storageState({ path: storageFor(persona) });
  return { context, page };
}

/** Same-origin management call with the session's CSRF cookie, as the SPA sends it. */
export async function api(page: Page, method: string, apiPath: string, data?: unknown) {
  const csrf = (await page.context().cookies(BASE_URL)).find(c => c.name === "omg_csrf")?.value ?? "";
  return page.request.fetch(`${BASE_URL}/api/v1${apiPath}`, { method, data, headers: { origin: BASE_URL, "x-csrf-token": csrf }, failOnStatusCode: false });
}

export async function chat(token: string) {
  return fetch(`${BASE_URL}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: JSON.stringify({ model: "mock/chat", messages: [{ role: "user", content: "Say hello" }], max_completion_tokens: 16 }),
  });
}

export async function expectNoHorizontalScroll(page: Page) {
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(overflow, "document must not scroll sideways").toBeLessThanOrEqual(0);
}

/** WCAG 2.x A/AA + best-practice scan; serious and critical violations fail the test. */
export async function expectAccessible(page: Page, label: string) {
  // Scan the settled UI, not mid-transition frames (popups fade in over a backdrop).
  await page.evaluate(() => Promise.all(document.getAnimations().filter(a => a.effect?.getComputedTiming().iterations !== Infinity).map(a => a.finished.catch(() => undefined))));
  const results = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa", "best-practice"]).analyze();
  const blocking = results.violations.filter(v => v.impact === "serious" || v.impact === "critical");
  // Non-blocking findings are kept for review (docs/accessibility.md), never silently dropped.
  expect(results.passes.length, `${label}: axe ran`).toBeGreaterThan(5);
  const record = (v: (typeof results.violations)[number], review: boolean) => appendFileSync(path.join(stateDir, "axe-findings.jsonl"), JSON.stringify({ page: label, review, id: v.id, impact: v.impact, nodes: v.nodes.length, sample: v.nodes.slice(0, 3).map(n => ({ target: n.target.join(" "), html: n.html.slice(0, 160), why: [...n.any, ...n.all, ...n.none][0]?.message })) }) + "\n");
  for (const v of results.incomplete) record(v, true);
  for (const v of results.violations) record(v, false);
  const summary = blocking.map(v => `${v.id} (${v.impact}): ${v.help}\n${v.nodes.slice(0, 5).map(n => `    ${n.target.join(" ")} — ${n.failureSummary?.split("\n").slice(1, 2).join("")}`).join("\n")}`);
  expect(summary, `${label}: axe serious/critical violations`).toEqual([]);
  return results.violations.filter(v => !blocking.includes(v));
}

/** The named table's default columns fit its frame: nothing scrolls sideways or is clipped (e.g. Status at 1440). */
export async function expectTableFits(page: Page, caption: string) {
  const table = page.getByRole("table", { name: caption });
  await expect(table).toBeVisible();
  const fit = await table.evaluate(t => {
    const frame = t.parentElement!;
    const last = t.querySelector("thead tr")!.lastElementChild!.getBoundingClientRect();
    return { overflow: frame.scrollWidth - frame.clientWidth, lastRight: last.right, frameRight: frame.getBoundingClientRect().right };
  });
  expect(fit.overflow, `${caption}: columns must fit without sideways scrolling`).toBeLessThanOrEqual(0);
  expect(fit.lastRight, `${caption}: the last column must not be clipped`).toBeLessThanOrEqual(fit.frameRight + 0.5);
}

/** Every stat tile in the named group draws its chart slot at the same height, size and distance from the tile bottom. */
export async function expectTileChartsAligned(page: Page, group: string) {
  const tiles = page.getByRole("group", { name: group });
  await expect(tiles).toBeVisible();
  const boxes = await tiles.evaluate(g => [...g.querySelectorAll("dl")].map(dl => {
    const tile = dl.parentElement!.getBoundingClientRect(), slot = dl.querySelector("dd > span[class*='_chart_']")?.getBoundingClientRect();
    return slot ? { top: Math.round(slot.top), bottomGap: Math.round(tile.bottom - slot.bottom), height: Math.round(slot.height), width: Math.round(slot.width), tileTop: Math.round(tile.top) } : null;
  }));
  const slots = boxes.filter((b): b is NonNullable<typeof b> => b !== null);
  expect(slots.length, `${group}: tiles with a chart slot`).toBeGreaterThan(1);
  for (const b of slots) {
    expect(b.bottomGap, `${group}: chart pinned to the tile bottom`).toBe(slots[0]!.bottomGap);
    expect(b.height, `${group}: chart height`).toBe(slots[0]!.height);
    expect(Math.abs(b.width - slots[0]!.width), `${group}: chart width`).toBeLessThanOrEqual(1);
    // Tiles on the same row (same top) draw their charts at the same height.
    for (const other of slots.filter(o => o.tileTop === b.tileTop)) expect(other.top, `${group}: chart position in a row`).toBe(b.top);
  }
}
