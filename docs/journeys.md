# Journeys: real-path checks against a running gateway

`scripts/journeys` runs user-facing flows against a forwarded demo environment: an environment on the Spark test platform (`scripts/spark-env.py`, AGENTS.md) or the local demo ([local demo](local-demo.md)). Both are the same passwordless demo, so the suite signs in as the five demo personas through the real OIDC redirect flow. It complements the unit, integration and browser suites with the one thing they cannot show: the changed build, deployed, answering real requests, including the refusals.

```sh
python3 -I scripts/spark-env.py up my-name --ref <tag or candidate commit>
python3 -I scripts/spark-env.py forward my-name &       # gateway on 127.0.0.1:3000, demo issuer on 127.0.0.1:18084
python3 -I scripts/journeys run --base http://127.0.0.1:3000 --summary summary.json
python3 -I scripts/journeys run --base http://127.0.0.1:3000 --only keys signin   # chosen journeys, in this order
python3 -I scripts/journeys list
```

`--base` must be the gateway's public origin, which for the demo is `http://127.0.0.1:3000`: another spelling of the same address (`localhost`) fails the Origin check on every mutation. `--shuffle [--seed N]` runs every journey in a random order; `--issuer URL` names the demo issuer when the forward uses another port. The exit code is 0 when every check passed, 1 when a check failed or a journey errored, 2 when the command was refused (below).

## What the suite promises

- **Safe to point at.** It refuses a `--base` (or `--issuer`) that is not loopback (127.0.0.1, `::1`, `localhost`) unless `--allow-remote` is given, prints which environment it is about to modify, and refuses a gateway that is not ready or whose sign-in does not go to a loopback demo issuer. Never aim it at production: it creates throwaway workspaces, keys, invitations, requests, access reviews, cost centres and admin tokens.
- **Re-runnable and order independent.** Every journey creates what it needs under a unique name (`journeys-<run>-<journey>-<thing>`), undoes it afterwards, also when a check fails or the journey errors, and the other journeys still run. A seeded persona is never disabled, re-roled or removed from a seeded workspace. Run it twice in a row, in any order, or with `--only`.
- **No secret in the output.** Keys, admin tokens, invitation codes, cookies, CSRF values and response bodies never reach the summary or the terminal, by construction rather than by scrubbing: the recorder takes only a check name from a closed alphabet, an expected and an actual HTTP status, and a note from a fixed list, and is never handed a response. Credentials are held in a `Secret` that prints as `<secret>`; an exception is reported by its type name only; the summary names no URL query, e-mail address (other than the fixed demo personas, which are never written either) or identifier.
- **Honest about the demo.** The demo has no model server or provider and its example connections and deployments start disabled, so no generation completes here. The inference journeys cover authentication, validation and the documented `model_not_found` refusal per protocol, not a completed response (see "Not covered").
- **Rate limits are respected.** Sign-ins are limited per address (`GATEWAY_LOGIN_RATE_PER_MINUTE`, default 60), so the persona sessions are shared across a run and a throttled step is retried after `Retry-After`. The personal data export is limited to 3 an hour per person, so its check accepts the documented 429.

## What each journey leaves behind

Nothing the API can delete is left. These objects have no delete route, so they stay in the environment (named `journeys-<run>-…`, harmless, and gone with `spark-env.py down`):

