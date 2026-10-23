# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.7.2](v0.7.2.md). They become the next release's notes.

## Added

- **Budget headroom (#103).** Workspace and key budget views separate settled estimates, pending holds and retained unknown-cost floors. Remaining budget is shown only for complete accounting; hidden parent usage stays restricted, and unknown or invalid accounting never becomes available money. Effective headroom is the lowest remaining amount across applicable layers **within each independent period**, not a sum of overlapping scopes. Older gateways without the new facts show headroom unavailable. Reporting only: admission, limits, schema and runtime privileges are unchanged.

## Batch planning no longer sizes the parent envelope for deterministic adapter rejections (#238)

Creating a gateway-run batch (`POST /v1/batches`) now runs every chat line through the same deterministic, pre-admission check `Engine::execute_protocol` runs for an interactive request (#228), in addition to the existing coarse capability check, before the parent envelope is sized or reserved.

- A line with an explicit maximum is not pinned to a route; the runner may dispatch it to any authorized, protocol-capable candidate deployment of the model. It is excluded from sizing only when *every* such candidate would refuse it - the same once-per-adapter/provider enumeration the engine uses, read-only (no admission, no routing-health writes), not just the one deployment planning happened to size the line against. When every capable candidate refuses, the reported kind comes only from the routes the read-only routing policy would actually use, exactly as the engine computes it for an interactive request.
- A line with an omitted maximum is pinned at plan time to the one deployment its price and output cap were computed against, and is judged on that route alone. When that pinned route deterministically refuses it, the runner's dispatch now classifies the refusal exactly as an interactive request pinned to that one route would (400 invalid request or 501 unsupported) - never a bare `model_not_found`, even when a different, unpinned route for the same model would have accepted it.
- The check runs against the line's own resolved output bound (the same clamped body `encode_native_line` already sees), not the client's raw, possibly-omitted request.
- A deterministic exclusion anywhere in the batch also forces gateway-run execution: a batch is never submitted to a provider's native batch API while one of its lines is excluded from the envelope, which would otherwise submit (and bill) a line nothing was reserved for.
- An explicit-maximum line's exclusion at planning is a read, not a persisted fact: if routing changes before dispatch - a route that accepts the line is added or enabled - the line is not pinned and can still run. It then goes through the runner's own live admission with its own reservation, like any other line, so this is not a budget bypass. Dispatch-time route claiming for explicit-maximum lines (so two concurrent runners cannot double-claim the same route once routing changes) is tracked separately in #243.

A batch whose lines are all such rejections is refused per line at creation (no batch, no reservation, nothing dispatched); a mixed batch reserves only for its valid lines. An excluded line is still copied into the batch and still dispatches; it is refused per line with no execution or reservation of its own. Unknown-hold retention and envelope release on close are unchanged. No schema or grant changes.

## Gateway-run batch dispatch now validates an omitted-maximum line against the same bound planning did (#245)

For an omitted-maximum (pinned) batch line, planning (`jobs/plan.rs`) already validated the line's own adapter with its resolved output bound - the same clamped value `encode_native_line` uses - not the client's raw omission (#238). At dispatch, though, the engine's pre-admission check (`Engine::execute_protocol`'s `validate_chat_request`, #228) still saw the unmodified line, with the maximum still omitted, while the real adapter `execute` call only received the resolved maximum afterward, once admission computed it. No shipped adapter's validation depends on the maximum today, so no request was ever wrongly accepted or refused by this gap, but a future ceiling-sensitive adapter could pass planning and still fail dispatch (or the reverse).

The runner now copies the line's plan-time resolved output bound onto the still-omitted request before the engine ever sees it (`jobs::runner::LineRun::execute`), so planning, the engine's pre-admission check and the real upstream attempt all judge the same body. This only affects batch lines already pinned to one deployment and price at plan time; admission still independently resolves the same bound from that same pinned price, so the admitted cap is unchanged (#215: the admission-pinned cap still equals the upstream cap).

Interactive requests are unchanged. An interactive request's candidate deployment, and therefore its price, is not chosen until admission; pre-filling its pre-admission check with a resolved maximum would require reading a price before a route is picked, which would violate #228's "no prices, no network" contract for that check. An interactive omitted maximum keeps reaching `validate_chat_request` as the raw `None` the client sent, exactly as before. No schema changes beyond an internal, backward-compatible field in the batch line's own private plan metadata (never client-visible); no grant changes.

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
