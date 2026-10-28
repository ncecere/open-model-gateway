# Provider-reported cost reconciliation

Compare uploaded **Anthropic Claude Platform Cost Report** charges with frozen aggregates of OMG's configured-rate estimates in the dashboard or browser-session API. This is **provider-reported cost reconciliation**, not a verified invoice, a complete payable balance, or customer invoicing. The #48 scope is Anthropic-only and USD-only. There are no automated provider pulls, organization credentials, outbound fetches, or changes to estimated costs or holds. OpenAI Costs export support is follow-up [#393](https://github.com/ncecere/open-model-gateway/issues/393).

**Mapping limit: each effective provider workspace/account binding maps to exactly one OMG connection in this slice. A provider workspace fed by multiple OMG connections must remain shared/unallocated; this slice cannot combine those connections into one fully covered comparison. Never attest exclusive coverage for only one of those connections.**

Anthropic's [Cost Report format](https://platform.claude.com/docs/en/api/beta/organization/cost_report/retrieve) uses decimal-string **cents**, not dollars. Its [usage/cost documentation](https://platform.claude.com/docs/en/manage-claude/usage-cost-api) excludes Priority Tier costs. Upload provenance and completeness are admin attestations, not independently verified provider evidence. Do not upload credentials, headers, prompts, or responses.

## Dashboard workflow

Open **Admin → Usage & costs → Provider reconciliation** (`/admin/costs?tab=provider-reconciliation`). Platform Admins and Auditors can open frozen comparisons; only Platform Admins see **Upload & manage**. Workspace administrators and ordinary users have no access. Inference keys and admin API tokens cannot use these routes. A write refused after live Admin demotion refreshes the session and removes write controls; it does not report success.

1. **Upload report:** select an Anthropic envelope JSON file in the format below, then choose **Upload report**. The browser sends the file bytes unmodified, without parsing/re-stringifying them; this preserves duplicate-key detection by the server. Empty files and files over 5 MiB are refused before sending; the server validates all evidence. A successful response shows the retained import ID, UTC coverage, signed total and line count. **Existing import** resumes an already retained revision without downloading its raw evidence. Response pages retain their original JSON text; the server serializes the enclosing evidence envelope, so stored evidence is not a byte-for-byte copy of the entire uploaded file.
2. **Mappings:** review provider dimensions and their **Draft period coverage**, then add or edit attested bindings. Choose an Anthropic connection, optional deployment/workspace UUIDs, effective UTC interval and opaque evidence reference. Exclusive attribution requires an OMG workspace and no non-OMG traffic. Coverage means an attested binding covers the **whole UTC day**, not that a current credential or a partial-day interval proves ownership. Apply each binding to the draft, review the **whole replacement revision**, supply its revision evidence reference, confirm and save. Existing bindings remain unless explicitly removed. Explicit model-mapping pairs are preserved and displayed read-only; editing those pairs is API-only. Switching tabs retains drafts; leaving with unsaved mapping changes requires confirmation.
3. **Freeze snapshot:** review the exact import and mapping revision IDs and choose **Freeze reconciliation**. The result returns a snapshot ID; **Open snapshot** opens that immutable comparison. Freezing does not change monetary ledger entries, governance reservations, inference executions, prices, budgets or rollups. Later usage, mapping changes and imports never recompute an existing snapshot.

An identical upload resolves to the existing import, not another charge. Changed exact-coverage reports require `supersedes` in the uploaded envelope naming the current revision; partial overlaps and stale/branching revisions return a conflict. The browser shows the server's refusal and clears the prior upload result rather than implying success. A stale mapping save also shows the conflict, retains the draft and blocks saving. Choose **Reload latest mapping and discard draft**, review the latest whole revision and attest again; nothing is silently overwritten.

The comparison tabs separate **Comparison**, **Estimates & holds**, **Unresolved coverage**, **Personal totals**, and **Source & provenance**. Charges and variance support signed amounts, including credits; absent or unsupported comparisons show **Unknown**, and incomplete estimates show **Incomplete**, never an invented zero variance. Holds are separate known floors, not extra settled charges or proven upper bounds. Overlapping unresolved coverage is not additive. Personal activity is combined into identity-free totals with no personal-workspace, key or request drill-down; those totals are subsets of snapshot-wide totals, not additional charges. Shared actual charges are never prorated. Missing categories and Priority Tier costs are **not supplied**, not zero; neither the upload nor the UI verifies an invoice.

## Authority and routes

