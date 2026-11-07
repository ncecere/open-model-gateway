import { expect, test, type Page } from "@playwright/test";
import { mkdirSync } from "node:fs";
import path from "node:path";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, signIn } from "./helpers";

/*
 * Reviewed workspace templates (#84): a Platform Admin creates a template in the dashboard,
 * cannot approve its own version, a second Platform Admin approves it, the author publishes
 * and creates a Team from it (owner and member chosen per slot), and an Auditor reads without
 * actions. The demo has one Platform Admin, so alex is granted a manual Admin role for this
 * spec and it is revoked afterwards (browser specs run one at a time; see playwright.config.ts).
 */
test.afterEach(expectNoCspViolations);
const shots = path.resolve("target/browser-tests/workspace-templates");
async function shot(page: Page, name: string) { mkdirSync(shots, { recursive: true }); await page.locator("main").screenshot({ path: path.join(shots, `${name}.png`) }); }
const versionActions = (page: Page) => page.getByRole("button", { name: "Actions for Version 1" });

test("a template is reviewed by another admin, published, and creates a Team", async ({ browser }) => {
  const unique = Date.now().toString(36);
  const name = `Browser template ${unique}`, team = `From template ${unique}`;
  const operator = await signIn(browser, "operator");
  const blair = await signIn(browser, "blair"); // signed in: entitled for a slot
  await blair.context.close();
  const alexId = (await (await api(operator.page, "GET", "/platform/users?q=alex%40demo.invalid&status=active&role=any")).json()).data[0].id as string;
  try {
    const { page } = operator;
    await page.goto("/admin/workspace-templates");
    await expect(page.getByRole("heading", { name: "Workspace templates" })).toBeVisible();
    await page.getByRole("button", { name: "Create template" }).click();
    const create = page.getByRole("dialog", { name: "Create a workspace template" });
    await create.getByRole("textbox", { name: "Name" }).fill(name);
    await create.getByRole("button", { name: "Create template" }).click();
    await expect(create).toBeHidden();
    await page.getByRole("link", { name }).click();
    await expect(page.getByText("Waiting for review")).toBeVisible();
    const detail = new URL(page.url()).pathname;
    expect(detail).toMatch(/^\/admin\/workspace-templates\/[0-9a-f-]{36}$/);
    await expectAccessible(page, "workspace template detail");
    await versionActions(page).click();
    const own = page.getByRole("menuitem", { name: /^Approve…/ });
    await expect(own).toHaveAttribute("aria-disabled", "true");
    await expect(own).toContainText("Another Platform Admin reviews the versions you write.");
    await page.keyboard.press("Escape");

    expect((await api(page, "POST", `/platform/users/${alexId}/roles`, { role: "admin" })).status()).toBe(200);
    const alex = await signIn(browser, "alex");
    try {
      await alex.page.goto(detail);
      await versionActions(alex.page).click();
      await alex.page.getByRole("menuitem", { name: "Approve…" }).click();
      await alex.page.getByRole("dialog", { name: "Approve version 1?" }).getByRole("button", { name: "Approve" }).click();
      await expect(alex.page.getByText("Reviewed", { exact: true })).toBeVisible();
    } finally { await alex.context.close(); }

    await page.reload();
    await versionActions(page).click();
    await page.getByRole("menuitem", { name: "Publish…" }).click();
    await page.getByRole("dialog", { name: "Publish version 1?" }).getByRole("button", { name: "Publish" }).click();
    await expect(page.getByText("Published", { exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Create workspace", exact: true }).click();
    const apply = page.getByRole("dialog", { name: `Create a Team from ${name}` });
    await apply.getByRole("textbox", { name: "Name" }).fill(team);
    for (const [slot, email] of [["owner \\(owner\\)", "alex@demo.invalid"], ["members \\(member\\)", "blair@demo.invalid"]] as const) {
      await apply.getByRole("combobox", { name: new RegExp(`^${slot}`) }).fill(email.split("@")[0]!);
      await page.getByRole("option", { name: email }).click();
    }
    await shot(page, "create-workspace");
    await apply.getByRole("button", { name: "Create workspace" }).click();
    await expect(page).toHaveURL(/\/admin\/teams\/[0-9a-f-]{36}$/);
    await expect(page.getByRole("heading", { name: team })).toBeVisible();
    await expectNoHorizontalScroll(page);
    await shot(page, "created-team");
    const ws = new URL(page.url()).pathname.split("/").at(-1)!;
    const members = await (await api(page, "GET", `/platform/workspaces/${ws}/members?limit=10`)).json();
    expect(members.data.map((m: { email: string; role: string }) => `${m.email}:${m.role}`).sort()).toEqual(["alex@demo.invalid:owner", "blair@demo.invalid:member"]);

    const auditor = await signIn(browser, "auditor");
    try {
      await auditor.page.goto(detail);
      await expect(auditor.page.getByText("Published", { exact: true })).toBeVisible();
      await expect(auditor.page.getByRole("button", { name: /^Actions for|^Add version$|^Create workspace$/ })).toHaveCount(0);
      await auditor.page.goto("/admin/workspace-templates");
      await expect(auditor.page.getByRole("link", { name })).toBeVisible();
      await expect(auditor.page.getByRole("button", { name: "Create template" })).toHaveCount(0);
    } finally { await auditor.context.close(); }
  } finally {
    await api(operator.page, "DELETE", `/platform/users/${alexId}/roles/admin`);
    await operator.context.close();
    // Later specs reuse each persona's stored session: refresh alex's after the role change.
    await (await signIn(browser, "alex")).context.close();
  }
});
