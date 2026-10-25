import { expect, test, type Browser, type Page } from "@playwright/test";
import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import pg from "pg";
import { api, expectAccessible, expectNoCspViolations, expectNoHorizontalScroll, expectTableFits, signIn } from "./helpers";

// Holds tab (#101 slice B). Aged unknown holds are seeded straight into the disposable browser database (the same
// rows the gateway writes when a lease expires with no usage), then every persona reads them through the real
// gateway API and SPA. Creating keys is setup only: never retain credentials in traces or images.
test.use({ trace: "off", screenshot: "off", video: "off" });
test.afterEach(expectNoCspViolations);

const shots = path.resolve("target/browser-tests/holds");
const BIG = "9007199254740993"; // 2^53 + 1: any Number math would print ...992.
type Seeded = { team: string; paging: string; personal: string; roots: Record<"a" | "b" | "c" | "d" | "fresh" | "settle", string>; executions: Record<"a" | "b" | "c" | "d" | "settle", string> };
const SETTLE_FLOOR = "123456";
const seedFile = path.resolve("target/browser-tests/state/holds-seed.json");
let seeded: Promise<Seeded> | undefined;

async function seed(browser: Browser): Promise<Seeded> {
  const db = new pg.Client({ connectionString: process.env.OMG_BROWSER_DATABASE_URL });
  await db.connect();
  const operator = await signIn(browser, "operator"), alex = await signIn(browser, "alex"), blair = await signIn(browser, "blair");
  try {
    const alexId = (await (await api(alex.page, "GET", "/me")).json()), blairId = (await (await api(blair.page, "GET", "/me")).json());
    const personal = alexId.workspaces.find((w: { kind: string }) => w.kind === "personal").id as string;
    const team = async (name: string) => {
      const created = await api(operator.page, "POST", "/platform/workspaces", { kind: "team", name, owner_user_id: alexId.user.id });
      expect(created.status()).toBe(200);
      return (await created.json()).id as string;
    };
    const holds = await team("Holds acceptance"), paging = await team("Holds paging");
    expect((await api(operator.page, "POST", `/platform/workspaces/${holds}/members`, { user_id: blairId.user.id, role: "member" })).status()).toBe(200);
    const key = async (page: Page, ws: string, name: string) => {
      const created = await api(page, "POST", `/workspaces/${ws}/keys`, { name, expires_in_days: 1, model_ids: null });
      expect(created.status()).toBe(200);
      return (await created.json()).id as string;
    };
    const keys = { blair: await key(blair.page, holds, "holds-blair"), alex: await key(alex.page, holds, "holds-alex"), alexPaging: await key(alex.page, paging, "holds-paging"), alexPersonal: await key(alex.page, personal, "holds-personal") };
    const { rows: [price] } = await db.query("SELECT id, deployment_id FROM deployment_prices ORDER BY created_at DESC LIMIT 1");
    expect(price, "the journey creates a priced route first").toBeDefined();
    // One hold = execution + unknown reservation + the unknown ledger entry that starts the age clock.
    const hold = async (ws: string, apiKey: string, model: string, floor: string | null, hours: number) => {
      const id = randomUUID();
      await db.query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,attempt_number,started_at) VALUES($1,$2,$3,$4,$5,'openai',false,'failed',$1,1,clock_timestamp()-make_interval(hours=>$6))", [id, ws, apiKey, price.deployment_id, model, hours]);
      await db.query("INSERT INTO governance_reservations(execution_id,workspace_id,api_key_id,deployment_id,admitted_at,minute_start,month_start,lease_expires_at,state,held_microusd,unbounded_cost,price_id) SELECT id,workspace_id,api_key_id,deployment_id,started_at,date_trunc('minute',started_at,'UTC'),date_trunc('month',started_at,'UTC'),started_at,'unknown',$2::bigint,$2::bigint IS NULL,$3 FROM inference_executions WHERE id=$1", [id, floor, price.id]);
      await db.query("INSERT INTO monetary_ledger(id,execution_id,admitted_at,kind,created_at) SELECT $1,execution_id,admitted_at,'unknown',admitted_at FROM governance_reservations WHERE execution_id=$2", [randomUUID(), id]);
      return id;
    };
    const a = await hold(holds, keys.blair, "holds-a/bounded", BIG, 96), b = await hold(holds, keys.blair, "holds-b/unbounded", null, 216);
    const c = await hold(holds, keys.alex, "holds-c/admin-only", "1500", 80), fresh = await hold(holds, keys.alex, "holds-e/fresh", "7", 2);
    const settle = await hold(holds, keys.alex, "holds-r/reconcile", SETTLE_FLOOR, 90);
    const d = await hold(personal, keys.alexPersonal, "holds-d/personal", "42", 120);
    for (let i = 0; i < 52; i++) await hold(paging, keys.alexPaging, `holds-p/${String(i).padStart(2, "0")}`, String(100 + i), 25 + i);
    return { team: holds, paging, personal, roots: { a, b, c, d, fresh, settle }, executions: { a, b, c, d, settle } };
  } finally {
    await db.end();
    for (const session of [operator, alex, blair]) await session.context.close();
  }
}
// Playwright starts a fresh worker after a failed test: reuse the first seeding (the state directory is reset per run)
// instead of adding duplicate holds.
async function load(browser: Browser): Promise<Seeded> {
  if (existsSync(seedFile)) return JSON.parse(readFileSync(seedFile, "utf8"));
  const ids = await seed(browser);
  mkdirSync(path.dirname(seedFile), { recursive: true }); writeFileSync(seedFile, JSON.stringify(ids));
  return ids;
}
const ensure = (browser: Browser) => (seeded ??= load(browser));
const queue = (page: Page) => page.getByRole("table", { name: "Aged unknown holds" });
const row = (page: Page, text: string) => queue(page).getByRole("row").filter({ hasText: text });
async function open(page: Page, href: string) {
  await page.goto(href);
  await expect(queue(page)).toBeVisible();
  // Rows, not the loading skeleton.
  await expect(queue(page).getByRole("row")).not.toHaveCount(1);
  await expectCellsDoNotOverlap(page, href);
}
/**
 * No two cells of a row, and no badge and its neighbour, draw their text over each other. The page-level overflow check
 * cannot see this: a long unbreakable value spills out of its card cell without widening the document.
 */