All paths below start with `/api/v1/platform`. **Browser sessions only:** Platform Admin creates imports, mappings and reconciliations; Platform Admin/Auditor reads. Ordinary users, workspace administrators, inference keys and **all admin API tokens**, regardless of scope, are refused. Writes require the normal same-origin/CSRF protections and recheck live Admin authority in their transaction.

| Method | Path | Result |
| --- | --- | --- |
| POST | `/provider-cost-imports` | `{ "id": "…" }`, including identical reuploads |
| GET | `/provider-cost-imports` | Import metadata pages |
| GET | `/provider-cost-imports/{id}` | Metadata and retained `evidence_json` text |
| GET | `/provider-cost-imports/{id}/lines` | Normalized line pages |
| POST | `/provider-cost-mappings` | New immutable whole mapping revision |
| GET | `/provider-cost-mappings` | Mapping metadata pages |
| GET | `/provider-cost-mappings/{id}` | Mapping metadata and bindings |
| POST | `/provider-reconciliations` | New frozen aggregate snapshot |
| GET | `/provider-reconciliations` | Snapshot metadata pages |
| GET | `/provider-reconciliations/{id}` | Metadata and `snapshot` |

Lists and lines accept only `limit` (default 100, maximum 200) and `offset` (default 0, maximum 100000), returning `{data,limit,offset}` ordered by immutable ID or line number. Details accept no query parameters. This slice has no cost-center, key, request or actor filter. Unknown write fields and unsupported query parameters are refused, not interpreted as allocation instructions. Future cost-center filtering must use admission snapshots and cannot allocate shared actual charges merely by filtering estimates.

## Upload a complete closed period

The upload is one JSON envelope with an account identifier, opaque evidence reference, export timestamp, optional `supersedes`, original initial request parameters and **ordered response pages**. The account identifier is supplied by the Admin; it is not an inference credential or inferred from a current credential reference.

Use daily UTC `[starting_at,ending_at)` coverage of **1–31 whole closed days**, `bucket_width:"1d"`, both `workspace_id` and `description` in `group_by`, and request `limit` 1–31. Include empty daily buckets with `results:[]`. The export timestamp must be after the period ends (or equal to its end) and not in the future. Missing/repeated days, repeated rows/pages, broken pagination or a final page that still has more data are refused. Intermediate pages need distinct nonempty `next_page` cursors; the final page must explicitly have `has_more:false,next_page:null`.

Example with placeholder IDs, based on the documented format:

```json
{
  "account_id": "organization_placeholder",
  "evidence_reference": "finance:export-1",
  "exported_at": "2025-08-03T00:00:00Z",
  "supersedes": null,
  "request": {
    "starting_at": "2025-08-01T00:00:00Z",
    "ending_at": "2025-08-02T00:00:00Z",
    "bucket_width": "1d",
    "group_by": ["workspace_id", "description"],
    "limit": 31
  },
  "pages": [{
    "data": [{
      "starting_at": "2025-08-01T00:00:00Z",
      "ending_at": "2025-08-02T00:00:00Z",
      "results": [{
        "amount": "123.78912",
        "currency": "USD",
        "workspace_id": "wrkspc_placeholder",
        "description": "Placeholder model - Input Tokens",
        "model": "claude-placeholder",
        "service_tier": "standard",
        "context_window": "0-200k",
        "cost_type": "tokens",
        "token_type": "uncached_input_tokens",
        "inference_geo": "global",
        "speed": "standard"
      }]
    }],
    "has_more": false,
    "next_page": null
  }]
}
```

JSON extraction failures return static messages, with no uploaded field names or values in responses or rejection logs. Statuses remain 400 for malformed JSON/body failures, 413 for an oversized body, 415 for a missing/unsupported JSON content type, and 422 for invalid typed fields. The upload body limit is handled without echoing its content.

Limits are **5 MiB per upload and 50,000 lines**. Raw response-page nesting is capped at 16 levels; fixed typed envelopes reject other shapes. Dimension/cursor strings are at most 4096 UTF-8 bytes, account IDs 200, evidence references 256 and timestamps 40. References are opaque text: never URLs fetched by the gateway, filesystem paths opened by it, or executable instructions. Unknown JSON fields and duplicate JSON object keys are rejected. Only USD is supported. Evidence is retained append-only, with no deletion path in this slice; only upload material appropriate for permanent financial history.

### Exact money and revision identity

