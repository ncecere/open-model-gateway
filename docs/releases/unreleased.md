# Open Model Gateway: unreleased

Notes for changes on `main` since v0.5.0. They become the next release's notes.

## Security

- rustls 0.23.45 → 0.23.46 for RUSTSEC-2026-0346 (session `Debug` output exposed TLS secrets), -0347 (data sent to the outer server after an ECH rejection), -0348 (unbounded Brotli certificate decompression) and -0349 (an unauthenticated ServerHello could downgrade later connections to classical key exchange).
- **Breaking (#140):** the SMTP password reference now resolves only from the new `GATEWAY_SMTP_SECRET_ENV_ALLOWLIST` (Helm: `gateway.smtpSecretEnvAllowlist`), never from the provider list `GATEWAY_SECRET_ENV_ALLOWLIST`. Before, a Platform Admin, who chooses the SMTP host, could set the SMTP password to a provider key reference and send that key to a mail server they control. The gateway refuses to start if a name is on both lists, or if the SMTP list names one of its own credentials (`DATABASE_URL`, `GATEWAY_*`, `AWS_*`, `GATEWAY_OIDC_CLIENT_SECRET`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`).

  **Upgrade:** move your SMTP password variable's name from `GATEWAY_SECRET_ENV_ALLOWLIST` to `GATEWAY_SMTP_SECRET_ENV_ALLOWLIST`. Until you do, invitations and alerts are not emailed (Settings › Email shows the reference as not allowed). If a provider key was ever used as an SMTP password, rotate it.

- New [threat model](../threat-model.md) (#32): STRIDE threats by area with the code and tests that mitigate each, residual risk, and a gaps table mapped to follow-up issues, for code reviews and the independent security review (#61).

## Verification

- Homelab cluster run (#25): a PgBouncer restart under load failed 2 of 5,400 requests and a dropped LISTEN connection none; primary failover promoted the standby in 18 s with the chart's 15 s smart shutdown. Accounting stayed exact through every fault. A second write outage after a 2-instance failover is tracked in #149, and the remaining steady-load stalls in #132. See [HA PostgreSQL](../postgresql-ha.md).
