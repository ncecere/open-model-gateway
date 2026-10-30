import { expect, test, type Page, type TestInfo } from "@playwright/test";
import { createHash, randomUUID } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import pg from "pg";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, signIn } from "./helpers";
import { ISSUER_URL } from "./stack.mjs";

// Standalone REAL STACK: signed OIDC, built SPA, real gateway APIs and the
// harness's disposable PostgreSQL database. No route mocks or paid requests.
// SQL is only for a historical settled attempt (there is no backdating API),
// following provider-reconciliation.spec.ts. Never connect to the admin fixture.
test.use({ trace: "off", screenshot: "off", video: "off" });
test.afterEach(expectNoCspViolations);
const listUrl = "/admin/costs?tab=provider-reconciliation";
const imports = "/platform/provider-cost-imports", mappings = "/platform/provider-cost-mappings", reconciliations = "/platform/provider-reconciliations";
const seedFile = path.resolve("target/browser-tests/state/provider-cost-upload-roles.json");
type Result = { id: string; import_id: string; mapping_id: string; account: string };

async function create(page: Page, apiPath: string, body: unknown) {
  const response = await api(page, "POST", apiPath, body);
  expect(response.status(), `${apiPath}: ${await response.text()}`).toBe(200);
  return (await response.json()).id as string;
}
async function read(page: Page, apiPath: string) {
  const response = await api(page, "GET", apiPath);
  expect(response.status(), apiPath).toBe(200);
  return response.json();
}
async function check(page: Page, label: string) {
  await expectNoHorizontalScroll(page);
  await expectAccessible(page, label);
}
const postTo = (endpoint: string) => (response: import("@playwright/test").Response) =>
  new URL(response.url()).pathname === `/api/v1${endpoint}` && response.request().method() === "POST";
async function keyboardActivate(page: Page, target: ReturnType<Page["getByRole"]>) {
  await target.focus();
  await expect(target).toBeFocused();
  await page.keyboard.press("Enter");
}
// Chromium omits file-backed fetch bodies from Request.postDataBuffer().
// Observe the actual fetch argument without changing the body, request or real
// response. Duplicate-key rejection below independently proves no reserialization.
async function observeUploadBytes(page: Page) {
  // Retain real response bodies for file-backed fetches in Chromium's inspector.
  const network = await page.context().newCDPSession(page);
  await network.send("Network.enable", { maxTotalBufferSize: 16 * 1024 * 1024, maxResourceBufferSize: 8 * 1024 * 1024, enableDurableMessages: true });
  await page.context().addInitScript(() => {
    const original = window.fetch.bind(window);
    window.fetch = (input, init) => {
      if (typeof input === "string" && input === "/api/v1/platform/provider-cost-imports" && init?.body instanceof File) {
        (window as unknown as { reportBytes: Promise<number[]> }).reportBytes = init.body.arrayBuffer().then(bytes => Array.from(new Uint8Array(bytes)));
      }
      return original(input, init);
    };
  });
}
async function expectUploadBytes(page: Page, expected: Buffer) {
  const sent = await page.evaluate(() => (window as unknown as { reportBytes: Promise<number[]> }).reportBytes);
  expect(Buffer.from(sent)).toEqual(expected);
}
async function uploadFile(page: Page, info: TestInfo, name: string, bytes: Buffer) {
  const file = info.outputPath(name);
  writeFileSync(file, bytes);
  await page.getByLabel("Anthropic report JSON", { exact: true }).setInputFiles(file);
}
async function saveMapping(page: Page, evidence: string) {
  await page.getByLabel("Mapping revision evidence reference", { exact: true }).fill(evidence);
  await page.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision", exact: true }).check();
  const response = page.waitForResponse(postTo(mappings));
  await keyboardActivate(page, page.getByRole("button", { name: "Save mapping revision", exact: true }));
  return response;
}

