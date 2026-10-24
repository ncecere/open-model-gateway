import { expect, test } from "@playwright/test";
import { createServer } from "node:http";
import { createHmac, timingSafeEqual } from "node:crypto";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import path from "node:path";
import { api, expectNoCspViolations, expectNoHorizontalScroll, signIn } from "./helpers";
import { PORTS, WEBHOOK_URL, WEBHOOK_SECRET_NAME } from "./stack.mjs";

test.afterEach(expectNoCspViolations);
const location = "/admin/settings/alerts?tab=webhooks";

test("Admin create → enable → signed test → deliveries; Auditor read-only; delete", async ({ browser }) => {
  const secret = process.env.OMG_BROWSER_WEBHOOK_SECRET;
  if (!secret) throw new Error("Browser stack did not configure its disposable webhook signing secret");
  const accepted: string[] = [], refused: string[] = [];
  const receiver = createServer((req, res) => {
    let bytes = Buffer.alloc(0);
    req.on("data", (chunk: Buffer) => { bytes = Buffer.concat([bytes, chunk]); if (bytes.length > 65536) req.destroy(); });
    req.on("end", () => {
      try {
        const signature = String(req.headers["x-omg-signature"] ?? "");
        const match = /^t=(\d+),v1=([0-9a-f]{64})$/.exec(signature);
        if (!match) throw new Error("signature format");
        const expected = createHmac("sha256", secret).update(`${match[1]}.`).update(bytes).digest();
        if (!timingSafeEqual(expected, Buffer.from(match[2]!, "hex"))) throw new Error("signature mismatch");
        if (Math.abs(Date.now() / 1000 - Number(match[1])) > 300) throw new Error("timestamp outside replay window");
        const body = JSON.parse(bytes.toString("utf8"));
        if (req.method !== "POST" || req.url !== "/hook" || body.schema !== "omg.alert.v1" || body.test !== true || body.event !== null || body.delivery_id !== req.headers["x-omg-delivery"]) throw new Error("fixed test envelope");
        accepted.push(body.delivery_id); res.writeHead(204).end();
      } catch (error) { refused.push((error as Error).message); res.writeHead(400).end(); }
    });
  });
  await new Promise<void>((resolve, reject) => { receiver.once("error", reject); receiver.listen(PORTS.webhook, "127.0.0.1", resolve); });
  const operator = await signIn(browser, "operator"), auditor = await signIn(browser, "auditor");
  let id: string | undefined;
  try {
    const page = operator.page;
    await page.goto(location);
    await expect(page.getByRole("tab", { name: "Webhooks", selected: true })).toBeVisible();
    await page.getByRole("button", { name: "Create webhook", exact: true }).click();
    const form = page.getByRole("dialog");
    await form.getByRole("textbox", { name: "Name", exact: true }).fill("Browser signed receiver");
    await form.getByRole("textbox", { name: "URL", exact: true }).fill(WEBHOOK_URL);
    await form.getByRole("textbox", { name: "Signing reference", exact: true }).fill(`env:${WEBHOOK_SECRET_NAME}`);
    await form.getByRole("button", { name: "Create webhook", exact: true }).click();
    await expect(form).toHaveCount(0);
    const row = page.getByRole("row").filter({ hasText: "Browser signed receiver" });
    await expect(row.getByText("Disabled", { exact: true })).toBeVisible();
    const list = await (await api(page, "GET", "/platform/alerts/webhooks")).json();
    id = list.data.find((d: { label: string }) => d.label === "Browser signed receiver").id;
    expect(list.data.find((d: { id: string }) => d.id === id).enabled).toBe(false);
    await row.getByRole("button", { name: "Actions for Browser signed receiver" }).click();
    await page.getByRole("menuitem", { name: "Enable…", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Enable webhook", exact: true }).click();
    await expect(row.getByText("Enabled", { exact: true })).toBeVisible();
    await row.getByRole("button", { name: /Actions/ }).click();
    await page.getByRole("menuitem", { name: "Send test", exact: true }).click();
    await expect(page.getByText(/Test queued/)).toBeVisible();
    const binary = process.env.OMG_BROWSER_GATEWAY_BIN ?? path.resolve("target/debug/open-model-gateway");
    await promisify(execFile)(binary, ["alerts", "evaluate", "--once"], { env: {
      ...process.env, DATABASE_URL: process.env.OMG_BROWSER_DATABASE_URL,
      GATEWAY_ENV_FILE: path.resolve("target/browser-tests/gateway.env"), GATEWAY_ENV: "development",
      GATEWAY_ALERT_WEBHOOK_URL_ALLOWLIST: WEBHOOK_URL, GATEWAY_ALERT_WEBHOOK_SECRET_ENV_ALLOWLIST: WEBHOOK_SECRET_NAME,
      [WEBHOOK_SECRET_NAME]: secret, GATEWAY_SECRET_ENV_ALLOWLIST: "", GATEWAY_SMTP_SECRET_ENV_ALLOWLIST: "",
    } });
    expect(refused).toEqual([]); expect(accepted).toHaveLength(1);
    await page.getByRole("dialog").getByRole("button", { name: "Refresh deliveries", exact: true }).click();
    await expect(page.getByRole("table", { name: "Webhook deliveries" }).getByText("Sent", { exact: true })).toBeVisible();
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 900 }); await expectNoHorizontalScroll(page);
      await page.screenshot({ path: path.resolve(`target/browser-tests/webhooks-admin-deliveries-${width}.png`), fullPage: true });
    }
    await page.getByRole("button", { name: "Close deliveries", exact: true }).click();
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 900 }); await expectNoHorizontalScroll(page);
      await page.screenshot({ path: path.resolve(`target/browser-tests/webhooks-admin-${width}.png`), fullPage: true });
    }
    await auditor.page.goto(location);
    await expect(auditor.page.getByText("Browser signed receiver", { exact: true })).toBeVisible();
    await expect(auditor.page.getByRole("button", { name: "Create webhook" })).toHaveCount(0);
    const redacted = await (await api(auditor.page, "GET", `/platform/alerts/webhooks/${id}`)).json();
    expect(redacted.url).toBeUndefined(); expect(redacted.current_secret_ref).toBeUndefined();
    await auditor.page.getByRole("button", { name: "Actions for Browser signed receiver" }).click();
    for (const name of ["Edit…", "Enable…", "Disable…", "Delete…", "Send test"]) await expect(auditor.page.getByRole("menuitem", { name, exact: true })).toHaveCount(0);
    await auditor.page.getByRole("menuitem", { name: "Deliveries", exact: true }).click();
    await expect(auditor.page.getByRole("table", { name: "Webhook deliveries" }).getByText("Sent", { exact: true })).toBeVisible();
    await expect(auditor.page.getByText(WEBHOOK_URL, { exact: true })).toHaveCount(0);
    await expect(auditor.page.getByText(`env:${WEBHOOK_SECRET_NAME}`, { exact: true })).toHaveCount(0);
    for (const width of [1440, 390]) {
      await auditor.page.setViewportSize({ width, height: 900 }); await expectNoHorizontalScroll(auditor.page);
      await auditor.page.screenshot({ path: path.resolve(`target/browser-tests/webhooks-auditor-${width}.png`), fullPage: true });
    }
    expect((await api(auditor.page, "POST", `/platform/alerts/webhooks/${id}/test`)).status()).toBe(403);
    await row.getByRole("button", { name: /Actions/ }).click();
    await page.getByRole("menuitem", { name: "Delete…", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Delete webhook", exact: true }).click();
    await expect(row).toHaveCount(0);
    expect((await api(page, "GET", `/platform/alerts/webhooks/${id}`)).status()).toBe(404); id = undefined;
  } finally {
    try { if (id) expect((await api(operator.page, "DELETE", `/platform/alerts/webhooks/${id}`)).ok()).toBe(true); }
    finally { await operator.context.close(); await auditor.context.close(); await new Promise<void>(resolve => receiver.close(() => resolve())); }
  }
});
