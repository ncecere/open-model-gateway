# Routing policies and passive health

Routing operates on deployments that the inference engine has already authorized for the credential's organization/workspace and filtered for protocol and capabilities. It does not choose providers by name, bypass model grants, resolve secrets, or make health-probe requests. Provider adapters remain registry entries.

## Defaults and ordering

A model without a policy uses `priority`, `max_attempts = 1`, `allow_ambiguous_failover = false`, `failure_threshold = 3`, and `cooldown_seconds = 30`. A deployment without routing configuration has priority `0`, weight `1`, residency `unspecified`, and no operator disablement.

- **Priority:** lower signed integer priorities run first. Ties retain the repository's deterministic deployment order (`created_at`, then ID). Default configuration therefore preserves first-available selection without retries.
- **Weighted:** priorities remain strict tiers; weights only affect ordering within the same tier. Selection is weighted without replacement, so a deployment never occurs twice in a plan. Weights are relative, not percentages. A gateway-generated request UUID, organization UUID, and model name seed the ordering; the same candidate snapshot and request produce the same order. Do not use client-controlled request IDs as a routing random source.
- Operator-disabled deployments and circuits whose `open_until` is in the future are excluded. Disabled model/deployment/provider connection rows are rechecked while loading routing metadata.
- Empty eligible sets return `ModelUnavailable`. More than **256** input candidates, duplicate deployment IDs, or invalid routing configuration are rejected rather than silently truncated. The repository fetches at most 257 enabled authorized deployments and rejects configurations exceeding 256 rather than truncating them. Adapter capability filtering then precedes routing.

`RoutePlan.deployment_ids` is a prioritized list, not permission to attempt every entry. The engine must enforce `max_attempts` (1–3), available IDs, one shared hard deadline, and admission/accounting for each actual attempt. Planning itself does not execute or retry anything.

## Failover and residency

Retries/failover are opt-in by setting `max_attempts` above one. Even then, every fallback must have the **same exact, non-`unspecified` residency label** as the first selected deployment. If the first selection is `unspecified`, no fallback is included—even another `unspecified` deployment. The first candidate is selected normally before applying this restriction; enabling retries does not steer the first request toward a different residency.

`required_residency` optionally fences **all primary and fallback selection**, including after a preferred route enters cooldown. Only exact matching labels are eligible; `unspecified` cannot be a required label. Without this requirement, a weighted model can distribute independent requests across regions, while an individual retry-enabled request still cannot fall back across its selected residency boundary. Routing policies, priorities, weights and residency labels are centrally controlled by platform operators; organization administrators cannot alter them. Labels are operator assertions; the gateway does not verify physical provider processing locations or derive residency from provider names/regions. Set consistent labels only after reviewing the providers' actual residency guarantees. Labels use 1–64 lowercase ASCII letters/digits and `._-`, starting with a letter/digit. `unspecified` is reserved for unknown residency.

`may_failover` permits:

| Attempt error | Another attempt? |
| --- | --- |
| Provider `Busy` | Only with `max_attempts > 1` |
| `UpstreamUnavailable` | Only with `max_attempts > 1` **and** `allow_ambiguous_failover = true` |
| Timeout, invalid upstream output, rejected/invalid request, unsupported capability, configuration, storage, unavailable model | Never |

**Ambiguous transport failures can have already incurred charges or executed tool-generating work upstream.** Enabling ambiguous failover accepts possible duplicate provider execution/charges; it is not a delivery or idempotency guarantee. Keep it off unless this risk is acceptable.

The engine must never invoke failover after receiving any `ProviderOutput::Stream`, including an unpolled stream. Stream errors, cancellation, truncation, and failures after response commitment cannot switch deployments. The shared deadline is never restarted between attempts. Local concurrency rejection is not a provider observation.

The stream watchdog drops upstream transport and releases process-local capacity at the deadline even when downstream is not polling. Lease reconciliation later marks unresolved work unknown without refunding charges. Downstream inactivity/local deadlines do not count as provider failures.

## Passive health

`record_result` records only actual provider outcomes:

