import { expect, test } from "@playwright/test";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, signIn } from "./helpers";
import { BASE_URL } from "./stack.mjs";

/*
 * Access reviews (#90): a Platform Admin starts a review in Admin › Access reviews; the Team's
 * owner gets a reminder, opens Review access and removes a manual membership created for this
 * spec, confirming what the removal does; the Auditor
 * reads the review without actions. Everything this spec creates is its own.
 */
test.afterEach(expectNoCspViolations);

test("a review is started, decided by the workspace owner and read by the auditor", async ({ browser }) => {
  const unique = Date.now().toString(36);
  const name = `Browser review ${unique}`;
  const operator = await signIn(browser, "operator");
  const alex = await signIn(browser, "alex");
  try {
    // A Team alex owns, with blair as a manual member.
    const me = await (await api(alex.page, "GET", "/me")).json();
    const created = await api(operator.page, "POST", "/platform/workspaces", { kind: "team", name: `Review team ${unique}`, owner_user_id: me.user.id });
    expect(created.status()).toBe(200);
    const team = (await created.json()).id as string;
    const blair = await signIn(browser, "blair");
    const blairId = (await (await api(blair.page, "GET", "/me")).json()).user.id as string;
    await blair.context.close();
    expect((await api(operator.page, "POST", `/platform/workspaces/${team}/members`, { user_id: blairId, role: "member" })).status()).toBe(200);

    const { page } = operator;
    await page.goto("/admin/access-reviews");
    await page.getByRole("button", { name: "Start review" }).click();
    const start = page.getByRole("dialog", { name: "Start an access review" });
    await start.getByRole("textbox", { name: "Name" }).fill(name);
    await start.getByRole("combobox", { name: /^Only these Teams and Projects/ }).fill(`Review team ${unique}`);
    await page.getByRole("option", { name: `Review team ${unique}` }).click();
    await start.getByRole("button", { name: "Start review" }).click();
    await expect(start).toBeHidden();
    await page.getByRole("link", { name }).click();
    await expect(page.getByRole("heading", { name })).toBeVisible();
    await expectAccessible(page, "access review detail");

    // The owner is reminded and reviews from Review access.
    await alex.page.goto("/notifications");
    const reminder = alex.page.getByRole("row").filter({ hasText: name });
    await reminder.getByRole("button", { name: /actions/i }).click();
    await alex.page.getByRole("menuitem", { name: "Open review" }).click();
    await expect(alex.page).toHaveURL(/\/access-reviews$/);
    const row = alex.page.getByRole("row").filter({ hasText: "blair@demo.invalid" }).filter({ hasText: name });
    await row.getByRole("button", { name: /^Actions for/ }).click();
    await alex.page.getByRole("menuitem", { name: "Remove…" }).click();
    const confirm = alex.page.getByRole("dialog");
    await expect(confirm).toContainText(`Removes their membership in Review team ${unique} now`);
    await confirm.getByRole("button", { name: "Remove access" }).click();
    await expect(confirm).toBeHidden();
    await expectNoHorizontalScroll(alex.page);
    const members = await (await api(alex.page, "GET", `/workspaces/${team}/members`)).json();
    expect(members.data.map((m: { email: string }) => m.email)).not.toContain("blair@demo.invalid");

    // The auditor reads the review and its decision, with no actions.
    const auditor = await signIn(browser, "auditor");
    try {
      await auditor.page.goto(new URL(page.url()).pathname);
      await expect(auditor.page.getByRole("row").filter({ hasText: "blair@demo.invalid" }).getByText("Removed", { exact: true })).toBeVisible();
      await expect(auditor.page.getByRole("button", { name: /^Close review$|^Actions for/ })).toHaveCount(0);
    } finally { await auditor.context.close(); }
    // Closed by the admin: nothing left to decide.
    await page.reload();
    await page.getByRole("button", { name: "Close review" }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Close review" }).click();
    await expect(page.getByText("Closed", { exact: true })).toBeVisible();
  } finally {
    await alex.context.close();
    await operator.context.close();
  }
});

/*
 * Keys (#90 slice 6): the Team's owner revokes a member's key from Review access, and the key
 * stops working; the Platform Admin's page never shows the key's name or its holder.
 */
test("a member's key is reviewed and revoked by the workspace owner, unnamed to the platform", async ({ browser }) => {
  const unique = Date.now().toString(36);
  const name = `Browser key review ${unique}`, keyName = `review-key-${unique}`, teamName = `Key review team ${unique}`;
  const operator = await signIn(browser, "operator");
  const alex = await signIn(browser, "alex");
  try {
    const me = await (await api(alex.page, "GET", "/me")).json();
    const created = await api(operator.page, "POST", "/platform/workspaces", { kind: "team", name: teamName, owner_user_id: me.user.id });
    expect(created.status()).toBe(200);
    const team = (await created.json()).id as string;
    const blair = await signIn(browser, "blair");
    let key: { id: string; token: string };
    try {
      const blairId = (await (await api(blair.page, "GET", "/me")).json()).user.id as string;
      expect((await api(operator.page, "POST", `/platform/workspaces/${team}/members`, { user_id: blairId, role: "member" })).status()).toBe(200);
      const issued = await api(blair.page, "POST", `/workspaces/${team}/keys`, { name: keyName, expires_in_days: 30 });
      expect(issued.status()).toBe(200);
      key = await issued.json();
    } finally { await blair.context.close(); }
    const models = () => fetch(`${BASE_URL}/v1/models`, { headers: { authorization: `Bearer ${key.token}` } });
    expect((await models()).status).toBe(200);

    const { page } = operator;
    await page.goto("/admin/access-reviews");
    await page.getByRole("button", { name: "Start review" }).click();
    const start = page.getByRole("dialog", { name: "Start an access review" });
    await start.getByRole("textbox", { name: "Name" }).fill(name);
    await start.getByRole("checkbox", { name: "Team and Project keys" }).check();
    await start.getByRole("checkbox", { name: "Team and Project memberships" }).uncheck();
    await start.getByRole("combobox", { name: /^Only these Teams and Projects/ }).fill(teamName);
    await page.getByRole("option", { name: teamName }).click();
    await start.getByRole("button", { name: "Start review" }).click();
    await expect(start).toBeHidden();
    await page.getByRole("link", { name }).click();
    await expect(page.getByRole("heading", { name })).toBeVisible();
    // The platform view: the key by kind and id, never its name or holder, and no reviewer to assign.
    await expect(page.getByRole("row").filter({ hasText: `Member's key ${key.id.slice(0, 8)}` })).toBeVisible();
    await expect(page.getByText(keyName)).toHaveCount(0);
    await expect(page.getByRole("table").getByText("blair@demo.invalid")).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^Actions for/ })).toHaveCount(0);
    await expectAccessible(page, "access review detail with keys");

    // The owner revokes it from Review access.
    await alex.page.goto("/access-reviews");
    const row = alex.page.getByRole("row").filter({ hasText: keyName });
    await expect(row).toContainText("Held by");
    await row.getByRole("button", { name: /^Actions for/ }).click();
    await expect(alex.page.getByRole("menuitem", { name: /^Flag/ })).toHaveCount(0);
    await alex.page.getByRole("menuitem", { name: "Remove…" }).click();
    const confirm = alex.page.getByRole("dialog", { name: `Revoke ${keyName} in ${teamName}?` });
    await expect(confirm).toContainText("A revoked key never returns");
    await confirm.getByRole("button", { name: "Revoke key" }).click();
    await expect(confirm).toBeHidden();
    await expectAccessible(alex.page, "review access with keys");
    await expectNoHorizontalScroll(alex.page);
    expect((await models()).status).toBe(401);

    await page.reload();
    await expect(page.getByRole("row").filter({ hasText: `Member's key ${key.id.slice(0, 8)}` }).getByText("Revoked", { exact: true })).toBeVisible();
  } finally {
    await alex.context.close();
    await operator.context.close();
  }
});
