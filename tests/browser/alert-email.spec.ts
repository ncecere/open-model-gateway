import { expect, test } from "@playwright/test";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { createServer } from "node:net";
import path from "node:path";
import pg from "pg";
import { api, expectNoCspViolations, signIn } from "./helpers";

test.afterEach(expectNoCspViolations);

// Real signed session + gateway incident API/UI + CLI worker + local SMTP.
// Only the fixture clock is advanced in PostgreSQL; no waits or real email.
test("incident history shows waiting, retrying, sent and exhausted delivery", async ({ browser }) => {
  const db = new pg.Client({ connectionString: process.env.OMG_BROWSER_DATABASE_URL });
  await db.connect();
  let temporary = true, accepted = 0;
  const relay = createServer(socket => {
    socket.on("error", () => {});
    socket.write("220 mock ESMTP\r\n");
    let buffer = "", data = false;
    socket.on("data", chunk => {
      buffer += chunk.toString();
      for (;;) {
        const end = buffer.indexOf("\r\n"); if (end < 0) break;
        const line = buffer.slice(0, end); buffer = buffer.slice(end + 2);
        if (data) { if (line === ".") { data = false; accepted++; socket.write("250 queued\r\n"); } continue; }
        if (line.startsWith("RCPT TO") && temporary) socket.write("450 try later\r\n");
        else if (line === "DATA") { data = true; socket.write("354 go\r\n"); }
        // Accepted DATA must succeed even when the relay never answers QUIT.
        else if (line === "QUIT") { /* deliberately no response */ }
        else socket.write("250 mock\r\n");
      }
    });
  });
  await new Promise<void>(resolve => relay.listen(0, "127.0.0.1", resolve));
  const port = (relay.address() as { port: number }).port;
  const { context, page } = await signIn(browser, "operator");
  const binary = process.env.OMG_BROWSER_GATEWAY_BIN ?? path.resolve("target/debug/open-model-gateway");
  const tick = () => promisify(execFile)(binary, ["alerts", "evaluate", "--once"], { env: { ...process.env, DATABASE_URL: process.env.OMG_BROWSER_DATABASE_URL, GATEWAY_ENV_FILE: path.resolve("target/browser-tests/gateway.env"), GATEWAY_ENV: "development", GATEWAY_SMTP_HOST_ALLOWLIST: "127.0.0.1", GATEWAY_SMTP_SECRET_ENV_ALLOWLIST: "" } });
  const due = () => db.query("UPDATE alert_recipient_outbox SET next_attempt_at=clock_timestamp()-interval '1 second' WHERE state='waiting'");
  const seed = async (summary: string) => {
    const row = await db.query("INSERT INTO alert_events(id,builtin,kind,subject_key,level,severity,summary) VALUES(gen_random_uuid(),'scim_last_admin','scim_last_admin',$1,1,'critical',$1) RETURNING id", [summary]);
    await db.query("INSERT INTO alert_deliveries(id,event_id,transition) VALUES(gen_random_uuid(),$1,'fired')", [row.rows[0].id]);
  };
  try {
    expect((await api(page, "PUT", "/platform/settings/email", { host: "127.0.0.1", port, tls: "none", from_address: "gateway@example.test" })).ok()).toBe(true);
    await seed("Browser retry delivery");
    await page.goto("/admin/settings/alerts?tab=history");
    await expect(page.getByRole("table", { name: "Alert history" }).getByText("Waiting", { exact: true })).toBeVisible();
    await tick(); await page.reload();
    const row = page.getByRole("row").filter({ hasText: "Browser retry delivery" });
    await expect(row).toContainText("Retrying · 1 waiting");
    await expect(row).toContainText("next");
    const events = await (await api(page, "GET", "/platform/alerts/events")).json();
    expect(events.data.find((e: { summary: string }) => e.summary === "Browser retry delivery").email).toMatchObject({ status: "pending", retrying: true, attempt_count: 1, waiting: 1 });
    expect(JSON.stringify(events)).not.toContain("fingerprint");
    temporary = false; await due(); await tick(); await page.reload();
    await expect(row).toContainText("Sent"); expect(accepted).toBe(1);
    temporary = true; await seed("Browser exhausted delivery");
    for (let i = 0; i < 5; i++) { await due(); await tick(); }
    await page.reload();
    await expect(page.getByRole("row").filter({ hasText: "Browser exhausted delivery" })).toContainText("Exhausted · 1");
    expect(accepted).toBe(1);
    await tick(); expect(accepted).toBe(1);
  } finally {
    // Shared-stack isolation: this spec's SMTP configuration is global
    // installation state, not per-test. Clear it before closing so later
    // specs (e.g. key-expiry) see the same unconfigured baseline they
    // expect regardless of project run order, and never dial this test's
    // relay after it closes.
    const reset = await api(page, "PUT", "/platform/settings/email", {}).catch(() => null);
    await context.close(); await db.end();
    await new Promise<void>((resolve, reject) => relay.close(error => error ? reject(error) : resolve()));
    // Asserted after teardown, so a failed reset can't skip it.
    expect(reset?.ok(), "email settings reset for later specs").toBe(true);
  }
});