Each signed decimal-cent amount is multiplied by **10,000** using checked integer arithmetic. Sub-micro fractions round **half-even per line**, never after summing. Amount strings are at most 64 bytes with at most 18 fractional decimal places, no exponent, whitespace or leading `+`; values outside checked signed 128-bit arithmetic are refused, never truncated. Aggregates also use checked signed 128-bit arithmetic.

The example produces `amount_microusd:"1237891"`, `original_amount:"123.78912"`, `conversion_residual_microusd:"0.2"`. The residual is exact **unrounded micro-USD minus rounded micro-USD**. For `"-0.00015"`, those values are `"-2"` and `"0.5"`. Original precision remains in `original_amount` and the raw page. Monetary totals/variance are signed integer micro-USD strings, never floats. The residual is explicitly a fractional conversion record, not an integer monetary total.

Lines retain their zero-based page index, page-relative JSON pointer, bucket, full structured dimensions, original amount and residual. Signed credits/adjustments are not inferred from variance. Supplied usage, fee, credit, discount, tax and unknown cost types remain separate; absent categories are **not supplied**, not zero.

An account-scoped canonical fingerprint ignores JSON object-key order, row order within a bucket, pagination boundaries, export timestamps and evidence-reference changes. It includes coverage and original amount strings. An identical reupload returns the original import ID, including an old revision reuploaded after supersession. The first retained provenance is not replaced. Repeated bucket/dimension rows fail even if their amounts differ.

A changed report needs `supersedes` naming the **current exact-coverage revision for the same account**. Partial overlaps, cross-account lineage and branching from an older revision fail with 409. No API automatically sums revisions. Imports and their complete child-line sets cannot be updated, removed, truncated or extended after commit.

## Attest a mapping revision

`POST /provider-cost-mappings` accepts a whole replacement set, not patches:

```json
{
  "account_id": "organization_placeholder",
  "supersedes": null,
  "evidence_reference": "finance:binding-revision-1",
  "bindings": [{
    "provider_workspace_id": "wrkspc_placeholder",
    "connection_id": "11111111-1111-4111-8111-111111111111",
    "deployment_id": null,
    "workspace_id": "22222222-2222-4222-8222-222222222222",
    "effective_start": "2025-08-01T00:00:00Z",
    "effective_end": "2025-08-02T00:00:00Z",
    "exclusive": true,
    "non_omg_traffic": false,
    "attested": true,
    "evidence_reference": "finance:exclusive-use-attestation-1",
    "model_mappings": []
  }]
}
```

**One provider workspace/account binding maps to exactly one OMG connection**, optionally narrowed to a deployment belonging to it. Overlapping bindings cannot be used to collect multiple connections for the same provider workspace. If that provider workspace is shared across OMG connections, leave its actual charges shared/unallocated; a nonexclusive binding can show one connection's estimates, not prove complete coverage of all contributing connections.

Connections must be Anthropic connections; an optional deployment must belong to the connection. Effective intervals use `[start,end)`. `exclusive:true` requires an OMG workspace, `non_omg_traffic:false`, an attestation and evidence reference. Historical credential ownership is **not** inferred from current settings. A nonexclusive binding may identify external traffic; actual charges then remain shared/unallocated even if the observed OMG traffic has only one workspace.

Provider `workspace_id:null` means the provider's default/missing identifier, not OMG Unallocated and not a wildcard over provider workspaces. Overlapping provider-workspace bindings or overlapping reverse ownership of a connection/deployment are refused. Mapping revisions allow at most 1000 bindings and 100 explicit model mappings per binding. A model mapping is `{"provider_model":"…","upstream_model":"…"}`; both names are bounded to 512 bytes. The set can be empty to record explicitly unmapped coverage.

Every new revision must supersede the latest mapping revision for that account. Old revisions stay readable and can be selected explicitly for historical snapshots. Actor, evidence references and bindings are immutable.

## Freeze a reconciliation

```http
POST /api/v1/platform/provider-reconciliations
Content-Type: application/json

{"import_id":"33333333-3333-4333-8333-333333333333","mapping_id":"44444444-4444-4444-8444-444444444444"}
```

The explicit revisions must belong to the same account. The resulting record pins both IDs, query version `aggregate-attempt-start-v1`, database observation timestamp, mapping provenance and aggregate rows. Reading it never recomputes against later usage, mappings or imports.

