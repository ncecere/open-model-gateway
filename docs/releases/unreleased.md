# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.9.3](v0.9.3.md). They become the next release's notes.

## Fixes

- Email tests against disallowed relay hosts now record a failed test with `host_not_allowed` instead of returning 409 (#411). Requires migration 0068.
- General settings validate support and logo URL length after percent-encoding, returning 400 instead of 409 when the stored form exceeds 2048 characters (#419).
- List cursors, including sessions, before PostgreSQL’s minimum timestamp now return 400 invalid input instead of 503 (#409).
- SCIM list filters containing control characters (for example a JSON-escaped NUL) are refused with 400 `invalidFilter` instead of reaching PostgreSQL and returning 500 (#414).
- Report run detail and list reads use a single database instant per statement, keeping expiry and snapshot visibility consistent at expiry boundaries (#406).
- Manual cost reconciliation refuses incoherent operator token usage with 400 instead of 503, retaining the unknown hold unchanged; provider settlement behaviour is unchanged (#408).

## Provider-reported cost reconciliation (#48)

- **Admin → Usage & costs → Provider reconciliation:** Platform Admin/Auditor frozen comparisons with signed charges and variance, separate holds, unresolved coverage, identity-free personal totals and source/mapping provenance. Platform Admins can upload an Anthropic Cost Report envelope, attest whole mapping revisions and freeze snapshots in the dashboard.
- Uploads send file bytes unchanged; duplicate JSON keys remain server-detectable. Identical reuploads resolve to the retained import; overlaps and stale revisions are refused, and changed exact-coverage reports require explicit supersession. Live demotion refuses writes and removes write controls. Upload, mapping and freeze leave existing financial history unchanged.
- **Boundary:** Anthropic only, USD only, manual uploads, not a verified invoice or complete payable balance. Priority Tier costs are not supplied. No automated pulls, estimate rewrites or hold settlement/release. OpenAI Costs export is follow-up [#393](https://github.com/ncecere/open-model-gateway/issues/393).
- No new migration, runtime grants or settings; the evidence tables shipped in migration 0064. See the [workflow and limitations](../provider-cost-reconciliation.md).

## Management API contract: model setup (#81, slice 3)

- `docs/openapi/management.openapi.json` now covers providers (per-kind credential reference, endpoint, Bedrock region and AWS role options), deployments and routing, model setup, models, catalogs and catalog defaults, prices (pricing versions, cache pricing, price lines), batch scheduling, and the workspace model, catalog, available-models and policy routes. Each schema is checked against the real handlers on PostgreSQL by response validation and HTTP boundary vectors.
- Where JSON Schema cannot state a handler rule, the description says so and a probe pins it. Examples: the 510-character Bedrock role path, the server allowlists for env references and Bedrock endpoints, canonical-only endpoints, and byte limits.
- Test harness: an `x-omg-unsendable-boundary` reason is judged only in runs where its location has a length or item bound.

## Management API contract: money (#81, slice 4)

- `docs/openapi/management.openapi.json` now covers usage and costs (overview, explore, cost reports and summaries, usage export), holds, cost centers, report schedules and runs (including the CSV downloads), cost reconcile, and provider-cost imports, mappings and reconciliations. Each schema is checked against the real handlers on PostgreSQL by response validation and HTTP boundary vectors.
- Money values are integer micro-USD strings bounded to i64 (i128 for imported amounts, matching `cents()`). Rules JSON Schema cannot state are documented and pinned by probes: upload page and bucket relations, mapping windows and overlaps, one budget per period, recipient uniqueness, workspace report filters equal to the route's workspace, and byte limits. Where the schema is stricter than the handler (canonical date and date-time spellings, canonical integers), the description says so.
- Test harness: recursive nested-DTO probes, a stricter `*_microusd` status-marker rule, and response samples for CSV downloads and required headers.
- No production behaviour change, migration or runtime-grant change.

## Inference API contract, part 1 (#82)

- `docs/openapi/inference.openapi.json`: a drift-checked OpenAPI 3.1 description of the supported inference subset, built on the #81 tooling and validated against real handler responses. Part 1 covers models, embeddings, rerank, System One, images and the realtime refusals; Chat Completions, Responses, Messages, media, files, batches, videos and realtime follow. Error envelopes are per family (OpenAI-shaped, Anthropic-shaped), never the management envelope.
- No production behaviour change.

## Dependencies

- Rust: hyper 1.12.0, tokio 1.54.1, webpki-roots 1.0.11, jsonschema 0.60.2, syn 3.0.9.
- Web: openai 7.45.0, lucide-react 1.62.0, @tanstack/react-query 5.105.0.
- Images: node 22.23.4-bookworm-slim and a refreshed rust image digest; CI: taiki-e/install-action 2.87.32.
## Fixes

- `/v1/embeddings` now returns the shared OpenAI-shaped `payload_too_large` error for oversized streamed request bodies (#432).

- Document inference media, Files, Batches, legacy Videos and Realtime WebSocket contracts, including multipart and truncated-stream limitations (#82, PR3). No runtime behavior change.

- Complete the inference OpenAPI contracts for Chat Completions, Responses and Messages, with validated JSON/SSE, explicit refusals, cancellation evidence and official SDK wire replay (#82).
