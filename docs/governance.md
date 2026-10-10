# PostgreSQL governance

## Admission boundary

PostgreSQL is the durable source of truth across replicas. Admission and settlement follow the [scoped lock order](#scoped-admission-lock-order) (0027): the catalog advisory lock, then authority locks of the key's workspace type, workspace, user and key lineage, then the reservation and totals rows. They no longer take the installation row lock. Every actual upstream attempt receives a unique execution/reservation and `root_request_id`/`attempt_number` (1–3). Planning or a denied admission consumes no quota. Fallbacks require another admission and can incur another charge.

Admission revalidates the key, live human entitlement/membership or shared service account, enabled workspace/model/target, live catalog/direct grant and immutable key restriction lineage. The cached target's upstream model, endpoint, credential reference, region and protocol metadata must still match. Stale authorization/configuration fails before dispatch. Already-admitted work may finish after revocation.

Fresh storage is `apps/gateway/enterprise_migrations/0001_enterprise.sql`, with no organization columns; `0002_multimodal_pricing.sql` adds workload kinds, meter evidence columns and pricing v3 (explicit `migrate` upgrade only; it revalidates model protocol sets and fails if a model mixes generation and embeddings). `0004_budget_period.sql` added a per-layer `budget_period` and an admission-time index on reservations. `0005_stacked_budgets.sql` moves every existing budget/period pair unchanged into `policy_budgets` (one row per scope and period: `day`, `week`, `month` or `lifetime`), drops the legacy budget columns, adds reversible `api_keys.disabled_at` and a per-key execution-time index. Policy layers are `workspace_type_policies`, `workspace_platform_policy_overrides`, `workspace_local_policies` and `key_policies`. The platform replacement header replaces type defaults, including an all-null override; local/key layers compose. Type limits are per workspace. There are no installation-wide limits (see [below](#no-installation-wide-limits)). Local/key edits cannot raise or clear previously stored caps. Rotation preserves lineage counters/holds; new keys remain subject to parents.

Requests/tokens use fixed UTC minutes, sampled from database wall-clock **after acquiring locks**. Concurrency counts pending unexpired leases. A child cap does not reserve capacity. Increasing policy does not reset consumption.

### Jobs at once

`0018_job_limits.sql` adds `concurrent_jobs` ("Jobs at once") to every policy layer: workspace-type default, platform override, workspace local and key lineage (0018 also added it to the former installation layer, removed in 0026). It is how many async jobs (video and batch, see [async jobs](async-jobs.md)) a scope may have active at the same time. It composes like the other rate limits: every applicable layer applies, an absent child limit inherits, local/key layers are tighten-only, and an override replaces the type default. Type defaults start at **2** per workspace.

- **Active job:** its reservation is pending with a live lease, and the job is not terminal and has no cancel request. A slot is released when the job reaches a terminal state, when cancel is requested, or when its lease expires (lease reconciliation keeps the hold as unknown).
- **Requests at once:** a job holds a "requests at once" slot only while its create call runs. Once the provider accepts it, it holds a job slot instead.
- **Per-minute limits:** video and batch admissions skip requests-per-minute and tokens-per-minute checks, and their reservations never count toward them, so a large batch no longer fails with `token_reservation_exceeds_limit`.
- **Budgets:** unchanged. The job's full ceiling is reserved, counted in the maintained totals (0015) and checked against every applicable budget.
- **Realtime:** a session still counts as one request against requests per minute and requests at once.
- **Denial:** `job_limit_exceeded` (HTTP 429, retryable). It ranks below budget/accounting denials and above other rate limits.

The count is taken in the same admission transaction, after the scope locks and with the workspace's and lineage's counter rows held, from the current minute and live leases only, so concurrent submissions cannot both take the last slot and the cost does not grow with history.

### Storage

`0020_files_api.sql` adds `storage_bytes` (**Storage**) to the workspace layers: workspace-type default, platform per-workspace override and workspace local. There is no installation or key storage layer. It is how many bytes a workspace may keep in the gateway's [file store](file-storage.md): live [Files API](files-api.md) uploads and batch inputs and outputs, plus uploads in progress.

- **Composition.** It works like the other limits: the override replaces the type default (an all-null override means no storage cap), the local layer is tighten-only (it can't exceed the platform layer, and a stored cap can't be raised or removed), and the lowest applicable value wins.
- **Defaults.** Type defaults start at **1 GiB** per workspace. Values are bytes, 1 to 2^50. Overrides that existed before 0020 have no storage value, like 0018's jobs-at-once column.
- **Enforcement.** The quota is enforced while an upload streams. The upload reserves bytes ahead (`stored_files.reserved_bytes`), and each reservation step takes the catalog advisory lock (shared, as admission does first) and then a per-workspace transaction advisory lock. It counts committed sizes plus other live reservations, so concurrent uploads never jointly exceed the quota. No I/O happens while the lock is held. A refusal aborts the upload, deletes the partial object and returns `413 storage_quota_exceeded`.
- **Dashboard.** Effective access and the limits pages show it as **Storage**. Workspace admins see a used/quota bar.

Storage usage is tracked as GB-days but **not charged** (see [Files API](files-api.md#storage-usage-not-charged)).

### Budget periods

Each scope may stack up to one budget per period, all enforced over their own half-open UTC window by **admission time**: `day` is the UTC calendar day, `week` the ISO week from Monday 00:00 UTC, `month` the UTC calendar month and `lifetime` all time since the scope was created. Each budget sums settled actual cost plus active pending/unknown holds of its scope (workspace or key lineage) admitted in its window; the unknown/unbounded and `budget_exceeded`/`unresolved_usage` rules are unchanged. Every budget at every layer applies, so a $10/day local budget under a $200/month override binds on whichever is exhausted first. Override budgets apply only while the replacement header exists; type-default budgets only without one.

Changing budgets only changes which windows later admissions sum. Consumption history (reservations, holds, ledger, admission time) is never reset, moved or rewritten, and a request stays in the window of its admission even when it settles or is reconciled later.

### Maintained budget totals

Admission does not scan history. `0015_budget_totals.sql` adds `budget_totals`, one row per consumption scope (`workspace` or `key` lineage; 0015 also kept an `installation` scope, removed in 0026), period (`day`, `week`, `month`, `lifetime`) and UTC period start. Each row holds the settled actual cost and the active pending/unknown holds in exact integer micro-USD (`numeric(38,0)`), plus counters for reservations, pending, unknown, unresolved (unsettled and unbounded or unpriced) and unreserved executions. Type-default, override and local budgets read the workspace row. Key budgets read the lineage row. Every period is maintained whether or not a budget uses it, so adding, changing or switching a budget period reads existing consumption and never resets it. Admission reads all applicable layers in one indexed lookup, so its cost is O(layers), not O(history). Rate and concurrency limits read maintained counters (next section).

`0025_budget_totals_detail.sql` splits the unknown-cost subset out of each row: `held_unknown_microusd` (holds of `unknown` reservations; unknown cost keeps its hold) and `unresolved_unknown` (unknown reservations that are also unbounded or unpriced). `held_microusd` is unchanged and is still what admission enforces. Budget alerts read these rows instead of scanning history (alert spend is settled plus *pending* holds; unknown-cost requests are reported separately), including `lifetime` windows, and the `gateway_reservations_held` gauge sums the pending/unknown counts of the workspace `lifetime` rows. The migration backfills both columns from existing reservations.

Statement-level triggers on `governance_reservations` and `inference_executions` update the rows in the same transaction as every write: admission, settlement, failure to unknown, lease expiry and reconciliation. Code paths therefore cannot forget them. Unknown cost keeps its hold. The migration backfills from existing reservations and executions with the former scan's rules and modifies no history rows. `open-model-gateway budget verify` compares the table with a full scan (see [operations](operations.md#budget-totals-verification)).

### Maintained rate counters

Per-minute and concurrency limits do not scan traffic either. `0024_rate_counters.sql` adds two trigger-maintained tables for the **workspace** scope (type-default, override and local layers) and the **key lineage** scope (key layer):

- `rate_minute_counters` (per scope and UTC minute): interactive admissions plus executions without a reservation (`requests`), the subset without a token reservation (`unreserved`; any such row denies tokens/minute, as before), and token consumption `greatest(reserved, input+output, normalized input+output)` of the minute's interactive reservations (`tokens`; observed excess raises it, nothing is refunded).
- `inflight_counters` (per scope): pending reservations not yet accepted as async jobs ("requests at once"), and pending video/batch reservations whose job is not terminal or cancel-requested ("jobs at once"). Gateway-run batch lines count toward neither.

Lease expiry is applied when reading: admission subtracts pending reservations whose lease expired at or before its admission instant (a small indexed set, bounded by reconciliation lag), so an expired lease releases its concurrency slot at exactly the same instant as before 0024. The counters are updated by statement-level triggers on `governance_reservations` and `inference_executions` and row triggers on `async_jobs`, in the same transaction as every write, and admission reads every layer, budget, total and counter in one statement. Results are identical to the former per-layer scan; oracle tests compare both across random interleavings, minute boundaries and lease boundaries on the pinned admission clock.

There is deliberately no installation-scope counter (one row would be touched by every request), and since 0026 no installation layer that would need one. Minute rows older than ten minutes are no longer read; maintenance prunes them every minute, and a trigger refuses removing newer ones. `budget verify` also compares the in-flight counters and the last five minutes with a full scan.

Tighten-only local/key rules are per period: a child budget for period P may not exceed a parent's budget for the same P; budgets of different periods are independent because each parent budget is still enforced over its own window, so a child can never loosen a parent. A stored local/key budget for P may only keep or lower its amount and cannot be removed. Denials name only the scope kind (workspace or API key), never amounts.

### No installation-wide limits

`0026_remove_installation_limits.sql` removes the installation policy layer (decision of 2026-10-09). There is no installation budget (any period) and no installation requests/tokens per minute, requests at once or jobs at once. Limits exist only on personal, team and project workspaces (type defaults, platform per-workspace overrides, tighten-only workspace local) and on keys, and all of them stay hard limits exactly as before. No admission or settlement reads or writes a global limit row.

- **Configuration.** `installation_policy` is dropped and `policy_budgets` refuses `layer='installation'`. The values that were set are recorded once in the audit log (`policy.installation_removed`, with rates and budgets).
- **Totals.** `budget_totals` keeps only the `workspace` and `key` scopes, and the trigger no longer touches an installation row. The former installation rows were derived data (trigger-maintained sums that `budget verify` recomputes from history) and each equalled the sum of the workspace rows of the same window exactly, so the migration deleted them instead of leaving them stale. Installation-wide spend is now that sum (partial index `budget_totals_installation_spend`). Reservations, executions, the ledger, prices and audit events are not touched.
- **Visibility.** An installation **spend** alert (`spend_threshold`, see [alerts](alerts.md)) reports total spend against an amount you choose. It never blocks anything. The migration converts installation budget alert rules to it, keeping thresholds and recipients.
- **Locks.** Since 0026 the installation row lock protected no limit data, only the serialization of admission with authorization, membership and policy changes. Scoped admission (0027, next section) replaced it in admission and settlement.
- **API.** `GET/PUT /platform/installation/policy` answer `410` with `reason:"installation_limits_removed"` (see [governance API](governance-api.md)).

### Scoped admission lock order

`0027_scoped_admission.sql` (scale plan P3) removes the installation row lock from admission and settlement. Admissions of different workspaces no longer wait for each other; admissions of one workspace serialize on that workspace's totals and counter rows, so budgets stay exact. Every transaction takes a subset of these locks, always in this order, which makes the protocol deadlock-free:

1. **Catalog advisory lock** `72419502`: shared in admission and ordinary management, exclusive for global catalog changes (unchanged).
2. **Installation row** (`FOR NO KEY UPDATE`): management only, for management-versus-management serialization such as last-admin checks, SCIM and sign-in grants. Scoped admission and settlement never take it.
3. **Authority advisory locks**, transaction-scoped, in the two-int key space `(class, key)`: workspace type `72419510` (key 1 personal, 2 team, 3 project), workspace `72419511`, user `72419512`, key lineage `72419513`. The key of a uuid scope is its first 32 bits (`omg_scope_key`). Within a class, keys are ascending. Admission takes the key's type, workspace, issuing user (human keys) and lineage **shared** in one call (`omg_admission_locks`). A management change that can affect admission takes the matching locks **exclusively** (`governance::locks::exclusive`). A key collision only serializes two unrelated scopes.
4. **An existing reservation row** (`FOR UPDATE`): settlement, lease reconciliation, realtime windows, batch envelopes, usage resolution.
5. **Totals and counter rows the write will change**, each table in primary-key order: `budget_totals`, then `rate_minute_counters`, then `inflight_counters` (`omg_lock_scope_rows`, which creates missing rows as zeros; zeros equal missing rows for every reader and for `budget verify`). The 0015/0024 triggers then only touch rows already held, in the same order.
6. **New rows** (execution, reservation, ledger).

Admission therefore runs: shared catalog and authority locks → live revalidation **without row locks** → deployment and entitlement check **without row locks**, which also reads the latest price id (its immutable row comes from the per-replica price cache, [below](#caches-and-change-notifications)) (the catalog lock fences catalog writers) → lock the workspace's and lineage's totals and counter rows at the admission instant → read policies, budgets, totals and counters → write → commit. The authority locks replace the former `FOR SHARE` row locks on keys, workspaces, users, grants, service accounts, deployments, providers and models, so the hot path creates no MultiXacts. A revocation or limit change takes its scope lock exclusively: it waits for admissions already holding the lock (they were admitted before it and may finish) and every later admission sees it. Settlement locks its reservation row and then the totals rows of its admission buckets, with no advisory lock.

**Deadlock retry.** Gateway transactions cannot deadlock with each other, but a hand-written session that changes several scopes out of order could. A governance transaction the database aborts as a deadlock victim (SQLSTATE `40P01` only) is re-run at most twice (`gateway_lock_deadlock_retries_total{path}`). The failed attempt rolled back entirely; admission runs before dispatch and settlement after upstream work ended, so no upstream work is ever repeated.

**Backstop triggers.** Authority triggers (0027) take the exclusive scope lock on every row change admission depends on, so no code path can forget it: management, SCIM, sign-in, lifecycle cleanup, the CLI or a manual SQL session. Statement triggers take the exclusive catalog lock on catalog writes. The gateway takes the same locks up front, in canonical order, before its first write. When `omg.scope_lock_audit` is on (the test suite sets it on every transaction), a transaction holding the catalog lock *shared* fails if it reaches such a write without having taken the lock up front, or if it requests scope locks out of canonical order. Transactions holding the catalog lock exclusively already exclude every admission.

**Rollback.** `GATEWAY_ADMISSION_MODE=global` (one release) restores the former protocol: admission and settlement take the shared catalog lock and the installation row, revalidation keeps its `FOR SHARE` row locks, and management keeps taking the installation row. Both modes run the same schema, and the test suite runs under both. Invalid values fail startup.

#### Authority change sites

Every management change that can affect an admission decision, and the locks it takes. All rows also hold the installation row (management serialization) after the catalog lock.

| Site | Change | Locks (exclusive unless noted) |
|---|---|---|
| `management/keys.rs` `update_key` | Disable or enable a key | Key lineage |
| `management/keys.rs` `revoke_key` | Revoke a key | Key lineage |
| `management/keys.rs` `rotate_key` | Rotate (new key in the lineage, old one revoked) | Key lineage |
| `management/keys.rs` `create_key` | Model allowlist and initial key limits of a new lineage | Key lineage (new) |
| `management/keys.rs` `update_account` | Service account disable (its keys revoked) or enable | Workspace, then the lineages of the account's keys |
| `management/members.rs` `set_member` (add, role change, platform add) and `accept_invite` | Manual grant replaced | User |
| `management/members.rs` `unset_member` (remove, platform remove) | Membership revoked, the member's keys in the workspace revoked | User, then the lineages of the member's keys there |
| `management/governance/policies.rs` `put_type_policy` | Workspace-type default limits and budgets (every workspace of the type) | Workspace type |
| `management/governance/policies.rs` `put_platform_workspace_policy`, `reset_platform_workspace_policy` | Platform per-workspace override and its budgets | Workspace |
| `management/governance/policies.rs` `put_workspace_policy` | Tighten-only local limits and budgets | Workspace |
| `management/governance/policies.rs` `put_key_policy`, `store_initial_key_limits` | Key lineage limits and budgets | Key lineage |
| `management/catalogs.rs` `change_model` (select, deselect, direct grant, revoke direct) | Workspace model grants; removal retires that workspace's ineligible grants and key allowlist entries | Workspace, then (removal) the lineages with model allowlists in it |
| `management/catalogs.rs` catalogs, catalog models, model catalogs, type catalogs, catalog defaults, workspace catalog overrides | Global or type-wide entitlement | Catalog lock exclusive (`catalog_tx(write)`) |
| `management/resources.rs` providers, models, deployments; `management/governance/prices.rs` prices and routing; `management/setup.rs`; `management/batch_scheduling.rs` | Global catalog, routes, prices | Catalog lock exclusive |
| `management/directory.rs` workspaces (create, disable, cost center), users (create, suspend, reactivate), platform role grant/revoke, entitlement-loss deactivation, group mappings, cost centers | Platform directory | Catalog lock exclusive (last-admin checks under the installation row) |
| `lifecycle.rs` `cleanup_inactive_accounts` | Cleanup of suspended accounts (keys, grants, memberships revoked, personal workspace disabled) | `lock_users`: personal workspaces, users, lineages of their keys |
| `identity.rs` `resolve_identity_with` | Sign-in: group grants, entitlement loss, cleanup, account rebinding | `lock_users` for the linked account, the email's account and the id a new account gets |
| `scim.rs` `persist_user` (create, replace, patch, delete user) | SCIM suspension or reactivation | `lock_users` for that user (last-admin check under the installation row) |
| `scim.rs` `sync_users` (group create, replace, patch, delete) | SCIM group grants, entitlement loss | `lock_users` for every affected user |
| `bootstrap.rs` `seed`, `demo.rs` | Development seed, demo | Catalog lock exclusive |
| `governance.rs` `resolve_usage` | Evidence-backed usage reconciliation | Shared workspace and actor-user locks, then the reservation row and its totals rows |

Changes that cannot affect admission take no scope lock: settings, branding, alerts, invitations (an invitation authorizes nothing until accepted), file and batch management, cost center renames (the name is snapshotted at admission; either value is a valid order). Grants and insertions that only widen access (new keys, new grants) need no lock: a racing admission simply ran before them.

Backstop triggers (`omg_authority_*`, `omg_catalog_*`): `users` (disable, cleanup), `platform_role_grants` and `workspace_membership_grants` (revoke, role or owner change, delete) → user; `workspaces` (disable, owner, cost center) and `service_accounts` (disable) → workspace; `api_keys` (revoke, disable, expiry) → lineage; `key_model_restrictions`, `key_model_selections`, `key_policies` → lineage; `policy_budgets` → by layer (type, workspace or lineage); `workspace_platform_policy_overrides`, `workspace_local_policies`, `workspace_catalog_overrides`, `workspace_catalog_override_items`, `workspace_model_grants` → workspace; `workspace_type_policies` (rate, concurrency, jobs) and `workspace_type_catalogs` → type; `catalogs`, `catalog_models`, `models`, `deployments`, `provider_connections`, `deployment_prices` → catalog exclusive.

Two later additions sit outside this order without changing it: the configuration-version bump of 0028 runs at **commit** (deferred constraint triggers), after every lock above, and a leased background job's fence (0029) locks only its `work_leases` row, first, which nothing else waits on (see the next sections).

**Semantics kept.** Revocation is live for new work: no admission commits after the revocation commits, and already-admitted work may finish and settle. Budgets are never overspent at admission. Unknown cost keeps its hold. Amounts stay exact integer micro-USD. Lease extension of an async job takes only its reservation row (a lease change touches no totals or counter row; an admission that already counted the lease as expired is ordered before the extension).

### Caches and change notifications

`0028_change_notifications.sql` (scale plan P4) lets each replica cache what it reads **before** admission. Admission itself never reads a cache: it re-checks the key (revoked, disabled, expired), user, platform role, membership or service account, workspace state, catalog eligibility, key restrictions, the latest price and every policy and budget live under its locks, exactly as above. A stale cache can therefore only send a request to an admission that refuses it; **revocation still blocks new upstream work immediately**, on every replica. Within the invalidation window a key that is no longer valid (revoked, disabled, expired, or its user, platform role, membership, service account or workspace no longer active) used on another replica is refused by the live re-check (admission, the candidate listing or `/v1/models`) with exactly the authentication refusal of the uncached path (`401 authentication_error`, Anthropic shape on `/v1/messages`, `WWW-Authenticate: Bearer`; nothing sent upstream), and that replica drops its cached entries of the key at once. `404 model_not_found` remains reserved for a valid key that may not use the model. A key revoked during a realtime session or a gateway-run batch fails the same way (close `1008 authentication_error`; batch lines `401` with the same body).

| Cache (per replica) | Key | Invalidated by topic | Used by |
|---|---|---|---|
| Key metadata | SHA-256 of the presented token (key id, workspace, issuing user, expiry; never the secret) | `keys`, `access` | authentication of engine routes: chat, responses, messages, embeddings, images, audio, rerank, System One, `/v1/models` |
| Candidate deployments | workspace, key, public model | `catalog`, `access`, `keys` | candidate planning |
| Routing snapshot | workspace, public model | `catalog`, `access` | routing policy and per-deployment priority, weight, residency |
| Passive health | deployment | 1 s TTL; this replica's own results at once | circuit state for planning; skips no-op success writes |
| Prices | price id and deployment | none (append-only, immutable) | admission bounds and settlement valuation |

Routes that return workspace data or start work outside the engine (files, batches, videos, realtime) authenticate live. Policies and budgets are not cached: a tightened budget applies to the very next admission.

- **Versions.** `config_versions` has one version per topic: `catalog` (models, deployments, provider connections, prices, routing policies and routes, catalogs, catalog models, type catalogs), `access` (user disable/cleanup, platform role and membership grants, workspace disable/owner, service-account disable, workspace model grants and catalog overrides), `keys` (key revoke, disable, expiry, lineage; key model restrictions and selections), `policy` (type defaults, overrides, local and key limits, budgets) and `settings` (installation settings). Deferred constraint triggers on those tables bump the topic **once per transaction at commit** and `NOTIFY omg_config, '<topic>:<version>'`; notifications are delivered only after commit. Inserts that only widen authorization (new users, workspaces, keys, service accounts, role or membership grants) and updates of columns no cached read uses (display names, sign-in's no-op personal-workspace upsert) do not bump: caches hold only positive authorization results. Versions never move backwards (trigger), and the runtime cannot add, rename or remove topics.
- **Locking.** The bump is the last thing a transaction does, after every authority and row lock, and the first bump locks every `config_versions` row in topic order, so configuration writers serialize there without deadlocks. Admission, settlement and background accounting never write these tables or notify.
- **Propagation.** Each replica LISTENs on one dedicated session connection (`GATEWAY_LISTEN_DATABASE_URL`, default `DATABASE_URL`; never a transaction-mode PgBouncer) and polls all versions every second through its pool. An entry is stamped with its topics' versions read before the database read that produced it, and is valid only while they are current; any (re)connect of the listener flushes everything. Measured on two replicas: 5–10 ms from commit to invalidation through LISTEN; at most about 1 s through the poll alone.
- **Fail closed.** Until the first poll succeeds, and whenever no poll has succeeded for 3 s, every cache is bypassed (live reads, `result="bypass"` in `gateway_cache_lookups_total`). `GATEWAY_CONFIG_CACHE=off` turns caching off entirely.

### Background work leases

`0029_work_leases.sql` (scale plan P5) runs singleton background jobs on exactly one replica at a time. Each replica renews every lease row every 10 s (30 s term). A job runs only on the holder of its lease, and each of its transactions starts with `omg_lease_fence`: it fails unless that term (`epoch`, the fencing token) is still current and unexpired, and holds the lease row `FOR SHARE` until commit, so a takeover waits for a running job transaction and a paused former leader can never commit after its term ended. A replica that dies is replaced once its term expires (at most about 40 s); a graceful shutdown releases its terms at once. Epochs only increase (trigger).

| Lease | Job | Before P5 |
|---|---|---|
| `lifecycle` | Account cleanup every minute (a lock-free check first; the catalog and installation locks only when an account is due) | every replica, installation row every minute |
| `maintenance` | Minute-counter pruning (every minute) and storage-usage hours (every 5 minutes) | every replica |
| `compaction` | Settled-detail compaction (hourly) | every replica |
| `alerts` | Alert rule evaluation | the tick lock, but each replica's own tick: about N evaluations per interval |
| `file_sweep` | Stored-file sweeper (every minute) | every replica |
| `metrics` | The installation-wide `gateway_reservations_held` gauge (only the holder exports it) | every replica exported it |

Queues stay on every replica with `FOR UPDATE SKIP LOCKED`: expired-lease reconciliation, async-job polling, gateway-run batch lines (per-batch runner leases) and alert deliveries. Per-replica state stays per replica: the issuer JWKS cache keeps its own single-flight refresh, and settings are re-read per replica when the `settings` version changes (and at least every minute; every 5 s while versions are unconfirmed). The in-process admission and settlement gates stay per replica: they bound one replica's share of its own pool, which a database gate would not improve. Explicit CLI one-shots (`alerts evaluate --once`, `files sweep --once`, `compact-history`, `reconcile-executions`) are not leased.

## Bounds and prices

`deployment_prices` is append-only. Each attempt pins its admission price ID/version; publishing a later version cannot reprice it. Rates are configured estimates, not invoices, GPU metering or customer billing.

Priced generation requires an explicit positive output maximum within the pinned output limit. Reservation uses the full configured hard aggregate input ceiling plus requested output maximum, not a tokenizer guess or automatic truncation. Without a price and without token/budget policies, generation can run with unknown valuation; enabling enforcement requires bounds. Unpriced history cannot acquire a missing admission price through reconciliation.

Embeddings and rerank are input-only workloads. They reserve the hard input ceiling and **zero output bound** without a generation maximum; settlement rejects nonzero output for them. System One reserves output up to the pinned price's `output_token_limit` because providers may report (often free) output tokens. Non-generation admissions also carry request-derived unit ceilings (`requests: 1`) that tighten, never loosen, v3 `max_units`. For example, a priced `requests` line is bounded without `max_units`, but `search_units` still needs a trusted `max_units` ceiling for budgeted admission. The deployment must still declare a protocol of the admitted workload. `output_tokens=0` is semantic non-applicability, not an invented generation observation. An embedding-only price may have `output_token_limit=0`. Missing input evidence remains unknown. Operators must enforce/verify the actual upstream non-truncating input limit; the configured ceiling is trusted, not measured by the gateway.

Pricing-v1 charges normalized inclusive input when billing metadata exists, or raw input for legacy observations without normalized metadata. Unknown inclusive input remains unresolved; it never falls back to an exclusive count. Pricing-v2 uses [cache-aware disjoint valuation](cache-pricing.md); pricing-v3 uses [price lines and meters](cache-pricing.md#pricing-v3-price-lines-and-meters). Missing rates/allocations are not zero. A known partial floor is never proof of a finite upper bound. Budget admission refuses unbounded cost; finite token reservation and monetary bound are separate requirements. A failed attempt with unknown usage keeps a finite hold when every category that could still be positive is priced. `not_applicable` categories are possible only when observed evidence forces usage into them; `unknown` rates with remaining capacity mark the attempt unbounded. Budget and unresolved-usage denials return distinct codes (see [governance API](governance-api.md#inference-admission-denials)).

## Settlement and holds

`governance_reservations` projects pending/unknown/settled state, pinned price, reserved tokens, held and actual micro-USD, raw/normalized usage, components and unbounded status. `monetary_ledger` appends hold/unknown/settlement/reconciliation evidence; events must **not** be added together as charges.

Each disjoint component uses `ceil(tokens × rate / 1,000,000)` with integer i128 intermediates and checked signed-64 storage. Complete successful usage settles, even above the reservation; excess consumption restricts later admissions. Failed/cancelled attempts, missing usage and incomplete cache allocation remain unknown with conservative holds. Known partial overspend can raise a hold; it cannot shrink it or clear an unbounded marker.

Budget consumption is settled actual cost or active unresolved hold admitted in the layer's current period window. A spanning request and later reconciliation stay in that admission period. Unresolved unpriced/unbounded history in the window blocks newly enabled budgets; missing finite token reservations block newly enabled minute quotas. Never delete history to bypass this denial. Minute token reservations are not refunded; observed excess raises consumption.

## Cancellation, reconciliation and retention

The engine's lease exceeds its whole active-work deadline and teardown margin. There is no heartbeat or permission to keep work running past the lease. Dropping futures/streams requests upstream cancellation but cannot guarantee zero charges. Deadline watchdogs drop transport even when downstream is not polling.

Expired-lease reconciliation marks started work cancelled/unknown and releases concurrency, **not monetary holds**. Late finalization cannot erase an expired hold. The serving maintenance task claims expired reservations in batches of 50 with `FOR UPDATE SKIP LOCKED`, one transaction per batch, so replicas never process (or wait on) the same rows; explicit `reconcile-executions --limit N` is also available.

Evidence-backed usage resolution requires Platform Admin authority plus normal owner-private workspace access, a terminal unresolved record, a pinned complete valuation, and a 1–200-character non-control evidence reference. It preserves every prior raw/normalized observation. Identical evidence and full breakdown are idempotent; changes fail closed. Audit, ledger and projection commit together. See [governance API](governance-api.md) for actual HTTP error mapping.

Account cleanup keeps financial attribution and immutable audit history. Request log retention is set in Admin › Settings › Data & privacy ([settings](settings.md#data--privacy)); optional `GATEWAY_EXECUTION_DETAIL_RETENTION_DAYS=30..3650` overrides and locks it. Either compacts old settled error/latency metadata and client session/app labels hourly in batches up to 1000 and sets `details_redacted_at`; aliases/provider and admission allocation remain for reports. Pending/unknown records, execution linkage, usage, prices, reservations and ledger are retained. `compact-history --older-than-days N --limit 1000` runs an explicit batch. No automatic monetary-history deletion is provided.

Implementation anchors: `src/governance.rs`, `src/billing.rs`, `src/inference/repository.rs`, `src/maintenance.rs` under `apps/gateway/`. Tests include exact arithmetic, races, unknown holds, lineage and immutable history in these modules. Their presence and historical [verification](verification.md) are not new full-stack execution evidence; load/lock contention and production privilege/recovery validation remain operational work.