| Journey | Left behind |
| --- | --- |
| `workspaces`, `keys`, `inference-auth`, `credential-boundaries`, `csrf-origin`, `members-invitations`, `platform-admin`, `auditor-readonly`, `access-requests`, `access-reviews`, `reports-usage` | Disabled throwaway Teams and Projects (disabled workspaces drop out of every list and every person's access); revoked keys; a disabled service account; accepted or revoked invitations; audit entries |
| `credential-boundaries` | One revoked admin API token |
| `platform-admin` | One archived cost centre (DELETE archives) |
| `access-requests` | Decided or withdrawn requests; notifications marked read |
| `access-reviews` | One closed campaign |
| `signin`, `me-summary`, `health-spa` | Nothing (sessions are signed out) |

## Feature map

Every journey is named in this table and every check named here exists; `tests/journeys.test.py` fails when a journey is missing from the map, a check id does not exist, or a "not covered" row has no reason. Check ids are matched against the check names written in the journey source (a name built from a persona or a route is matched as a pattern).

| Flow | Journey | Checks |
| --- | --- | --- |
| Health endpoints, readiness reports the database | `health-spa` | `health.live`, `health.ready`, `health.ready-reports-database` |
| SPA shell, deep-link fallback, built asset | `health-spa` | `spa.shell`, `spa.deep-link-falls-back`, `spa.built-asset` |
| Unknown `/api`, `/v1`, `/health` paths and missing assets are not swallowed by the SPA fallback | `health-spa` | `api.unknown-path`, `api.unknown-path-not-html`, `inference.unknown-path`, `health.unknown-path-not-html`, `spa.missing-asset` |
| Anonymous auth config | `health-spa` | `auth.config-is-anonymous` |
| Sign-in through the OIDC redirect for Admin, Auditor, Team Admin, User | `signin` | `signin.operator.callback`, `signin.auditor.callback`, `signin.alex.callback`, `signin.blair.callback`, `signin.alex.identity`, `signin.operator.platform-role` |
| Unentitled user: authenticates, platform access denied, no session | `signin` | `signin.unentitled.denied`, `signin.unentitled.no-session-cookie`, `signin.unentitled.me-refused` |
| Sign-out, needing Origin and CSRF; a signed-out cookie stays dead | `signin` | `signout.blair.no-csrf-refused`, `signout.blair.foreign-origin-refused`, `signout.blair.logout`, `signout.blair.old-cookie-refused` |
| Callback replay, forged or missing login state | `signin` | `callback.replayed-code-refused`, `callback.without-login-refused`, `callback.bare-refused` |
| `/api/v1/me` and the self-service reads (summary, keys, models, notifications, sessions, review inbox, requests) | `me-summary` | `operator.me`, `alex.summary`, `blair.keys`, `blair.models`, `alex.notifications`, `anonymous.me-refused` |
| Money values are text, capabilities follow the role | `me-summary` | `blair.summary-money-is-text`, `operator.capabilities-match-role` |
| Own sessions: list, revoke one, revoke all others | `me-summary` | `sessions.list`, `sessions.revoke-one`, `sessions.revoked-session-dead`, `sessions.revoke-others-kills-others` |
| Personal data export (rate limited to 3 an hour) | `me-summary` | `export.own-data`, `anonymous.export-refused` |
| Workspace listing and access (own, Teams, Projects) | `workspaces` | `workspace.owner-reads`, `workspace.listed-for-owner`, `workspace.not-listed-for-non-member`, `workspace.owner-reads-access` |
| Cross-workspace refusal, unknown id, anonymous | `workspaces` | `workspace.non-member-refused`, `workspace.unknown-id`, `workspace.anonymous-refused` |
| Platform staff read workspace metadata with no workspace powers | `workspaces` | `workspace.platform-admin-reads-metadata`, `workspace.platform-admin-has-no-workspace-powers`, `workspace.auditor-reads-metadata` |
| Personal workspaces are owner-private against staff | `workspaces` | `personal.operator-cannot-read`, `personal.auditor-cannot-list-keys`, `personal.blair-cannot-read-requests`, `personal.cannot-be-shared` |
| Create, rename, disable and re-enable a Team | `workspaces` | `platform.create-by-member-refused`, `workspace.rename`, `platform.disable`, `workspace.disabled-is-gone-for-owner`, `platform.reenable` |
| Key create, list, detail, stats, access, policy read | `keys` | `keys.list`, `keys.detail`, `keys.stats`, `keys.key-access`, `keys.key-policy-read`, `keys.issuance-budget` |
| Key secret returned once, never again | `keys` | `keys.list-never-returns-token`, `keys.detail-never-returns-token` |
| Key validation (lifetime, name, missing fields, installation maximum) | `keys` | `keys.lifetime-zero-refused`, `keys.lifetime-over-365-refused`, `keys.blank-name-refused`, `keys.missing-field-refused`, `keys.refusals-create-nothing` |
| Key rotate, disable, enable, revoke; a revoked key no longer authenticates and never returns | `keys` | `keys.rotate`, `keys.rotated-old-key-refused`, `keys.disabled-key-refused`, `keys.revoke`, `keys.revoked-key-refused`, `keys.revoked-key-cannot-reenable`, `keys.revoked-key-reason` |
| Key visibility between members; platform staff and non-members cannot mint | `keys` | `keys.member-cannot-see-others-key`, `keys.admin-reads-members-key`, `keys.platform-admin-cannot-mint`, `keys.auditor-cannot-mint`, `keys.anonymous-create-refused` |
| Service accounts and their keys | `keys` | `service-accounts.create`, `service-accounts.mint-key`, `service-accounts.key-works`, `service-accounts.disabled-key-refused`, `service-accounts.member-cannot-create` |
| Inference authentication on `/v1/models` (valid, anonymous, bad key, wrong scheme, cookie) | `inference-auth` | `models.valid-key`, `models.anonymous-refused`, `models.unknown-key-refused`, `models.basic-scheme-refused`, `models.session-cookie-is-not-a-key` |
| Chat Completions: authentication and the `model_not_found` refusal when no deployment is enabled | `inference-auth` | `chat.anonymous-refused`, `chat.unknown-key-refused`, `chat.unknown-model-refused`, `chat.catalogue-model-without-deployment-refused`, `chat.refusal-code`, `chat.stream-refused-before-streaming` |
| Responses: the same | `inference-auth` | `responses.anonymous-refused`, `responses.unknown-model-refused`, `responses.refusal-code` |
| Messages: `anthropic-version` required, Bearer or `x-api-key`, never both | `inference-auth` | `messages.anonymous-refused`, `messages.unknown-model-refused`, `messages.version-required`, `messages.unsupported-version-refused`, `messages.x-api-key-accepted`, `messages.two-credentials-refused`, `messages.refusal-type` |
| Embeddings, files, batches and videos: authentication and listing | `inference-auth` | `embeddings.anonymous-refused`, `embeddings.unknown-model-refused`, `files.anonymous-refused`, `batches.valid-key-lists` |
| Request validation after authentication (method, malformed JSON, empty body, missing model) | `inference-auth` | `chat.wrong-method`, `chat.malformed-json-refused`, `chat.empty-body-refused`, `chat.missing-model-refused` |
| An inference key is not a management credential | `credential-boundaries` | `key-as-bearer.me-refused`, `key-as-bearer.platform-users-refused`, `key-as-bearer.mutation-refused`, `key-as-session-cookie-refused`, `key-with-session-cookie-refused` |
| Admin API tokens: create, scoped read, refused writes, not a workspace or inference credential, revoke | `credential-boundaries` | `admin-token.create`, `admin-token.read-scope-reads`, `admin-token.read-scope-cannot-write`, `admin-token.cannot-list-keys`, `admin-token.is-not-an-inference-key`, `admin-token.revoked-token-refused`, `admin-token.user-cannot-create` |
| Missing or wrong CSRF token, missing, foreign or `null` Origin on a mutation | `csrf-origin` | `create-key.no-csrf-refused`, `create-key.wrong-csrf-refused`, `rename-workspace.no-origin-refused`, `platform-create-workspace.foreign-origin-refused`, `platform-discoverable.null-origin-refused`, `refused-mutations-created-no-key`, `other-persons-csrf-refused` |
| Reads need no CSRF; the control mutation succeeds | `csrf-origin` | `get-needs-no-csrf`, `rename-with-both-succeeds` |
| Members: list, add, remove, last-owner protection, candidate picker | `members-invitations` | `members.owner-lists`, `members.add`, `members.remove`, `members.last-owner-protected`, `candidates.search`, `candidates.excludes-members`, `members.non-member-refused` |
| Platform views of members | `members-invitations` | `platform-members.admin-lists`, `platform-members.auditor-lists`, `platform-members.admin-adds`, `platform-members.auditor-cannot-add` |
| Invitations: create, list, accept by the invitee only, single use, revoke | `members-invitations` | `invitations.create`, `invitations.accept`, `invitations.wrong-person-refused`, `invitations.single-use`, `invitations.revoke`, `invitations.revoked-code-refused`, `invitations.member-cannot-create` |
| Platform Admin reads: directory, overview, catalogues, models, deployments, providers, settings, audit, mappings | `platform-admin` | `admin.users`, `admin.catalogs`, `admin.models`, `admin.deployments`, `admin.settings-general`, `admin.audit`, `admin.group-mappings`, `users.lists-demo-personas`, `users.detail` |
| Platform reads refused to members and anonymous | `platform-admin` | `member.users-refused`, `anonymous.users-refused`, `installation-policy.removed-for-admin`, `route.undeclared-platform-route-denied` |
| Catalogue and deployment reads | `platform-admin` | `catalog.models-have-readiness`, `catalog.model-detail`, `catalog.deployment-detail`, `catalog.deployment-prices` |
| Policy and limits: platform override, local tighter limit, composition, no loosening, clearing | `platform-admin` | `policy.platform-read`, `policy.override`, `policy.limits-compose`, `policy.local-cannot-loosen`, `policy.loosen-reason`, `policy.clear-override`, `policy.local-limit-survives-override-removal`, `policy.money-is-text` |
| Discoverability toggle, cost centres | `platform-admin` | `discoverable.on`, `discoverable.off`, `cost-centers.create`, `cost-centers.delete`, `cost-centers.delete-archives` |
| Auditor reads configuration, usage and audit | `auditor-readonly` | `auditor.reads-users`, `auditor.reads-settings-general`, `auditor.reads-audit`, `auditor.reads-cost-report`, `auditor.capabilities-read-only` |
| Auditor mutations are refused and change nothing | `auditor-readonly` | `auditor.cannot-create-workspace`, `auditor.cannot-workspace-policy`, `auditor.cannot-settings-general`, `auditor.cannot-change-role`, `auditor.cannot-create-admin-token`, `auditor.refusals-changed-nothing`, `auditor.refusals-started-no-review` |
| Auditor sees shared-workspace totals, not activity, and no personal workspace | `auditor-readonly` | `auditor.reads-workspace-usage-overview`, `auditor.cannot-read-workspace-requests`, `auditor.cannot-list-workspace-keys`, `auditor.cannot-read-personal-workspace` |
| Access requests: discover, request, withdraw, deny, approve (workspace admin and Platform Admin) | `access-requests` | `targets.discoverable-workspace-listed`, `request.create`, `request.withdraw`, `request.deny`, `request.approve-by-workspace-admin`, `request.approve-by-platform-admin`, `request.membership-after-approve` |
| Access request refusals | `access-requests` | `request.hidden-workspace-refused`, `request.requester-cannot-approve`, `request.auditor-cannot-approve`, `request.denied-cannot-be-withdrawn`, `request.anonymous-refused`, `request.no-csrf-refused` |
| Access reviews: start, snapshot, reviewer inbox, decide, assign, export, close | `access-reviews` | `review.start`, `review.counts-two-memberships`, `review.reviewer-inbox`, `review.decide-keep`, `review.clear-reviewer`, `review.export-admin`, `review.export-is-csv`, `review.close` |
| Access review refusals (role, own item, closed, group-only flag) | `access-reviews` | `review.member-cannot-start`, `review.auditor-cannot-start`, `review.member-cannot-decide-own-item`, `review.owner-cannot-decide-own-item`, `review.flag-requires-group-access`, `review.close-twice-refused`, `review.decision-after-close-refused` |
| Platform reports and usage (overview, explore, storage, cost report, request logs, schedules) | `reports-usage` | `platform.usage-overview-admin`, `platform.usage-explore-auditor`, `platform.cost-report-admin`, `platform.request-logs-admin`, `platform.usage-bad-date-refused`, `platform.usage-missing-dates-refused` |
| Workspace reports and usage for admins and members; auditors see totals only | `reports-usage` | `workspace.usage-overview-admin`, `workspace.cost-report-member`, `workspace.usage-export-is-csv`, `workspace.storage-usage-member-refused`, `workspace.requests-auditor-refused`, `workspace.usage-overview-auditor-totals` |
| Reports refuse non-members and anonymous; empty data is not invented | `reports-usage` | `workspace.requests-non-member-refused`, `workspace.usage-overview-anonymous-refused`, `workspace.request-log-starts-empty`, `workspace.cost-summary-money-is-text` |
| A completed generation (any protocol), streaming, token and cost accounting of real requests | not covered | The demo has no model server or provider and its example deployments start disabled. A candidate needs a provider-backed environment (`docs/provider-adapters.md`, opt-in provider acceptance) |
| Files, batches, videos, audio, images, rerank, realtime and embeddings that succeed | not covered | They need an enabled deployment of that kind; only authentication, listing and refusals are checked |
| Failover, retries, cancellation of a dropped stream, admission and budget exhaustion under load | not covered | Need a running upstream; covered by the Rust mock-upstream and PostgreSQL integration tests and the load test (`scripts/loadtest.py`) |
| Enabling or disabling a model, deployment or connection; prices; routing; bulk disable | not covered | Shared installation state that cannot be restored exactly, and enabling one calls a provider. The reads and the refusals are covered |
| Installation settings writes (general, privacy, e-mail, storage, audit export, logo), alerts and webhooks | not covered | They reach external systems (SMTP, object storage, webhooks, a SIEM) that the demo does not have, or change installation-wide state |
| User lifecycle (create, disable, change role, remove personal data, revoke a person's sessions), group mappings writes | not covered | Users and role grants cannot be deleted, and the seeded personas must not be changed. The reads and the refusals are covered |
| Workspace and onboarding templates, report schedules, provider cost imports and reconciliation, legal holds | not covered | Records without a delete route that would stay in the environment, or that need provider data; the reads are covered where an endpoint is listed in `platform-admin` and `reports-usage` |
| Browser UI flows, responsive layout, accessibility | not covered | Not an API journey; `npm run test:browser` (Playwright and axe) covers them |
| SCIM provisioning, the Prometheus metrics listener, OIDC sign-in against a real identity provider | not covered | Not enabled in the demo; covered by Rust tests, `docs/identity.md` acceptance and the staging rehearsal |
| Multi-replica behaviour, the Pooler, NetworkPolicy and ingress | not covered | The Spark environment is one pod (AGENTS.md); covered by the staging and Helm tests |
