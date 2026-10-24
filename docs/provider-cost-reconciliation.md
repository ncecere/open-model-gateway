# Provider-reported cost reconciliation

This API-only first slice of #48 compares uploaded **Anthropic Claude Platform Cost Report** charges with frozen aggregates of OMG's configured-rate estimates. It is **not a verified invoice**, a complete payable balance, or customer invoicing. There is no UI, automatic provider pull, organization credential, outbound fetch, or change to estimated costs or holds.

Anthropic's [Cost Report format](https://platform.claude.com/docs/en/api/beta/organization/cost_report/retrieve) uses decimal-string **cents**, not dollars. Its [usage/cost documentation](https://platform.claude.com/docs/en/manage-claude/usage-cost-api) excludes Priority Tier costs. Upload provenance and completeness are admin attestations, not independently verified provider evidence. Do not upload credentials, headers, prompts, or responses.

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

`settled_estimate_microusd` is a known configured-rate subtotal. `held_floor_microusd`, unresolved counts and unbounded-hold counts stay separate; `holds_upper_bound_microusd` is null, not a claim that a retained floor bounds the final charge. Variance is null for shared or incomplete comparisons. Category coverage says `supplied`/`not_supplied`. The overall snapshot always states `verified_invoice:false,complete:false`; this source does not establish invoice completeness.

Personal activity is exposed only as financial aggregates, never key IDs, request/root IDs, actor activity, public aliases, prompts or responses. All existing prices, ledger entries, reservations, policy budgets and usage rollups remain unchanged. Reconciliation does not settle, release, refund or rewrite anything in the inference accounting path.

## Explicit schema operation

Migration **0064_provider_cost_reconciliation** adds five append-only history tables and a connection/deployment scoped foreign key. It briefly locks `deployments` to add the supporting unique constraint. Runtime grants on the new tables are **SELECT/INSERT only**, backed by append-only triggers, complete-revision guards and executable UPDATE/DELETE/TRUNCATE refusal probes in `grants verify`.

**0058–0063 are reserved by the preceding PR chain. Merge this migration last, after that ordered prefix exists.** Do not add placeholders, relax lineage readiness, or apply this branch as a replacement for the prefix. The coordinator must explicitly select a fresh database, apply the complete migration chain and runtime grants, and run PostgreSQL/runtime-role and running-gateway acceptance before merge. `serve` never migrates or bootstraps an existing installation implicitly.
