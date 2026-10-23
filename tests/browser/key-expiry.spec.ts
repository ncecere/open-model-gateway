import { expect, test } from "@playwright/test";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, signIn } from "./helpers";

// Creating a credential is setup only: never retain its response in traces or images.
test.use({ trace: "off", screenshot: "off", video: "off" });
test.afterEach(expectNoCspViolations);

function evaluate() {
  const database = process.env.OMG_BROWSER_DATABASE_URL;
  expect(database).toBeDefined();
  const binary = process.env.OMG_BROWSER_GATEWAY_BIN ?? path.resolve("target/debug/open-model-gateway");
  const result = spawnSync(binary, ["alerts", "evaluate", "--once"], {
    env: { ...process.env, DATABASE_URL: database, GATEWAY_ENV_FILE: path.resolve("target/browser-tests/gateway.env") },
    encoding: "utf8", timeout: 30_000,
  });
  expect(result.status, result.stderr).toBe(0);
}

for (const width of [1440, 390]) {
  test(`authorized expiry reminder: real evaluation, bell, read, keys and access loss at ${width}px`, async ({ browser }, testInfo) => {
    const operator = await signIn(browser, "operator"), holder = await signIn(browser, "alex");
    // The shared sign-in helper checks the desktop sidebar's Account button.
    // Exercise the reminder journey itself at the requested mobile viewport.
    await holder.page.setViewportSize({ width, height: 900 });
    const auditor = await signIn(browser, "auditor");
    try {
      const me = await (await api(holder.page, "GET", "/me")).json();
      const admin = await (await api(operator.page, "GET", "/me")).json();
      const workspaceName = `Expiry acceptance ${width}`;
      const created = await api(operator.page, "POST", "/platform/workspaces", { kind: "team", name: workspaceName, owner_user_id: admin.user.id });
      expect(created.status()).toBe(200);
      const workspace = (await created.json()).id;
      expect((await api(operator.page, "POST", `/platform/workspaces/${workspace}/members`, { user_id: me.user.id, role: "member" })).status()).toBe(200);
      // Flush pre-existing suite keys, then read them so this key adds exactly one unread.
      evaluate();
      expect((await api(holder.page, "POST", "/me/notifications/read", { all: true })).status()).toBe(200);
      const key = await api(holder.page, "POST", `/workspaces/${workspace}/keys`, { name: "Never show this credential name in reminders", expires_in_days: 1, model_ids: null });
      expect(key.status()).toBe(200);
      const keyId = (await key.json()).id;
      evaluate(); evaluate(); // One-shot restarts must not replay the event.
      const notifications = await (await api(holder.page, "GET", "/me/notifications?limit=200")).json();
      const reminders = notifications.data.filter((n: { workspace: { id: string } | null }) => n.workspace?.id === workspace);
      expect(reminders).toHaveLength(1);
      expect(reminders[0]).toMatchObject({ kind: "key_expiring", level: 1, read: false, rule: null, email: { status: "not_configured" } });
      expect(JSON.stringify(reminders)).not.toContain(keyId);
      expect(JSON.stringify(reminders)).not.toContain("Never show this credential name");
      expect((await (await api(holder.page, "GET", "/me/notifications/summary")).json()).unread).toBe(1);
      await holder.page.goto("/home");
      const bell = holder.page.getByRole("link", { name: "Notifications, 1 unread", exact: true });
      await expect(bell).toBeVisible();
      await bell.focus(); await holder.page.keyboard.press("Enter");
      await expect(holder.page).toHaveURL(/\/notifications$/);
      const row = holder.page.getByRole("row").filter({ has: holder.page.getByText(workspaceName, { exact: true }) });
      await expect(row.getByText("An API key expires within 1 day", { exact: true })).toBeVisible();
      await expect(row.getByText(/Built-in key-expiry reminder/)).toBeVisible();
      await expect(row.locator("time").first()).toBeVisible();
      await expect(row.getByText(/Email: Email not set up/)).toBeVisible();
      await expectNoHorizontalScroll(holder.page);
      await expectAccessible(holder.page, `key expiry notifications ${width}`);
      // Screenshot only the secret-free notification table (not account identity or key disclosure).
      await holder.page.getByRole("table", { name: "Notifications" }).screenshot({ path: testInfo.outputPath(`key-expiry-${width}.png`) });
      const actions = row.getByRole("button", { name: /actions/i });
      await actions.focus(); await holder.page.keyboard.press("Enter");
      const openKeys = holder.page.getByRole("menuitem", { name: "Open keys", exact: true });
      await expect(openKeys).toHaveAttribute("href", `/workspaces/${workspace}/keys`);
      await expect(holder.page.getByRole("menuitem", { name: "Open rule", exact: true })).toHaveCount(0);
      await holder.page.getByRole("menuitem", { name: "Mark as read", exact: true }).click();
      await expect(holder.page.getByRole("link", { name: "Notifications", exact: true })).toBeVisible();
      expect((await (await api(holder.page, "GET", "/me/notifications/summary")).json()).unread).toBe(0);
      expect((await (await api(holder.page, "GET", "/me/notifications?limit=200")).json()).data.find((n: { id: string }) => n.id === reminders[0].id).read).toBe(true);
      await actions.click(); await holder.page.getByRole("menuitem", { name: "Open keys", exact: true }).click();
      await expect(holder.page).toHaveURL(new RegExp(`/workspaces/${workspace}/keys$`));
      // Workspace owner receives the human-key reminder; platform-only readers never do.
      expect((await (await api(operator.page, "GET", "/me/notifications?limit=200")).json()).data.some((n: { id: string }) => n.id === reminders[0].id)).toBe(true);
      expect((await (await api(auditor.page, "GET", "/me/notifications?limit=200")).json()).data.some((n: { id: string }) => n.id === reminders[0].id)).toBe(false);
      expect((await api(operator.page, "DELETE", `/platform/workspaces/${workspace}/members/${me.user.id}`)).status()).toBe(200);
      expect((await (await api(holder.page, "GET", "/me/notifications?limit=200")).json()).data.some((n: { id: string }) => n.id === reminders[0].id)).toBe(false);
      expect((await api(holder.page, "POST", "/me/notifications/read", { ids: [reminders[0].id] })).status()).toBe(200);
      expect((await api(holder.page, "GET", `/workspaces/${workspace}/keys`)).status()).toBe(403);
      await holder.page.goto("/notifications");
      await expect(holder.page.getByText(workspaceName, { exact: true })).toHaveCount(0);
      await expectNoHorizontalScroll(holder.page);
    } finally {
      for (const session of [operator, holder, auditor]) await session.context.close();
    }
  });
}
