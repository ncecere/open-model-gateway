// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import type { Model } from "../lib/api";
import type { DeploymentRouting, Price } from "../lib/governance";
import { routeTiers } from "../lib/model-setup";
import { RoutesTable, routeColumns, tilePrices, type RouteDetail, type RouteRow, type RouteSet } from "../pages/model-page";
import { basePrice } from "../lib/pricing";
import { DashboardNavigationProvider } from "./navigation-link";
import { markup, model } from "../lib/test-fixtures";

const price = (id: string, input: string, output: string): Price => ({ id: `price-${id}`, deployment_id: id, pricing_version: 2, input_microusd_per_million: input, output_microusd_per_million: output, input_token_limit: 8000, output_token_limit: 64, cache_pricing: null, created_at: "2026-10-01T00:00:00Z" });
const routing = (priority: number, weight = 1): DeploymentRouting => ({ routing: { priority, weight, residency: null, failure_threshold: 3, cooldown_seconds: 30 }, health: { consecutive_failures: 0, open_until: null } });
function route(id: string, opts: { name?: string; priority?: number; enabled?: boolean; connection?: boolean; input?: string; output?: string; policy?: string } = {}): RouteRow {
  const enabled = opts.enabled ?? true, connection = opts.connection ?? true;
  const detail: RouteDetail = { id, model_id: model.id, provider_connection_id: `conn-${id}`, provider_name: "OpenAI prod", upstream_model: opts.name ?? id, enabled, provider: "openai", connection_enabled: connection, region: null, workload: "generation", protocols: ["chat_completions"], data_policy: { data_collection: opts.policy ?? "unknown", basis: "not_configured" }, price: price(id, opts.input ?? "100000", opts.output ?? "500000") };
  const r = opts.priority === undefined ? undefined : routing(opts.priority);
  return { id, route: { id, model_id: model.id, provider_connection_id: `conn-${id}`, provider_name: "OpenAI prod", upstream_model: opts.name ?? id, enabled }, detail, routing: r, enabled, connectionEnabled: connection, priority: r?.routing.priority, weight: r?.routing.weight };
}
/** A route set as useModelRoutes returns it, from fixed rows (default, fallback, unknown order and not serving). */
function routeSet(rows: RouteRow[], policy: { strategy: "priority" | "weighted"; max_attempts: number } | undefined): RouteSet {
  const tiers = routeTiers(rows, policy);
  return { list: { data: rows.map(r => r.route), isError: false, isPending: false, refetch: () => {} }, policy: { isError: false, data: policy ? { policy } : undefined }, rows, tiers, ordered: [...tiers.primary, ...tiers.fallback, ...tiers.unknown, ...tiers.inactive] } as unknown as RouteSet;
}
function table(set: RouteSet, m: Model = model) {
  const html = markup(<DashboardNavigationProvider search={{ page: "model-detail", record: m.id }} navigate={() => {}}><RoutesTable set={set} model={m} workload="generation" writable /></DashboardNavigationProvider>);
  const doc = new DOMParser().parseFromString(html, "text/html"), el = doc.querySelector("table");
  if (!el) throw new Error("No table rendered");
  return el;
}
const span = (row: HTMLTableRowElement) => [...row.cells].reduce((n, cell) => n + (cell.colSpan || 1), 0);
const kind = (row: HTMLTableRowElement) => row.cells.length === 1 && row.cells[0]!.colSpan > 1 ? "divider" : "route";

describe("Admin model page › Routes table (D-1)", () => {
  const long = "configure-your-cloud-model-with-a-very-long-upstream-name";
  const rows = [route("a", { priority: 1, name: "gpt-6-luna" }), route("b", { priority: 2, name: long, input: "1000000", output: "3000000" }), route("c", { enabled: false, name: "disabled-route" }), route("d", { connection: false, priority: 1, name: "connection-off" })];

  it("lines every row type up with the header: route, fallback divider, not-serving divider and not-serving rows", () => {
    const el = table(routeSet(rows, { strategy: "priority", max_attempts: 2 }));
    const headers = [...el.tHead!.rows[0]!.cells].map(c => c.textContent);
    expect(headers).toEqual(["Route", "Input", "Output", "Priority", "Data policy", "Status", "Actions"]);
    const body = [...el.tBodies[0]!.rows];
    expect(body.map(kind)).toEqual(["route", "divider", "route", "divider", "route", "route"]);
    expect(body.map(r => r.textContent)).toEqual(expect.arrayContaining([expect.stringContaining("Fallback only"), expect.stringContaining("Not serving")]));
    for (const row of body) expect(span(row)).toBe(headers.length);
  });

  it("also lines up the 'routing order unknown' divider while routing settings are missing", () => {
    const el = table(routeSet([route("a"), route("c", { enabled: false })], undefined)), width = el.tHead!.rows[0]!.cells.length;
    const body = [...el.tBodies[0]!.rows];
    expect(body.map(kind)).toEqual(["divider", "route", "divider", "route"]);
    for (const row of body) expect(span(row)).toBe(width);
  });

  it("puts each price under its own header, compact and exact, with no hidden label column shifting it", () => {
    const el = table(routeSet(rows, { strategy: "priority", max_attempts: 2 })), first = el.tBodies[0]!.rows[0]!;
    expect(first.cells[1]!.textContent).toBe("$0.10/M tokens");
    expect(first.cells[2]!.textContent).toBe("$0.50/M tokens");
    expect(first.cells[3]!.textContent).toBe("1Weight 1");
    expect(el.querySelector("tbody dl")).toBeNull();
    expect(first.cells[1]!.getAttribute("data-numeric")).not.toBeNull();
  });

  it("gives every column but the route a fixed width, truncates long route names with the full name in a tooltip, and keeps status short", () => {
    const el = table(routeSet(rows, { strategy: "priority", max_attempts: 2 }));
    const heads = [...el.tHead!.rows[0]!.cells];
    expect(heads[0]!.getAttribute("style")).toBeNull();
    for (const th of heads.slice(1)) expect(th.getAttribute("style")).toMatch(/width:/);
    expect(el.className).toMatch(/routesTable/);
    const named = el.querySelector(`[title="${long}"]`);
    expect(named?.className).toMatch(/truncate/);
    expect(el.textContent).toContain("Connection off");
    expect(el.textContent).not.toContain("Connection disabled");
  });

  it("columns come from one definition (headline meters + fixed columns)", () => {
    expect(routeColumns("generation").columns).toHaveLength(7);
    expect(routeColumns("embeddings").columns).toHaveLength(6);
  });
});

describe("Header price tiles", () => {
  it("show short amounts with units in the hint, and unknown as unknown", () => {
    const p = price("a", "100000", "500000");
    expect(tilePrices([basePrice(p, "input_tokens"), basePrice(p, "output_tokens")])).toEqual({ value: "$0.10 / $0.50", units: "per M input tokens / M output tokens" });
    expect(tilePrices([basePrice(null, "input_tokens")]).value).toBeNull();
  });
});
