# Governance API contract

All endpoints use existing browser sessions, exact Origin/CSRF for mutations, tenant authorization and sanitized audit events. Inference keys never authorize management. USD monetary values are decimal **micro-USD integer strings**, not floating-point dollars. Costs use configured rates; they are estimates, not vendor invoices.

## Policies

GET/PUT `/api/v1/platform/orgs/{org}/policy` — platform-controlled hard organization ceiling, current operator only.
GET/PUT `/api/v1/orgs/{org}/policy`
GET/PUT `/api/v1/workspaces/{ws}/policy`
GET/PUT `/api/v1/workspaces/{ws}/keys/{key}/policy`

GET returns `{policy:{requests_per_minute:number|null,tokens_per_minute:number|null,concurrent_requests:number|null,monthly_budget_microusd:string|null}}`. PUT accepts the inner policy object (all four fields), returns `{ok:true}`. Null disables that particular scope's limit, never an inherited limit. Scoped GET responses additionally expose a read-only `ceiling` from applicable parent policies. Org admins write local policies; shared workspace administrators may tighten their own workspace/key policies. Explicit values above a parent ceiling are rejected. No child setting reserves a fixed slice or removes a parent limit. Workspace/key reads obey existing workspace/key visibility. Personal workspaces remain owner-private; the organization policy governs them without listing them. A rotated key and its predecessors resolve to the same policy and consumption lineage, so rotating does not reset limits. Separately created keys have separate scopes; organization/workspace limits provide the overall ceiling.

Requests and token limits use fixed UTC minute windows; budgets use UTC calendar months; concurrency uses database leases. Limits apply to upstream attempts (fallbacks consume another admission). Pricing bounds and explicit output token limits are required for token/budget enforcement; no guessed tokenizer counts. Unknown billable usage retains reservations until reconciled.

## Versioned pricing

GET/POST `/api/v1/platform/deployments/{deployment}/prices`

GET `{data:[{id,input_microusd_per_million:string,output_microusd_per_million:string,input_token_limit:number,output_token_limit:number,created_at:string}]}` newest first. POST accepts the four price/limit fields and returns `{id}`. Platform operator reads/writes central pricing; organizations do not configure infrastructure. Versions are immutable and executions pin their admission version. The input limit must be an accurate hard upstream model ceiling, not a guessed input estimate; full ceiling is reserved. Never claim invoice-level accuracy.

## Routing

GET/PUT `/api/v1/platform/models/{model}/routing`

GET `{policy:{strategy:"priority"|"weighted",max_attempts:number,allow_ambiguous_failover:boolean,failure_threshold:number,cooldown_seconds:number,required_residency:string|null}}`; PUT accepts policy, returns `{ok:true}`. Platform operators only. Default one attempt; at most three. Priorities ascending; weights apply within priority tiers. An optional required residency label filters every primary/fallback candidate, including after cooldowns. Fallbacks additionally require identical explicitly configured residency labels. Ambiguous network/unavailability failover is a separate opt-in because it can duplicate provider charges. No failover once a stream is returned, even before its first event.

GET/PUT `/api/v1/platform/deployments/{deployment}/routing`

GET `{routing:{priority:number,weight:number,residency:string},health:{consecutive_failures:number,open_until:string|null}}`; PUT accepts inner routing object, returns `{ok:true}`. All routing configuration is platform-operator-only; organization administrators consume assigned models without managing routes. The label is an operator assertion, not a verified provider geography. Health is passive observed failures/cooldowns, not an external uptime claim.

## Usage/cost reporting

GET `/api/v1/workspaces/{ws}/cost-summary` → `{currency:"USD",known_cost_microusd:string,held_microusd:string,unknown_cost_requests:number,requests:number}` (current UTC month).
GET `/api/v1/workspaces/{ws}/costs?limit=100&offset=0` → `{data:[{id,public_model,provider,state,started_at,input_tokens,output_tokens,price_id,cost_microusd:string|null,reserved_microusd:string|null,cost_status:string}]}`. Same visibility as executions: admins workspace-wide, members own human keys only.
GET `/api/v1/workspaces/{ws}/usage-export?limit=1000&offset=0` → bounded CSV with execution/known-usage/cost fields, no prompts/secrets; pagination required beyond the requested limit.
POST `/api/v1/workspaces/{ws}/costs/{execution}/reconcile` `{input_tokens:number,output_tokens:number,evidence:string}` → `{ok:true}`. Platform operator only AND normal personal-workspace privacy. Evidence is an operator reference to authoritative usage, not a prompt or invoice upload. Only unresolved terminal records, pinned original rates; ledger/audit recorded atomically.

Collection limits otherwise follow existing management pagination. Configuration is transactional with audit records. Automatic crash reconciliation must close stale executions without pretending unknown charges were zero. Retention never deletes unsettled reservations or the monetary ledger.
