# Open Model Gateway: unreleased

Notes for changes on `main` since v0.5.0. They become the next release's notes.

## Security

- Sign-in abuse controls (#78): per-address limits on sign-in starts and callbacks (`GATEWAY_LOGIN_RATE_PER_MINUTE`, default 60 a minute, per replica), and at most 10,000 pending sign-in attempts installation-wide (503 beyond). Refusals return 429 or 503 with `Retry-After` and store nothing. Behind a reverse proxy, set `GATEWAY_TRUSTED_PROXIES` (Helm `gateway.trustedProxies`) to its addresses, or every user shares the proxy's budget. New metric `gateway_auth_throttled_total`. Migration 0040 adds an index on `oidc_login_attempts(expires_at)`; it is a plain `CREATE INDEX`, so sign-in waits while it builds (the table holds only sign-in attempts, but they have never been deleted).
- An admin suspension is audited as `user.suspended` (with the reason) and a reactivation as `user.reactivated`, instead of `user.updated` without detail. `user.updated` now flags `email_changed` or `oidc_link_changed`, never the values, and is written only when something changed (#158).
- After a restore, `scripts/backup.py reapply-revocations` copies every revocation (keys, grants, sessions, invitations) and suspension (users, keys, service accounts, workspaces) from the old database onto the restored one, so access removed after the backup stays removed (#146). It's a dry run without `--yes`, applies in one transaction and refuses a different installation. The restore runbook in docs/operations.md now covers revocations, sessions and lost spend.

- Revocations are final in the database: migration 0039 adds triggers that refuse clearing or changing `revoked_at` once set on API keys, membership and platform role grants, browser sessions and invitations, for every role (#145). Hand-written SQL that "un-revokes" a key or grant now fails; issue a new one instead.

- The OIDC client (discovery, token exchange, JWKS) no longer uses ambient `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` or implicit retries, like every provider client (#144). If sign-in only worked through such a proxy, allow the gateway to reach the issuer directly.

- CI pins every GitHub Action by commit SHA and the `Dockerfile` build images by digest, and `npm run test:supply-chain` fails CI on any unpinned one (#143).

- Every response now carries a Content-Security-Policy, `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer` (#142). The dashboard's policy allows only its own scripts, styles, fonts and API (no inline script or `eval`) and can't be framed; API and file responses get `default-src 'none'`. If you embed the dashboard in a frame or load it through a proxy that injects scripts, that no longer works.

- **Breaking (#141):** Teams and Projects have a new setting, **who sees files, batches and videos** (Workspace › Settings › General; `object_visibility` on `GET/PATCH /api/v1/workspaces/{ws}`, migration 0038). **Per user**, the default for new and existing shared workspaces: a regular member's API keys and dashboard see only objects created with that member's keys; service-account keys and workspace owners/admins see everything. **Workspace-wide:** every key sees everything. In both modes only the creator, an owner/admin or a service-account key may delete or cancel (`403`); an object the caller can't see returns `404`, also as a batch's `input_file_id`. Before, any key of a shared workspace could read, delete or cancel every member's objects through `/v1/files`, `/v1/batches` and `/v1/videos`.

  **Upgrade:** run the migration and reapply the runtime grants (`object_visibility` is added to the runtime role's `UPDATE` on `workspaces`). Clients that read other members' objects with a member key get `404` until a workspace admin sets the workspace to Workspace-wide; use a service-account key for workspace-wide automation.

- rustls 0.23.45 → 0.23.46 for RUSTSEC-2026-0346 (session `Debug` output exposed TLS secrets), -0347 (data sent to the outer server after an ECH rejection), -0348 (unbounded Brotli certificate decompression) and -0349 (an unauthenticated ServerHello could downgrade later connections to classical key exchange).

- **Breaking (#140):** the SMTP password reference now resolves only from the new `GATEWAY_SMTP_SECRET_ENV_ALLOWLIST` (Helm: `gateway.smtpSecretEnvAllowlist`), never from the provider list `GATEWAY_SECRET_ENV_ALLOWLIST`. Before, a Platform Admin, who chooses the SMTP host, could set the SMTP password to a provider key reference and send that key to a mail server they control. The gateway refuses to start if a name is on both lists, or if the SMTP list names one of its own credentials (`DATABASE_URL`, `GATEWAY_*`, `AWS_*`, `GATEWAY_OIDC_CLIENT_SECRET`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`).

  **Upgrade:** move your SMTP password variable's name from `GATEWAY_SECRET_ENV_ALLOWLIST` to `GATEWAY_SMTP_SECRET_ENV_ALLOWLIST`. Until you do, invitations and alerts are not emailed (Settings › Email shows the reference as not allowed). If a provider key was ever used as an SMTP password, rotate it.

- New [threat model](../threat-model.md) (#32): STRIDE threats by area with the code and tests that mitigate each, residual risk, and a gaps table mapped to follow-up issues, for code reviews and the independent security review (#61).

## Verification

- Homelab cluster run (#25): a PgBouncer restart under load failed 2 of 5,400 requests and a dropped LISTEN connection none; primary failover promoted the standby in 18 s with the chart's 15 s smart shutdown. Accounting stayed exact through every fault. A second write outage after a 2-instance failover is tracked in #149, and the remaining steady-load stalls in #132. See [HA PostgreSQL](../postgresql-ha.md).