// No dependency on journey.spec.ts: role provisioning is via real management
// APIs, before these personas sign in and receive their live group grants.
// Role tests read a separate immutable API-seeded snapshot, never Admin-test
// output. Like provider-reconciliation's seed(), reuse it on worker restart so
// an Admin failure cannot cascade into duplicate provisioning or missing state.
test.beforeAll(async ({ browser }) => {
  if (existsSync(seedFile)) return;
  const admin = await signIn(browser, "operator");
  try {
    for (const [group, role] of [["omg/platform-auditor", "auditor"], ["omg/platform-user", "user"]]) {
      await create(admin.page, "/platform/oidc/group-mappings", {
        issuer: ISSUER_URL, group_value: group, target_kind: "platform", platform_role: role,
        workspace_id: null, workspace_role: null, enabled: true,
      });
    }
    const scope = await historicalScope(admin.page, "role-fixture");
    const account = "browser-upload-role-fixture";
    const report = JSON.parse(readFileSync(path.resolve("tests/browser/fixtures/anthropic-upload.json"), "utf8").replace("browser-upload-ACCOUNT", account));
    const import_id = await create(admin.page, imports, report);
    const mapping_id = await create(admin.page, mappings, {
      account_id: account, supersedes: null, evidence_reference: "finance:role-fixture-mapping",
      bindings: [{
        provider_workspace_id: "provider-file-workspace", connection_id: scope.connection, deployment_id: null, workspace_id: scope.workspace,
        effective_start: report.request.starting_at, effective_end: report.request.ending_at,
        exclusive: true, non_omg_traffic: false, attested: true, evidence_reference: "finance:role-fixture-attestation", model_mappings: [],
      }],
    });
    const id = await create(admin.page, reconciliations, { import_id, mapping_id });
    const frozen = await read(admin.page, `${reconciliations}/${id}`);
    expect(frozen.snapshot).toMatchObject({ import_id, mapping_id, imported_microusd: "-762111", estimates: { settled_estimate_microusd: "2500", attempts: "1" } });
    writeFileSync(seedFile, JSON.stringify({ id, import_id, mapping_id, account } satisfies Result));
  } finally { await admin.context.close(); }
});

async function historicalScope(page: Page, label: number | string) {
  const me = await read(page, "/me");
  const workspace = await create(page, "/platform/workspaces", { name: `Upload acceptance ${label}`, kind: "team", owner_user_id: me.user.id });
  const connection = await create(page, "/platform/providers", { name: `Upload Anthropic ${label}`, provider: "anthropic", credential_ref: "env:OMG_BROWSER_UNUSED_ANTHROPIC", enabled: false });
  const model = await create(page, "/platform/models", { public_name: `acceptance/upload-${label}`, display_name: "Upload historical fixture", enabled: false, supported_protocols: ["messages"] });
  const deployment = await create(page, "/platform/deployments", { model_id: model, provider_connection_id: connection, upstream_model: "claude-sonnet-4-20250514", enabled: false });
  const key = await create(page, `/workspaces/${workspace}/keys`, { name: "upload historical fixture", expires_in_days: 1, model_ids: null });
  // Guard against accidentally using the coordinator's administrative database.
  const databaseUrl = process.env.OMG_BROWSER_DATABASE_URL;
  expect(databaseUrl).toBeTruthy();
  expect(new URL(databaseUrl!).pathname).toMatch(/^\/omg_browser_/);
  const db = new pg.Client({ connectionString: databaseUrl });
  await db.connect();
  try {
    const execution = randomUUID();
    await db.query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,upstream_model,streamed,state,root_request_id,started_at,input_tokens,output_tokens) VALUES($1,$2,$3,$4,$5,'anthropic','claude-sonnet-4-20250514',false,'succeeded',$1,'2025-08-01T12:00:00Z',100,20)", [execution, workspace, key, deployment, `acceptance/upload-${label}`]);
    await db.query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,actual_microusd,input_tokens,output_tokens,price_tier) VALUES($1,$2,$3,$4,'2025-08-01T12:00:00Z','2025-08-01T12:00:00Z','2025-08-01T00:00:00Z',now(),'settled',0,2500,100,20,'standard')", [execution, workspace, key, deployment]);
    await db.query("INSERT INTO monetary_ledger(id,execution_id,kind,amount_microusd,admitted_at) VALUES($1,$2,'settlement',2500,'2025-08-01T12:00:00Z')", [randomUUID(), execution]);
  } finally { await db.end(); }
  return { workspace, connection };
}