async function expectCellsDoNotOverlap(page: Page, label: string) {
  const problems = await queue(page).evaluate(table => {
    const box = (node: Element) => { const r = document.createRange(); r.selectNodeContents(node); const b = r.getBoundingClientRect(); return { left: b.left, right: b.right, top: b.top, bottom: b.bottom, width: b.width, height: b.height }; };
    const hit = (a: ReturnType<typeof box>, b: ReturnType<typeof box>) => a.width > 0 && b.width > 0 && a.height > 0 && b.height > 0 && a.left < b.right - 0.5 && b.left < a.right - 0.5 && a.top < b.bottom - 0.5 && b.top < a.bottom - 0.5;
    const out: string[] = [];
    for (const [index, tr] of [...table.querySelectorAll("tbody > tr")].entries()) {
      const cells = [...tr.children].filter(c => c.textContent?.trim());
      const boxes = cells.map(c => ({ name: c.getAttribute("data-label") ?? c.textContent!.slice(0, 12), box: box(c), cell: c.getBoundingClientRect() }));
      for (const [i, a] of boxes.entries()) {
        // Text must stay inside its own cell.
        if (a.box.right > a.cell.right + 0.5 || a.box.left < a.cell.left - 0.5) out.push(`row ${index}: "${a.name}" text spills out of its cell`);
        for (const b of boxes.slice(i + 1)) if (hit(a.box, b.box)) out.push(`row ${index}: "${a.name}" overlaps "${b.name}"`);
      }
      // The badge never sits on top of its neighbouring text.
      for (const badge of tr.querySelectorAll("span[data-tone]")) for (const other of boxes) if (!other.name.includes("Floor") && other.name !== "Unknown for" && hit(box(badge), other.box)) out.push(`row ${index}: badge overlaps "${other.name}"`);
    }
    return out;
  });
  expect(problems, `${label}: cell contents must not overlap or spill`).toEqual([]);
}
async function shot(page: Page, name: string) { mkdirSync(shots, { recursive: true }); await page.locator("main").screenshot({ path: path.join(shots, `${name}.png`) }); }

