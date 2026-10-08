# Enterprise costs and allocation

## What the gateway measures

Cloud and local deployments use configured rates and provider-reported token evidence. Local models are not implicitly free: an explicit zero rate is different from a missing rate. GPU, electricity, infrastructure amortization, taxes and vendor-specific extra fees are not metered here. Reports are configured-rate estimates, not provider invoices or a customer billing system.

The installation has no organization accounting layer. Activity belongs to Personal, Team or Project workspaces, with one execution per actual admitted upstream attempt. A request with explicit fallback can have several attempts and costs; root-request counts distinguish it from attempt counts. Embeddings are input-only: budget reservation uses the hard input ceiling, output is semantic zero, and missing input evidence is unresolved.

## Cost centers

Cost-center assignment is optional and controlled only by Platform Admins. Multiple workspaces can share a center. Unassigned usage is **Unallocated** and remains fully tracked. A center is an allocation label, never a permission grant.

Create/read/update/archive centers through `/api/v1/platform/cost-centers`. Assign or clear a workspace:

```http
PATCH /api/v1/platform/workspaces/11111111-1111-4111-8111-111111111111
Content-Type: application/json

{"cost_center_id":"22222222-2222-4222-8222-222222222222"}
```

Use `{"cost_center_id":null}` to clear future allocation. Omission leaves it unchanged. An archived center cannot be newly assigned; archival clears current assignments but retains historical records.

Admission stores the center ID and exact name/code snapshot on each execution. Reassignment, rename or archive cannot rewrite those snapshots. Cost-center breakdowns group stable IDs and choose a deterministic recorded label, not the current live name; detail/CSV preserve each attempt's exact admission label. Compare historical labels with care after renames.

## Budgets are admission controls

Platform defaults apply independently by workspace kind. Platform workspace overrides replace type defaults; local/key restrictions only tighten. An optional installation-wide monthly budget provides a **shared additional ceiling**, not allocated capacity per workspace. Installation rate/concurrency ceilings can also apply.

Absent child caps do not remove a parent, and specified caps do not reserve a slice. Allowance changes/rotation never reset prior consumption. Ordinary users do not see shared installation headroom that could reveal others' private consumption.

Before each attempt the gateway pins a price and reserves conservative cost. Missing rates, missing prices or an unbounded prior charge are not treated as zero. Unknown holds survive cancellation, errors and lease expiry; known floors do not prove finite upper bounds. Usage above a hold can increase subsequent denial pressure. Configure accurate hard upstream ceilings and immutable prices before traffic. See [governance](governance.md).

## Read reports safely

Platform Admin/Auditor reporting includes personal financial totals, without foreign personal keys or request details. Shared administrators see whole-workspace activity; ordinary members their own human-key activity. Platform totals do not grant a private detail/export endpoint.

Financial amounts/counts are decimal strings and unknown measurements null. UTC reports use **[start,end)** with exclusive end, 1–93 days; comparisons use the same filters and equal preceding length. Known settled cost, active holds, unbounded/unknown coverage and overlapping health indicators must remain distinct. Do not sum holds as extra charges, or health indicators as independent failed requests.

Bounded live CSV pages are not frozen statements. Reporting lock contention, reconciliation staffing, authoritative evidence sources and invoice comparison require operational processes. Account cleanup and optional settled error/latency compaction preserve accounting/audit history rather than deleting spend.

For payloads see [governance API](governance-api.md); for field interpretation see [cost reporting](cost-reporting.md); for cache overlap, zeros and exact arithmetic see [cache pricing](cache-pricing.md). Broader non-token modalities remain separately scoped rebuild work, not token-cost features silently inherited by images/audio/video.
