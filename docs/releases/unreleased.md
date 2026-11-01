# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.9.3](v0.9.3.md). They become the next release's notes.

## Provider-reported cost reconciliation (#48)

- **Admin → Usage & costs → Provider reconciliation:** Platform Admin/Auditor frozen comparisons with signed charges and variance, separate holds, unresolved coverage, identity-free personal totals and source/mapping provenance. Platform Admins can upload an Anthropic Cost Report envelope, attest whole mapping revisions and freeze snapshots in the dashboard.
- Uploads send file bytes unchanged; duplicate JSON keys remain server-detectable. Identical reuploads resolve to the retained import; overlaps and stale revisions are refused, and changed exact-coverage reports require explicit supersession. Live demotion refuses writes and removes write controls. Upload, mapping and freeze leave existing financial history unchanged.
- **Boundary:** Anthropic only, USD only, manual uploads, not a verified invoice or complete payable balance. Priority Tier costs are not supplied. No automated pulls, estimate rewrites or hold settlement/release. OpenAI Costs export is follow-up [#393](https://github.com/ncecere/open-model-gateway/issues/393).
- No new migration, runtime grants or settings; the evidence tables shipped in migration 0064. See the [workflow and limitations](../provider-cost-reconciliation.md).

## Management API contract: model setup (#81, slice 3)

- `docs/openapi/management.openapi.json` now covers providers (per-kind credential reference, endpoint, Bedrock region and AWS role options), deployments and routing, model setup, models, catalogs and catalog defaults, prices (pricing versions, cache pricing, price lines), batch scheduling, and the workspace model, catalog, available-models and policy routes. Each schema is checked against the real handlers on PostgreSQL by response validation and HTTP boundary vectors.
- Where JSON Schema cannot state a handler rule, the description says so and a probe pins it. Examples: the 510-character Bedrock role path, the server allowlists for env references and Bedrock endpoints, canonical-only endpoints, and byte limits.
- Test harness: an `x-omg-unsendable-boundary` reason is judged only in runs where its location has a length or item bound.
- No production behaviour change, migration or runtime-grant change.

## Fixes

- Complete the inference OpenAPI contracts for Chat Completions, Responses and Messages, with validated JSON/SSE, explicit refusals, cancellation evidence and official SDK wire replay (#82).

- Align generation response tool-name and model-trim contracts with the real handlers; keep request-side tool-name restrictions (#82).
