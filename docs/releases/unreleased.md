# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.9.0](v0.9.0.md). They become the next release's notes.

## Security

- Move the gateway and development loadtest runtime to a digest-pinned Debian 13 distroless base, replacing the deprecated Debian 12 base and its vulnerable OpenSSL package (CVE-2026-84782). UID/GID `10001:10001`, read-only execution and the no-shell runtime remain unchanged. The CA trust store is refreshed too; verify outbound TLS integrations before upgrading. No database migration is required.

