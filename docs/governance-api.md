# Governance API

All paths below are under `/api/v1`, authenticated by browser session. Mutations require exact Origin/CSRF. Platform reads allow Admin/Auditor; writes require Admin unless a workspace/key tightening operation permits scoped authority. Personal keys and request details remain owner-private.

Money and financial aggregate/token counts are decimal **strings**; unknown measurements are `null`, never zero. Limit fields, pagination and attempt numbers remain JSON numbers. USD values are exact integer micro-USD. Configured rates are estimates, not invoices.

## Policy payloads

| Path | Methods / layer |
| --- | --- |
| `/platform/installation/policy` | GET/PUT additional shared ceilings; optional budget. |
| `/platform/workspace-types/{kind}/policy` | GET/PUT per-workspace defaults, kind personal/team/project. |
| `/platform/workspaces/{ws}/policy` | GET/PUT replacement override; DELETE restores live type defaults. |
| `/workspaces/{ws}/policy` | GET/PUT local tightening by owner/shared administrator. |
| `/workspaces/{ws}/keys/{key}/policy` | GET/PUT visible key-lineage tightening; rotations retain consumption. |

PUT accepts the **inner** object, with the three rate fields explicit plus one budget form, and returns `{ok:true}`. `concurrent_jobs` ("Jobs at once", 0018) is optional: absent keeps the stored value, null clears it (subject to tighten-only rules). GET always returns it. `storage_bytes` ("Storage", 0020; bytes, 1 to 2^50) works the same way on the workspace-type default, platform override and workspace local layers. Installation and key layers have no storage limit: GET returns null, and a non-null value on their PUT is `400`. Workspace policy responses add `storage:{quota_bytes, used_bytes, usage_visible}` ([Files API](files-api.md#storage-quota)).

```json
{"requests_per_minute":120,"tokens_per_minute":null,"concurrent_requests":8,"concurrent_jobs":2,
 "budgets":[{"period":"day","amount_microusd":"5000000"},{"period":"month","amount_microusd":"100000000"}]}
```

**Stacked budgets.** Each scope (installation, workspace-type default, platform workspace override, workspace local, key lineage) holds at most one budget per period: `day` (UTC day), `week` (ISO week from Monday 00:00 UTC), `month` (UTC calendar month) or `lifetime` (all time since the scope was created). Every applicable budget is enforced. `budgets` is the full replacement set (at most four entries, unique periods, positive integer micro-USD strings; `[]` clears).

**Deprecated single-budget fields.** Instead of `budgets`, a body may carry the legacy `monthly_budget_microusd` (string or null, required in this form) and optional `budget_period`; it replaces the set with that one budget (or none). Absent `budget_period` keeps the single stored budget's period (`month` for none; a new workspace override starts from the layer it replaces); explicit null or an unknown value is rejected. Sending both forms is `400`. A legacy body for a layer that stores more than one budget returns `409` `reason:"stacked_budgets_require_budgets_field"` instead of silently dropping budgets. On GET, `monthly_budget_microusd`/`budget_period` mirror the smallest budget of that object (ties prefer the shorter period), or `null`/`"month"` when there is none. Audit metadata records the mirrored `budget_period`/`previous_budget_period` and the budget `count`.

Every GET policy object (`policy`, `effective`, `provenance.*`) includes `budgets:[{period,amount_microusd}]` sorted day, week, month, lifetime. Installation/type GET returns `{policy:...}`. Workspace/key GET also returns `effective`, `mode:"inherit"|"replace"`, `provenance:{platform_source:"type_default"|"workspace_override",platform,local,key}` and `budgets`. `key` is null outside a key scope. The platform workspace GET adds `provenance.type_default` (the live type default, also while an override replaces it). `effective` takes the minimum rate caps and, per period, the minimum budget across platform, local and key layers; budgets of different periods all apply independently. The top-level `budgets` lists every applicable layer × period window:

```json
{"layer":"local","period":"day","amount_microusd":"5000000","monthly_budget_microusd":"5000000","budget_period":"day","window_start":"2026-10-08T00:00:00Z","window_end":"2026-10-09T00:00:00Z","usage_visible":true,"used_microusd":"1250000","unresolved_usage":false,"exhausted":false}
```

`layer` is `platform`, `local` or `key`. Lifetime windows report the scope's creation time as `window_start` and `window_end:null`. `used_microusd` is settled actual plus active holds admitted in that window (a lower bound when `unresolved_usage`); `exhausted` is `used >= amount`. They are present only where the caller may see that scope's activity: workspace-wide layers for platform readers and workspace administrators, the key layer to anyone who may read that key's policy; otherwise `usage_visible:false` and usage fields are null. Effective workspace limits and budgets never expose installation headroom; additional installation ceilings can independently deny admission.

Local/key PUTs (and key creation with an initial policy) are tighten-only per period: see [budget periods](governance.md#budget-periods). A rate cap above a parent's, or a budget for period P above a parent's budget for the same P, returns 400; other period combinations are independent. Removing or raising a stored cap or a stored budget for any period returns 403.

### Policy rejection reasons

Tighten-only rejections keep their HTTP status and add a stable `error.reason` plus one detail field. They never include an amount, so a caller learns which rule failed but not another scope's value. The first violation is reported (parent checks before stored-value checks, rate caps before budgets).

| Status | `reason` | Detail | Cause |
| --- | --- | --- | --- |
| 400 | `exceeds_parent_budget` | `period` | Budget for P is above a parent budget for the same P. |
| 400 | `exceeds_parent_rate` | `limit` | `requests_per_minute`, `tokens_per_minute`, `concurrent_requests`, `concurrent_jobs` or `storage_bytes` is above a parent cap. |
| 403 | `stored_budget_raise_not_allowed` | `period` | Raises the stored budget for that period. |
| 403 | `period_change_not_allowed` | `period` | Removes the stored budget for that period, including moving it to another period. |
| 403 | `stored_rate_loosen_not_allowed` | `limit` | Raises or clears a stored rate cap. |

Malformed bodies remain plain `400` without a reason. `PUT /workspaces/{ws}/policy` on a personal workspace returns `403` `reason:"personal_limits_platform_controlled"`: personal limits come from the personal type default, an optional platform override and the installation policy. Earlier personal local rows stay enforced and are shown read-only in `provenance.local`; per-key caps remain editable.

## Inference admission denials

Admission denials happen before dispatch and consume no quota. All use HTTP **429**, following OpenAI's `insufficient_quota` convention rather than 402, but they have distinct codes:

| Cause | Chat/Responses/Embeddings/Rerank/System One `error.type` / `error.code` | Message |
| --- | --- | --- |
| Request/token minute or concurrency limit (gateway or provider) | `rate_limit_error` / `rate_limit_error` | Gateway or provider concurrency/rate limit reached |
| The attempt's token reservation (price `input_token_limit` plus output reservation) alone exceeds a tokens-per-minute limit | `rate_limit_error` / `token_reservation_exceeds_limit` | The model's input+output token ceiling exceeds this API key's/workspace's (or the installation-wide) tokens-per-minute limit; lower the price ceilings or raise the limit |
| A video or batch job would exceed a "Jobs at once" limit ([async jobs](async-jobs.md#job-limits)) | `rate_limit_error` / `job_limit_exceeded` | Too many jobs are running for this API key/workspace (jobs at once limit); wait for one to finish or cancel one, or the installation-wide jobs at once limit is reached |
| Budget for the layer's current period would be exceeded | `insufficient_quota` / `budget_exceeded` | Budget for this API key/workspace would be exceeded in its current period, or Installation-wide budget cannot admit this request in its current period |
| Unresolved unbounded-cost usage in the layer's current budget window | `insufficient_quota` / `unresolved_usage` | Unresolved usage with unbounded cost blocks budgeted admission for this workspace/API key until reconciled |

**Unbounded prices are not budget denials.** Under pricing v3, a pinned price whose hold cannot be bounded (a possibly-used meter without a line, stated `unknown`, or without `max_units`) cannot be checked against any budget. Whenever a budget applies, admission is refused before dispatch with HTTP **503** `price_unbounded` (OpenAI `error.type` equals the code; `/v1/messages` uses `api_error`), aligned with the 503 `provider_configuration_error` an unpriced route gets under a budget, and with `x-should-retry: false`, because only publishing a complete price helps. It reports no scope, amount or headroom. Without any budget the attempt is admitted with an unbounded hold. Realtime window growth on such a price is refused the same way. V1/v2 unbounded prices keep the legacy `provider_configuration_error` denial.

Budget, unresolved-usage and `token_reservation_exceeds_limit` denials (and `price_unbounded`) also send `x-should-retry: false` because retrying does not help (the last can never succeed until the price's ceilings or the limit change; it ranks with budget denials by scope, after them at the same scope). The official OpenAI and Anthropic SDKs honor this header. `/v1/messages` keeps Anthropic's `rate_limit_error` type for every 429 and uses the message to show which case applies. When layers deny at the same time, the narrowest scope wins (key, then workspace, then installation), and budget/accounting denials take precedence over rate limits (`job_limit_exceeded` ranks between them; it is retryable). Messages name only the scope kind, never amounts, identities or headroom. The installation-wide ceiling always reports `budget_exceeded`, even when another workspace's unresolved usage is the cause, so the denial never reveals another scope's activity. Reconcile unresolved attempts with [evidence-backed reconciliation](#evidence-backed-reconciliation).

An all-null platform override still replaces defaults. Local/key null cannot remove a previously stored cap. Child limits never reserve capacity or remove parents. Requests/tokens use fixed UTC minutes; each budget uses its own UTC day, ISO week, calendar month or lifetime window by admission time. Budget changes and key rotation never reset or rewrite consumption.

## Append-only prices

`GET/POST /platform/deployments/{deployment}/prices`. GET uses `limit=1..200`, `offset=0..100000`, newest first, `{data,has_more}`. POST returns `{id}`:

```json
{
  "pricing_version":2,
  "input_microusd_per_million":"1000000",
  "output_microusd_per_million":"3000000",
  "input_token_limit":8192,
  "output_token_limit":1024,
  "cache_pricing":{
    "read":{"status":"priced","microusd_per_million":"100000"},
    "write":{"status":"priced","microusd_per_million":"1250000"},
    "write_5m":{"status":"priced","microusd_per_million":"1250000"},
    "write_1h":{"status":"priced","microusd_per_million":"2000000"}
  }
}
```

GET adds `id,deployment_id,created_at`. Rates are nonnegative strings. Each cache rate is `priced`, `unknown` or `not_applicable`; all four are explicit in v2. Input ceiling includes cache reads/writes. Embedding-only output ceiling may be zero. Unknown cache rates may be published but cannot establish a finite budget reservation. Rates do not enable request caching.

Omitting `pricing_version` uses legacy v1 with `cache_pricing:null`; the dashboard now publishes v3 price lines (v1/v2 history stays readable). V1 requires normalized inclusive input when billing metadata exists; raw-only legacy observations remain supported. An unknown inclusive total cannot fall back to an exclusive raw count. V1 has no invented cache-component split. Publication never updates past rows or pins a missing historical price. See [cache pricing](cache-pricing.md).

### Pricing v3

`pricing_version:3` POSTs immutable price lines instead of scalar rates (`input_microusd_per_million`/`output_microusd_per_million`/`cache_pricing` must be absent); `max_units` is optional (default `{}`) and accepts only non-token meters with decimal-string ceilings. Every meter the route's workload can use must be stated, as a priced line, `{"meter":...,"not_applicable":true}` or `{"meter":...,"unknown":true}` (a text-generation route below):

```json
{
  "pricing_version":3,"input_token_limit":8192,"output_token_limit":1024,
  "price_lines":[
    {"meter":"input_tokens","microusd_per_batch":"100000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"},
    {"meter":"input_tokens","microusd_per_batch":"500000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input","min_prompt_tokens":100000},
    {"meter":"output_tokens","microusd_per_batch":"500000","batch":1000000,"unit_label":"/M tokens","sku_label":"Output"},
    {"meter":"cache_read_tokens","microusd_per_batch":"10000","batch":1000000,"unit_label":"/M tokens","sku_label":"Cache read"},
    {"meter":"cache_write_tokens","unknown":true},
    {"meter":"cache_write_5m_tokens","not_applicable":true},{"meter":"cache_write_1h_tokens","not_applicable":true},
    {"meter":"output_images","not_applicable":true},{"meter":"input_characters","not_applicable":true},
    {"meter":"input_audio_seconds_ms","not_applicable":true},{"meter":"output_audio_seconds_ms","not_applicable":true},
    {"meter":"search_units","not_applicable":true},
    {"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}
  ],
  "max_units":{"requests":"1"}
}
```

A body that leaves out a meter the route can use returns 400 with `error.reason:"price_meters_incomplete"` (`error.code` is `"400"`, as for every management error) and `error.missing_meters` (for example `["search_units","requests"]`); the message names the workload and the missing meters. `unknown` lines must be exactly `{meter, unknown:true}` and the meter's only line; they value like a missing line (budgeted keys get `price_unbounded`) and are stored as no line, so GET never returns them. The per-workload meter list is in [cache pricing](cache-pricing.md#meter-completeness). Prices published before this rule are unchanged.

Validation, tiers, variants and admission bounds are in [cache pricing](cache-pricing.md#pricing-v3-price-lines-and-meters). GET rows add `price_lines`, `max_units`, `display_lines` (aligned with `price_lines`) and `display_summary`; v1/v2 rows return null for these, v3 rows return null scalar rates. Strings are exact and computed without floating point.

### Batch price lists

A v3 POST may add `batch_price_lines`: the rates the provider publishes for its batch API (for example OpenAI Batch or Anthropic Message Batches). They apply only to **native** batches ([batches](batches.md#pricing-and-budgets)); gateway-run batch lines and interactive requests always use `price_lines`.

```json
{"pricing_version":3,"input_token_limit":8192,"output_token_limit":1024,
 "price_lines":[{"meter":"input_tokens","microusd_per_batch":"2500000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"},
                {"meter":"output_tokens","microusd_per_batch":"10000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Output"}],
 "batch_price_lines":[{"meter":"input_tokens","microusd_per_batch":"1250000","batch":1000000,"unit_label":"/M tokens","sku_label":"Input"},
                      {"meter":"output_tokens","microusd_per_batch":"5000000","batch":1000000,"unit_label":"/M tokens","sku_label":"Output"}]}
```

- **Validation:** same as `price_lines`, and it must cover exactly the same meters (meter, variant, tier, not-applicable and unknown keys). It shares the token ceilings and `max_units`. v1/v2 bodies reject it (400). The example above abbreviates both lists to the token lines; a real body states every meter, as in the previous example.
- **Never derived:** the gateway never computes batch rates from standard ones. Without a batch list, native batches use `price_lines`, and the batch is shown as "No batch price".
- **Pinning:** a native batch's reservation pins the price version and its tier (`governance_reservations.price_tier`: `batch` or `standard`). A later price version never reprices it.
- **GET:** rows add `batch_price_lines`, `batch_display_lines` and `batch_display_summary` (null when absent). The model route detail returns `batch_price_lines` in its current price.

### OpenRouter price suggestion

`GET /platform/deployments/{deployment}/price-suggestion` (Platform Admin) drafts v3 lines for a deployment on an `openrouter` connection (`409` otherwise, `404` if the slug is not in the catalog, `502` if the catalog is unavailable). The server fetches the public `https://openrouter.ai/api/v1/models?output_modalities=all` (and `/images/models/{id}/endpoints` for image models) without any key, over HTTPS with redirects/proxies/retries disabled, a 5-second deadline, a 4 MiB body cap and a 10-minute cache. No database lock is held during the fetch.

Response: `{deployment_id,upstream_model,source:"openrouter_public_catalog",catalog_model_id,workload,needs_review,lines,warnings,display_lines,draft}`. `lines` are annotated (`needs_review`, `source`, optional `note`; unknown prices have `microusd_per_batch:null`). `draft` is a v3 POST body with the fully priced and not-applicable lines, plus `{meter, unknown:true}` for each meter the catalog does not price, so it states every meter. Its token ceilings are conservative defaults, never the full context window: `input_token_limit = min(context_length, 8192)` (8192 when the catalog lists none; 0 when every input-family token meter is not applicable) and `output_token_limit = min(max_completion_tokens or context_length, 1024)` (0 when output tokens are not applicable), so imported routes fit default tokens-per-minute limits. `ceilings` reports these with the catalog's `context_length` and `max_completion_tokens`. The dashboard keeps ceilings an admin already entered when re-importing (rates only) and asks before replacing them. For token-priced workloads the server also reads `/models/{id}/endpoints` and prices from one concrete endpoint: the endpoint whose list price matches the provider-reported cost (`usage.cost` evidence, within per-part rounding) of up to 20 recent succeeded attempts on this route, otherwise the cheapest currently available endpoint (flagged `needs_review` when a pricier endpoint exists, because OpenRouter may route there). `endpoint` reports `{provider,selection:"evidence"|"cheapest"|"catalog",matched_attempts,endpoint_count,input_rate}` and a warning names the provider and rate used; `catalog` means the endpoint list was unavailable and the top-provider catalog price was used. Prompt-size tiers are listed only for the top provider; they are kept when the chosen endpoint's rates match it and otherwise reported for review. Warnings use plain language, not field names. Units come from the model's workload, never the bare catalog key: generation/embeddings/decisions map `prompt`/`completion`/cache keys to token meters with `min_prompt_tokens` overrides as tiers (time-window overrides are reported, not guessed); speech maps `prompt` to `input_characters`; transcription maps `prompt` to input audio interpreted per second and is always `needs_review`; image prices come from per-image endpoint lines (`needs_review`; megapixel/token units and discounts are reported); rerank prices are not in the catalog and are left for manual entry. Rates that are not whole micro-USD per batch are rounded up and flagged. Publishing remains the normal immutable price POST.

## Routing payloads

`GET/PUT /platform/models/{model}/routing`: GET `{policy:...}`; PUT inner fields:

```json
{"strategy":"priority","max_attempts":1,"allow_ambiguous_failover":false,"required_residency":null}
```

`GET/PUT /platform/deployments/{deployment}/routing`: GET `{routing:...,health:{consecutive_failures,open_until,last_observed_at}}`; PUT inner fields:

```json
{"priority":0,"weight":1,"residency":null,"failure_threshold":3,"cooldown_seconds":30}
```

**Failure threshold/cooldown belong to deployments, not model policy.** Weight 1–1000; cooldown 1–3600 seconds; max attempts 1–3. Disabled deployment state is managed through deployment `enabled`, not a routing `operator_disabled` field. Residency is an operator assertion; null means unspecified. No observation means unknown, not healthy. See [routing](routing.md).

## Reports, detail and CSV

- `GET /platform/cost-report`: authorized platform aggregates including personal totals, no request-detail endpoint; platform `service_accounts` breakdown is empty. Each personal workspace's `workspaces` breakdown row is named `Personal · <owner>` (display name, else email, `former user` after cleanup) instead of the bare workspace name.
- `GET /workspaces/{ws}/cost-report`: workspace-wide for administrators, own human-key activity for ordinary members.
- `GET /workspaces/{ws}/costs`: `{data,has_more}` with authorized detail rows, `limit=1..200`, `offset=0..100000`.
- `GET /workspaces/{ws}/usage-export`: one CSV page, `limit=1..1000`, `offset=0..100000`, no-store and formula-safe quoted cells. Headers include `x-export-limit`, `x-export-offset`, `x-export-rows`.
- `GET /workspaces/{ws}/cost-summary`: compatibility current-month totals only; financial values/counts are strings.

Reports/details/CSV require `start_date,end_date` in strict YYYY-MM-DD. UTC **[start,end)**, end exclusive, length **1–93 days**, end no later than tomorrow UTC. Optional `compare=none|previous_period` (default none) compares an equal preceding period. Unknown/repeated parameters, invalid IDs/statuses and underflowing comparison ranges are rejected. Filters: `workspace_id`, `model` (public alias), `provider`, `cost_center_id` (UUID or `unallocated`), `actor_user_id`, `service_account_id`, `accounting_status=pending|unknown|settled|missing`, `key_id` (exact API key; at platform scope a personal-workspace key never matches) and `status` (comma-separated attempt states `succeeded|failed|cancelled|indeterminate|in_progress`, matching any; `in_progress` is a started attempt). Details and CSV apply the same filters. Workspace ID must match the path where supplied. Aggregate routes do not accept limit/offset.

```http
GET /api/v1/platform/cost-report?start_date=2025-01-01&end_date=2025-02-01&compare=previous_period&cost_center_id=unallocated
```

Response shape and interpretation are in [cost reporting](cost-reporting.md). Reports add `meter_usage` (sums of observed meter counters as decimal strings, null when unobserved), `meter_relevant_attempts` (per meter, attempts whose workload can produce it and whose pinned v3 price does not mark it not applicable), `meter_unknown_attempts` (relevant attempts where it was not observed, excluding settled pre-processing rejections; when nonzero the `meter_usage` total is only a lower bound) and `provider_reported_cost_microusd` (evidence sum, not a charge); `cost_components` adds the six v3 meter keys (token keys now sum v2 and v3 settlements). Detail rows add `meter_usage`, `output_image_variant` and `provider_cost_microusd`; CSV appends the same three columns after `cost_center_code`, keeping earlier columns in place. Filters and privacy predicates precede aggregation/dimension discovery. Reports use a full aggregate SQL snapshot, not sampled detail pages, with a ten-second handler deadline including lock acquisition. They are live estimates, not frozen statements.

## Evidence-backed reconciliation

`POST /workspaces/{ws}/costs/{execution}/reconcile`, Platform Admin plus normal workspace privacy, returns `{ok:true}`:

```json
{
  "input_tokens":"30","output_tokens":"2",
  "billing_usage":{
    "total_input_tokens":"30","uncached_input_tokens":"10",
    "cache_read_input_tokens":"5","cache_write_input_tokens":"15",
    "cache_write_default_input_tokens":"3","cache_write_5m_input_tokens":"4",
    "cache_write_1h_input_tokens":"8"
  },
  "evidence":"provider-usage-reference-123"
}
```

`billing_usage` must be explicit (nullable for applicable v1 usage). Optional `meter_usage` (all six counters explicit, each a decimal string or null), `output_image_variant` and `provider_cost_microusd` (decimal string) add meter evidence; once recorded, meter counters may only grow and variant/provider cost may not change or be omitted. Evidence is 1–200 non-control characters, not prompts/secrets. Only terminal unresolved records with complete pinned valuation can resolve. All prior raw/normalized observations must be preserved. Identical evidence/full breakdown is idempotent; altered evidence/counts fail closed.

Actual handlers currently return **400** for invalid/conflicting reconciliation evidence, **409** for no complete pinned valuation and **503** for unavailable reconciliation/storage. Do not assume every conflict returns 409 or a machine-specific financial error code: handler errors use the common numeric status-string envelope. This distinction matters when implementing retry/error UI.
