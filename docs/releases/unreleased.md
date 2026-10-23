# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.7.2](v0.7.2.md). They become the next release's notes.

## Added

- **Budget headroom (#103).** Workspace and key budget views separate settled estimates, pending holds and retained unknown-cost floors. Remaining budget is shown only for complete accounting; hidden parent usage stays restricted, and unknown or invalid accounting never becomes available money. Effective headroom is the lowest remaining amount across applicable layers **within each independent period**, not a sum of overlapping scopes. Older gateways without the new facts show headroom unavailable. Reporting only: admission, limits, schema and runtime privileges are unchanged.

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
