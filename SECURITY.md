# Security policy

## Reporting a vulnerability

Please report vulnerabilities **privately**. Don't open a public issue, pull request or discussion.

Use GitHub's private vulnerability reporting: **[Report a vulnerability](https://github.com/ncecere/open-model-gateway/security/advisories/new)** on the repository's Security tab. Only the maintainers see the report, and we work on the fix with you in a private advisory.

Include:
- the affected version or commit;
- what an attacker can do, and what they need first (an account, a platform or workspace role, an inference key, SCIM token, network position);
- steps or a proof of concept to reproduce it;
- any suggested fix.

Open Model Gateway has one maintainer. We aim to reply within 5 business days and to agree a disclosure date with you once we understand the problem. We credit reporters in the fix's release notes unless you'd rather we didn't. Please give us reasonable time to fix the problem before you disclose it. Don't access or change data that isn't yours while testing, and don't send paid provider traffic through an installation you don't operate.

## Supported versions

Open Model Gateway is pre-1.0 development software. Security fixes are made on `main` and released as a patch of the latest minor release. Older minor releases don't get fixes, so upgrade to the latest release to receive them.

| Version | Supported |
|---|---|
| 0.3.x (latest patch) | Yes |
| 0.2.x, 0.1.x and earlier builds | No |

Release images are signed from v0.3.0. Verify an image before you deploy it, as the [release notes](docs/releases/v0.3.0.md#verifying-the-images) describe.

## Scope

In scope: the code in this repository, including
- the `open-model-gateway` binary (API, inference engine, background workers, migrations and CLI) and its default configuration;
- the web dashboard in `apps/web/`;
- the container image built from the `Dockerfile` and its entrypoint;
- the staging deployment in `deploy/staging/`, including `runtime-grants.sql`.

Areas we care most about:
- **workspace isolation:** one workspace reading or changing another's keys, models, limits, files, batches, logs or usage, or an inference key acting outside its workspace;
- **personal-workspace privacy:** anyone other than the owner, including Platform Admins and Auditors, seeing a personal workspace's keys or request details (platform reports may show aggregate totals only);
- **key and credential handling:** inference keys authorizing management, revoked keys working again, plaintext keys, upstream credentials or SCIM tokens in responses, logs, the UI or the database, or inference credentials forwarded upstream;
- **budget and ledger integrity:** bypassing quotas or budgets, admitting work without a reservation, treating unknown usage as zero, or changing immutable price, ledger or audit history;
- **SSO and SCIM:** signing in without a valid signed ID token and verified email, platform access from OIDC alone, group-mapping or SCIM changes granting roles they shouldn't, or removing the last Platform Admin;
- **file storage encryption:** reading stored files (batch inputs and outputs, Files API uploads) without the master keys, cross-workspace file access, or weakening the per-object encryption;
- **local endpoint approval:** reaching an upstream that isn't an explicitly approved, pinned destination (SSRF), redirects, ambient proxies, or plain HTTP to a cloud provider;
- **prompt and response non-storage:** prompt or response bodies, batch lines or file contents written to the database or logs by default;
- browser sessions, CSRF and Origin checks, and the runtime database role's privileges.

Out of scope:
- a specific installation's infrastructure, identity provider, provider accounts or configuration choices (report those to the installation's operator);
- the local demo (`bootstrap-demo`, `scripts/demo-oidc.mjs` and its passwordless issuer) and the development credentials in `compose.yaml` and `.env.example`;
- cost differences between the gateway's configured price estimates and a provider's invoice;
- behaviour of upstream providers and models, including model output and prompt injection that stays inside the caller's own workspace;
- denial of service by volume alone, and findings that need a compromised host, database owner role or master key;
- vulnerabilities in dependencies with no demonstrated impact on this project (report them upstream).

Operators: see [architecture](docs/architecture.md) for the security boundaries, [identity](docs/identity.md), [file storage](docs/file-storage.md), [staging](docs/staging.md) for the database role split, and [operations](docs/operations.md) for upgrades, backups and secret rotation.