// Counts plus a checksum of every column in stable row order. Read only the
// suite's disposable database, never the coordinator-owned admin database.
async function contentState(tables: readonly string[]) {
  const databaseUrl = process.env.OMG_BROWSER_DATABASE_URL;
  expect(databaseUrl).toBeTruthy();
  expect(new URL(databaseUrl!).pathname).toMatch(/^\/omg_browser_/);
  const db = new pg.Client({ connectionString: databaseUrl });
  await db.connect();
  try {
    await db.query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY");
    const state: Record<string, { count: string; checksum: string }> = {};
    for (const table of tables) {
      // Table identifiers are fixed test constants, never browser input.
      const { rows: [row] } = await db.query(`SELECT count(*)::text AS count, coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]')::text AS content FROM ${table} t`);
      state[table] = { count: row.count, checksum: createHash("sha256").update(row.content).digest("hex") };
    }
    await db.query("COMMIT");
    return state;
  } finally { await db.end(); }
}
const financialTables = ["monetary_ledger", "governance_reservations", "inference_executions"] as const;
const providerTables = ["provider_cost_imports", "provider_cost_lines", "provider_cost_mappings", "provider_cost_bindings", "provider_reconciliations"] as const;

for (const width of [1440, 390]) {
  test(`Admin uploads exact file bytes, refuses invalid evidence, maps with conflict recovery and freezes without changing financial history at ${width}px`, async ({ browser }, info) => {
    const session = await signIn(browser, "operator"), page = session.page;
    try {
      await page.setViewportSize({ width, height: 900 });
      await observeUploadBytes(page);
      const scope = await historicalScope(page, width);
      const financialBefore = await contentState(financialTables);
      for (const table of financialTables) expect(BigInt(financialBefore[table].count), `${table} must not be an empty proof`).toBeGreaterThan(0n);
      const account = `browser-upload-${width}`;
      // Preserve the fixture's whitespace/newline; only substitute its account
      // sentinel. Playwright uploads a real disk FILE, not a JSON object.
      const bytes = Buffer.from(readFileSync(path.resolve("tests/browser/fixtures/anthropic-upload.json"), "utf8").replace("browser-upload-ACCOUNT", account));
      await page.goto(listUrl);
      await keyboardActivate(page, page.getByRole("button", { name: "Upload & manage", exact: true }));
      await expect(page.getByRole("heading", { name: "Upload & manage provider costs", exact: true })).toBeFocused();
      await check(page, `Admin ${width} upload`);

      await uploadFile(page, info, "anthropic-report.json", bytes);
      const uploaded = page.waitForResponse(postTo(imports));
      await keyboardActivate(page, page.getByRole("button", { name: "Upload report", exact: true }));
      const first = await uploaded;
      expect(first.status()).toBe(200);
      const import_id = (await first.json()).id as string;
      await expectUploadBytes(page, bytes);
      const result = page.getByRole("region", { name: "Selected import result", exact: true });
      await expect(result).toContainText(`Import resolved: ${import_id}`);
      await expect(result).toContainText("UTC coverage [2025-08-01T00:00:00+00:00, 2025-08-02T00:00:00+00:00)");
      await expect(result.getByText("-$0.762111", { exact: true })).toBeVisible();
      await expect(result).toContainText("2 lines");
      await expect(result).toHaveAttribute("aria-live", "polite");
      const lines = (await read(page, `${imports}/${import_id}/lines`)).data;
      expect(lines.map((line: { original_amount: string; amount_microusd: string }) => [line.original_amount, line.amount_microusd])).toEqual([["123.78912", "1237891"], ["-200.00015", "-2000002"]]);

      // Idempotent server response, not a fabricated client success/new revision.
      await uploadFile(page, info, "same-report.json", bytes);
      const duplicate = page.waitForResponse(postTo(imports));
      await page.getByRole("button", { name: "Upload report", exact: true }).click();
      const again = await duplicate;
      expect(again.status()).toBe(200);
      expect((await again.json()).id).toBe(import_id);
      await expectUploadBytes(page, bytes);
      await expect(result).toContainText(`Import resolved: ${import_id}`);
      expect((await read(page, imports)).data.filter((r: { account_id: string }) => r.account_id === account)).toHaveLength(1);

      // Duplicate a top-level key. JSON.parse/stringify would erase the duplicate
      // and turn this into an accepted import; the real server must reject it.
      const invalidBytes = Buffer.from(bytes.toString().replace('"supersedes": null,', '"supersedes": null, "supersedes": null,'));
      await uploadFile(page, info, "duplicate-keys.json", invalidBytes);
      const invalid = page.waitForResponse(postTo(imports));
      await page.getByRole("button", { name: "Upload report", exact: true }).click();
      const refusal = await invalid;
      expect(refusal.status()).toBe(422);
      expect((await refusal.json()).error.message).toBe("Invalid provider cost request fields");
      await expectUploadBytes(page, invalidBytes);
      const alert = page.getByRole("alert").filter({ hasText: "Invalid provider cost request fields" });
      await expect(alert).toBeVisible(); await expect(alert).toBeFocused();
      await expect(result).toHaveCount(0);
      await check(page, `Admin ${width} server refusal`);

      let uploadPosts = 0;
      const countPosts = (request: import("@playwright/test").Request) => { if (request.method() === "POST" && new URL(request.url()).pathname === `/api/v1${imports}`) uploadPosts++; };
      page.on("request", countPosts);
      for (const [name, contents, message] of [
        ["empty.json", Buffer.alloc(0), "The report file is empty."],
        ["oversize.json", Buffer.alloc(5 * 1024 * 1024 + 1, 32), "The report exceeds the 5 MiB (5,242,880 bytes) upload limit."],
      ] as const) {
        await uploadFile(page, info, name, contents);
        await page.getByRole("button", { name: "Upload report", exact: true }).click();
        const clientAlert = page.getByRole("alert").filter({ hasText: message });
        await expect(clientAlert).toHaveText(message); await expect(clientAlert).toBeFocused();
        expect(uploadPosts, "client validation must not send evidence").toBe(0);
      }
      page.off("request", countPosts);
      await page.getByLabel("Existing import", { exact: true }).selectOption(import_id);
      await page.getByRole("tab", { name: "Mappings", exact: true }).click();
      await expect(page.getByText("Base mapping revision: None (first revision)", { exact: true })).toBeVisible();
      await page.getByLabel("Gateway connection", { exact: true }).selectOption(scope.connection);
      await page.getByLabel("OMG workspace ID (optional)", { exact: true }).fill(scope.workspace);
      await page.getByLabel("Binding evidence reference", { exact: true }).fill("finance:unsaved-attestation");
      await page.getByRole("checkbox", { name: "Exclusive OMG workspace use for this interval", exact: true }).check();
      await page.getByRole("checkbox", { name: "I attest this binding and its effective coverage", exact: true }).check();
      // A real tab round-trip preserves *unapplied* fields and attestation too.
      await keyboardActivate(page, page.getByRole("tab", { name: "Upload report", exact: true }));
      await keyboardActivate(page, page.getByRole("tab", { name: "Mappings", exact: true }));
      await expect(page.getByLabel("Binding evidence reference", { exact: true })).toHaveValue("finance:unsaved-attestation");
      await expect(page.getByLabel("Gateway connection", { exact: true })).toHaveValue(scope.connection);
      await expect(page.getByLabel("OMG workspace ID (optional)", { exact: true })).toHaveValue(scope.workspace);
      await expect(page.getByRole("checkbox", { name: "I attest this binding and its effective coverage", exact: true })).toBeChecked();
      await page.getByRole("button", { name: "Apply binding to draft", exact: true }).click();
      const coverage = page.getByRole("table", { name: "Provider dimensions", exact: true }).getByRole("row").filter({ hasText: "provider-file-workspace" });
      await expect(coverage.locator('[data-label="Draft period coverage"]')).toHaveText("Mapped");
      const savedResponse = await saveMapping(page, "finance:first-ui-revision");
      expect(savedResponse.status()).toBe(200);
      const firstMapping = (await savedResponse.json()).id as string;
      await expect(page.getByRole("status").filter({ hasText: `Saved mapping revision: ${firstMapping}` })).toBeVisible();
      await check(page, `Admin ${width} mapped`);

      const retained = await read(page, `${mappings}/${firstMapping}`);
      expect(retained.bindings[0]).toMatchObject({ provider_workspace_id: "provider-file-workspace", connection_id: scope.connection, workspace_id: scope.workspace, attested: true, exclusive: true });
      const { binding_number: _number, ...binding } = retained.bindings[0];
      const revision = (supersedes: string, evidence: string) => ({ account_id: account, supersedes, evidence_reference: evidence, bindings: [binding] });
      // Background API change leaves the browser holding its real stale base.
      const superseding = await create(page, mappings, revision(firstMapping, "finance:concurrent-admin"));
      const stale = await saveMapping(page, "finance:stale-do-not-overwrite");
      expect(stale.status()).toBe(409);
      const conflictMessage = "Overlapping coverage or invalid supersession; select the current exact revision";
      expect((await stale.json()).error.message).toBe(conflictMessage);
      const conflict = page.getByRole("alert").filter({ hasText: conflictMessage });
      await expect(conflict).toHaveText(conflictMessage); await expect(conflict).toBeFocused();
      await expect(page.getByRole("button", { name: "Save mapping revision", exact: true })).toBeDisabled();
      await expect(page.getByLabel("Mapping revision evidence reference", { exact: true })).toHaveValue("finance:stale-do-not-overwrite");
      expect((await read(page, mappings)).data.filter((r: { account_id: string }) => r.account_id === account)).toHaveLength(2);
      expect((await read(page, `${mappings}/${superseding}`)).evidence_reference).toBe("finance:concurrent-admin");
      await check(page, `Admin ${width} mapping conflict`);
      await page.getByRole("button", { name: "Reload latest mapping and discard draft", exact: true }).click();
      await expect(page.getByText(`Base mapping revision: ${superseding}`, { exact: true })).toBeVisible();
      await expect(page.getByLabel("Mapping revision evidence reference", { exact: true })).toHaveValue("");
      await expect(conflict).toHaveCount(0);
      await expect(coverage.locator('[data-label="Draft period coverage"]')).toHaveText("Mapped");
      const recovered = await saveMapping(page, "finance:reviewed-after-reload");
      expect(recovered.status()).toBe(200);
      const mapping_id = (await recovered.json()).id as string;
      expect((await read(page, `${mappings}/${mapping_id}`)).supersedes).toBe(superseding);

      await page.getByRole("tab", { name: "Freeze snapshot", exact: true }).click();
      await expect(page.getByText(`Import revision: ${import_id} · Mapping revision: ${mapping_id}`, { exact: true })).toBeVisible();
      await check(page, `Admin ${width} freeze`);
      const freezing = page.waitForResponse(postTo(reconciliations));
      await keyboardActivate(page, page.getByRole("button", { name: "Freeze reconciliation", exact: true }));
      const frozenResponse = await freezing;
      expect(frozenResponse.status()).toBe(200);
      const id = (await frozenResponse.json()).id as string;
      await expect(page.getByRole("status").filter({ hasText: `Snapshot frozen: ${id}` })).toBeVisible();
      await expect(page.getByRole("button", { name: "Freeze reconciliation", exact: true })).toBeDisabled();
      await keyboardActivate(page, page.getByRole("button", { name: `Open snapshot ${id}`, exact: true }));
      await expect(page).toHaveURL(new RegExp(`record=${id}`));
      await expect(page.getByRole("heading", { name: `Reconciliation ${id}`, exact: true })).toBeFocused();
      await expect(page.getByRole("group", { name: "Snapshot-wide imported charges", exact: true })).toContainText("-$0.762111");
      await expect(page.getByRole("group", { name: "Snapshot-wide configured-rate estimates", exact: true })).toContainText("$0.0025");
      const credit = page.getByRole("table", { name: "Provider comparison lines", exact: true }).getByRole("row").filter({ hasText: "Credit" });
      await expect(credit.locator('[data-label="Imported charges"]')).toHaveText("-$2.000002");
      await check(page, `Admin ${width} frozen PR1 detail`);
      const frozen = await read(page, `${reconciliations}/${id}`);
      expect(frozen.snapshot).toMatchObject({ import_id, mapping_id, imported_microusd: "-762111", estimates: { settled_estimate_microusd: "2500", attempts: "1" } });
      expect(await contentState(financialTables), "upload, mapping and freeze must preserve every financial row and column").toEqual(financialBefore);
      await create(page, mappings, { account_id: account, supersedes: mapping_id, evidence_reference: "finance:post-freeze-change", bindings: [] });
      expect(await read(page, `${reconciliations}/${id}`)).toEqual(frozen);
      await page.getByRole("tab", { name: "Source & provenance", exact: true }).click();
      await expect(page.getByRole("table", { name: "Frozen mapping provenance (shared scopes only)", exact: true })).toContainText(scope.connection);
    } finally { await session.context.close(); }
  });

  test(`Admin sees overlapping-import refusal and explicitly supersedes exact coverage at ${width}px`, async ({ browser }, info) => {
    const session = await signIn(browser, "operator"), page = session.page;
    try {
      await page.setViewportSize({ width, height: 900 });
      await observeUploadBytes(page);
      const account = `overlap-${width}-${randomUUID()}`;
      const original = JSON.parse(readFileSync(path.resolve("tests/browser/fixtures/anthropic-upload.json"), "utf8"));
      original.account_id = account;
      await page.goto(listUrl);
      await page.getByRole("button", { name: "Upload & manage", exact: true }).click();
      const send = async (name: string, report: unknown) => {
        await uploadFile(page, info, name, Buffer.from(JSON.stringify(report)));
        const response = page.waitForResponse(postTo(imports));
        await page.getByRole("button", { name: "Upload report", exact: true }).click();
        return response;
      };
      const first = await send("original.json", original);
      expect(first.status()).toBe(200);
      const firstId = (await first.json()).id as string;
      await expect(page.getByRole("region", { name: "Selected import result", exact: true })).toContainText(`Import resolved: ${firstId}`);
      const before = await contentState(providerTables);
      const changed = structuredClone(original);
      changed.pages[0].data[0].results[0].amount = "124";
      const message = "Overlapping coverage or invalid supersession; select the current exact revision";
      const refused = async (name: string, report: unknown) => {
        const response = await send(name, report);
        expect(response.status()).toBe(409);
        expect((await response.json()).error.message).toBe(message);
        const alert = page.getByRole("alert").filter({ hasText: message });
        await expect(alert).toHaveText(message); await expect(alert).toBeFocused();
        await expect(page.getByRole("region", { name: "Selected import result", exact: true })).toHaveCount(0);
        expect(await contentState(providerTables)).toEqual(before);
        await check(page, `Admin ${width} ${name} conflict`);
      };
      await refused("changed-without-supersession.json", changed);
      const overlap = structuredClone(changed);
      overlap.supersedes = firstId;
      overlap.request.ending_at = "2025-08-03T00:00:00Z";
      const next = structuredClone(overlap.pages[0].data[0]);
      next.starting_at = "2025-08-02T00:00:00Z"; next.ending_at = overlap.request.ending_at;
      overlap.pages[0].data.push(next);
      await refused("partial-overlap.json", overlap);
      changed.supersedes = firstId;
      const superseded = await send("explicit-exact-supersession.json", changed);
      expect(superseded.status()).toBe(200);
      const secondId = (await superseded.json()).id as string;
      expect(secondId).not.toBe(firstId);
      const result = page.getByRole("region", { name: "Selected import result", exact: true });
      await expect(result).toContainText(`Import resolved: ${secondId}`);
      await expect(result).toContainText(`Supersedes import: ${firstId}`);
      expect((await read(page, `${imports}/${secondId}`)).supersedes).toBe(firstId);
      expect((await read(page, imports)).data.filter((r: { account_id: string }) => r.account_id === account)).toHaveLength(2);
      await check(page, `Admin ${width} explicit import supersession`);
    } finally { await session.context.close(); }
  });

  for (const stage of ["upload", "mapping", "freeze"] as const) {
    test(`Mid-session Admin demotion refuses ${stage}, removes writes and persists nothing at ${width}px`, async ({ browser }, info) => {
      const operator = await signIn(browser, "operator"), actor = await signIn(browser, "alex"), page = actor.page;
      let roles: string | undefined, active: (() => Promise<string[]>) | undefined, initial: string[] | undefined;
      const added: string[] = [];
      try {
        await page.setViewportSize({ width, height: 900 });
        const me = await read(page, "/me");
        roles = `/platform/users/${me.user.id}/roles`;
        // Active grants of every source. DELETE revokes a role across sources,
        // so this test only touches roles alex holds from NO source beforehand,
        // and cleanup must restore the exact initial role/source set.
        active = async () => ((await read(operator.page, `/platform/users/${me.user.id}`)).role_grants as { role: string; source: string; revoked_at: string | null }[])
          .filter(g => g.revoked_at === null).map(g => `${g.role}:${g.source}`).sort();
        initial = await active();
        const held = (role: string) => initial!.some(g => g.startsWith(`${role}:`));
        expect(held("admin"), "alex must start without Admin authority from any source").toBe(false);
        // Operator stays the safety Admin; alex gets Admin only from this test.
        for (const role of ["user", "admin"]) {
          if (held(role)) continue;
          expect((await api(operator.page, "POST", roles, { role })).status()).toBe(200);
          added.push(role);
        }
        const account = `demotion-${stage}-${width}-${randomUUID()}`;
        const report = JSON.parse(readFileSync(path.resolve("tests/browser/fixtures/anthropic-upload.json"), "utf8"));
        report.account_id = account;
        let importId: string | undefined;
        if (stage !== "upload") importId = await create(operator.page, imports, report);
        if (stage === "freeze") await create(operator.page, mappings, { account_id: account, supersedes: null, evidence_reference: "finance:demotion-base", bindings: [] });
        await page.goto(listUrl);
        await page.getByRole("button", { name: "Upload & manage", exact: true }).click();
        if (stage === "upload") await uploadFile(page, info, "demoted-upload.json", Buffer.from(JSON.stringify(report)));
        else {
          await page.getByLabel("Existing import", { exact: true }).selectOption(importId!);
          await page.getByRole("tab", { name: stage === "mapping" ? "Mappings" : "Freeze snapshot", exact: true }).click();
          if (stage === "mapping") {
            await page.getByLabel("Mapping revision evidence reference", { exact: true }).fill("finance:must-not-persist");
            await page.getByRole("checkbox", { name: "I confirm the whole replacement mapping revision", exact: true }).check();
          }
        }
        const button = page.getByRole("button", { name: stage === "upload" ? "Upload report" : stage === "mapping" ? "Save mapping revision" : "Freeze reconciliation", exact: true });
        await expect(button).toBeEnabled();
        await check(page, `Admin ${width} before ${stage} demotion`);
        const before = await contentState(providerTables);
        // A separate authenticated Admin performs a REAL management mutation
        // while the stale editor remains open. Do not reload/probe actor access
        // before its next write; that write must reach Rust and be refused.
        expect((await api(operator.page, "DELETE", `${roles}/admin`)).status()).toBe(200);
        await page.evaluate(() => {
          Reflect.set(window, "fabricatedProviderSuccess", false);
          new MutationObserver(() => {
            if (/Import resolved:|Saved mapping revision:|Snapshot frozen:/.test(document.body.textContent ?? "")) Reflect.set(window, "fabricatedProviderSuccess", true);
          }).observe(document.body, { subtree: true, childList: true, characterData: true });
        });
        const denied = page.waitForResponse(postTo(stage === "upload" ? imports : stage === "mapping" ? mappings : reconciliations));
        const refreshed = page.waitForResponse(r => new URL(r.url()).pathname === "/api/v1/me" && r.request().method() === "GET" && r.status() === 200);
        await button.click();
        expect((await denied).status()).toBe(403);
        await expect((await refreshed).json()).resolves.toMatchObject({ user: { platform_role: "user" }, capabilities: { platform_write: false } });
        await expect(page).toHaveURL("/home");
        await expect(page.getByRole("heading", { name: "Welcome, Alex", exact: true })).toBeVisible();
        await expect(page.getByRole("heading", { name: "Upload & manage provider costs", exact: true })).toHaveCount(0);
        await expect(page.getByRole("button", { name: /upload|mapping|freeze/i })).toHaveCount(0);
        await expect(page.locator('input[type="file"]')).toHaveCount(0);
        expect(await page.evaluate(() => Reflect.get(window, "fabricatedProviderSuccess"))).toBe(false);
        expect(await contentState(providerTables), "403 must not append evidence, bindings or snapshots").toEqual(before);
        await check(page, `User ${width} after ${stage} demotion`);
      } finally {
        // Revoke only roles this test added (admin first; demotion may already
        // have removed it), then prove alex's role/source set is exactly restored.
        try {
          if (roles && active && initial) {
            for (const role of [...added].reverse()) {
              if ((await active()).some(g => g.startsWith(`${role}:`))) expect((await api(operator.page, "DELETE", `${roles}/${role}`)).status()).toBe(200);
            }
            expect(await active(), "alex's active platform-role grants must equal the initial set").toEqual(initial);
          }
        } finally { await actor.context.close(); await operator.context.close(); }
      }
    });
  }

  test(`Auditor reads frozen provider costs without upload, mapping or freeze authority at ${width}px`, async ({ browser }) => {
    const ids: Result = JSON.parse(readFileSync(seedFile, "utf8"));
    const session = await signIn(browser, "auditor"), page = session.page;
    try {
      await page.setViewportSize({ width, height: 900 });
      expect((await read(page, "/me")).user.platform_role).toBe("auditor");
      await page.goto(listUrl);
      await expect(page.getByRole("main").getByText("Read-only", { exact: true })).toBeVisible();
      await expect(page.getByRole("button", { name: /upload|mapping|freeze/i })).toHaveCount(0);
      await expect(page.locator('input[type="file"]')).toHaveCount(0);
      await expect(page.getByRole("textbox")).toHaveCount(0);
      await check(page, `Auditor ${width} list`);
      await keyboardActivate(page, page.getByRole("button", { name: new RegExp(`^Open reconciliation ${ids.id}:`) }));
      await expect(page.getByRole("heading", { name: `Reconciliation ${ids.id}`, exact: true })).toBeFocused();
      await expect(page.getByRole("group", { name: "Snapshot-wide imported charges", exact: true })).toContainText("-$0.762111");
      await expect(page.getByRole("button", { name: /upload|mapping|freeze/i })).toHaveCount(0);
      await check(page, `Auditor ${width} detail`);
      for (const [endpoint, body] of [[imports, {}], [mappings, {}], [reconciliations, { import_id: ids.import_id, mapping_id: ids.mapping_id }]] as const) {
        expect((await api(page, "POST", endpoint, body)).status(), `Auditor POST ${endpoint}`).toBe(403);
      }
    } finally { await session.context.close(); }
  });

  test(`Plain user cannot enter provider-cost pages or call their APIs at ${width}px`, async ({ browser }) => {
    const ids: Result = JSON.parse(readFileSync(seedFile, "utf8"));
    const session = await signIn(browser, "blair"), page = session.page;
    try {
      await page.setViewportSize({ width, height: 900 });
      expect((await read(page, "/me")).user.platform_role).toBe("user");
      for (const url of [listUrl, `${listUrl}&record=${ids.id}`]) {
        await page.goto(url);
        await expect(page).toHaveURL("/home");
        await expect(page.getByRole("heading", { name: "Welcome, Blair", exact: true })).toBeVisible();
        await expect(page.getByRole("button", { name: /upload|mapping|freeze/i })).toHaveCount(0);
        await expect(page.getByRole("tab", { name: "Provider reconciliation", exact: true })).toHaveCount(0);
        await check(page, `Plain user ${width} refused`);
      }
      for (const endpoint of [imports, mappings, reconciliations, `${reconciliations}/${ids.id}`]) expect((await api(page, "GET", endpoint)).status()).toBe(403);
      for (const endpoint of [imports, mappings, reconciliations]) expect((await api(page, "POST", endpoint, {})).status()).toBe(403);
    } finally { await session.context.close(); }
  });
}
