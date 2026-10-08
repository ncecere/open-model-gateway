import { describe, expect, it } from "vitest";
import type { Price } from "./governance";
import { basePrice, cheapestPrice, compareDecimal, formatDecimalMicroUsd, priceItems, usdPerMillionFilter } from "./pricing";
import { filterCatalog, modelSectionFor, protocolEndpoints, protocolProfiles, routeDataPolicy, routeSectionFor, routeTiers, sortCatalog, typeCounts, type CatalogModel } from "./model-setup";
import { dashboardSearch } from "./permissions";
import { dashboardHref, parseDashboardLocation } from "./locations";

const v3 = (lines: NonNullable<Price["price_lines"]>, extra: Partial<Price> = {}): Price => ({ id: "p", deployment_id: "d", pricing_version: 3, input_microusd_per_million: null, output_microusd_per_million: null, input_token_limit: 8000, output_token_limit: 1000, cache_pricing: null, price_lines: lines, created_at: "2026-01-01T00:00:00Z", ...extra });
const line = (meter: "input_tokens" | "output_tokens" | "output_images", microusd: string, extra: { batch?: number; min_prompt_tokens?: number; variant?: string } = {}) => ({ meter, microusd_per_batch: microusd, batch: extra.batch ?? (meter === "output_images" ? 1 : 1000000), unit_label: "/M tokens", sku_label: meter === "input_tokens" ? "Input" : meter === "output_tokens" ? "Output" : "Image output", ...extra });

describe("exact catalog price display", () => {
  it("formats fractional decimal micro-USD exactly and keeps unknown unknown", () => {
    expect(formatDecimalMicroUsd("100000")).toBe("$0.10");
    expect(formatDecimalMicroUsd("15000000")).toBe("$15.00");
    expect(formatDecimalMicroUsd("0.5")).toBe("$0.0000005");
    expect(formatDecimalMicroUsd("8100")).toBe("$0.0081");
    expect(formatDecimalMicroUsd("9007199254740993")).toBe("$9,007,199,254.740993");
    expect(formatDecimalMicroUsd(null)).toBeNull(); expect(formatDecimalMicroUsd("1e3")).toBeNull(); expect(formatDecimalMicroUsd("-1")).toBeNull();
  });
  it("compares decimal strings exactly with unknown last", () => {
    expect(compareDecimal("0.5", "1")).toBe(-1); expect(compareDecimal("9007199254740993", "9007199254740992")).toBe(1); expect(compareDecimal("2.50", "2.5")).toBe(0);
    expect(compareDecimal(null, "1")).toBe(1); expect(compareDecimal("1", undefined)).toBe(-1);
    expect(usdPerMillionFilter("0.15")).toBe("150000"); expect(usdPerMillionFilter("abc")).toBeUndefined(); expect(usdPerMillionFilter("0.0000001")).toBeUndefined();
  });
  it("groups prompt-size tiers and variants, marks missing meters unknown and not-applicable together", () => {
    const price = v3([line("input_tokens", "100000"), line("input_tokens", "200000", { min_prompt_tokens: 272000 }), line("output_tokens", "500000"), { meter: "cache_read_tokens", not_applicable: true }]);
    const items = priceItems(price, "generation");
    const input = items.find(i => i.meter === "input_tokens")!;
    expect(input.tiers).toEqual([{ label: "≤272K", amount: "100000", unit: "M input tokens" }, { label: ">272K", amount: "200000", unit: "M input tokens" }]);
    expect(items.find(i => i.meter === "output_tokens")).toMatchObject({ amount: "500000", unit: "M output tokens" });
    expect(items.find(i => i.meter === "requests")).toMatchObject({ amount: null }); // unknown, never free
    expect(items.find(i => i.key === "not_applicable")).toMatchObject({ notApplicable: true });
    const images = priceItems(v3([line("output_images", "20500", { variant: "768" }), line("output_images", "41000", { variant: "4K" })]), "images");
    expect(images.filter(i => i.meter === "output_images").map(i => i.label)).toEqual(["Image output (768)", "Image output (4K)"]);
  });
  it("maps v1 prices to token rows, with embeddings input-only", () => {
    const v1: Price = { id: "v1", deployment_id: "d", pricing_version: 1, input_microusd_per_million: "1", output_microusd_per_million: null, input_token_limit: 1, output_token_limit: 0, cache_pricing: null, created_at: "" };
    expect(priceItems(v1, "embeddings").map(i => [i.meter, i.amount])).toEqual([["input_tokens", "1"]]);
    expect(priceItems(v1, "generation").find(i => i.meter === "output_tokens")?.amount).toBeNull();
  });
  it("picks the cheapest base rate exactly across units and reports unknown routes", () => {
    const a = v3([line("input_tokens", "300000")]), b = v3([line("input_tokens", "250", { batch: 1000 })]);
    expect(basePrice(a, "input_tokens")).toMatchObject({ amount: "300000", unit: "M input tokens" });
    const cheapest = cheapestPrice([a, b, null], "input_tokens");
    expect(cheapest.price).toMatchObject({ amount: "250", batch: 1000 }); expect(cheapest.someUnknown).toBe(true); expect(cheapest.varies).toBe(true);
    expect(cheapestPrice([v3([{ meter: "output_tokens", not_applicable: true }])], "output_tokens").price).toMatchObject({ amount: null, notApplicable: true });
  });
});

