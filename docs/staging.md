# Staging deployment and live-integration preparation

This is a **single-host staging baseline, not approval for public production launch**. It packages the Rust API and built React SPA, uses HTTPS ingress, and separates schema ownership from runtime database access. The default rehearsal binds only `127.0.0.1`; it neither alters `gateway_demo` nor enables paid providers. OIDC is intentionally disabled until you supply and validate your identity configuration.

## Architecture and prerequisites

- Docker Engine with Compose v2 or later, Python 3, and curl on the operator machine.
- Multi-stage `Dockerfile`: Node builds the SPA, Rust builds the locked binary, and the runtime is distroless (`gcr.io/distroless/cc-debian13`, pinned by digest): no shell, package manager, curl, Node or Cargo. The binary is the entrypoint and does its own `_FILE` secret import and health check ([container image](operations.md#container-image)). `GATEWAY_WEB_DIR=/app/web`.
- Caddy terminates HTTPS and forwards unbuffered SSE to Rust. It has no exposed admin API or access log. Only ingress publishes ports; Postgres and Rust do not.
- Dedicated PostgreSQL 17 volume/network; the database network is internal. Rust also has outbound connectivity for OIDC/provider calls. This is not an egress firewall.
- Rust runs as UID/GID 10001, with a read-only root filesystem, dropped Linux capabilities, no-new-privileges, a small `/tmp` tmpfs, and a 150-second shutdown grace period.
- Runtime readiness verifies migration versions/checksums. Startup never runs migrations or provisions identities. The default pool is 10 connections per replica: size database capacity accordingly.
- The image `HEALTHCHECK` runs `open-model-gateway healthcheck` in exec form; ingress waits for it. Use `docker compose exec gateway /usr/local/bin/open-model-gateway healthcheck` for a manual probe; `sh`/`curl` inside the gateway container do not exist.

Tag the gateway image with the Git commit and promote the tested digest. For rollback, use the previously tested digest. The Dockerfile pins the Node, Rust and runtime base images by digest. Staging Compose also pins its default PostgreSQL and Caddy images to reviewed digests at `mirror.gcr.io`. To replace either default, set `STAGING_POSTGRES_IMAGE` or `STAGING_CADDY_IMAGE` to the reviewed image and digest in `staging.env` or the environment.

The gateway, migration and grant services use the gateway image already in the local image store. Build the staging gateway first, or pull the selected release digest before setting `GATEWAY_IMAGE` to that reference. The release rehearsal follows this pull-by-digest path. [Image verification](verification.md#no-docker-hub-pulls-in-ci)

## Image CI and registry publication

For pull requests and branches, CI's `staging-image` job builds a Linux image, runs the isolated migration/runtime-permission/HTTPS/restore smoke without real credentials, then `npm run test:container` on the same image.

On pushes to `main` and `v*` tags, once the tests pass, `.github/workflows/image.yml` publishes `ghcr.io/bitop-dev/open-model-gateway`. The next release after v0.18.0 is the first release under this path; [historical images retain their original path and identity](#image-paths-and-signature-identities).

1. The `build` job builds each architecture (linux/amd64, linux/arm64) on a native runner, with SBOM and provenance attestations, and pushes it **by digest only**, without a tag. It can write packages, so it runs nothing from outside this repository except `actions/checkout` and the `docker/*` build, login and Buildx actions. Its job outputs are the two digests.
2. The `verify` job, one per architecture, has a read-only token (`contents: read`, `packages: read`). It pulls that exact digest, which must report the `Cargo.toml` version (`docker run --rm IMAGE --version`, exec form). It then runs this rehearsal (`init`, `db`, `migrate`, `up`, `verify`, HTTPS readiness through `local-ca`, `restore-check`) with `GATEWAY_IMAGE` set to the digest, and checks that the gateway container ran it. The runtime contract test (`tests/container-image.test.mjs`) then runs on the digest, and a Trivy scan fails on fixable HIGH or CRITICAL findings. Trivy is a pinned release binary (version and SHA-256 in `image.yml`), not an action.
3. Only when both `verify` jobs pass does `publish` join the tested digests into one multi-arch index, tag it (`sha-<short>` for `main`; `vX.Y.Z`, `vX.Y` and `latest-release` for final tags; never `latest`) and sign it with cosign keyless signing (a pinned cosign binary, checked against its SHA-256).
4. For a `v*` tag, a GitHub Release is created from `docs/releases/vX.Y.Z.md` with the per-platform SBOMs, the digest and checksums.

No third-party action runs in a job that can write (#194); see [verification](verification.md#supply-chain).

The workflow can also be run by hand on `main` or a `v*` tag; other refs are refused:

```sh
gh workflow run image.yml --ref main
```

The job summary prints the digest and the `cosign verify` command. Verify the signature and promote by digest ([image paths and signature identities](#image-paths-and-signature-identities)). The workflow does not deploy to a host or change provider settings. Configure a registry credential helper on the target host if the package is private; do not place registry tokens in Compose or Git. Each architecture is rehearsed on GitHub's runners only; acceptance on your own target hosts is still needed.

## Image paths and signature identities

The repository moved to `bitop-dev`; existing container packages stayed with `ncecere`. Published releases through **v0.18.0** keep their original path and signing identity. The next release and new `main`/development builds use `bitop-dev`. A missing tag at the new path is not a reason to use an older image or accept a different certificate. Historical release notes remain authoritative for their own releases.

For a new release after v0.18.0, take its exact tag and digest from the GitHub Release:

```sh
cosign verify ghcr.io/bitop-dev/open-model-gateway@sha256:<digest> \
  --certificate-identity https://github.com/bitop-dev/open-model-gateway/.github/workflows/image.yml@refs/tags/<tag> \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

For a historical release, for example v0.18.0, use that release's original digest:

```sh
cosign verify ghcr.io/ncecere/open-model-gateway@sha256:<digest> \
  --certificate-identity https://github.com/ncecere/open-model-gateway/.github/workflows/image.yml@refs/tags/v0.18.0 \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Use cosign v3 or newer and the **exact identity**, not a pattern accepting either owner. Development images (`dev-<sha12>` and `dev-<sha12>-arm64`) are unsigned and skip the release scan/rehearsal. `main` images use `@refs/heads/main` in the certificate identity instead of a release tag. The loadtest image is a separate development artifact at `ghcr.io/bitop-dev/open-model-gateway-loadtest`.

At release preparation, update the chart's `image.repository` to `ghcr.io/bitop-dev/open-model-gateway` together with the release tag and chart versions. The current chart still defaults to v0.18.0 at its original path. Set repository and verified digest together in rollout values, including migration jobs; for rollback, retain the old repository/digest pair and follow the release's database restore requirements. See [next-release upgrade guidance](releases/unreleased.md#upgrade).

## First local HTTPS rehearsal

From the repository root:

```sh
python3 scripts/staging.py init
python3 scripts/staging.py build
python3 scripts/staging.py db
python3 scripts/staging.py migrate
python3 scripts/staging.py up
python3 scripts/staging.py verify
python3 scripts/staging.py local-ca
curl --cacert .local/staging/local-ca.crt https://localhost:18443/health/ready
curl --cacert .local/staging/local-ca.crt https://localhost:18443/api/v1/auth/config
```

`verify` probes readiness inside the gateway container with the binary's own `healthcheck`, checks with `docker inspect` that it runs as `10001:10001` from a read-only root filesystem with the binary as entrypoint and a healthy exec-form health check, confirms that `/bin/sh`, bash, busybox, curl, perl, apt-get, dpkg, Node and Cargo cannot be started in it, and runs the database privilege assertions.

Open **https://localhost:18443**. Caddy uses its own local CA for localhost. The `local-ca` command copies only its public certificate; it does not modify system/browser trust. Trust that certificate explicitly in a disposable browser profile if needed; do not normalize bypassing certificate validation for real staging. The plain HTTP redirect is intended for normal public ports; use the HTTPS URL directly for this nonstandard-port rehearsal.

The auth configuration should report `enabled:false`, and unauthenticated management calls should return 401. That is expected, not a reason to enable the demo issuer. Production mode forbids the HTTP loopback demo issuer.

`init` generates independent random bootstrap, migrator, and runtime credentials. It refuses to overwrite existing state. Do not rerun it to rotate credentials or delete existing state to repair an error. Initialization creates private host directories (0700), configuration (0600), and file secrets (0444). File secrets must be readable by non-root container users; their parent directories prevent other host users from traversing them. Docker/host administrators can still read them. Compose secrets are file mounts, **not an encrypted secret-management service**.

All generated state is under ignored `.local/staging/` and excluded from the Docker build context. Do not paste the generated database URLs or secrets into chat, tickets, logs, or Git.

## Database authority and releases

Three distinct identities exist:

| Role | Purpose |
|---|---|
| `gateway_bootstrap` | Dedicated cluster initialization, backup/restore administration; never mounted into Rust |
| `gateway_migrator` | Owns database/schema and runs migrations/trusted provisioning in an explicit one-shot container |
| `gateway_runtime` | Explicit operation/column grants only; no DDL, database/schema ownership, role membership, TEMP, migration writes, or direct platform-user privilege changes |

`deploy/staging/runtime-grants.sql` is a reviewed allowlist, not a blanket default grant. New tables/operations require an explicit grant review. Prices, ledger and audit records are insert/read-only to runtime. User column ACLs allow ordinary OIDC user creation/link consumption but deny setting `platform_admin`. Shared grant tables have a minimal column UPDATE permission because PostgreSQL requires it even for `SELECT … FOR SHARE`. Public trigger functions are not directly executable by runtime; PostgreSQL still invokes existing triggers, under the caller's table permissions.

**Use a dedicated gateway database.** `open-model-gateway grants apply` (or the SQL file run directly) revokes `TEMPORARY` from both `PUBLIC` and `gateway_runtime` on the **connected database only**, including an old explicit runtime grant. PostgreSQL normally grants TEMP to `PUBLIC`; revoking only the runtime grant would leave temporary-table creation allowed. Removing the PUBLIC grant affects **every role that relied on it in this database**, not just runtime. It does not change other databases, database ownership, or the owner's/migrator's privileges. Runtime code does not use temporary tables; some migrations do, so keep using the owner/migrator for migrations.

Run `grants apply` as the database and schema/object owner (`gateway_migrator` in staging), not merely a role with table grants: it must be able to revoke database privileges as well as reapply the table/function allowlist. `grants verify` needs a connecting role allowed to `SET ROLE gateway_runtime`; it includes a rollback-only `CREATE TEMP TABLE` refusal probe. These commands never run implicitly in `serve`, and `grants apply` does not run migrations. Configure database credentials privately; never paste connection URLs or secrets into logs or issue comments.

`migrate` runs the migration job **and then reapplies grants**, both with the migrator secret. Never deploy new application code without its matching migration and grant policy. All replicas must use the matching schema version; rolling old/new mixed-schema replicas is not promised by the readiness contract.

Upgrade procedure:

1. Record current image digest and configuration; build/test the new image without real secrets in its build context.
2. Quiesce/stop ingress and Rust for incompatible migrations. Back up and complete a restore rehearsal.
3. Set `GATEWAY_IMAGE` in `.local/staging/staging.env` to the tested image tag/digest (pull it separately for a remote registry).
4. Run `python3 scripts/staging.py migrate`, then `up`, then `verify`.
5. Complete HTTPS, authentication, access and bounded inference checks before reopening traffic.

Never run `bootstrap-dev` or `bootstrap-demo` in this stack. Never use the migrator connection as a runtime shortcut to work around a denied privilege. Diagnose the missing operation and update/test the allowlist deliberately.

## Real hostname and HTTPS

Edit the generated `.local/staging/staging.env`, not the committed example:

```dotenv
GATEWAY_HOST=gateway.your-domain.example
GATEWAY_PUBLIC_URL=https://gateway.your-domain.example
STAGING_BIND_ADDRESS=0.0.0.0
STAGING_HTTP_PORT=80
STAGING_HTTPS_PORT=443
```

Use your real DNS name; the example is not deployable. Point DNS to the host, allow the required ACME challenge traffic, and persist Caddy's certificate volume. Retain a firewall/VPN/access restriction for your pilot until security/abuse testing is complete. `GATEWAY_PUBLIC_URL` must be the browser-visible canonical HTTPS origin, not the internal Rust URL. Registering an internal origin breaks cookies, callback validation and CSRF.

Database transport in this single-host Compose baseline is unencrypted inside its isolated bridge network. If moving PostgreSQL to another host/service, use verified PostgreSQL TLS (`sslmode=verify-full` with the correct trust configuration) and an equivalent role/grant setup. Do not expose the local database port to the Internet.

## OIDC information needed next

Provide the following **non-secret** information:

- Browser-facing staging hostname/origin.
- OIDC issuer URL (exact issuer, not the authorization endpoint).
- Client ID and whether the client is public PKCE or confidential.
- Approved initial platform administrator's verified email.
- Confirmation that the ID token supplies stable `sub`, `email`, and `email_verified:true`; note any provider-specific claim mapping requirements.

Register this redirect exactly:

```text
https://YOUR-STAGING-HOST/api/v1/auth/callback
```

Then set:

```dotenv
STAGING_OIDC_ENABLED=1
GATEWAY_OIDC_ISSUER=https://YOUR-ISSUER
GATEWAY_OIDC_CLIENT_ID=YOUR-CLIENT-ID
```

For a confidential client only, place its secret in `.local/staging/secrets/oidc_client_secret`, with the same protected parent directories and readable file permissions, and set `STAGING_OIDC_CONFIDENTIAL=1`. The gateway binary reads it via `_FILE` at startup, never a Docker build argument or command-line argument. Do not create an empty placeholder secret or set unused OIDC variables to empty strings.

Provision the explicitly approved initial identity, then restart with discovery enabled:

```sh
python3 scripts/staging.py provision-user --email APPROVED-VERIFIED-EMAIL
python3 scripts/staging.py up
```

The client must support authorization code + PKCE. IdP groups do not automatically grant gateway roles. First identity linking is explicit; do not weaken verified-email checks to accommodate a claim mismatch.

Known staging limitations: issuer key rotation relies on the bounded JWKS refresh ([identity](identity.md#signing-keys-jwks)), not yet exercised against the real issuer; SCIM passed one live acceptance run against Authentik 2025.10.3 on v0.9.1 (2026-10-28, #60) on a separate isolated installation, not in this rehearsal, and no later release has been run ([SCIM](scim.md#operations-and-limits)). Session/attempt cleanup, login abuse controls, independent security review and production-load acceptance also remain. Keep this pilot private.

## First real provider: explicit opt-in only

No provider calls, demo users, prices, deployments or API keys are created by this deployment. Do not supply provider credentials until the platform administrator can sign in and the access boundary is verified.

Existing OpenAI/Anthropic acceptance can use the optional Compose overlays through these flags:

```dotenv
# Choose ONLY the provider being tested:
STAGING_OPENAI_ENABLED=1
GATEWAY_SECRET_ENV_ALLOWLIST=OPENAI_API_KEY
# Alternative:
# STAGING_ANTHROPIC_ENABLED=1
# GATEWAY_SECRET_ENV_ALLOWLIST=ANTHROPIC_API_KEY
```

Write the corresponding `.local/staging/secrets/openai_api_key` or `anthropic_api_key` file. If both are intentionally enabled, explicitly list both names in the allowlist. Bedrock should use an appropriate workload identity; this baseline disables EC2 metadata and does not mount workstation AWS credentials.

Planned additional connections are OpenAI-compatible Chat/Responses servers, vLLM and SGLang. They are not implemented by these deployment overlays. Their custom upstream origins require an explicit server-side allowlist and per-adapter capability tests before rollout.

Use [live acceptance](live-acceptance.md) before enabling a paid deployment. Cost limits use configured estimates, not provider invoices. Start with one model, one test organization/workspace, one short-lived key, a low organization ceiling and a small explicit output-token limit. Never claim a tiny budget is an absolute vendor-billing guarantee.

## Backup, restore and recovery

For checksummed manifests, lineage-checked restores into empty databases, PITR guidance, metrics/alerting and the load-test baseline, see the [operations runbook](operations.md) and `scripts/backup.py`.

```sh
python3 scripts/staging.py backup
python3 scripts/staging.py restore-check
# Or check a specific retained backup:
python3 scripts/staging.py restore-check --backup .local/staging/backups/YOUR-BACKUP.dump
```

Backups use PostgreSQL custom format, retain data/migration history, and omit ownership/ACLs so those can be restored under the target migrator and current grant policy. They contain sensitive identities and accounting data. Files are owner-readable only, **not encrypted**: encrypt them using your approved system before off-host storage, restrict access, establish retention, and retain required configuration/secret versions separately. A same-host dump is not disaster recovery.

Keep Caddy certificate/private-key volumes and the protected deployment secret/configuration versions in your separate encrypted disaster-recovery plan as well; the database dump does not contain them.

`restore-check` always creates a newly named disposable database in the isolated staging cluster and drops only that database afterwards. It never restores over `gateway` or the demo. It verifies that the dump loads and key history tables exist; it does not certify all application recovery workflows, a recovery-time objective, or off-host availability. For real recovery, provision a separate replacement stack, restore under its migrator, apply the reviewed runtime grants, validate schema/checksums/accounting and OIDC, then deliberately cut over DNS/ingress. Do not run a blind down migration or delete unknown reservations to make reconciliation pass.

`python3 scripts/staging.py down` stops this project but deliberately retains volumes and credentials. Do not use `docker compose down -v` on a populated staging deployment. No shutdown command here targets the original demo containers.
