# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.7.2](v0.7.2.md). They become the next release's notes.

## Trace exporter preview: contract and transport only (#98, PR 1)

Adds an opt-in, bounded OTLP/HTTP JSON trace recorder/exporter using existing dependencies, with typed attributes, deterministic sampling/correlation primitives, dedicated secret headers and the gateway's egress protections. Disabled unless `GATEWAY_TRACES_OTLP_URL` is set; `OTEL_*` is ignored. Collector failure never blocks or retries inference. No engine instrumentation or production spans are emitted yet: those arrive in PR 2, with collector acceptance in PR 3. See [operations](../operations.md#traces-preview-transport-only-98-pr-1).

## Instrumentation and correlation for interactive generation traces (#98, PR 2)

PR 1's exporter now has a production span source, limited to one complete interactive generation path: chat completions, Responses and Messages through the shared engine. Nothing else (async jobs, batches, embeddings, images, audio, realtime) is instrumented.

- **Spans:** `inference.request` (engine entry through terminal settlement, including streamed consumption), `routing`, `admission` and `settlement` per attempt (each with its own real sub-phase boundaries from `governance.rs`), `upstream.attempt` per attempt (at most 3, only under the existing explicit failover policy; first content means the first non-empty text or tool-call delta, never headers, empty deltas, usage or finish, and stays absent for non-streaming requests), and independent background roots for the existing `rate_prune`/`lock_wait_prune` runs.
- **Eligibility is unchanged and now live:** the `SharedAdmission` export capability is constructed in exactly one place - the commit branch of `governance::admit_unobserved`, once commit has succeeded for a live-revalidated `team`/`project` workspace - never for personal workspaces, never from client input, never before commit. A personal request, or one that never reaches admission, exports nothing.
- **Streaming is drop-safe:** the trace context is moved into the stream alongside the existing `ExecutionGuard` and finishes (export, or drop-count) exactly once on completion, error, timeout, client cancellation (including before the first poll) and settlement write failure. A cancelled attempt's settlement still runs, in a detached task outside the phase scope: its sub-phase spans are legitimately absent from a cancelled trace, not a gap.
- **Logs correlation:** the Logs request list and detail responses add `trace_id` - the deterministic hex id for a shared-workspace row, `null` for personal, absent for an unauthorized viewer because the row itself is withheld. The dashboard's request detail page shows a "Trace ID" field when it is present. The same id is attached to the existing reconciliation log lines once eligibility is known. `x-request-id` is unchanged.

See [operations](../operations.md#traces-98-pr-2-instrumentation-and-correlation). Collector acceptance and chart assertions remain PR 3's scope.

## Helm: CNPG backups through the Barman Cloud plugin (#261)

`deploy/helm/open-model-gateway`'s CNPG `Cluster` backup gains a `cnpg.cluster.backup.method` choice: `barmanObjectStore` (default, CNPG's in-tree backup, rendering unchanged from earlier releases) or `plugin` (the Barman Cloud plugin upstream CNPG now recommends, and the only mode the homelab cluster runs).

In `plugin` mode the chart additionally renders a `barmancloud.cnpg.io/v1` `ObjectStore` (named `<cluster>-backup-store`, or `cnpg.cluster.backup.plugin.objectStoreName`) from `cnpg.cluster.backup.plugin.objectStore`; the Cluster gets `spec.plugins: [{name: barman-cloud.cloudnative-pg.io, isWALArchiver: true, parameters: {barmanObjectName: ...}}]` and no in-tree `spec.backup` block; the `ScheduledBackup` gets `method: plugin` and `pluginConfiguration.name: barman-cloud.cloudnative-pg.io`, keeping its existing `schedule` and `backupOwnerReference: self`. S3 credentials are Secret name/key references only (`s3Credentials.accessKeyId`/`secretAccessKey`); `values.schema.json` validates `method`'s enum and refuses an inline credential string. `wal`/`data` tuning and the plugin sidecar's `instanceSidecarConfiguration` resources render as given.

