import { expect, test, type Browser, type Page } from "@playwright/test";
import { randomUUID } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import pg from "pg";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, readFixture, signIn } from "./helpers";

// Real OIDC + gateway + PostgreSQL + built SPA. Like holds.spec.ts, only historical
// execution/reservation/ledger fixtures use SQL: there is no API for backdating
// attempts. Imports, mappings and frozen snapshots are all made by the real API.
// No paid requests; Anthropic connections are disabled and have no secret value.
test.use({ trace: "off", screenshot: "off", video: "off" });
test.afterEach(expectNoCspViolations);
const listUrl = "/admin/costs?tab=provider-reconciliation";
const endpoint = "/platform/provider-reconciliations";
const seedFile = path.resolve("target/browser-tests/state/provider-reconciliation.json");
type Seeded = { id: string; import_id: string; mapping_id: string; personal: string; ownerName: string; ownerEmail: string; ownerId: string };

async function create(page: Page, apiPath: string, body: unknown) {
  const response = await api(page, "POST", apiPath, body);
  expect(response.status(), `${apiPath}: ${await response.text()}`).toBe(200);
  return (await response.json()).id as string;
}
async function seed(browser: Browser): Promise<Seeded> {
  // A restarted worker reuses the first immutable snapshot, not a duplicate.
  if (existsSync(seedFile)) return JSON.parse(readFileSync(seedFile, "utf8"));
  const operator = await signIn(browser, "operator"), owner = await signIn(browser, "alex");
  const db = new pg.Client({ connectionString: process.env.OMG_BROWSER_DATABASE_URL });
  await db.connect();
  try {
    const me = await (await api(owner.page, "GET", "/me")).json();
    const personal = me.workspaces.find((w: { kind: string }) => w.kind === "personal").id as string;
    const { product, research } = readFixture();
    expect(product).toBeTruthy(); expect(research).toBeTruthy();
    // Closed UTC day is required by the Anthropic parser. A fixed historical day
    // also keeps these attempts out of other browser tests' current-period totals.
    const start = "2025-08-01T00:00:00Z", end = "2025-08-02T00:00:00Z";
    const model = await create(operator.page, "/platform/models", { public_name: "acceptance/reconciliation", display_name: "Reconciliation fixture", enabled: false, supported_protocols: ["messages"] });
    const scope = async (name: string) => {
      const connection = await create(operator.page, "/platform/providers", { name, provider: "anthropic", credential_ref: "env:OMG_BROWSER_UNUSED_ANTHROPIC", enabled: false });
      const deployment = await create(operator.page, "/platform/deployments", { model_id: model, provider_connection_id: connection, upstream_model: "claude-sonnet-4-20250514", enabled: false });
      return { connection, deployment };
    };
    const exclusive = await scope("Reconciliation exclusive"), ambiguous = await scope("Reconciliation ambiguous"), privateScope = await scope("Reconciliation private");
    const key = async (page: Page, ws: string) => create(page, `/workspaces/${ws}/keys`, { name: "reconciliation fixture", expires_in_days: 1, model_ids: null });
    const teamKey = await key(operator.page, product!), projectKey = await key(operator.page, research!), personalKey = await key(owner.page, personal);
    // Same historical fixtures as provider_cost_matching_tests.rs and the browser
    // Holds tests: settled ledger entries plus an unknown reservation and ledger
    // entry (unavailable usage/tier). SQL stays in the suite's disposable database.
    const attempt = async (workspace: string, apiKey: string, deployment: string, amount: string | null) => {
      const id = randomUUID();
      await db.query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,upstream_model,streamed,state,root_request_id,started_at,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'acceptance/reconciliation','anthropic','claude-sonnet-4-20250514',false,$5,$1,'2025-08-01T12:00:00Z',100,20)", [id, workspace, apiKey, deployment, amount === null ? "indeterminate" : "succeeded"]);
      if (amount !== null) {
        await db.query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,actual_microusd,input_tokens,output_tokens,price_tier) VALUES($1,$2,$3,$4,'2025-08-01T12:00:00Z','2025-08-01T12:00:00Z','2025-08-01T00:00:00Z',now(),'settled',0,$5,100,20,'standard')", [id, workspace, apiKey, deployment, amount]);
        await db.query("INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,admitted_at) VALUES($1,$2,'settlement',$3,'2025-08-01T12:00:00Z')", [randomUUID(), id, amount]);
      } else {
        await db.query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,unbounded_cost) VALUES($1,$2,$3,$4,'2025-08-01T12:00:00Z','2025-08-01T12:00:00Z','2025-08-01T00:00:00Z',now(),'unknown',123456,false)", [id, workspace, apiKey, deployment]);
        await db.query("INSERT INTO monetary_ledger(id,execution_id,kind,admitted_at) VALUES($1,$2,'unknown','2025-08-01T12:00:00Z')", [randomUUID(), id]);
      }
    };
    await attempt(product!, teamKey, exclusive.deployment, "2500");
    await attempt(product!, teamKey, exclusive.deployment, null);
    // An exclusive attestation contradicted by two actual OMG workspaces is
    // ambiguous. Neither the provider charge nor its variance may be allocated.
    await attempt(product!, teamKey, ambiguous.deployment, "500");
    await attempt(research!, projectKey, ambiguous.deployment, "700");
    await attempt(personal, personalKey, privateScope.deployment, "1200");
    const line = (workspace: string, amount: string, description: string) => ({ amount, currency: "USD", workspace_id: workspace, description, model: "claude-sonnet-4-20250514", service_tier: "standard", context_window: "0-200k", cost_type: "tokens", token_type: "uncached_input_tokens", inference_geo: "global", speed: "standard" });
    // Anthropic amounts are decimal CENTS, not dollars. Preserve the six-digit
    // converted amounts and signed credit, including half-even rounding.
    const import_id = await create(operator.page, "/platform/provider-cost-imports", {
      account_id: "browser-reconciliation-account", evidence_reference: "finance:browser-export", exported_at: "2025-08-03T00:00:00Z", supersedes: null,
      request: { starting_at: start, ending_at: end, bucket_width: "1d", group_by: ["workspace_id", "description"], limit: 31 },
      pages: [{ data: [{ starting_at: start, ending_at: end, results: [
        line("provider-exclusive", "123.78912", "Sonnet input usage"),
        line("provider-ambiguous", "200", "Shared account input usage"),
        line("provider-personal", "50", "Personal input usage"),
        { amount: "-2.00015", currency: "USD", workspace_id: "billing-adjustments", description: "Service credit", cost_type: "credit" },
      ] }], has_more: false, next_page: null }],
    });
    const binding = (provider: string, connection: string, workspace: string) => ({ provider_workspace_id: provider, connection_id: connection, deployment_id: null, workspace_id: workspace, effective_start: start, effective_end: end, exclusive: true, non_omg_traffic: false, attested: true, evidence_reference: "finance:browser-attestation", model_mappings: [] });
    const mapping_id = await create(operator.page, "/platform/provider-cost-mappings", {
      account_id: "browser-reconciliation-account", supersedes: null, evidence_reference: "finance:browser-mapping",
      bindings: [binding("provider-exclusive", exclusive.connection, product!), binding("provider-ambiguous", ambiguous.connection, product!), binding("provider-personal", privateScope.connection, personal)],
    });
    const id = await create(operator.page, endpoint, { import_id, mapping_id });
    const response = await api(operator.page, "GET", `${endpoint}/${id}`);
    expect(response.status()).toBe(200);
    const { snapshot } = await response.json();
    expect(snapshot.imported_microusd).toBe("3717889");
    expect(snapshot.estimates).toMatchObject({ settled_estimate_microusd: "4900", held_floor_microusd: "123456", attempts: "5", unresolved_attempts: "1", complete: false });
    expect(snapshot.personal_activity.estimates).toMatchObject({ settled_estimate_microusd: "1200", attempts: "1", complete: true });
    const ids = { id, import_id, mapping_id, personal, ownerName: me.user.display_name, ownerEmail: me.user.email, ownerId: me.user.id };
    expect(ids.ownerName).toBeTruthy(); expect(ids.ownerEmail).toBe("alex@demo.invalid");
    writeFileSync(seedFile, JSON.stringify(ids));
    return ids;
  } finally { await db.end(); await owner.context.close(); await operator.context.close(); }
}