const row = (id: string, extra: Partial<CatalogModel> = {}): CatalogModel & { workload: NonNullable<CatalogModel["workload"]> } => ({ id, public_name: id, display_name: id.toUpperCase(), enabled: true, supported_protocols: ["chat_completions"], workload: "generation", readiness: { routes: 1, enabled_routes: 1, priced_enabled_routes: 1, catalogs: 1, direct_workspaces: 0, connections: [{ id: "c1", name: "One" }] }, min_input_microusd_per_million: "100000", created_at: "2026-01-01T00:00:00Z", ...extra } as CatalogModel & { workload: NonNullable<CatalogModel["workload"]> });

describe("catalog facets, sort and counts", () => {
  const rows = [row("b"), row("a", { min_input_microusd_per_million: null, created_at: "2026-03-01T00:00:00Z" }), row("c", { enabled: false, workload: "embeddings", supported_protocols: ["embeddings"], readiness: { routes: 0, enabled_routes: 0, priced_enabled_routes: 0, catalogs: 0, direct_workspaces: 0, connections: [] }, min_input_microusd_per_million: "0.5" })];
  it("filters by connection, status, readiness and exact price bounds (unknown excluded while bounded)", () => {
    expect(filterCatalog(rows, { connections: ["c1"] }).map(r => r.id)).toEqual(["b", "a"]);
    expect(filterCatalog(rows, { enabled: "false" }).map(r => r.id)).toEqual(["c"]);
    expect(filterCatalog(rows, { readiness: ["needs_setup"] }).map(r => r.id)).toEqual(["c"]);
    expect(filterCatalog(rows, { maxPrice: "1" }).map(r => r.id)).toEqual(["c"]);
    expect(filterCatalog(rows, { minPrice: "1" }).map(r => r.id)).toEqual(["b"]);
  });
  it("sorts by name, price (unknown last) and newest; counts per type", () => {
    expect(sortCatalog(rows).map(r => r.id)).toEqual(["a", "b", "c"]);
    expect(sortCatalog(rows, "price").map(r => r.id)).toEqual(["c", "b", "a"]);
    expect(sortCatalog(rows, "newest")[0].id).toBe("a");
    expect(typeCounts(rows)).toMatchObject({ all: 3, generation: 2, embeddings: 1, images: 0 });
  });
});

