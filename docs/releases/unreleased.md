# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.9.3](v0.9.3.md). They become the next release's notes.

## Provider-reported cost reconciliation (#48)

- **Admin → Usage & costs → Provider reconciliation:** Platform Admin/Auditor frozen comparisons with signed charges and variance, separate holds, unresolved coverage, identity-free personal totals and source/mapping provenance. Platform Admins can upload an Anthropic Cost Report envelope, attest whole mapping revisions and freeze snapshots in the dashboard.
- Uploads send file bytes unchanged; duplicate JSON keys remain server-detectable. Identical reuploads resolve to the retained import; overlaps and stale revisions are refused, and changed exact-coverage reports require explicit supersession. Live demotion refuses writes and removes write controls. Upload, mapping and freeze leave existing financial history unchanged.
- **Boundary:** Anthropic only, USD only, manual uploads, not a verified invoice or complete payable balance. Priority Tier costs are not supplied. No automated pulls, estimate rewrites or hold settlement/release. OpenAI Costs export is follow-up [#393](https://github.com/ncecere/open-model-gateway/issues/393).
- No new migration, runtime grants or settings; the evidence tables shipped in migration 0064. See the [workflow and limitations](../provider-cost-reconciliation.md).
