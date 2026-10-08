# UI kit templates

Shared building blocks for the approved UX program (`.local/enterprise-rebuild/ux-program.md`). They are compositions of the vendored Bitop primitives in `components/ui/**` (never edited here) and use semantic tokens only, so they follow light/dark themes and look native next to Grounded. No drawers or sheets: details open in place (`ExpandableRow`) or on a routed page (`PrevNext`).

Rules shared by every element:

- Money is an integer micro-USD string. Shares and changes use BigInt (`kit-format.ts`); amounts go through `formatMicroUsd`, so sub-cent values read `$0.0081`, never `$0`.
- Unknown is not zero. `null` amounts, counts, durations and policies render "Unknown" (amber where it matters), with no bars and no fabricated deltas.
- Colour never carries meaning alone (icon + word). Motion is disabled under `prefers-reduced-motion`. Layouts hold at 390px (≤600px breakpoint, `NARROW_QUERY`).
- Inline styles only set CSS custom properties (`--percent`, `--ring-offset`, `--width`).
- Values that belong in the URL are plain strings, with `*ToSearch` / `*FromSearch` helpers that drop anything not offered.

Tests: `ui-kit.test.tsx` (components, jsdom) and `kit-format.test.ts` (exact formatting).

| Element | File | Use for |
|---|---|---|
| `StatTile`, `StatTileGrid` | `stat-tile.tsx` | KPI tiles: value, sparkline, Δ vs previous period (`increaseIs: good/bad/neutral`), link/onClick, `details` line that may hold its own link (e.g. "View unresolved") |
| `PivotControls`, `pivotToSearch`, `pivotFromSearch` | `pivot-controls.tsx` | Explore: metric × group by × then by × top N |
| `PercentBarCell` | `percent-bar-cell.tsx` | %-of-total cell with a thin bar |
| `ViewDataTable`, `ColumnChooser`, `DensityToggle`, `useStoredColumns`, `tableViewToSearch`/`FromSearch` | `table-view.tsx` | Column chooser + density for DataTable (wrapper) or hand-built tables; `useStoredColumns` lifts DataTable's remembered Columns menu into a FilterToolbar's `end` |
| `UsageBar`, `usageText` | `usage-bar.tsx` | "$12.40 / $50.00 · Monthly", "$x / ∞" |
| `BudgetRing` | `budget-ring.tsx` | SVG ring with accessible meter text |
| `PeriodPills`, `PeriodBadge` | `period-pills.tsx` | Daily / Weekly / Monthly / Lifetime |
| `PresetChoice` | `preset-choice.tsx` | Presets + Custom input (limits, expiry) |
| `StatusPill`, `toLifecycleStatus`, `isInactiveStatus` | `status-pill.tsx` | Active / Disabled / Revoked / Unknown / Pending |
| `Timeline` | `timeline.tsx` | Ordered attempts: status, title, meta, duration, marker |
| `CopyId`, `shortId` | `copy-id.tsx` | Short id + copy button (full value only in title/clipboard) |
| `LayerTable` | `layer-table.tsx` | Effective policy by layer with counts and in-place reasons |
| `PriceLine`, `PriceLines` | `price-line.tsx` | Amount + unit, tiers, struck list price, "Unknown · not free" |
| `FilterToolbar`, `ToolbarField`, `toolbarValuesToSearch`/`FromSearch` | `filter-toolbar.tsx` | **The** filter row above every list, catalog and report (see below) |
| `StickySaveBar` | `sticky-save-bar.tsx` | Bitop SaveBar that reserves its height (`scroll-padding`) so it never covers controls |
| `TypeTabs` | `type-tabs.tsx` | Type tabs with counts (Bitop pill tabs) |
| `SectionNav`, `SectionNavLayout`, `useScrollSpy` | `section-nav.tsx` | Sticky in-page anchors with scroll-spy |
| `ExpandableRow`, `expandColumn`, `TableDividerRow` | `table-rows.tsx` | In-place row details; "Fallback only · not used by default" divider |
| `DataPolicyBadge`, `toDataPolicy` | `data-policy-badge.tsx` | Keeps data / doesn't keep / unknown (amber) |
| `PrevNext` | `prev-next.tsx` | Record navigation, optional j/k |
| `InfoBanner`, `DangerZone`, `DangerAction` | `notices.tsx` | Bitop Alert banner; Grounded-style danger card |

Bitop primitives added for the kit with the supported CLI (`bitop add sparkline meter copy-button --registry <checkout>`; tracked in `bitop-lock.json`): `sparkline`, `meter`, `copy-button`.

## Filters: one `FilterToolbar` everywhere

Filters always sit in one row **above** the content, never in a side column (`FilterSidebar` and `CollapsibleFilters` were removed). `FilterToolbar` wraps Bitop's vendored `FilterBar`, so it matches Grounded's ListPage/DataTable toolbar: search box first (label for screen readers; `showLabel` shows it), then facets with small `--font-size-xs` labels **above** `sm` controls, bottom-aligned, wrapping with `--space-3`/`--space-4` gaps; page actions (Sort, View, Columns, density, Export) at the end of the same row; then one chip per active filter and "Clear all".