This chart's `NetworkPolicy` templates govern only the gateway's own pods; they do not and never did select CNPG's instance pods in either backup mode, so an operator running a default-deny namespace policy must add their own egress rule for the CNPG pods to reach the object store (documented in `docs/kubernetes.md#backups`, matching the homelab's own `postgres-egress` policy). Restoring from either backup mode is a CNPG-native recovery into a fresh Cluster, not a chart operation; `docs/kubernetes.md#backups` points to #146 and `docs/operations.md`'s "Backups and restore" runbook for re-applying revocations and the spend-since-backup caveat afterward.

See [kubernetes](../kubernetes.md#backups).

Switching an **existing** cluster between modes is a one-way cutover: WAL archiving restarts under the new mode and the plugin doesn't adopt the old backup catalog, so take a fresh base backup right away and don't rely on point-in-time recovery across the switch ([Kubernetes › Backups](../kubernetes.md#backups)). Omitting S3 credentials in plugin mode now fails with the required-value message instead of a template error.

## Vendored upstream-base advisory coverage (#214)

The existing read-only cargo-deny job now runs a separately labelled advisory-only check for explicitly registered vendored dependencies. It validates root workspace metadata registrations and the complete graph, then projects only the reviewed crates.io base name/version/source identity and graph references in temporary scanner input. Unknown path dependencies, stale or mismatched registrations, non-canonical/escaping paths and malformed/incomplete graphs fail closed. There are no vendored dependencies today; the check passes and says so.

The normal `cargo deny --all-features check` is unchanged. No dependency is vendored here; `Cargo.lock`, builds, license/ban/source checks, SBOMs and provenance are unchanged. This is upstream-base advisory coverage, not certification of patched code. Offline tests with the real pinned scanner and a synthetic local advisory database prove the path-package miss and projected detection, plus positive and refusal controls. See [verification](../verification.md#supply-chain).

## Added

- **Logs Refresh, and a Sessions-to-Requests hint (#232).** Workspace and Admin Logs gained an explicit **Refresh** action (next to Columns/density) that re-fetches the current tab's rows and its summary tiles for the current filters; Logs never polled and still doesn't. The empty **Sessions** tab keeps its explanation of how sessions form and now also says that requests without session metadata are listed under **Requests**, with a button that switches to that tab keeping the same filters. No backend change: #232's diagnosis found the data correct; only the Logs page didn't refresh itself, and an empty Sessions tab read like "no logs." Admin Logs still excludes personal workspaces; no new data is exposed.
- **Budget headroom (#103).** Workspace and key budget views separate settled estimates, pending holds and retained unknown-cost floors. Remaining budget is shown only for complete accounting; hidden parent usage stays restricted, and unknown or invalid accounting never becomes available money. Effective headroom is the lowest remaining amount across applicable layers **within each independent period**, not a sum of overlapping scopes. Older gateways without the new facts show headroom unavailable. Reporting only: admission, limits, schema and runtime privileges are unchanged.

## Alert email retries: a durable per-recipient outbox (#46)

Alert emails, including the new key-expiry reminders (#100), now go through a durable outbox. Before, each email was sent once and a failure was only recorded.

- **Retries:** each recipient is retried on transient SMTP failures, up to five attempts within 24 hours, with 1 m/5 m/30 m/2 h backoff. A refusal from the relay is permanent. EOF before the relay accepts the message counts as a retryable connection failure; it's never recorded as success, and it may duplicate a message the relay had in fact accepted.
- **Replicas:** claims are leased across replicas and fenced, so a result from an expired claim is ignored. SMTP I/O never holds a database connection, and each tick has a bounded send budget.
- **Status:** incident History shows Waiting, Retrying, Sent, Exhausted or Failed per delivery, plus the waiting count and the next attempt time.
- **Monitoring:** new metrics `gateway_alert_email_attempts_total{outcome}` and `gateway_alert_email_deliveries{state}`, and an example Prometheus rule `GatewayAlertEmailPersistentFailure`.
- **Key-expiry reminders:** live eligibility is rechecked just before each send, instead of #100's authority locks held across SMTP. A revocation that lands during the send itself (bounded by the 40 s send deadline) can still receive that one reminder. The reminder never contains a key name or ID.
- **Privacy:** recipient addresses aren't stored in the outbox, only delivery-scoped fingerprints. These are pseudonymous personal data and never appear in the API, metrics or logs.

### Upgrade
- Apply migration `0050_alert_recipient_outbox.sql` explicitly, then reapply and verify runtime grants (new table and column grants).
- **Stop every older delivery worker before applying 0050**: scale the old pods to zero, or unset `GATEWAY_ALERT_INTERVAL_SECONDS` on them, then start the new ones. Old binaries don't understand the outbox. The database refuses an old binary's attempt to finalize a delivery whose recipients are still queued (SQLSTATE 55006), so outbox state is never lost. But an old worker sends before it writes, and retries every interval, so an old worker left running repeatedly **re-sends** that delivery's mail to every recipient.
- Historical terminal deliveries are not replayed. Outbox rows are append-only and retained like other audit tables.

## Authorized key-expiry reminders (#100)

Two built-in in-app and email reminders, fixed thresholds, no new configuration surface: "An API key expires within 7 days" and "...within 1 day" for every active, authorized API key that has an expiry.

- Recipients follow the key, never a rule or a platform role: a personal workspace's key reminds its owner only (never another Platform Admin or Auditor); a shared human key reminds its holder plus that workspace's current owners and admins; a service-account key reminds that workspace's current owners and admins only. Every recipient needs live platform entitlement, and a shared recipient also needs effective workspace membership right now. Revoked, disabled, expired and expiry-less keys, disabled workspaces/service accounts, and human keys whose holder lost workspace membership, are excluded entirely.
- `(credential id, expiry, threshold)` is permanently deduplicated, including resolved history: at most one incident is recorded per combination, not at-most-once email. A crash after relay acceptance but before the delivery transaction commits can resend email. The first evaluation that sees a key already inside one day emits only the urgent reminder. Rotation and expiry changes start a new cycle; re-enabling a key never creates another incident for an already-recorded tuple. The 1-day reminder silently supersedes an open 7-day one; nothing ever sends a "resolved" email for this kind. Each workspace is capped at 100 new `key_expiring` incidents in any rolling 24 hours (database time, including resolved incidents). Excess candidates are not recorded or permanently deduplicated and remain eligible later, possibly at the 1-day threshold instead. Cap warnings log the workspace id and counts, never key names. Evaluation preserves expiry ordering, applies workspace allowances before the global 500-new-reminders/tick cap so capped backlogs cannot block other workspaces, and retires at most 500 incidents/tick inside the existing fenced alert loop and interval (`GATEWAY_ALERT_INTERVAL_SECONDS`).
- No key names, ids, prefixes, hashes or holder identities appear in the event, the email or the logs. The email links to Notifications; the UI's **Open keys** action links to the workspace's keys list, never a key identifier. Notifications/the bell show reminders only to live recipients. Workspace history remains admin/reader-only: members get `403` and see their own reminders through the feed/bell instead; admins/readers see reminders in history only if the live recipient predicate allows them. Platform Admins/Auditors never see another owner's personal-workspace reminders. These reminders never appear in platform history.
- **Rolling-upgrade safety:** a `key_expiring` incident carries neither `rule_id` nor `builtin`, so the released gateway's feed/summary/read-marking, history and recipient queries cannot match it. An old replica never displays or emails these events. Its email worker can consume a pending delivery with no recipients, so best-effort email can be lost during overlap; completed deliveries are not replayed after upgrade.
- Migration 0049 adds `alert_events.api_key_id`/`key_expires_at`/`threshold_days` (immutable, no runtime UPDATE grant, permanently unique together), a scoped-workspace trigger, indexes on `alert_events` (permanent tuple deduplication, open-incident retirement and workspace/firing-time rolling-cap counts) and `api_keys(expires_at, id)` for candidate selection; it also adds the pre-existing `host_not_allowed` delivery-error value to `alert_deliveries`'s outcome `CHECK` (previously present in code but missing from the constraint, so recording that failure rolled back instead of recording it). Migration 0049 alters constraints on `alert_events` and `alert_deliveries` and builds indexes on `alert_events` and `api_keys`; on large installations apply it during a quiet period. `lock_timeout` bounds lock waits, not execution time.

See [docs/alerts.md](../alerts.md#authorized-key-expiry-reminders-100).

## Batch planning no longer sizes the parent envelope for deterministic adapter rejections (#238)

Creating a gateway-run batch (`POST /v1/batches`) now runs every chat line through the same deterministic, pre-admission check `Engine::execute_protocol` runs for an interactive request (#228), in addition to the existing coarse capability check, before the parent envelope is sized or reserved.

- A line with an explicit maximum is not pinned to a route; the runner may dispatch it to any authorized, protocol-capable candidate deployment of the model. It is excluded from sizing only when *every* such candidate would refuse it - the same once-per-adapter/provider enumeration the engine uses, read-only (no admission, no routing-health writes), not just the one deployment planning happened to size the line against. When every capable candidate refuses, the reported kind comes only from the routes the read-only routing policy would actually use, exactly as the engine computes it for an interactive request.
- A line with an omitted maximum is pinned at plan time to the one deployment its price and output cap were computed against, and is judged on that route alone. When that pinned route deterministically refuses it, the runner's dispatch now classifies the refusal exactly as an interactive request pinned to that one route would (400 invalid request or 501 unsupported) - never a bare `model_not_found`, even when a different, unpinned route for the same model would have accepted it.
- The check runs against the line's own resolved output bound (the same clamped body `encode_native_line` already sees), not the client's raw, possibly-omitted request.
- A deterministic exclusion anywhere in the batch also forces gateway-run execution: a batch is never submitted to a provider's native batch API while one of its lines is excluded from the envelope, which would otherwise submit (and bill) a line nothing was reserved for.
- An explicit-maximum line's exclusion at planning is a read, not a persisted fact: if routing changes before dispatch - a route that accepts the line is added or enabled - the line is not pinned and can still run. It then goes through the runner's own live admission with its own reservation, like any other line, so this is not a budget bypass. Dispatch-time route claiming for explicit-maximum lines (so two concurrent runners cannot double-claim the same route once routing changes) is tracked separately in #243.

A batch whose lines are all such rejections is refused per line at creation (no batch, no reservation, nothing dispatched); a mixed batch reserves only for its valid lines. An excluded line is still copied into the batch and still dispatches; it is refused per line with no execution or reservation of its own. Unknown-hold retention and envelope release on close are unchanged. No schema or grant changes.

## Egress no longer over-refuses the IPv6 documentation range's neighbors (#244)

The outbound address policy (`egress.rs`) classified the IPv6 documentation
range `3fff::/20` (RFC 9637) with `(s[0] & 0xfff0) == 0x3ff0`. That mask only
checks the top 12 bits of the first hextet, so it matched the much wider
`3ff0::/12`: every address whose first hextet was `0x3ff0`-`0x3fff` was
refused as "documentation", not only the `3fff::/20` IANA and RFC 9637
actually reserve.

**Egress now allows** connections to `3ff0::/12` addresses outside the exact
`3fff::/20` block (for example `3ff0::1`, `3ffe:ffff::1` or `3ff1:dead::1`):
they are ordinary, unallocated global unicast space, like the rest of
`2000::/3`, not documentation. `3fff::/20` itself (for example `3fff::1`) is
still refused, as it always was.

This is an availability-only fix: before #244 the gateway refused outbound
connections to addresses nothing allocates and no installation could
plausibly need to reach; now it classifies them like the rest of the public
IPv6 space. Nothing was ever wrongly *allowed* by the bug, and no other
classification, local-upstream approval decision, or pending-refusal warning
changes: `providers/local/endpoints.rs`'s own accept/refuse rule already maps
its `Public` and `Documentation` categories to the same decision, so that
module's behavior for these addresses is unaffected.

## Upgrade note: local-upstream approvals in special-use ranges will be refused next minor release (#203)

`GATEWAY_LOCAL_UPSTREAMS` approvals for an HTTPS endpoint still accept, this
release, the ranges `providers/local/endpoints.rs` always accepted but
`egress.rs` already refuses for every other client:

- `198.18.0.0/15` and `2001:2::/48` (benchmarking)
- `192.0.0.0/24` (IETF protocol assignments)
- `192.88.99.0/24` (deprecated 6to4 relay)
- `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `2001:db8::/32`, `3fff::/20` (documentation)
- `fd00:ec2::/32` beyond the already-refused `fd00:ec2::254` (AWS IPv6 metadata;
  unlike the other ranges above, this one is accepted over plain HTTP too, not
  only HTTPS, the same as an ordinary private address)

**The next minor release will refuse these ranges too and fail startup** for
an installation that approved one, naming the approval and the setting to
change. `64:ff9b::/96` (NAT64) stays refused as it is today; it was never
accepted.

**Find out before upgrading:**

- The gateway logs a `WARN` at startup for each affected approval, naming its
  host (no credentials) and the category, for example: `approved local
  upstream is in an address range the egress policy already refuses for
  every other client; the next minor release will refuse it at startup too`.
- `GET /health/ready` adds a non-failing `checks.local_upstream_approvals`
  field: `"ok"` normally, `"warning"` when any configured approval is in one
  of the ranges above. It never changes the overall `ready`/`not_ready`
  decision, and (like the rest of `/health/ready`) carries no endpoint names
  or other identifying detail.

**What to change:** replace the approval's address with one in an ordinary
public or RFC 1918 private range, or remove it if it is no longer needed. An
installation with only public or RFC 1918 approvals is not affected and sees
no warning.

No other behavior changes: every address this release accepts or refuses
today keeps doing so.

## The batch scheduler no longer claims a route that would deterministically reject the line (#243)

The runner's dispatch-time candidate filter for an unpinned (explicit-maximum) batch line (`jobs::runner::Runner::plan_route_inner`) previously checked only the coarse capability bool before choosing and claiming a route. With several routes on one model, it could claim and pin a route that deterministically rejects the line's exact body (`ProviderAdapter::validate_chat_request`, #228), even though another authorized, protocol-capable route for the same model would accept it - refusing the line (400/501, or 503 `model_temporarily_unavailable` if every refused route happens to be in its circuit-breaker cooldown at that moment) when an interactive request would have succeeded elsewhere.

- Before choosing a route, the runner now applies the same deterministic eligibility `Engine::execute_protocol` runs before admission, once per adapter, and chooses only among routes that pass. If every candidate refuses, nothing is claimed; the line proceeds to the engine's own unpinned refusal path and is reported with the engine's own classification - 400 invalid request or 501 unsupported capability, or 503 route-cooling-down if every refused route is currently cooling down.
- An omitted-maximum (pinned) line is unaffected: its candidate list is already narrowed to its one pinned route, and it stays judged on that route alone, exactly as before (#238).
- **A claimed-nothing ("ungated") line may now only be refused, never admitted.** "Ungated" means no scheduler route-capacity claim for this attempt - a creation-time plan (an omitted-maximum line's price/deployment pin from upload, `jobs::lines::LinePlan`) does not by itself count: the scheduler may still never have claimed capacity for that particular dispatch. When the runner claims no route - every candidate deterministically rejected the line above, or (pre-existing, since before #238) none existed at all - the line's `batch_lines` row records no `deployment_id` and it dispatches expecting only the engine's own refusal. That dispatch-time decision is read, not persisted, and is cached for up to the replan interval (10s): the live catalog or an adapter's deterministic eligibility can change before the line actually executes. The repository the runner's engine instance uses (`jobs::runner::LineRepository`) tracks this attempt's scheduler claim separately from its routing/pricing plan (the plan still decides price and route when the attempt *is* admitted, unchanged), and refuses admission outright for any attempt that was never claimed - even an omitted-maximum line whose pinned route was briefly ineligible at dispatch and recovered before admission, and even if a candidate decides to accept the request by the time admission is reached - instead of letting the engine admit and execute it on a route the scheduler never claimed capacity for, which would bypass route windows, live-load gates, fair-share ordering and cross-runner route-concurrency accounting. The refusal reuses the existing, already-retryable `rate_limit_error`/`Busy` denial (no new error path); a batch with its own explicit retry policy (`omg_retries`) gets its own bounded additional attempts, each independently and correctly refused the same way while still ungated. This closes the same gap for both the new deterministic-rejection case above and the pre-existing zero-candidate case from before #238 - they share this one dispatch and admission path. Because that refusal never reached a provider, it does not count toward the batch's own upstream-429 backoff (`RunState::rate_limited`): only a claimed attempt's `Busy` does, so an ungated refusal cannot stall the rest of the batch's dispatch waiting out a backoff it never earned.
- A transient `Storage` or `Timeout` error while planning a line's route (not a genuine no-route or configuration refusal) is treated like a cooling-down route: the line waits and is planned again rather than dispatching ungated into the refusal above. Because the batch's own completion window (24h by default) is the only other bound on that wait, a *persistent* Storage/Timeout condition - not a transient blip - now has a line wait up to that whole window before it is reported `expired`, instead of failing fast the way it did before this change.
- Live admission, lock order, scheduler fairness/turn-taking and routing-health observations are unchanged; this only narrows which route dispatch may claim and blocks admission with no claim behind it. No migrations or grant changes.

## Gateway-run batch dispatch now validates an omitted-maximum line against the same bound planning did (#245)

For an omitted-maximum (pinned) batch line, planning (`jobs/plan.rs`) already validated the line's own adapter with its resolved output bound - the same clamped value `encode_native_line` uses - not the client's raw omission (#238). At dispatch, though, the engine's pre-admission check (`Engine::execute_protocol`'s `validate_chat_request`, #228) still saw the unmodified line, with the maximum still omitted, while the real adapter `execute` call only received the resolved maximum afterward, once admission computed it. No shipped adapter's validation depends on the maximum today, so no request was ever wrongly accepted or refused by this gap, but a future ceiling-sensitive adapter could pass planning and still fail dispatch (or the reverse).

The runner now copies the line's plan-time resolved output bound onto the still-omitted request before the engine ever sees it (`jobs::runner::LineRun::execute`), so planning, the engine's pre-admission check and the real upstream attempt all judge the same body. This only affects batch lines already pinned to one deployment and price at plan time; admission still independently resolves the same bound from that same pinned price, so the admitted cap is unchanged (#215: the admission-pinned cap still equals the upstream cap).

Interactive requests are unchanged. An interactive request's candidate deployment, and therefore its price, is not chosen until admission; pre-filling its pre-admission check with a resolved maximum would require reading a price before a route is picked, which would violate #228's "no prices, no network" contract for that check. An interactive omitted maximum keeps reaching `validate_chat_request` as the raw `None` the client sent, exactly as before. No schema changes beyond an internal, backward-compatible field in the batch line's own private plan metadata (never client-visible); no grant changes.

## Fixes

- **Key Budgets card spacing (#242).** The key detail page's Limits tab Budgets card now gives "Effective headroom" its own spaced section instead of it sitting directly under the last budget line above it. Cosmetic only; data and accessibility are unchanged.