for (const width of [1440, 390]) {
  test.describe(`Holds tab at ${width}px`, () => {
    test("Platform Admin: Team/Project rows, exact floors, one identity-free personal total, keyboard reconcile entry", async ({ browser }) => {
      const ids = await ensure(browser), operator = await signIn(browser, "operator");
      const page = operator.page;
      try {
        await page.setViewportSize({ width, height: 900 });
        // Reach the tab with the keyboard from the Usage & costs page.
        await page.goto("/admin/costs");
        const tab = page.getByRole("tab", { name: "Holds" });
        await tab.focus(); await page.keyboard.press("Enter");
        await expect(page).toHaveURL(/\/admin\/costs\?tab=holds$/);
        await expect(queue(page)).toBeVisible();
        await expect(queue(page).getByRole("row")).not.toHaveCount(1);
        await expectCellsDoNotOverlap(page, `admin ${width}`);
        const a = row(page, "holds-a/bounded");
        await expect(a).toContainText("At least $9,007,199,254.740993");
        await expect(a).toContainText("4 days"); await expect(a).toContainText("Holds acceptance");
        await expect(a).not.toContainText("Unbounded");
        const b = row(page, "holds-b/unbounded");
        await expect(b).toContainText("No floor known"); await expect(b.getByText("Unbounded", { exact: true })).toBeVisible(); await expect(b).toContainText("9 days");
        await expect(row(page, "holds-c/admin-only")).toContainText("At least $0.0015");
        await expect(page.getByText("holds-e/fresh")).toHaveCount(0); // 2 hours is not aged
        await expect(a.getByRole("link", { name: /^Request / })).toHaveAttribute("href", `/admin/logs/${ids.roots.a}`);
        // Floor is not an upper bound; nothing is refunded.
        await expect(page.getByText(/not an upper bound/).first()).toBeVisible(); await expect(page.getByText(/Nothing is refunded/)).toBeVisible();
        // Personal workspaces: one combined total, no identity, no drill-down.
        const personal = page.getByRole("group", { name: "Personal workspaces, combined" });
        await expect(personal).toContainText("1 hold"); await expect(personal).toContainText("At least $0.000042");
        await expect(personal.getByRole("link")).toHaveCount(0); await expect(personal.getByRole("button")).toHaveCount(0);
        await expect(page.getByText("holds-d/personal")).toHaveCount(0);
        const wire = JSON.stringify(await (await api(page, "GET", "/platform/holds?limit=200")).json());
        for (const secret of [ids.personal, ids.roots.d, ids.executions.d, "holds-d/personal", "holds-personal", "alex@"]) expect(wire).not.toContain(secret);
        await expectNoHorizontalScroll(page);
        if (width === 1440) await expectTableFits(page, "Aged unknown holds");
        await expectAccessible(page, `admin holds ${width}`);
        await shot(page, `admin-${width}`);
        // The existing reconcile dialog opens from the keyboard and Escape returns focus to its row button.
        const reconcile = a.getByRole("button", { name: /^Reconcile request / });
        await reconcile.focus(); await page.keyboard.press("Enter");
        const dialog = page.getByRole("dialog", { name: "Reconcile pinned usage" });
        await expect(dialog).toBeVisible();
        await expect(dialog.getByLabel(/Authoritative evidence reference/)).toBeVisible();
        await expectAccessible(page, `admin holds reconcile dialog ${width}`);
        await page.keyboard.press("Escape");
        await expect(dialog).toHaveCount(0);
        await expect(reconcile).toBeFocused();
      } finally { await operator.context.close(); }
    });

    test("Auditor reads the queue but is never offered Reconcile", async ({ browser }) => {
      const ids = await ensure(browser), auditor = await signIn(browser, "auditor");
      const page = auditor.page;
      try {
        await page.setViewportSize({ width, height: 900 });
        await open(page, "/admin/costs?tab=holds");
        await expect(row(page, "holds-a/bounded")).toContainText("At least $9,007,199,254.740993");
        await expect(row(page, "holds-a/bounded").getByRole("link", { name: /^Request / })).toHaveAttribute("href", `/admin/logs/${ids.roots.a}`);
        await expect(page.getByRole("button", { name: /^Reconcile/ })).toHaveCount(0);
        await expect(page.getByRole("group", { name: "Personal workspaces, combined" })).toBeVisible();
        await expectNoHorizontalScroll(page);
        await expectAccessible(page, `auditor holds ${width}`);
      } finally { await auditor.context.close(); }
    });

    test("workspace admin sees the whole workspace and opens a request; no Reconcile", async ({ browser }) => {
      const ids = await ensure(browser), alex = await signIn(browser, "alex");
      const page = alex.page;
      try {
        await page.setViewportSize({ width, height: 900 });
        await open(page, `/workspaces/${ids.team}/costs?tab=holds`);
        for (const model of ["holds-a/bounded", "holds-b/unbounded", "holds-c/admin-only"]) await expect(row(page, model)).toBeVisible();
        await expect(page.getByText("holds-e/fresh")).toHaveCount(0); await expect(page.getByText("holds-d/personal")).toHaveCount(0);
        await expect(page.getByText("Everyone in Holds acceptance").first()).toBeVisible();
        await expect(page.getByRole("button", { name: /^Reconcile/ })).toHaveCount(0);
        await expect(page.getByRole("group", { name: "Personal workspaces, combined" })).toHaveCount(0);
        await expect(page.getByRole("combobox", { name: /Period/ })).toHaveCount(0);
        await expectNoHorizontalScroll(page);
        if (width === 1440) await expectTableFits(page, "Aged unknown holds");
        await expectAccessible(page, `workspace admin holds ${width}`);
        await shot(page, `workspace-admin-${width}`);
        await row(page, "holds-a/bounded").getByRole("link", { name: /^Request / }).click();
        await expect(page).toHaveURL(new RegExp(`/workspaces/${ids.team}/requests/${ids.roots.a}$`));
        await expect(page.getByRole("alert")).toHaveCount(0);
      } finally { await alex.context.close(); }
    });

    test("member sees only their own keys' holds and the page says so", async ({ browser }) => {
      const ids = await ensure(browser), blair = await signIn(browser, "blair");
      const page = blair.page;
      try {
        await page.setViewportSize({ width, height: 900 });
        await open(page, `/workspaces/${ids.team}/costs?tab=holds`);
        await expect(row(page, "holds-a/bounded")).toBeVisible(); await expect(row(page, "holds-b/unbounded")).toBeVisible();
        await expect(page.getByText("holds-c/admin-only")).toHaveCount(0);
        await expect(page.getByText("Only holds from your own keys in Holds acceptance").first()).toBeVisible();
        await expect(page.getByText("Everyone in Holds acceptance")).toHaveCount(0);
        await expect(page.getByRole("button", { name: /^Reconcile/ })).toHaveCount(0);
        // Aggregates are computed after the visibility filter: the member's totals count only their two holds.
        const wire = await (await api(page, "GET", `/workspaces/${ids.team}/holds`)).json();
        expect(wire.data).toHaveLength(2); expect(wire.totals.count).toBe("2"); expect(wire.personal).toBeNull();
        expect(JSON.stringify(wire)).not.toContain("holds-c/admin-only");
        await expectNoHorizontalScroll(page);
        await expectAccessible(page, `member holds ${width}`);
        await shot(page, `member-${width}`);
      } finally { await blair.context.close(); }
    });

    test("personal owner sees their own holds, never another person's", async ({ browser }) => {
      const ids = await ensure(browser), alex = await signIn(browser, "alex"), blair = await signIn(browser, "blair");
      const page = alex.page;
      try {
        await page.setViewportSize({ width, height: 900 });
        await open(page, `/workspaces/${ids.personal}/costs?tab=holds`);
        await expect(row(page, "holds-d/personal")).toContainText("At least $0.000042");
        await expect(row(page, "holds-d/personal")).toContainText("5 days");
        await expect(page.getByText("Your holds").first()).toBeVisible();
        await expect(row(page, "holds-d/personal").getByRole("link", { name: /^Request / })).toHaveAttribute("href", `/workspaces/${ids.personal}/requests/${ids.roots.d}`);
        await expect(page.getByRole("button", { name: /^Reconcile/ })).toHaveCount(0);
        await expectNoHorizontalScroll(page);
        await expectAccessible(page, `personal holds ${width}`);
        await shot(page, `personal-${width}`);
        // Another person's personal workspace answers like an unknown one (#282), also to platform roles.
        expect((await api(blair.page, "GET", `/workspaces/${ids.personal}/holds`)).status()).toBe(404);
      } finally { for (const s of [alex, blair]) await s.context.close(); }
    });
  });
}