- Facets: `toggle` (segmented, for a few options; `multiple`, `allLabel`), `select` (`multiple` = popover with counts via `counts`), `date-range`, and `range` (min/max with `prefix`, validation, `≤ $5` chips). Custom leading controls (Period, Workspace) go in `start` inside `ToolbarField`; give them `extraChips` / `extraActive`.
- URL: pages keep values in typed `DashboardSearch` keys; `toolbarValuesToSearch`/`FromSearch` handle comma lists and `min..max`. Search commits after `debounceMs` (300).
- More than four facets: pass the least-used facet ids in `more`. On wide screens they sit behind a "More filters (n)" popover button right after the inline facets (Bitop `Popover`, facets stacked), so the row stays on one line at 1440px; their chips still show under the row. Admin › Models uses it for Data collection, Readiness, Status and Retired.
- ≤640px: the row collapses **in place** behind a "Filters (n)" disclosure button (no drawer or sheet). Every control, multi-selects included, is full width (wrapper CSS; the vendored FilterBar is not edited), and `more` facets show inline.
- Lists: `DirectoryTable`, `LocalTable` and `CollectionTable` (people.tsx / ui.tsx) render it automatically from `search`/`facets` with Columns at the end (`useStoredColumns`), as do API keys, Requests, Usage & costs and both Models catalogs.

## Examples

```tsx
// Usage & costs overview
<StatTileGrid label="Usage summary" columns={5}>
  <StatTile label="Spend" value={formatMicroUsd(t.spend)} series={daily.map(d => Number(d.spend))} formatSeriesValue={v => formatMicroUsd(String(v))}
    delta={{ current: t.spend, previous: prev.spend, increaseIs: "bad", label: "vs previous 30 days" }} render={<Link to="/usage/spend" />} />
  <StatTile label="Cache hit" value={hitRate === null ? null : `${hitRate}%`} delta={{ current: hitRate, previous: prevHitRate, increaseIs: "good" }} />
</StatTileGrid>

// Explore
const pivot = pivotFromSearch(search, defaults, options);
<PivotControls {...options} value={pivot} onChange={next => navigate({ search: { ...search, ...pivotToSearch(next) } })} />
cell: r => <PercentBarCell value={formatMicroUsd(r.spend)} part={r.spend} total={totals.spend} />

// Requests list and detail page
<ViewDataTable caption="Requests" columns={cols} data={rows} view={tableViewFromSearch(search, ids)} onViewChange={v => navigate({ search: { ...search, ...tableViewToSearch(v) } })} />
<CopyId value={request.id} label="request ID" />
<Timeline label="Upstream attempts" items={attempts} showDurationBars showTotal />
<PrevNext noun="request" position={{ index, total }} prev={…} next={…} shortcuts />

// API keys
<UsageBar label="Key budget" used={k.used_microusd} limit={k.limit_microusd} period="month" size="sm" />
<BudgetRing label="Key limit" used={k.used_microusd} limit={k.limit_microusd} period="month" />
<StatusPill status={k.revoked_at ? "revoked" : k.enabled ? "active" : "disabled"} />
<PresetChoice legend="Spending limit" presets={[{ value: "", label: "No limit" }, { value: "10", label: "$10" }]} value={limit} onChange={setLimit}
  custom={{ label: "Custom limit (USD)", prefix: "$", inputMode: "decimal" }} />
<PeriodPills label="Reset period" value={period} onChange={setPeriod} periods={["day", "week", "month"]} showReset
  disabled={presetChoiceText(limit) === ""} disabledReason="Set a limit first" />
<DangerZone><DangerAction title="Revoke key" description="Requests using it fail immediately. This can't be undone." action={<Button variant="danger">Revoke key</Button>} /></DangerZone>

// Limits by layer
<LayerTable caption="Effective limits" valueHeader="Budget" showCounts layers={layers} effective={{ value: budgetText(effective) }} />

// Models catalog (type tabs, then the filter row) and model page
<TypeTabs label="Model type" items={types} value={type} onChange={setType} />
<FilterToolbar search={{ label: "Search models", value: search.q ?? "", onChange: q => go({ q }) }} facets={facets} counts={counts}
  values={toolbarValuesFromSearch(search, facets)} onChange={v => go(toolbarValuesToSearch(facets, v))}
  end={<><ToolbarField label="Sort"><NativeSelect size="sm" …/></ToolbarField><ToolbarField label="View" group><ToggleGroup …/></ToolbarField></>} />
<SectionNavLayout nav={<SectionNav items={[{ id: "routes", label: "Routes" }, { id: "pricing", label: "Pricing" }]} />}>…sections with matching ids…</SectionNavLayout>
<Table caption="Routes" columns={[expandColumn, "Route", "Price", "Data policy"]}>
  {primary.map(r => <ExpandableRow key={r.id} label={r.name} colSpan={4} details={<RouteDetail r={r} />}>
    <Th>{r.name}</Th><Td><PriceLine amount={r.input} unit="M tokens" /></Td><Td><DataPolicyBadge policy={toDataPolicy(r.policy)} /></Td>
  </ExpandableRow>)}
  <TableDividerRow colSpan={4} label="Fallback only · not used by default" reason="Used only when earlier routes fail." />
</Table>
<InfoBanner title="Prices are estimates">They are configured rates, not provider invoices.</InfoBanner>
```