async function identityFree(page: Page, ids: Seeded) {
  // Check the entire DOM, not just visible tab text (hidden content counts too).
  const dom = await page.locator("body").innerHTML();
  for (const privateValue of [ids.personal, ids.ownerId, ids.ownerEmail, ids.ownerName, "provider-personal"]) expect(dom).not.toContain(privateValue);
}
async function noWrites(page: Page) {
  await expect(page.getByRole("main").getByText("Read-only", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: /^(upload|import|create|freeze|map|delete|reconcile|settle|release)/i })).toHaveCount(0);
  await expect(page.getByRole("textbox")).toHaveCount(0);
}
async function comparison(page: Page, ids: Seeded, label: string) {
  const table = page.getByRole("table", { name: "Provider comparison lines", exact: true });
  await expect(table).toBeVisible();
  await expect(page.getByRole("group", { name: "Snapshot-wide imported charges" })).toContainText("$3.717889");
  await expect(page.getByRole("group", { name: "Snapshot-wide configured-rate estimates" })).toContainText("$0.0049");
  const credit = table.getByRole("row").filter({ hasText: "billing-adjustments" });
  await expect(credit.locator('[data-label="Imported charges"]')).toHaveText("-$0.020002");
  await expect(credit.locator('[data-label="Variance (imported − estimate)"]')).toHaveText("Unknown");
  const incomplete = table.getByRole("row").filter({ hasText: "provider-exclusive" });
  await expect(incomplete.locator('[data-label="Imported charges"]')).toHaveText("$1.237891");
  await expect(incomplete.locator('[data-label="Variance (imported − estimate)"]')).toHaveText("Incomplete");
  await expect(incomplete).toContainText("Incomplete estimate");
  const ambiguous = table.getByRole("row").filter({ hasText: "provider-ambiguous" });
  await expect(ambiguous.locator('[data-label="Imported charges"]')).toHaveText("$2.00");
  await expect(ambiguous.locator('[data-label="Actual attribution (OMG workspace)"]')).toHaveText("Shared / unallocated");
  await expect(ambiguous.locator('[data-label="Variance (imported − estimate)"]')).toHaveText("Unknown");
  await expect(ambiguous).toContainText("Ambiguous scope");
  for (const row of [credit, incomplete, ambiguous]) await expect(row.getByText("$0.00", { exact: true })).toHaveCount(0);
  await noWrites(page); await identityFree(page, ids);
  await expectNoHorizontalScroll(page); await expectAccessible(page, `${label} comparison`);
  await page.getByRole("tab", { name: "Estimates & holds", exact: true }).click();
  const holds = page.getByRole("group", { name: "Separate holds", exact: true });
  await expect(holds.locator("dl").filter({ hasText: "Held floor (at least)" }).locator("dt + dd")).toHaveText("$0.123456");
  await expect(holds.locator("dl").filter({ hasText: "Holds upper bound" }).locator("dd")).toHaveText("Unknown");
  await expectNoHorizontalScroll(page); await expectAccessible(page, `${label} holds`);
  await page.getByRole("tab", { name: "Unresolved coverage", exact: true }).click();
  await expect(page.getByRole("table", { name: "Unresolved coverage (overlapping, not additive)", exact: true }).getByRole("row")).toHaveCount(2);
  await expect(page.getByText(/Do not add unresolved coverage across buckets/)).toBeVisible();
  await identityFree(page, ids); await expectNoHorizontalScroll(page); await expectAccessible(page, `${label} unresolved`);
  await page.getByRole("tab", { name: "Personal totals", exact: true }).click();
  const personal = page.getByRole("region", { name: "Personal workspaces, combined totals only", exact: true });
  await expect(personal.getByRole("group", { name: "Combined personal actual charges", exact: true })).toContainText("$0.50");
  await expect(personal.getByRole("group", { name: "Combined personal estimates", exact: true })).toContainText("$0.0012");
  await expect(personal.getByRole("link")).toHaveCount(0); await expect(personal.getByRole("button")).toHaveCount(0);
  await identityFree(page, ids); await noWrites(page);
  await expectNoHorizontalScroll(page); await expectAccessible(page, `${label} personal`);
  await page.getByRole("tab", { name: "Source & provenance", exact: true }).click();
  await expect(page.getByRole("table", { name: "Frozen mapping provenance (shared scopes only)", exact: true }).getByRole("row")).toHaveCount(3);
  await identityFree(page, ids); await expectNoHorizontalScroll(page); await expectAccessible(page, `${label} provenance`);
}

