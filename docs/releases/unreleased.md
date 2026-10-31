# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.9.3](v0.9.3.md). They become the next release's notes.

## Provider-reported cost reconciliation (#48)

- **Admin → Usage & costs → Provider reconciliation:** Platform Admin/Auditor frozen comparisons with signed charges and variance, separate holds, unresolved coverage, identity-free personal totals and source/mapping provenance. Platform Admins can upload an Anthropic Cost Report envelope, attest whole mapping revisions and freeze snapshots in the dashboard.
- Uploads send file bytes unchanged; duplicate JSON keys remain server-detectable. Identical reuploads resolve to the retained import; overlaps and stale revisions are refused, and changed exact-coverage reports require explicit supersession. Live demotion refuses writes and removes write controls. Upload, mapping and freeze leave existing financial history unchanged.
- **Boundary:** Anthropic only, USD only, manual uploads, not a verified invoice or complete payable balance. Priority Tier costs are not supplied. No automated pulls, estimate rewrites or hold settlement/release. OpenAI Costs export is follow-up [#393](https://github.com/ncecere/open-model-gateway/issues/393).
- No new migration, runtime grants or settings; the evidence tables shipped in migration 0064. See the [workflow and limitations](../provider-cost-reconciliation.md).

## Management API contract: money (#81, slice 4)

- `docs/openapi/management.openapi.json` now covers usage and costs (overview, explore, cost reports and summaries, usage export), holds, cost centers, report schedules and runs (including the CSV downloads), cost reconcile, and provider-cost imports, mappings and reconciliations. Each schema is checked against the real handlers on PostgreSQL by response validation and HTTP boundary vectors.
- Money values are integer micro-USD strings bounded to i64 (i128 for imported amounts, matching `cents()`). Rules JSON Schema cannot state are documented and pinned by probes: upload page and bucket relations, mapping windows and overlaps, one budget per period, recipient uniqueness, workspace report filters equal to the route's workspace, and byte limits. Where the schema is stricter than the handler (canonical date and date-time spellings, canonical integers), the description says so.
- Test harness: recursive nested-DTO probes, a stricter `*_microusd` status-marker rule, and response samples for CSV downloads and required headers.
- No production behaviour change, migration or runtime-grant change.