- Validated success resets consecutive failures to zero and clears `open_until`.
- Provider `Busy`, `UpstreamUnavailable`, and provider-operation `Timeout` increment the count. Reaching the model's threshold opens the circuit for its configured cooldown. Further failures at/above threshold extend the cooldown.
- Invalid/rejected requests, invalid upstream output, unsupported capabilities, unavailable models, local configuration errors, and storage errors leave observations unchanged.
- Do not record success when opening a stream. Success requires validated completion, including a valid terminal `Done` for a stream. Do not report cancellation or local admission/deadline/accounting failures as provider failures.

Updates use a single PostgreSQL upsert and row locking, so concurrent failures do not lose increments. Counters saturate at the maximum signed 32-bit integer. Concurrent completions are observed in database update order; a successful in-flight request can reset failures recorded by another in-flight request. Policy is read at recording time, so administrative changes affect subsequent observations.

Cooldown is **bounded time-based reopening, not an exclusive half-open probe**. Once `open_until` expires, new requests can select the route again, potentially concurrently. Planning claims no lease, avoiding cancellation leaks between selection and execution; it does not implement a one-request half-open gate. An unsuccessful request after reopening opens another cooldown; a validated success resets the circuit. Existing plans are snapshots, and health/config changes cannot revoke an already-running attempt. The engine may recheck eligibility immediately before admission if it requires fresher selection.

`health(store, organization_id, deployment_id)` returns only the deployment ID, operator-disabled flag, failure count, circuit timestamp/state, and last-observed timestamp. It returns no endpoint, credential reference, upstream body, prompt, or raw error. **No observation means unknown, not healthy.** A closed/expired circuit means eligible, not proof of availability. There are no synchronous external probes or fabricated success metrics. Management callers must independently authorize the current platform operator; inference credentials do not authorize management health views.

## Storage contract

Base migration: `0005_routing.sql`; `0007_platform_catalog.sql` converts configuration and passive health to platform-global resource keys. Historical organization IDs are nullable provenance only, never ownership or authorization. All three tables reference their global parent model/deployment and cascade when that parent is deleted. Routing configuration is separate from `Deployment`, with no provider-specific columns or switches.

### `model_routing_policies`

Primary key and model foreign key: `model_id UUID`.

| Column | Type | Default / constraint |
| --- | --- | --- |
| `strategy` | text | `priority`; `priority` or `weighted` |
| `max_attempts` | integer | `1`; 1–3 |
| `allow_ambiguous_failover` | boolean | `false` |
| `failure_threshold` | integer | `3`; at least 1 |
| `cooldown_seconds` | integer | `30`; 1–3600 |
| `required_residency` | nullable text | none; valid label other than `unspecified` |

### `deployment_routing`

Primary key and deployment foreign key: `deployment_id UUID`.

| Column | Type | Default / constraint |
| --- | --- | --- |
| `priority` | integer | `0`; full signed 32-bit range |
| `weight` | integer | `1`; 1–1000 |
| `residency` | text | `unspecified`; label format above |
| `operator_disabled` | boolean | `false` |

### `deployment_route_health`

Primary key and deployment foreign key: `deployment_id UUID`.

| Column | Type | Default / constraint |
| --- | --- | --- |
| `consecutive_failures` | integer | `0`; nonnegative |
| `open_until` | timestamptz, nullable | no cooldown |
| `last_observed_at` | timestamptz | `now()` on insertion; updated on observed outcomes |

No health row is created merely by reading/planning. Management writes validate complete policy/config payloads, enforce current platform-operator authority, and audit changes without storing credentials or prompt data.

## Tests

Pure unit tests cover deterministic defaults, stable priorities, weighted distribution and strict tiers, disabled/open exclusion, residency safety, failover classification, invalid configuration, duplicates, and candidate limits:

```sh
cargo test -p open-model-gateway --lib routing::tests
```

PostgreSQL tests additionally cover threshold opening, cooldown expiry/reopening, successful reset, non-provider errors, atomic concurrent increments, and composite tenant constraints. They require a `DATABASE_URL` whose user can create test databases:

```sh
cargo test -p open-model-gateway --features integration-tests --lib routing::tests::database
```
