# Wave 3 — #48 provider-invoice reconciliation: closure plan

Read-only planning at main `c26e72af` (Plan agent, 2026-10-28). Not yet approved.

## Already shipped
- `apps/gateway/src/management/provider_costs/parser.rs:85` — exact signed cents, half-even residuals, bounded Anthropic parsing.
- `apps/gateway/src/route_policy.rs:218–277` — ten browser-only import/mapping/reconciliation operations; Admin writes, Admin/Auditor reads.
- `deploy/staging/runtime-grants.sql:21` — SELECT/INSERT-only grants.
- `apps/gateway/enterprise_migrations/0064_provider_cost_reconciliation.sql:4` — five immutable tables.
- Tests: `management/provider_cost_tests.rs:76` (duplicate/overlap/supersession); `provider_cost_matching_tests.rs:54,142,225,304` (incomplete variance, personal privacy, ambiguity, negative credits); `provider_cost_auth_tests.rs:186` (CSRF, token denial, demotion).
- Docs: `docs/provider-cost-reconciliation.md:3`, `docs/enterprise-costs.md:5`, `docs/cost-reporting.md:5`.

## Gaps
- No reconciliation UI. Existing web money formatters reject negatives. Upload transport must preserve the raw bytes (duplicate keys) rather than parse/re-stringify the evidence.

## Recommended boundary: Anthropic-only closure (owner approval needed)
Defer additional providers/coverage to a follow-up issue. Three sequential PRs; each needs real PostgreSQL/runtime-role tests plus running-local-stack browser acceptance (including refusals), and two independent reviews (one adversarial) because they touch authz, parsing and money.
1. **Comparison UI** (Admin/Auditor read): signed BigInt amounts, null variance, separate holds, identity-free personal totals.
2. **Upload/mapping UI** (Admin): byte preservation, size limits and refusals, revision conflict, attestation; upload → mapping → freeze.
3. **Acceptance/docs**: duplicates/overlap, negative adjustments, ambiguity, incomplete variance, demotion, privacy, unchanged financial history.

## Next provider (follow-up, not #48)
Prefer the OpenAI organization **Costs** JSON; usage data alone cannot establish charges. Its schema is not documented in-repo, so precision, dimensions and coverage are open questions. Never allocate costs from usage proportions. USD-only; no automated pulls, no estimate rewrites, no "verified invoice" claims.

## Owner decisions
1. Rescope #48 closure to Anthropic-only plus the UI.
2. Defer the second provider to a new issue.
3. Accept the remaining coverage exclusions.