The matcher uses **raw historical attempts**, restricted to attested connection/deployment intervals, with a maximum of **100,000 attempts**. A larger comparison is refused with 422, not sampled; select shorter coverage or a narrower attested scope. It never retrofits upstream-model/account dimensions from hourly rollups or today's public aliases. A provider model must match historical upstream-model snapshots/observations or an explicit model mapping. Conflicting served/configured versions without an explicit mapping remain unmatched. Two provider model names claiming the same historical estimate aggregate remain ambiguous rather than dividing or counting that estimate twice.

Token charges aggregate by UTC day, provider workspace, structured model and tier; original token/context/description dimensions remain in each aggregate's `dimensions`. Only recognized token dimensions with standard/unspecified speed and global/not-available/unspecified geography are compared in this slice. Other categories/dimensions remain explicit unsupported buckets. Estimates are whole model/tier aggregates, never invented per-request actual charges. A binding must cover a whole provider bucket for matching; a partial-day binding cannot prorate it.

Rows expose imported charges, workspace aggregate estimates, nullable signed variance and reasons:

- `provider_only`, `estimate_only`: evidence exists on only one comparable side.
- `ambiguous_scope`, `unknown_model`, `unsupported_category`: attribution or dimensions cannot be resolved safely.
- `shared_unallocated`: one actual bucket retained without allocation; workspace estimates appear alongside it, **without prorating actual charges**. Contradictory observed workspace use also defeats an exclusive attestation.
- `incomplete_estimate`: unresolved/missing reservation, redacted detail or overlapping archived history prevents a complete comparison.
- `timing_basis`: OMG uses attempt-start UTC; provider accounting buckets need not use that clock.

When an attempt lacks historical model or tier detail (including a missing reservation), every compatible comparison bucket is marked `incomplete_estimate` and its variance is null. A missing tier can affect both standard and batch; a missing model can affect multiple models. Known workspace subtotals remain visible with `complete:false`. `unresolved_coverage` separately reports compatible unresolved shared-workspace coverage; it overlaps the snapshot's total estimates and estimate-only rows and may appear in multiple buckets. **Do not sum it across buckets or add it to estimates or holds.** Personal coverage is retained only in the identity-free personal totals.

`settled_estimate_microusd` is a known configured-rate subtotal. `held_floor_microusd`, unresolved counts and unbounded-hold counts stay separate; `holds_upper_bound_microusd` is null, not a claim that a retained floor bounds the final charge. Variance is null for shared or incomplete comparisons. Category coverage says `supplied`/`not_supplied`. The overall snapshot always states `verified_invoice:false,complete:false`; this source does not establish invoice completeness.

Personal activity is projected **before the immutable snapshot is stored**, identically for Admin and Auditor reads. `personal_activity.estimates` combines personal activity across workspaces, days, models and tiers, including matched and estimate-only activity. Exclusively attributed personal charges are combined in `exclusively_attributed_imported_microusd`, with signed category subtotals in `exclusively_attributed_imported_by_category`; shared actual charges are never allocated to that total. These totals are subsets of the snapshot-wide totals, not additional amounts.

This is deliberately stricter than the platform cost report, usage explore and storage usage, which show a personal workspace's id and owner label with its totals ([personal workspace visibility](platform-administration.md#ownership)). Personal activity never appears in model-linked workspace estimates or estimate-only rows. Exclusive personal actual buckets also collapse into the identity-free financial totals, without model-linked variance or dimensions. Personal bindings are omitted from the snapshot's inline `mapping_provenance`; the selected immutable mapping revision retains the original operator attestation. No personal workspace UUID, historical upstream model, key ID, request/root ID, actor activity, public alias, prompt or response is attached to personal activity in the frozen snapshot.

All existing prices, ledger entries, reservations, policy budgets and usage rollups remain unchanged. Reconciliation does not settle, release, refund or rewrite anything in the inference accounting path.

## Explicit schema operation

Migration **0064_provider_cost_reconciliation** adds five append-only history tables and a connection/deployment scoped foreign key. It briefly locks `deployments` to add the supporting unique constraint. Runtime grants on the new tables are **SELECT/INSERT only**, backed by append-only triggers, complete-revision guards and executable UPDATE/DELETE/TRUNCATE refusal probes in `grants verify`.

Migration 0064 shipped with the API evidence slice; the dashboard adds no migration or runtime grant. Apply the complete recognized migration chain and runtime grants explicitly to an operator-selected database, following the [upgrade guidance](operations.md). Never replace a migration prefix, add placeholders or initialize a legacy installation implicitly. `serve` never migrates or bootstraps an installation.
