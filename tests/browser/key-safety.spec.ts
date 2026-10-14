import { expect, test, type BrowserContext, type Page } from "@playwright/test";
import { api, readFixture, signIn, expectNoCspViolations } from "./helpers";

test.afterEach(expectNoCspViolations);
import { captureCallback, credentialsContained, installKeyProbe, invokeCallback, probeCounts, waitForIdle } from "./key-lifetime-probe";

test.describe.configure({ mode: "serial" });
// Credential-bearing acceptance produces boolean/count evidence only, never traces or images.
test.use({ trace: "off", screenshot: "off", video: "off" });
type Flow = "creation" | "rotation";
const flows: Flow[] = ["creation", "rotation"];
let operator: Page, operatorContext: BrowserContext, alexId: string, workspace: string, personal: string;
let number = 0;
const name = () => `Key safety ${++number}`;
const workspacePath = () => `/workspaces/${workspace}`;

test.beforeAll(async ({ browser }) => {
  ({ page: operator, context: operatorContext } = await signIn(browser, "operator"));
  const owner = await (await api(operator, "GET", "/me")).json();
  const alex = await signIn(browser, "alex");
  const session = await (await api(alex.page, "GET", "/me")).json();
  alexId = session.user.id;
  personal = session.workspaces.find((entry: { kind: string }) => entry.kind === "personal").id;
  await alex.context.close();
  const created = await api(operator, "POST", "/platform/workspaces", { kind: "team", name: "Key safety acceptance", owner_user_id: owner.user.id });
  expect(created.status()).toBe(200);
  workspace = (await created.json()).id;
  expect((await api(operator, "POST", `/platform/workspaces/${workspace}/members`, { user_id: alexId, role: "admin" })).ok()).toBe(true);
  const model = readFixture().model;
  expect(model).toBeDefined();
  expect((await api(operator, "POST", `/platform/workspaces/${workspace}/models`, { model_id: model })).ok()).toBe(true);
});
test.afterAll(async () => { await operatorContext?.close(); });

async function ready(page: Page) {
  await page.goto(`${workspacePath()}/keys`);
  await expect(page.getByRole("button", { name: "Create key", exact: true })).toBeEnabled();
  await installKeyProbe(page);
}
async function dismissNotices(page: Page) {
  const dismiss = page.getByRole("button", { name: "Dismiss notification", exact: true });
  const count = await dismiss.count();
  for (let index = 0; index < count; index++) await dismiss.first().click();
}
async function open(page: Page, flow: Flow) {
  if (flow === "creation") {
    await page.getByRole("button", { name: "Create key", exact: true }).click();
    await page.getByRole("dialog").getByRole("textbox", { name: /^Name/ }).fill(name());
  } else {
    const keyName = name();
    const result = await api(page, "POST", `${workspacePath()}/keys`, { name: keyName, expires_in_days: 30, model_ids: null });
    expect(result.status()).toBe(200);
    await page.reload();
    await installKeyProbe(page);
    await page.getByRole("button", { name: `Actions for ${keyName}`, exact: true }).click();
    await page.getByRole("menuitem", { name: "Rotate key…", exact: true }).click();
    await page.getByRole("dialog").getByRole("spinbutton", { name: "Expires in (days)", exact: true }).fill("45");
  }
  const form = page.getByRole("dialog").locator("form");
  await captureCallback(page, "submit", form, "onSubmit");
  await waitForIdle(page);
  return form;
}
async function issued(page: Page, flow: Flow) {
  await open(page, flow);
  await page.getByRole("dialog").locator('button[type="submit"]').click();
  const disclosure = page.getByRole("dialog").locator("textarea");
  await disclosure.waitFor({ state: "visible" });
  expect(await disclosure.evaluate(node => {
    if (!(node instanceof HTMLTextAreaElement)) throw new Error("Expected the actual disclosure.");
    return window.__omgKeyLifetimeAcceptance.disclosureMatches(node);
  })).toBe(true);
  expect(await probeCounts(page)).toMatchObject({ posts: 1, credentialResponses: 1 });
  await captureCallback(page, "copy", page.getByRole("button", { name: "Copy key", exact: true }), "onClick");
  expect(await invokeCallback(page, "copy")).toBe(true);
  expect(await probeCounts(page)).toMatchObject({ copies: 1, exactCopies: 1 });
  expect(await credentialsContained(page)).toBe(true);
  await dismissNotices(page);
  await waitForIdle(page);
}
async function acceptDiscard(page: Page) {
  await page.getByRole("alertdialog").getByRole("button", { name: "Discard", exact: true }).click();
}
async function noNewWork(page: Page, before: Awaited<ReturnType<typeof probeCounts>>) {
  await waitForIdle(page);
  expect(await probeCounts(page)).toEqual(before);
  expect(await credentialsContained(page)).toBe(true);
}