describe("route order and data policy", () => {
  const r = (id: string, priority?: number, extra: Partial<{ enabled: boolean; connectionEnabled: boolean }> = {}) => ({ id, enabled: true, connectionEnabled: true, priority, ...extra });
  it("uses the first priority route by default; the rest are fallback only with a reason", () => {
    const tiers = routeTiers([r("late", 5), r("first", 0), r("off", 0, { enabled: false }), r("conn", 0, { connectionEnabled: false })], { strategy: "priority", max_attempts: 1 });
    expect(tiers.primary.map(x => x.id)).toEqual(["first"]); expect(tiers.fallback.map(x => x.id)).toEqual(["late"]); expect(tiers.inactive.map(x => x.id)).toEqual(["off", "conn"]);
    expect(tiers.fallbackReason).toContain("one attempt");
    expect(routeTiers([r("a", 0), r("b", 3)], { strategy: "priority", max_attempts: 2 }).fallbackReason).toContain("up to 2 attempts");
  });
  it("splits weighted traffic within the lowest tier and flags priority ties", () => {
    expect(routeTiers([r("a", 1), r("b", 1), r("c", 2)], { strategy: "weighted", max_attempts: 1 }).primary.map(x => x.id)).toEqual(["a", "b"]);
    expect(routeTiers([r("a", 1), r("b", 1)], { strategy: "priority", max_attempts: 1 }).tieNote).toContain("share priority 1");
  });
  it("never claims a default when routing is unknown", () => {
    const unknownPolicy = routeTiers([r("a", 0)]);
    expect(unknownPolicy.primary).toEqual([]); expect(unknownPolicy.fallback).toEqual([]); expect(unknownPolicy.unknown.map(x => x.id)).toEqual(["a"]);
    expect(routeTiers([r("a", 0), r("b")], { strategy: "priority", max_attempts: 1 }).unknown.map(x => x.id)).toEqual(["b"]);
  });
  it("labels data policy without overstating; unknown is amber", () => {
    expect(routeDataPolicy({ data_collection: "deny", basis: "current_configuration" }).policy).toBe("no_keep");
    expect(routeDataPolicy({ data_collection: "allow", basis: "current_configuration" }).policy).toBe("keeps");
    expect(routeDataPolicy({ data_collection: "unknown", basis: "not_configured" })).toMatchObject({ policy: "unknown", detail: "Not configured for this provider" });
    expect(routeDataPolicy(undefined).policy).toBe("unknown");
  });
  it("maps old ?tab= values to sections", () => {
    expect(modelSectionFor("deployments")).toBe("routes"); expect(modelSectionFor("pricing")).toBe("pricing"); expect(modelSectionFor("settings")).toBeUndefined();
    expect(routeSectionFor("pricing")).toBe("price-history"); expect(routeSectionFor("settings")).toBeUndefined(); // the Status section is gone: enable/disable is the header action
  });
  it("documents every protocol and which adapters carry it", () => {
    for (const p of Object.keys(protocolEndpoints) as (keyof typeof protocolEndpoints)[]) { expect(protocolEndpoints[p].path).toMatch(/^\/v1\//); expect(protocolProfiles[p].length).toBeGreaterThan(0); }
    expect(protocolEndpoints.messages.headers.map(h => h.name)).toContain("anthropic-version");
    expect(protocolProfiles.responses).toEqual(["openai"]);
  });
});

describe("catalog URL state", () => {
  it("keeps only offered facet values and round trips them", () => {
    const s = dashboardSearch({ page: "models", type: "embeddings", sort: "price", layout: "table", connections: "c1,bad id,c2", min_price: "0.", max_price: "1e3", policy: "deny,zzz", readiness: "ready", deprecated: "hide", eligibility: "selected,nope", cols: "price,created", density: "compact" });
    expect(s).toMatchObject({ type: "embeddings", sort: "price", layout: "table", connections: "c1,c2", min_price: "0.", policy: "deny", readiness: "ready", deprecated: "hide", eligibility: "selected" });
    expect(s.max_price).toBeUndefined();
    expect(dashboardSearch({ type: "chat", sort: "random", layout: "grid" })).toEqual({ ws: undefined, page: undefined });
    const href = dashboardHref({ page: "models", type: "images", connections: "c1", policy: "unknown" });
    expect(href).toBe("/admin/models?type=images&connections=c1&policy=unknown");
    expect(parseDashboardLocation(href)).toMatchObject({ page: "models", type: "images", connections: "c1", policy: "unknown" });
  });
});