for (const width of [1440, 390]) {
  for (const persona of ["operator", "auditor"] as const) {
    test(`${persona} reads real frozen provider comparison at ${width}px; keyboard focus and private totals`, async ({ browser }) => {
      const ids = await seed(browser), session = await signIn(browser, persona);
      const page = session.page;
      try {
        await page.setViewportSize({ width, height: 900 });
        await page.goto(listUrl);
        const origin = page.getByRole("button", { name: new RegExp(`^Open reconciliation ${ids.id}: `) });
        await expect(origin).toBeVisible();
        await expect(page.getByRole("table", { name: "Frozen reconciliations", exact: true })).toContainText("browser-reconciliation-account");
        await noWrites(page); await identityFree(page, ids);
        await expectNoHorizontalScroll(page); await expectAccessible(page, `${persona} ${width} list`);
        // Actual keyboard activation, not click() on a focusable element.
        await origin.focus(); await page.keyboard.press("Enter");
        await expect(page.getByRole("heading", { name: `Reconciliation ${ids.id}`, exact: true })).toBeFocused();
        await expect(page).toHaveURL(new RegExp(`record=${ids.id}`));
        await comparison(page, ids, `${persona} ${width}`);
        const back = page.getByRole("button", { name: "Back to reconciliations", exact: true });
        await back.focus(); await page.keyboard.press("Enter");
        await expect(origin).toBeFocused();
        await expect(page).toHaveURL(listUrl);
        if (persona === "auditor") {
          // Server refusal supplements the absence of controls.
          expect((await api(page, "POST", endpoint, { import_id: ids.import_id, mapping_id: ids.mapping_id })).status()).toBe(403);
          expect((await api(page, "POST", "/platform/provider-cost-imports", {})).status()).toBe(403);
          expect((await api(page, "POST", "/platform/provider-cost-mappings", {})).status()).toBe(403);
        }
      } finally { await session.context.close(); }
    });
  }
  test(`plain platform user is refused comparison list and direct detail at ${width}px`, async ({ browser }) => {
    const ids = await seed(browser), session = await signIn(browser, "blair");
    const page = session.page;
    try {
      await page.setViewportSize({ width, height: 900 });
      const me = await (await api(page, "GET", "/me")).json();
      expect(me.user.platform_role).toBe("user");
      for (const apiPath of [endpoint, `${endpoint}/${ids.id}`, "/platform/provider-cost-imports", "/platform/provider-cost-mappings"]) expect((await api(page, "GET", apiPath)).status()).toBe(403);
      for (const url of [listUrl, `${listUrl}&record=${ids.id}`]) {
        await page.goto(url);
        // The existing route guard refuses Admin navigation by returning to Home;
        // the real API independently denies both the list and selected snapshot.
        await expect(page).toHaveURL("/home");
        await expect(page.getByRole("heading", { name: "Welcome, Blair", exact: true })).toBeVisible();
        await expect(page.getByRole("table", { name: /Frozen reconciliations|Provider comparison lines/ })).toHaveCount(0);
        await expect(page.getByRole("tab", { name: "Provider reconciliation", exact: true })).toHaveCount(0);
        await expect(page.getByText("$3.717889", { exact: true })).toHaveCount(0);
        await identityFree(page, ids); await expectNoHorizontalScroll(page); await expectAccessible(page, `plain user refused ${width}`);
      }
    } finally { await session.context.close(); }
  });
}
