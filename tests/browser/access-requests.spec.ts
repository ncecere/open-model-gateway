import { expect, test, type Browser, type Page } from "@playwright/test";
import { mkdirSync } from "node:fs";
import path from "node:path";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, signIn } from "./helpers";

/*
 * Self-service membership requests (#83, slice A): discover a Team/Project, submit a request,
 * a workspace owner/admin approves/denies it, the requester withdraws a separate one, and an
 * Auditor reads the platform queue without ever being offered a decision. Every workspace used
 * here is created fresh so this spec does not depend on another spec's seeded fixtures.
 */
test.afterEach(expectNoCspViolations);
const shots = path.resolve("target/browser-tests/access-requests");
async function shot(page: Page, name: string) { mkdirSync(shots, { recursive: true }); await page.locator("main").screenshot({ path: path.join(shots, `${name}.png`) }); }

async function createTeam(browser: Browser, name: string): Promise<{ id: string; name: string }> {
  const operator = await signIn(browser, "operator");
  try {
    const alex = await signIn(browser, "alex");
    try {
      const me = await (await api(alex.page, "GET", "/me")).json();
      const created = await api(operator.page, "POST", "/platform/workspaces", { kind: "team", name, owner_user_id: me.user.id });
      expect(created.status()).toBe(200);
      const id = (await created.json()).id as string;
      const made = await api(operator.page, "PUT", `/platform/workspaces/${id}/discoverable`, { discoverable: true });
      expect(made.status()).toBe(200);
      return { id, name };
    } finally { await alex.context.close(); }
  } finally { await operator.context.close(); }
}
const discoverTable = (page: Page) => page.getByRole("table", { name: "Discoverable workspaces" });
const myRequests = (page: Page) => page.getByRole("table", { name: "My requests" });
const queueTable = (page: Page) => page.getByRole("table", { name: "Access requests" });
const rowWith = (table: ReturnType<Page["getByRole"]>, text: string) => table.getByRole("row").filter({ hasText: text });

/**
 * Discovers `team` and sends the request. The reason always embeds the team's own (per-test
 * unique) name, so every row lookup below can filter on it instead of a generic status/placeholder
 * string that another test running in parallel against a different team could also produce.
 */
function reasonFor(team: { name: string }) { return `Need access: ${team.name}`; }
async function requestAccess(page: Page, team: { name: string }) {
  await page.goto("/access-requests");
  await rowWith(discoverTable(page), team.name).getByRole("button", { name: /^Actions for/ }).click();
  await page.getByRole("menuitem", { name: "Request access…" }).click();
  const dialog = page.getByRole("dialog", { name: new RegExp(`^Request to join ${team.name}\\?`) });
  await dialog.getByRole("textbox", { name: /^Reason/ }).fill(reasonFor(team));
  await dialog.getByRole("button", { name: "Send request" }).click();
  await expect(dialog).toBeHidden();
}

for (const width of [1440, 390]) {
  test(`requester submits, an admin approves and membership becomes visible at ${width}px`, async ({ browser }) => {
    const team = await createTeam(browser, `Access requests ${width}`);
    // Sign in at the default desktop viewport (the mobile header collapses the account button
    // the sign-in check waits for), then resize for the rest of the test, as holds.spec.ts does.
    const alex = await signIn(browser, "alex");
    const blair = await signIn(browser, "blair");
    try {
      await alex.page.setViewportSize({ width, height: 900 });
      await blair.page.setViewportSize({ width, height: 900 });
      // Blair discovers the team and submits a bounded, plain-text reason.
      await blair.page.goto("/access-requests");
      await rowWith(discoverTable(blair.page), team.name).getByRole("button", { name: /^Actions for/ }).click();
      await blair.page.getByRole("menuitem", { name: "Request access…" }).click();
      const submit = blair.page.getByRole("dialog", { name: new RegExp(`^Request to join ${team.name}\\?`) });
      await submit.getByRole("textbox", { name: /^Reason/ }).fill(reasonFor(team));
      await expectNoHorizontalScroll(blair.page);
      await shot(blair.page, `requester-dialog-${width}`);
      await submit.getByRole("button", { name: "Send request" }).click();
      await expect(submit).toBeHidden();

      // "My requests" shows the pending status, the submitted reason as plain text, and a Withdraw action.
      await blair.page.getByRole("tab", { name: "My requests" }).click();
      const mine = rowWith(myRequests(blair.page), reasonFor(team));
      await expect(mine).toContainText("Pending");
      await expectNoHorizontalScroll(blair.page);
      await shot(blair.page, `my-requests-${width}`);
      await expectAccessible(blair.page, `access requests requester ${width}`);

      // Alex, the workspace owner, approves it from Workspace Settings › Access requests.
      await alex.page.goto(`/workspaces/${team.id}/settings?tab=access-requests`);
      const row = rowWith(queueTable(alex.page), reasonFor(team));
      await expect(row).toBeVisible();
      await expectNoHorizontalScroll(alex.page);
      await shot(alex.page, `admin-queue-${width}`);
      await row.getByRole("button", { name: /^Actions for/ }).click();
      await alex.page.getByRole("menuitem", { name: "Approve…" }).click();
      const approve = alex.page.getByRole("dialog", { name: "Approve this request?" });
      await expect(approve).toBeVisible();
      await expectAccessible(alex.page, `access requests approve dialog ${width}`);
      await approve.getByRole("button", { name: "Approve" }).click();
      await expect(approve).toBeHidden();
      await expect(row).toHaveCount(0);

      // Membership is now real: Blair's own session lists the team, and the gateway's member list agrees.
      const me = await (await api(blair.page, "GET", "/me")).json();
      expect(me.workspaces.some((w: { id: string; role: string | null }) => w.id === team.id && w.role === "member")).toBe(true);
      const members = await (await api(alex.page, "GET", `/workspaces/${team.id}/members`)).json();
      expect(members.data.some((m: { user_id: string; role: string }) => m.user_id === me.user.id && m.role === "member")).toBe(true);

      // My requests now shows Approved and offers no Withdraw.
      await blair.page.reload();
      await blair.page.getByRole("tab", { name: "My requests" }).click();
      const decided = rowWith(myRequests(blair.page), reasonFor(team));
      await expect(decided).toContainText("Approved");
      await expect(decided.getByRole("button", { name: /^Actions for/ })).toHaveCount(0);
      await expectNoHorizontalScroll(blair.page);
    } finally { for (const s of [alex, blair]) await s.context.close(); }
  });
}