test("Platform Admin settles an aged unknown hold through the real reconcile dialog and it leaves the queue", async ({ browser }) => {
  const ids = await ensure(browser), operator = await signIn(browser, "operator");
  const page = operator.page, db = new pg.Client({ connectionString: process.env.OMG_BROWSER_DATABASE_URL });
  await db.connect();
  try {
    const snapshot = async () => { const body = await (await api(page, "GET", "/platform/holds?limit=200")).json(); return { totals: body.totals, rows: body.data.map((r: { execution_id: string }) => r.execution_id) as string[] }; };
    const before = await snapshot();
    expect(before.rows).toContain(ids.executions.settle);
    await open(page, "/admin/costs?tab=holds");
    const target = row(page, "holds-r/reconcile");
    await expect(target).toContainText("At least $0.123456");
    const reconcile = target.getByRole("button", { name: /^Reconcile request / });
    await reconcile.focus(); await page.keyboard.press("Enter");
    const dialog = page.getByRole("dialog", { name: "Reconcile pinned usage" });
    await expect(dialog).toBeVisible();
    // Authoritative usage for the priced route, as the existing flow requires (v2/v3 prices need the explicit billing counters).
    const { rows: [price] } = await db.query("SELECT p.pricing_version FROM governance_reservations r JOIN deployment_prices p ON p.id=r.price_id WHERE r.execution_id=$1", [ids.executions.settle]);
    expect([1, 2, 3], "the journey's price is a token price").toContain(price.pricing_version);
    const fill = async (label: string | RegExp, value: string) => dialog.getByLabel(label).fill(value);
    await fill(/Original raw input tokens/, "100"); await fill(/Original raw output tokens/, "20");
    if (price.pricing_version >= 2) for (const [label, value] of [["Total input · inclusive", "100"], ["Uncached input", "100"], ["Cache read", "0"], ["Cache write · overlapping aggregate", "0"], ["Cache write · default", "0"], ["Cache write · 5-minute", "0"], ["Cache write · 1-hour", "0"]]) await fill(label, value);
    await fill(/Authoritative evidence reference/, "browser-acceptance-usage-ref-1");
    await dialog.getByRole("button", { name: "Reconcile usage" }).click();
    await expect(dialog).toHaveCount(0);
    // The refreshed queue no longer lists the hold; the totals drop by exactly its count and floor.
    await expect(target).toHaveCount(0);
    const after = await snapshot();
    expect(after.rows).not.toContain(ids.executions.settle);
    expect(BigInt(before.totals.count) - BigInt(after.totals.count)).toBe(1n);
    expect(BigInt(before.totals.known_floor_microusd) - BigInt(after.totals.known_floor_microusd)).toBe(BigInt(SETTLE_FLOOR));
    await expect(page.getByRole("group", { name: "Aged unknown hold totals" })).toContainText(BigInt(after.totals.count).toLocaleString("en-US"));
    // Other holds are untouched.
    await expect(row(page, "holds-a/bounded")).toBeVisible();
    // The accounting is real: settled with a final cost and a reconciliation ledger entry, and the evidence is the only input kept.
    const { rows: [settled] } = await db.query("SELECT state, actual_microusd::text actual, input_tokens::text input, output_tokens::text output FROM governance_reservations WHERE execution_id=$1", [ids.executions.settle]);
    expect(settled).toMatchObject({ state: "settled", input: "100", output: "20" }); expect(BigInt(settled.actual)).toBeGreaterThan(0n);
    const { rows: ledger } = await db.query("SELECT kind, evidence FROM monetary_ledger WHERE execution_id=$1 ORDER BY created_at", [ids.executions.settle]);
    expect(ledger.map(l => l.kind)).toEqual(["unknown", "reconciliation"]); expect(ledger[1].evidence).toBe("browser-acceptance-usage-ref-1");
    const { rows: [audit] } = await db.query("SELECT count(*)::int n FROM audit_events WHERE action='usage.reconciled' AND resource_id=$1", [ids.executions.settle]);
    expect(audit.n).toBe(1);
    await expectNoHorizontalScroll(page);
    await expectAccessible(page, "holds after reconcile");
  } finally { await db.end(); await operator.context.close(); }
});