for (const flow of flows) {
  test(`${flow}: canceled valid submit cannot POST; a fresh owner can`, async ({ browser }) => {
    const { page, context } = await signIn(browser, "alex");
    try {
      await ready(page); await open(page, flow);
      const before = await probeCounts(page);
      await page.getByRole("dialog").getByRole("button", { name: "Cancel", exact: true }).click();
      if (flow === "rotation") await acceptDiscard(page);
      else expect(await page.getByRole("alertdialog").count()).toBe(0);
      await expect.poll(() => page.getByRole("dialog").count()).toBe(0);
      expect(await invokeCallback(page, "submit")).toBe(false);
      await noNewWork(page, before);
      await issued(page, flow);
    } finally { await context.close(); }
  });
  test(`${flow}: accepted SPA navigation retires submit and disclosure`, async ({ browser }) => {
    const { page, context } = await signIn(browser, "alex");
    try {
      await ready(page); await open(page, flow);
      const draft = await probeCounts(page);
      await page.evaluate(() => window.__omgKeyLifetimeAcceptance.navigate());
      await acceptDiscard(page); await page.waitForURL("**/home");
      expect(await invokeCallback(page, "submit")).toBe(false);
      await noNewWork(page, draft);
      await ready(page); await issued(page, flow);
      const disclosed = await probeCounts(page);
      await page.evaluate(() => window.__omgKeyLifetimeAcceptance.navigate());
      await page.waitForURL("**/home");
      expect(await page.getByRole("alertdialog").count()).toBe(0);
      expect(await invokeCallback(page, "copy")).toBe(false);
      await noNewWork(page, disclosed);
    } finally { await context.close(); }
  });
  test(`${flow}: retiring an admitted response does not roll back or replay Rust's committed key`, async ({ browser }) => {
    const { context, page } = await signIn(browser, "alex");
    try {
      await ready(page);
      const form = await open(page, flow);
      await captureCallback(page, "pending", form, "onSubmit");
      const before = await (await api(page, "GET", `${workspacePath()}/keys?limit=200`)).json();
      expect(before.data.length).toBeLessThan(200); // Current API is offset-paged; this fresh fixture must fit in one complete page.
      const activeBefore: string[] = before.data.filter((row: { status: string }) => row.status === "active").map((row: { id: string }) => row.id);
      await page.evaluate(() => window.__omgKeyLifetimeAcceptance.holdNextResponse());
      expect(await invokeCallback(page, "pending")).toBe(true);
      await page.waitForFunction(() => window.__omgKeyLifetimeAcceptance.responseHeld());
      expect((await probeCounts(page)).posts).toBe(1);
      await page.evaluate(() => window.__omgKeyLifetimeAcceptance.navigate());
      if (flow === "rotation") await acceptDiscard(page);
      await page.waitForURL("**/home");
      await expect(page.getByRole("heading", { name: /^Welcome,/ })).toBeVisible();
      expect(await invokeCallback(page, "pending")).toBe(false);
      if (flow === "rotation") expect(await page.evaluate(() => window.__omgKeyLifetimeAcceptance.heldResponseAborted())).toBe(true);
      await page.evaluate(async () => {
        window.__omgKeyLifetimeAcceptance.releaseResponse();
        await new Promise<number>(resolve => requestAnimationFrame(resolve));
      });
      await page.waitForFunction(() => !window.__omgKeyLifetimeAcceptance.responseHeld());
      await waitForIdle(page);
      const after = await (await api(page, "GET", `${workspacePath()}/keys?limit=200`)).json();
      expect(after.data.length).toBeLessThan(200);
      const activeAfter: string[] = after.data.filter((row: { status: string }) => row.status === "active").map((row: { id: string }) => row.id);
      expect(activeAfter.filter(id => !activeBefore.includes(id))).toHaveLength(1);
      expect(activeAfter).toHaveLength(activeBefore.length + (flow === "creation" ? 1 : 0));
      if (flow === "rotation") expect(activeBefore.filter(id => !activeAfter.includes(id))).toHaveLength(1);
      await expect(page.getByRole("dialog")).toHaveCount(0);
      expect(await invokeCallback(page, "pending")).toBe(false);
      const counts = await probeCounts(page);
      expect(counts.posts).toBe(1); expect(counts.copies).toBe(0);
      expect(await credentialsContained(page)).toBe(true);
    } finally { await context.close(); }
  });

  for (const boundary of ["omg:unauthorized", "omg:access-refresh"] as const) {
    test(`${flow}: ${boundary} retires connected submit and Copy synchronously`, async ({ browser }) => {
      const { page, context } = await signIn(browser, "alex");
      try {
        await ready(page); await open(page, flow);
        const draft = await probeCounts(page);
        expect(await invokeCallback(page, "submit", boundary)).toBe(true);
        await noNewWork(page, draft);
        await expect.poll(() => page.getByRole("dialog").count()).toBe(0);
        await ready(page); await issued(page, flow);
        const disclosed = await probeCounts(page);
        expect(await invokeCallback(page, "copy", boundary)).toBe(true);
        await noNewWork(page, disclosed);
        await expect.poll(() => page.getByRole("dialog").count()).toBe(0);
      } finally { await context.close(); }
    });
  }
}

