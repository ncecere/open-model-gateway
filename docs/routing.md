# Routing and passive health

Routing receives an already authorized, protocol/request-capability-filtered set of deployments. It does not grant model access, resolve credentials or probe providers. Every actual attempt still revalidates live workspace/key authorization and target configuration during admission.

## Defaults and configuration

A model without policy uses priority ordering, one attempt, ambiguous failover disabled and no required residency. A deployment without routing metadata uses priority 0, weight 1, unspecified residency, failure threshold 3 and cooldown 30 seconds.

The enterprise schema separates:

| Resource | Fields |
| --- | --- |
| `routing_policies` (model) | `strategy`, `max_attempts`, `allow_ambiguous_failover`, nullable `required_residency` |
| `deployment_routing` | `priority`, `weight`, nullable `residency`, **`failure_threshold`, `cooldown_seconds`** |
| `deployment_health` | `consecutive_failures`, `open_until`, `last_observed_at` |

Threshold/cooldown are deployment settings, not model API fields, even though an internal routing type retains default fields. Disable a deployment through its `enabled` state, not a routing `operator_disabled` flag. Admin writes/Auditor reads use [governance API](governance-api.md).

- **Priority:** lower signed integer first; ties retain deterministic repository order (`created_at,id`). Defaults preserve first-eligible selection without retries.
- **Weighted:** strict priority tiers, weighted order without replacement within a tier. Weights 1–1000 are relative, not percentages. Workspace UUID, gateway-generated request UUID and public alias seed ordering; caller IDs are not trusted random sources.
- Open circuits and disabled model/deployment/provider rows are excluded. More than 256 candidates, duplicate IDs and invalid settings fail rather than silently truncate. Repository fetches 257 to detect overflow before adapter filtering.
- A route plan is not permission to execute every candidate. Engine enforces attempt count (1–3), available IDs, one shared hard deadline and separate admission/accounting.

## Failover and residency

Retries are opt-in with `max_attempts>1`. Every fallback requires the **same explicit residency label** as the first selected target. An unspecified first target permits no fallback, even to another unspecified target. Required residency fences all primary/fallback choices, including when preferred routes enter cooldown.

Labels are 1–64 lowercase ASCII letters/digits and `._-`, starting with a letter/digit. API null means unspecified; the literal `unspecified` cannot be supplied as a required/asserted label. Labels are operator assertions, not verified provider geography or a derivation from provider names/regions.

| Failure before stream return | Eligible for another attempt? |
| --- | --- |
| Provider Busy | With attempts remaining |
| Upstream unavailable / ambiguous transport | Only with `allow_ambiguous_failover:true` and attempts remaining |
| Timeout, invalid output/request, unsupported capability, storage/configuration failure | No |

Ambiguous failover may duplicate upstream work or charges. It is not idempotent delivery. No fallback occurs after a provider stream is returned, even before the first event is polled. Deadlines do not restart between attempts. Local capacity/admission rejection is not a provider-health observation.

## Passive observations

Validated completion resets failures and clears cooldown. Provider Busy, UpstreamUnavailable and provider-operation Timeout increment the deployment's failures; reaching its threshold opens/extends its configured cooldown. Invalid requests/output, unsupported options and local configuration/storage/accounting errors leave observations unchanged. Opening a stream is not success; terminal validated completion is required. Local cancellation/deadline inactivity is not a provider failure.

PostgreSQL upsert/row locking prevents lost failure increments; counts saturate at signed-32 maximum. Concurrent completions are observed in database order, so an in-flight success can reset other recorded failures. Deployment thresholds are read at recording time.

Cooldown expiry reopens eligibility by time, **not an exclusive half-open probe**. Multiple requests can select a newly reopened route; planning holds no probe lease. Existing plans/running attempts are snapshots, not retroactively revoked by health changes.

Management health includes failure count, nullable last observation and cooldown timestamps. No row/observation means unknown, not healthy. Closed/expired circuits mean eligible, not verified uptime. No external probes or provider response bodies appear in these views.

## Cancellation and evidence

Deadline watchdogs drop upstream transport even with unpolled downstream streams. Dropping transport cannot guarantee upstream cancellation or zero cost; expired leases later become unknown holds rather than refunds. See [governance](governance.md).

Current implementation is `apps/gateway/src/routing.rs` plus engine/admission code. Unit and disposable-PostgreSQL tests are present; commands such as `cargo test -p open-model-gateway --lib routing::tests` do not themselves establish a fresh pass here. Historical routing milestones in [verification](verification.md) refer to earlier builds, not validation of this enterprise lineage or production load.