test("keyset Load more from the keyboard appends the rest and then goes away", async ({ browser }) => {
  const ids = await ensure(browser), alex = await signIn(browser, "alex");
  const page = alex.page;
  try {
    await open(page, `/workspaces/${ids.paging}/costs?tab=holds`);
    await expect(queue(page).getByRole("row")).toHaveCount(51); // header + a full page of 50
    await expect(page.getByText(/Showing 50 of 52/)).toBeVisible();
    const more = page.getByRole("button", { name: "Load more" });
    await more.focus(); await page.keyboard.press("Enter");
    await expect(queue(page).getByRole("row")).toHaveCount(53);
    await expect(page.getByText(/Showing 52 of 52/)).toBeVisible();
    await expect(more).toHaveCount(0);
    // Oldest first: the 76-hour-old hold leads and the 25-hour-old one is last.
    await expect(queue(page).getByRole("row").nth(1)).toContainText("holds-p/51");
    await expect(queue(page).getByRole("row").nth(52)).toContainText("holds-p/00");
    await expectNoHorizontalScroll(page);
    await expectAccessible(page, "holds load more");
  } finally { await alex.context.close(); }
});

test("an older gateway (404) shows 'not available', not an empty queue", async ({ browser }) => {
  const ids = await ensure(browser), alex = await signIn(browser, "alex");
  const page = alex.page;
  try {
    await page.route("**/api/v1/workspaces/*/holds*", route => route.fulfill({ status: 404, contentType: "application/json", body: JSON.stringify({ error: { code: "404", message: "Not found" } }) }));
    await page.goto(`/workspaces/${ids.team}/costs?tab=holds`);
    await expect(page.getByText("Holds review isn't available")).toBeVisible();
    await expect(queue(page)).toHaveCount(0); await expect(page.getByText("No aged unknown holds")).toHaveCount(0);
    await expectAccessible(page, "holds not available");
  } finally { await alex.context.close(); }
});
