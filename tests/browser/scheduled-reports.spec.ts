import { expect, test, type Page } from "@playwright/test";
import pg from "pg";
import path from "node:path";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, readFixture, signIn, watchCsp } from "./helpers";

test.afterEach(expectNoCspViolations);
// These are real scheduler/snapshot reads. Only the disposable DB's due cursor
// is seeded; no test route, fabricated browser API response or live mail relay.
async function makeDue(schedule: string) {
  const connectionString = process.env.OMG_BROWSER_DATABASE_URL;
  if (!connectionString || !new URL(connectionString).pathname.startsWith("/omg_browser_")) throw new Error("Scheduled-report fixtures require the disposable browser database");
  const db = new pg.Client({ connectionString }); await db.connect();
  try { await db.query("UPDATE report_schedules SET next_period_end=date_trunc('week',clock_timestamp(),'UTC')-interval '7 days',next_run_at=clock_timestamp()-interval '1 day' WHERE id=$1 AND state='active'", [schedule]); }
  finally { await db.end(); }
}
async function create(page: Page, route: string, base: string) {
  await page.goto(route);
  await expect(page.getByRole("heading", { name: "Scheduled reports", exact: true })).toBeVisible();
  const before = await (await api(page, "GET", base)).json();
  await page.getByRole("button", { name: "Create schedule", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("textbox", { name: "Local delivery time" }).fill("00:01");
  await dialog.getByRole("button", { name: "Create schedule", exact: true }).click();
  await expect(dialog).toHaveCount(0);
  const after = await (await api(page, "GET", base)).json();
  const schedule = after.data.find((s: { id: string }) => !before.data.some((b: { id: string }) => b.id === s.id));
  expect(schedule.state).toBe("paused");
  const row = page.getByRole("table", { name: "Report schedules" }).getByRole("row").filter({ hasText: schedule.id.slice(0, 8) });
  await row.getByRole("button", { name: "Resume", exact: true }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Resume schedule", exact: true }).click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await expect(row.getByText("Active", { exact: true })).toBeVisible();
  await makeDue(schedule.id);
  await expect.poll(async () => (await (await api(page, "GET", `${base}/${schedule.id}/runs`)).json()).data.length, { timeout: 100_000, intervals: [1000, 2000] }).toBe(1);
  await page.getByRole("button", { name: "Refresh runs", exact: true }).click();
  await page.getByRole("table", { name: "Recent report runs" }).getByRole("link").click();
  await expect(page.getByRole("heading", { name: "Scheduled report", exact: true })).toBeVisible();
  await expect(page.getByText("Full totals", { exact: true })).toBeVisible();
  const runUrl = new URL(page.url()).pathname;
  const downloadEvent = page.waitForEvent("download");
  await page.getByRole("link", { name: "Download aggregate CSV" }).click();
  const download = await downloadEvent;
  expect(download.suggestedFilename()).toBe("scheduled-report.csv");
  const stream = await download.createReadStream();
  if (!stream) throw new Error("Missing CSV download");
  let text = ""; for await (const chunk of stream) text += chunk.toString();
  expect(text).toContain("dimension,id,name,known_cost_microusd,held_microusd");
  expect(text).toContain('"total"'); expect(text).not.toContain("execution_id"); expect(text).not.toContain("root_request_id");
  const run = (await (await api(page, "GET", `${base}/${schedule.id}/runs`)).json()).data[0];
  const response = await api(page, "GET", `${base}/${schedule.id}/runs/${run.id}/csv`);
  expect(response.headers()["cache-control"]).toBe("no-store");
  await expectAccessible(page, "immutable scheduled report");
  return { schedule: schedule.id as string, run: run.id as string, runUrl };
}
async function screenshots(page: Page, name: string) {
  for (const width of [1440, 390]) {
    await page.setViewportSize({ width, height: 900 }); await expectNoHorizontalScroll(page);
    await page.screenshot({ path: path.resolve(`target/browser-tests/scheduled-reports/${name}-${width}.png`), fullPage: true });
  }
}
test("create → fenced generation → immutable report → CSV, workspace and platform authority", async ({ browser }) => {
  test.setTimeout(240_000);
  const operator = await signIn(browser, "operator"), owner = await signIn(browser, "alex"), auditor = await signIn(browser, "auditor"), member = await signIn(browser, "blair");
  const ws = readFixture().product!;
  const created: { page: Page; base: string; schedule: string }[] = [];
  try {
    const platform = await create(operator.page, "/admin/costs?tab=scheduled-reports", "/platform/report-schedules");
    created.push({ page: operator.page, base: "/platform/report-schedules", schedule: platform.schedule });
    expect(platform.runUrl).toBe(`/admin/report-schedules/${platform.schedule}/runs/${platform.run}`);
    await screenshots(operator.page, "platform-run");
    await auditor.page.goto(platform.runUrl);
    await expect(auditor.page.getByRole("link", { name: "Download aggregate CSV" })).toBeVisible();
    await auditor.page.goto("/admin/costs?tab=scheduled-reports");
    await expect(auditor.page.getByText("Read-only · Platform Auditor")).toBeVisible();
    for (const name of ["Create schedule", "Edit schedule", "Resume", "Pause", "Delete"]) await expect(auditor.page.getByRole("button", { name, exact: true })).toHaveCount(0);
    expect((await api(auditor.page, "PATCH", `/platform/report-schedules/${platform.schedule}`, { state: "paused" })).status()).toBe(403);
    await screenshots(auditor.page, "auditor-list");
    expect((await api(member.page, "GET", `/platform/report-schedules/${platform.schedule}/runs/${platform.run}`)).status()).toBe(403);
    const base = `/workspaces/${ws}/report-schedules`;
    const scoped = await create(owner.page, `/workspaces/${ws}/costs?tab=scheduled-reports`, base);
    created.push({ page: owner.page, base, schedule: scoped.schedule });
    expect(scoped.runUrl).toBe(`/workspaces/${ws}/report-schedules/${scoped.schedule}/runs/${scoped.run}`);
    await screenshots(owner.page, "workspace-run");
    // Follow an email's exact destination while signed out; OIDC must return
    // here rather than dropping the recipient on Home.
    const recipient = await browser.newContext();
    try {
      await watchCsp(recipient);
      const linked = await recipient.newPage(); await linked.goto(scoped.runUrl);
      await linked.getByRole("link", { name: "Sign in with single sign-on" }).click();
      await linked.locator('button[name="account"][value="alex"]').click();
      await linked.waitForURL(url => url.pathname === scoped.runUrl);
      await expect(linked.getByText("Full totals", { exact: true })).toBeVisible();
    } finally { await recipient.close(); }
    for (const suffix of ["", "/csv"]) expect((await api(member.page, "GET", `${base}/${scoped.schedule}/runs/${scoped.run}${suffix}`)).status()).toBe(403);
    expect((await api(auditor.page, "GET", `${base}/${scoped.schedule}/runs/${scoped.run}/csv`)).status()).toBe(403);
    await member.page.goto(`/workspaces/${ws}/costs`);
    await expect(member.page.getByRole("tab", { name: "Scheduled reports" })).toHaveCount(0);
    await member.page.goto(scoped.runUrl);
    await expect(member.page.getByText("Access not available", { exact: true })).toBeVisible();
    await expect(member.page.getByRole("link", { name: "Download aggregate CSV" })).toHaveCount(0);
    const me = await (await api(owner.page, "GET", "/me")).json();
    const personal = me.workspaces.find((w: { kind: string }) => w.kind === "personal").id;
    await owner.page.goto(`/workspaces/${personal}/costs?tab=scheduled-reports`);
    await owner.page.getByRole("button", { name: "Create schedule", exact: true }).click();
    await expect(owner.page.getByRole("dialog").getByText(/only you can receive/)).toBeVisible();
    await expect(owner.page.getByRole("dialog").getByRole("textbox", { name: "Recipient user IDs" })).toHaveCount(0);
    await owner.page.getByRole("dialog").getByRole("button", { name: "Cancel", exact: true }).click();
    expect((await api(operator.page, "GET", `/workspaces/${personal}/report-schedules`)).status()).toBe(404);
    await owner.page.goto(`/workspaces/${ws}/costs?tab=scheduled-reports`);
    // Wait past the initial "Loading schedules…" state so the screenshot shows the created schedule, not a loading placeholder.
    await expect(owner.page.getByRole("table", { name: "Report schedules" }).getByText(scoped.schedule.slice(0, 8))).toBeVisible();
    await screenshots(owner.page, "workspace-list");
    await owner.page.getByRole("button", { name: "Pause", exact: true }).click();
    await owner.page.getByRole("dialog").getByRole("button", { name: "Pause schedule", exact: true }).click();
    await expect(owner.page.getByRole("dialog")).toHaveCount(0);
    await owner.page.getByRole("button", { name: "Delete", exact: true }).click();
    await owner.page.getByRole("dialog").getByRole("button", { name: "Delete schedule", exact: true }).click();
    await expect(owner.page.getByRole("dialog")).toHaveCount(0);
    expect((await api(owner.page, "GET", `${base}/${scoped.schedule}/runs/${scoped.run}/csv`)).status()).toBe(404);
  } finally {
    for (const s of created) { const r = await api(s.page, "DELETE", `${s.base}/${s.schedule}`); expect.soft([200, 404], "rehearsal schedule cleanup").toContain(r.status()); }
    for (const s of [operator, owner, auditor, member]) await s.context.close();
  }
});
