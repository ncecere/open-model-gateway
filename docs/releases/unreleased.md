# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.7.2](v0.7.2.md). They become the next release's notes.

## Admin API tokens: storage and lifecycle (#37 PR1)

The first of three PRs for long-lived Admin API tokens. This PR adds only the database schema and the Rust storage/lifecycle primitives; no route, request authentication or UI reads or writes an admin token yet (PR2 and PR3). Nothing here is reachable from any API.

- Migration `0050_admin_api_tokens.sql` adds `admin_api_tokens`: owner (FK `RESTRICT` to `users`), a bounded name, the SHA-256 digest of the whole token (never the token itself), a non-empty enumerated scope list (`platform:read`, `catalog:write`, `access:write`, `governance:write`, `settings:write`; no wildcard, no duplicates), an optional 1-32 entry CIDR allowlist, creation/expiry/revocation timestamps and last-use tracking. Expiry is enforced at 1-90 days after creation. Identity, the digest, scopes, expiry and the allowlist are immutable (no rename, no renewal); only revocation and successful-use tracking ever change, and revocation reuses the existing `omg_revocation_is_final()` trigger (migration 0039), so it is final like every other revocation. No row is ever deleted.
- **Owner-invalidation triggers**: when a user loses their last active platform `admin` grant (manual or group-derived), is suspended, is SCIM-deactivated, or is cleaned up, every token they own is revoked in the same transaction as whatever caused the loss, and regaining Admin access later never restores them.
- `apps/gateway/src/admin_tokens.rs`: the token format is `omg_admin_<32-hex uuid>.<64-hex secret>`, 256 secret bits from the same fallible OS CSPRNG inference keys use (`entropy.rs`), with a strict parser that rejects malformed input before any database access. Create (returns the plaintext once), idempotent revoke, a lookup-for-authorization that checks unrevoked, unexpired, owner active and currently effective role exactly `admin` (no positive authorization cache, so every check re-reads the primary database), and a last-use update throttled to once per 5 minutes via a conditional `UPDATE`.
- `deploy/staging/runtime-grants.sql`/`verify-privileges.sql`: the runtime role gets `SELECT`/`INSERT` on the table and `UPDATE` only on `revoked_at`, `last_used_at` and `last_used_address`; everything else is enforced by column grants and triggers, with ACL assertions and rollback-only probes covering creation, immutability, revocation finality, idempotent revocation, no-delete and owner-invalidation.
- `scripts/backup.py`'s `reapply-revocations` (#146) now also covers `admin_api_tokens`, so restoring an older backup cannot resurrect a token the old database had already revoked.
- **Coordinator review follow-up (snapshot-bound guards).** The creation and role-loss authority guards took the owner's user-scope advisory lock, but a caller running under `REPEATABLE READ`/`SERIALIZABLE` keeps one snapshot for the whole transaction: the lock still serializes with a concurrent demotion, but the guard's own `EXISTS` check could still read a snapshot that predated a commit it had just waited on, letting a token be created for (or kept live past) an owner who had already, and verifiably, lost Admin. PostgreSQL's Serializable Snapshot Isolation does not reliably abort this either. Both guards now refuse outright (`SQLSTATE OM002`, mapped to `CreateAdminTokenError::UnsupportedIsolation`, no message-text matching) unless the transaction is READ COMMITTED - the gateway's own isolation level everywhere that writes `platform_role_grants`, `users.disabled_at`/`cleaned_at` or `admin_api_tokens` (verified by grep: no production path runs those writes at an elevated isolation level). The owner-not-admin refusal also moved to its own dedicated code, `SQLSTATE OM001`.
- **Coordinator review follow-up (the PR2 contract used to hang).** `create_admin_token`/`revoke_admin_token` ran on a connection of their own (`&self.pool`), but the owner's user-scope advisory lock migration 0050's trigger takes is transaction-scoped: a caller that (as documented, and as PR2's handler will) takes that lock exclusive up front in its own transaction, then calls `create_admin_token`, would have hung forever - the `INSERT`'s own lock acquisition waits for the caller's transaction to end, which it cannot do until the call returns. Both are now free functions that take the caller's `&mut Transaction<'_, Postgres>` directly (the pattern every other `apps/gateway/src/management` write helper already follows), so the trigger's re-entrant re-acquisition inside the one transaction is free instead of a cross-connection wait. `revoke_admin_token` moved for the same reason and so it is atomic with whatever else the caller's transaction does (PR2's audit insertion, in particular) and rolls back with it.
- **Coordinator review follow-up (a group re-sync used to destroy tokens).** `lifecycle::apply_group_grants` revokes stale group-derived grants in one statement and inserts their replacements in the next (for example, an IdP group mapping recreated with a new id but the same group value and role - the practical effect of renaming it). The role-loss trigger was an immediate `AFTER ROW` trigger, so it fired at the end of the first statement, before the replacement existed, and revoked every token the affected admin owned even though they never actually lost Admin. It is now `DEFERRABLE INITIALLY DEFERRED`, so it fires once per affected row at commit, against the transaction's final state; a real loss with no replacement still revokes, just at commit instead of mid-transaction (unobservable to any caller, since nothing reads a token's revocation state from inside the same transaction that revoked its owner's last grant). The suspension/cleanup triggers stay immediate and unconditional, since a suspension or cleanup is always a real loss.
- **Coordinator review follow-up (uncovered grant-loss shapes).** Migration 0027 treats `UPDATE OF revoked_at,user_id,role OR DELETE` on `platform_role_grants` as authority changes; the role-loss trigger only watched `revoked_at`. Changing a grant's `role` away from `admin`, deleting the row outright, or moving it to another `user_id` each left a token unrevoked and usable again after a later re-grant. The trigger now watches all three additional shapes too and checks the right user for each (the row's old owner for a delete or a `user_id` move; the new owner is also checked for a move, though it can only ever find nothing to do). The runtime role has none of these three privileges at all (only `revoked_at` is runtime-writable, with no `DELETE`); `verify-privileges.sql` now asserts that directly, and the three new shapes are each exercised by a dedicated test run as the owner/migrator role.
- **Coordinator review follow-up (smaller fixes).** The IP allowlist is now validated and canonicalized in Rust before any database round trip - `addr/len`, a parseable address, `len<=32`/`128`, no host bits set, no IPv4-mapped IPv6 network - reusing the exact same checks `egress::parse_network` already applies to `GATEWAY_EGRESS_PRIVATE_ALLOW` (extracted from `Networks::parse` so both share one implementation); a malformed entry is now the typed `InvalidAllowlist`, not a raw PostgreSQL `22P02` error. `touch_admin_token_use` takes a parsed `std::net::IpAddr`, not an arbitrary string. The 0050 migration comment that claimed an earlier row's trigger firing "may still see a sibling row not yet updated" was already wrong (`AFTER ROW` triggers fire at the end of their statement) and is doubly obsolete now that the trigger is deferred to commit; it is corrected. `NewAdminToken`'s plaintext `token` field is documented as deliberately not zeroized on drop, since returning it to the caller is this type's entire purpose.

## Vendored upstream-base advisory coverage (#214)

The existing read-only cargo-deny job now runs a separately labelled advisory-only check for explicitly registered vendored dependencies. It validates root workspace metadata registrations and the complete graph, then projects only the reviewed crates.io base name/version/source identity and graph references in temporary scanner input. Unknown path dependencies, stale or mismatched registrations, non-canonical/escaping paths and malformed/incomplete graphs fail closed. There are no vendored dependencies today; the check passes and says so.

The normal `cargo deny --all-features check` is unchanged. No dependency is vendored here; `Cargo.lock`, builds, license/ban/source checks, SBOMs and provenance are unchanged. This is upstream-base advisory coverage, not certification of patched code. Offline tests with the real pinned scanner and a synthetic local advisory database prove the path-package miss and projected detection, plus positive and refusal controls. See [verification](../verification.md#supply-chain).

## Added

- **Logs Refresh, and a Sessions-to-Requests hint (#232).** Workspace and Admin Logs gained an explicit **Refresh** action (next to Columns/density) that re-fetches the current tab's rows and its summary tiles for the current filters; Logs never polled and still doesn't. The empty **Sessions** tab keeps its explanation of how sessions form and now also says that requests without session metadata are listed under **Requests**, with a button that switches to that tab keeping the same filters. No backend change: #232's diagnosis found the data correct; only the Logs page didn't refresh itself, and an empty Sessions tab read like "no logs." Admin Logs still excludes personal workspaces; no new data is exposed.
- **Budget headroom (#103).** Workspace and key budget views separate settled estimates, pending holds and retained unknown-cost floors. Remaining budget is shown only for complete accounting; hidden parent usage stays restricted, and unknown or invalid accounting never becomes available money. Effective headroom is the lowest remaining amount across applicable layers **within each independent period**, not a sum of overlapping scopes. Older gateways without the new facts show headroom unavailable. Reporting only: admission, limits, schema and runtime privileges are unchanged.

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