test("the workspace admin can deny a request instead", async ({ browser }) => {
  const team = await createTeam(browser, "Access requests deny");
  const alex = await signIn(browser, "alex"), blair = await signIn(browser, "blair");
  try {
    await requestAccess(blair.page, team);

    await alex.page.goto(`/workspaces/${team.id}/settings?tab=access-requests`);
    const row = rowWith(queueTable(alex.page), reasonFor(team));
    await row.getByRole("button", { name: /^Actions for/ }).click();
    await alex.page.getByRole("menuitem", { name: "Deny…" }).click();
    const deny = alex.page.getByRole("dialog", { name: "Deny this request?" });
    await deny.getByRole("button", { name: "Deny" }).click();
    await expect(deny).toBeHidden();
    await expect(row).toHaveCount(0);

    const me = await (await api(blair.page, "GET", "/me")).json();
    expect(me.workspaces.some((w: { id: string }) => w.id === team.id)).toBe(false);
    await blair.page.goto("/access-requests");
    await blair.page.getByRole("tab", { name: "My requests" }).click();
    await expect(rowWith(myRequests(blair.page), reasonFor(team))).toContainText("Denied");
  } finally { for (const s of [alex, blair]) await s.context.close(); }
});

test("the requester can withdraw a pending request before any decision", async ({ browser }) => {
  const team = await createTeam(browser, "Access requests withdraw");
  const blair = await signIn(browser, "blair");
  try {
    await requestAccess(blair.page, team);

    await blair.page.getByRole("tab", { name: "My requests" }).click();
    const row = rowWith(myRequests(blair.page), reasonFor(team));
    const trigger = row.getByRole("button", { name: /^Actions for/ });
    await trigger.click();
    await blair.page.getByRole("menuitem", { name: "Withdraw…" }).click();
    const withdraw = blair.page.getByRole("dialog", { name: "Withdraw this request?" });
    await withdraw.getByRole("button", { name: "Withdraw" }).click();
    await expect(withdraw).toBeHidden();
    await expect(row).toContainText("Withdrawn");
    // Withdrawing removes the row's only action, so its trigger unmounts along with the menu: the
    // table itself (DataTable's built-in recovery for "a deleted row's action", data-table.tsx) is
    // the stable element that regains focus, not the vanished per-row button.
    await expect(trigger).toHaveCount(0);
    await expect(myRequests(blair.page)).toBeFocused();
  } finally { await blair.context.close(); }
});

test("an Auditor reads the platform queue but is never offered a decision, by the UI or the API", async ({ browser }) => {
  const team = await createTeam(browser, "Access requests auditor");
  const blair = await signIn(browser, "blair");
  try { await requestAccess(blair.page, team); } finally { await blair.context.close(); }

  const auditor = await signIn(browser, "auditor");
  try {
    await auditor.page.goto("/admin/access-requests");
    await expect(queueTable(auditor.page)).toBeVisible();
    await expect(queueTable(auditor.page).getByRole("button", { name: /^Actions for/ })).toHaveCount(0);
    const list = await (await api(auditor.page, "GET", "/platform/access-requests")).json();
    const pending = list.data.find((r: { status: string; workspace_id: string }) => r.status === "pending" && r.workspace_id === team.id);
    expect(pending, "the submitted request is visible in the Auditor's read").toBeTruthy();
    expect((await api(auditor.page, "POST", `/platform/access-requests/${pending.id}/approve`)).status()).toBe(403);
    await expectNoHorizontalScroll(auditor.page);
    await expectAccessible(auditor.page, "access requests auditor queue");
  } finally { await auditor.context.close(); }
});