test("corrected creation validation sends exactly one POST", async ({ browser }) => {
  const { page, context } = await signIn(browser, "alex");
  try {
    await ready(page);
    await page.getByRole("button", { name: "Create key", exact: true }).click();
    const dialog = page.getByRole("dialog"), field = dialog.getByRole("textbox", { name: /^Name/ });
    await field.fill("   ");
    await dialog.locator('button[type="submit"]').click();
    await expect(dialog.getByText("Enter a name.", { exact: true })).toBeVisible();
    expect((await probeCounts(page)).posts).toBe(0);
    await field.fill(name()); await dialog.locator('button[type="submit"]').click();
    await dialog.locator("textarea").waitFor({ state: "visible" });
    expect(await probeCounts(page)).toMatchObject({ posts: 1, credentialResponses: 1 });
    expect(await credentialsContained(page)).toBe(true);
    expect(await page.evaluate(() => window.__omgKeyLifetimeAcceptance.mutationKeyNegativeControl())).toBe(true);
    expect(await credentialsContained(page)).toBe(true);
  } finally { await context.close(); }
});

test("real membership revocation, human/service and Personal authorization remain live", async ({ browser }) => {
  const { page, context } = await signIn(browser, "alex");
  try {
    await ready(page); await issued(page, "creation");
    const rows = await (await api(page, "GET", `${workspacePath()}/keys`)).json();
    const human = rows.data.find((entry: { issued_to_user_id: string }) => entry.issued_to_user_id === alexId);
    expect(human).toBeDefined();
    // Even a workspace owner/Platform Admin cannot rotate someone else's human key.
    expect((await api(operator, "POST", `${workspacePath()}/keys/${human.id}/rotate`, { expires_in_days: 30 })).status()).toBe(403);
    expect((await api(operator, "GET", `/workspaces/${personal}/keys`)).status()).toBe(403);
    expect((await api(operator, "POST", `/workspaces/${personal}/service-accounts`, { name: "Forbidden" })).status()).toBe(403);
    expect((await api(page, "POST", `/workspaces/${personal}/service-accounts`, { name: "Forbidden personal service" })).status()).toBe(400);
    const service = await api(page, "POST", `${workspacePath()}/service-accounts`, { name: "Acceptance automation" });
    expect(service.status()).toBe(200);
    const serviceId = (await service.json()).id;
    const serviceKey = await api(page, "POST", `${workspacePath()}/keys`, { name: "Acceptance service key", expires_in_days: 30, service_account_id: serviceId });
    expect(serviceKey.status()).toBe(200);
    const serviceKeyId = (await serviceKey.json()).id;
    expect((await api(operator, "POST", `${workspacePath()}/keys/${serviceKeyId}/rotate`, { expires_in_days: 30 })).status()).toBe(200);
    const personalKey = await api(page, "POST", `/workspaces/${personal}/keys`, { name: "Acceptance personal key", expires_in_days: 30 });
    expect(personalKey.status()).toBe(200);
    const personalKeyId = (await personalKey.json()).id;
    expect((await api(page, "POST", `/workspaces/${personal}/keys/${personalKeyId}/rotate`, { expires_in_days: 30 })).status()).toBe(200);
    expect((await api(operator, "POST", `/platform/workspaces/${workspace}/members`, { user_id: alexId, role: "member" })).ok()).toBe(true);
    expect((await api(page, "POST", `${workspacePath()}/keys`, { name: "Denied service key", expires_in_days: 30, service_account_id: serviceId })).status()).toBe(403);
    expect((await api(page, "POST", `${workspacePath()}/keys`, { name: "Allowed member key", expires_in_days: 30 })).status()).toBe(200);
    expect((await api(operator, "DELETE", `/platform/workspaces/${workspace}/members/${alexId}`)).ok()).toBe(true);
    expect((await api(page, "GET", `${workspacePath()}/keys`)).status()).toBe(403);
    expect((await api(page, "POST", `${workspacePath()}/keys`, { name: "Denied after revocation", expires_in_days: 30 })).status()).toBe(403);
    const before = await probeCounts(page);
    // Rust has committed the revocation; the existing boundary retires the still-connected UI owner.
    expect(await invokeCallback(page, "copy", "omg:access-refresh")).toBe(true);
    await noNewWork(page, before);
    await expect.poll(() => page.getByRole("dialog").count()).toBe(0);
    await page.waitForURL("**/home");
    expect((await api(operator, "POST", `/platform/workspaces/${workspace}/members`, { user_id: alexId, role: "admin" })).ok()).toBe(true);
    // Returning access never reactivates the previously revoked human credential.
    expect((await api(page, "POST", `${workspacePath()}/keys/${human.id}/rotate`, { expires_in_days: 30 })).status()).toBe(404);
    expect((await api(page, "PATCH", `${workspacePath()}/keys/${human.id}`, { disabled: false })).status()).toBe(409);
    expect(await invokeCallback(page, "copy")).toBe(false);
    await noNewWork(page, before);
    await ready(page); await issued(page, "creation");
  } finally {
    await api(operator, "POST", `/platform/workspaces/${workspace}/members`, { user_id: alexId, role: "admin" });
    await context.close();
  }
});
