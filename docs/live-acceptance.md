# Live identity and provider acceptance

Use a dedicated staging hostname, database, identities and provider project. This checklist is **not yet live-provider/enterprise-IdP verification**. The automated suite uses fixtures; the staging smoke leaves OIDC disabled and sends no paid traffic.

## Isolated SCIM #60 preparation — NOT PUBLISHED / NOT RUN / NOT ACCEPTED

This section prepares a **fresh, disposable, single-enterprise** staging installation only.
No deployment, Kubernetes/API/DNS/secret operation, IdP mutation or live acceptance is
performed by adding these files. The legacy checklist below (including organizations and
installation ceilings) is historical guidance, **not current proof or the runbook for #60**;
[enterprise rebuild](enterprise-rebuild.md) and [SCIM](scim.md) define current behavior.
Historical Authentik results in `scim.md` do not accept this image or environment.

### Fixed scope and local review

- Release/fullname `omg-scim60`, namespace **`dev-omg` only**; no use of production
  `omg-acceptance`, the loopback demo issuer or the `dev-env` demo workflow. No backend,
  chart-feature or demo-issuer exception is needed. No provider credentials or paid inference.
- [`scim-values.yaml`](../deploy/staging/scim-values.yaml) starts with **zero gateway
  replicas**, HPA/PDB off, ClusterIP only, ingress disabled and `migrations.run=false`.
  Merely applying it would still create database resources: these are preparation files,
  not permission to apply. Enable migrations only by explicit initial-install override.
- **Operator-approved routing, not published:** only `https://omg-scim60.bitop.dev`
  through Pangolin → Newt → **`http://omg-scim60.dev-omg.svc.cluster.local:8080`**.
  Target this specific Service, never a namespace-wide route, production host or metrics
  port `9464`. Chart NetworkPolicy is explicitly enabled: its ingress peer requires
  namespace label `kubernetes.io/metadata.name: pangolin` **AND** pod label
  `app.kubernetes.io/name: newt` in the **same peer**, selecting only release
  `omg-scim60` / `component: gateway` for TCP `8080`. Metrics ingress is disabled;
  this adds no metrics allowance, not a claim that metrics are globally unreachable.
  No Traefik provider-namespace allowlist change or global NetworkPolicy broadening is
  approved or needed. The staging profile alone disables chart `allowHttpsEgress`,
  avoiding its default unrestricted `443` allowance; chart templates/defaults stay unchanged.
- Gateway and standalone bootstrap use
  `ghcr.io/ncecere/open-model-gateway@sha256:31d34b3bcde748148fa96b252c9d73b6964b5e224a7269aaecdf636342c3dc4a`
  (`dev-731ca715e202`, source `731ca715e202`, whose tree matches base `26cf0114` and
  includes #382). This is an **unsigned, unscanned amd64 branch image**, not release
  qualification; the tag is informational and the digest selects the image.
- Dedicated CNPG Cluster `omg-scim60-db`, database `omg_scim60`, one PG18 instance,
  `local-dev` data 5Gi/WAL 1Gi, zero synchronous replicas and `max_wal_size=512MB`.
  The qualified clean homelab `postgres-18` catalog supplied the plain image
  `ghcr.io/cloudnative-pg/postgresql:18-minimal-trixie@sha256:37ade18dbdddba430858c72725aceeec66f33fa5333e82ea1df4942f6c1c83a3`;
  the values use that direct pin, **not** template/vector bootstrap or extra extensions.
  Exactly one transaction-mode Pooler is required: the separately owned
  [`scim-pooler.yaml`](../deploy/staging/scim-pooler.yaml), named `omg-scim60-pooler` in
  `dev-omg`, targeting `omg-scim60-db`. `cnpg.pooler.enabled=false` disables **only chart
  creation**, not pooling in the installation. Keep that value false for activation and
  upgrades to avoid an unpinned duplicate. The existing chart helper still wires runtime
  and schema-wait URLs to `omg-scim60-pooler`; LISTEN and migrator URLs still target
  `omg-scim60-db-rw` directly, with unchanged database and role Secret references.
- **Explicit namespace-local Pooler pin:** operator-default image pinning was unproven:
  read-only preflight found no `PGBOUNCER_IMAGE_NAME` in the live operator Deployment or
  `cnpg-controller-manager-config`. Rather than changing the operator or chart, the
  standalone Pooler's `spec.template.spec.containers[name=pgbouncer].image` selects
  `ghcr.io/cloudnative-pg/pgbouncer@sha256:b15c428057d1ffbc19fb70496911a5f1a47066dcc1315bf794f4e85ade29e2e5`.
  The coordinator verified the published OCI root index's exact descriptor digest and
  four index descriptors. This is **publication metadata only**, not a pull, execution,
  scan, signature verification or release qualification claim. No mutable tag selects the
  image. Preserve the existing chart's Pooler pod labels, resource sizing, transaction mode
  and prepared-statement settings; no extra egress, ingress or operator change is needed.
  This manifest has **not** been deployed; verify the actual selected pin/readiness later.

Local-only checks (no cluster or new dependencies):

```sh
helm template omg-scim60 deploy/helm/open-model-gateway --namespace dev-omg \
  -f deploy/staging/scim-values.yaml
helm template omg-scim60 deploy/helm/open-model-gateway --namespace dev-omg \
  -f deploy/staging/scim-values.yaml --set migrations.run=true
helm lint deploy/helm/open-model-gateway -f deploy/staging/scim-values.yaml
helm lint deploy/helm/open-model-gateway -f deploy/staging/scim-values.yaml --set migrations.run=true
helm lint deploy/helm/open-model-gateway -f deploy/staging/scim-values.yaml --set replicaCount=1
node --test tests/helm-chart.test.mjs
```

The existing mandatory `npm run test:helm` also executes these appended render/static
assertions. They are configuration checks, not schema, authorization, browser or live-IdP proof.
The coordinator must read every diff, obtain two independent security reviews (one adversarial)
and complete final gates before any push or apply.

### Operator preflight and future sequence — not executed

Before **any external mutation**, settle and record in the issue the operator, selected
namespace/environment record, resource ownership, disposable identity/group names, endpoint,
TLS/route design, real IdP app/provider choices, lifetime, explicit teardown and guarded
recovery procedure. Isolated `dev-omg` staging and dedicated Authentik test objects are approved
in principle, not yet enacted. The operator explicitly approved publishing
**`https://omg-scim60.bitop.dev`** and the minimal reviewed staging routing above;
publication, DNS/TLS registration and real IdP setup remain **NOT PUBLISHED / NOT RUN**.
This does not authorize production hosts/routes, Traefik configuration changes, global
policy broadening or PostgreSQL remediation. Any additional exception needs separate
approval; do not substitute production routing or alter another namespace/repository.

1. **Prove freshness and selection.** Check the explicitly selected record, Helm release,
   Cluster identity/UID, standalone Pooler identity/cluster target, PVC ownership and bound
   PV claim references all belong to `dev-omg` / `omg-scim60-db`, with Pooler name exactly
   `omg-scim60-pooler`. This boundary is **not `dev-env-pg`**. Before creation, any existing
   release, Cluster, Pooler, matching old PVC/PV, retained volume or unexpected resource
   is a stop condition, not permission to adopt/reset it. After creation, match
   the newly created Cluster UID through PVC owner references and PV bindings to the
   selected record before migrations. A failed lookup/permission error is not proof of
   absence. Never reuse, reset, migrate or delete an old installation implicitly.
2. **Verify reachability, without inventing migration policy.** The reviewed static
   homelab `kubernetes/platform/dev-areas/base/network-policies.yaml` has default-deny
   plus `allow-cluster-dns`, `allow-same-namespace` and public-web egress for **all pods**.
   These allow bootstrap/migrator DNS and direct same-namespace PostgreSQL; public-web
   egress still permits public `443` with RFC1918 exclusions. This is **additive
   namespace-level development trust, not per-Job or ingress isolation**, nor proof of
   issuer connectivity. A narrow chart policy cannot tighten those existing allowances.
   Disabling chart `allowHttpsEgress` avoids defeating the namespace's private-range
   exclusions with an additional unrestricted `443` rule; it does not remove the existing
   namespace public `443` allowance. The chart's three policies select only release
   `omg-scim60` / `component: gateway`; no migration-specific policy exists in this base.
   Bootstrap's release/app labels and `component: migrate` identify ownership/convention,
   **not a selector granting connectivity**. No extra egress is added here. The operator
   must verify the **deployed** namespace policy and actual Newt namespace/pod labels
   before apply; the rendered conjunctive peer is not a live connectivity proof.
   Read-only namespace/policy metadata checks do not prove freshness, Helm history or
   connection reachability; no
   Helm-history Secret lookup or connection probe is claimed. Real issuer DNS/discovery/JWKS
   and HTTPS reachability also need preflight: same-namespace allowance does not allow
   a cross-namespace IdP. An issuer resolving to a private destination requires a separate,
   precise destination-scoped network gate and gateway private-egress approval; do not
   assume public-web egress reaches it or re-enable unrestricted chart `443` to bypass
   RFC1918 exclusions. Never disable TLS or identity checks to pass preflight.
3. **Create fresh references only, then explicitly install at zero replicas.** Generate
   URL-safe random role passwords operationally, never in Git, logs or shell transcripts.
   `omg-scim60-migrator-role` and `omg-scim60-runtime-role` are separate
   `kubernetes.io/basic-auth` Secrets, keys `username`/`password`, usernames
   `gateway_migrator` / `gateway_runtime`. The supported chart `roleCredEnv`/URL pattern
   expands username/password refs before the dependent `DATABASE_URL`; no plaintext URL
   or password is committed. OIDC uses native `secrets.oidc`, reference `omg-scim60-oidc`,
   default keys `GATEWAY_OIDC_ISSUER`, `GATEWAY_OIDC_CLIENT_ID`, `GATEWAY_OIDC_CLIENT_SECRET`
   (the secret is file-mounted). Issuer must be the exact ordinary HTTPS issuer under
   **`login.bitop.dev` for this dedicated app**, supplied only through that Secret, never
   the demo `18084`/localhost issuer. `omg-scim60-scim` holds `OMG_SCIM60_TOKEN`, injected
   only by `extraEnv.secretKeyRef`; `GATEWAY_SCIM_TOKEN_ENV` names it. It is neither a
   ConfigMap token nor a provider secret-allowlist entry. Follow [SCIM token requirements](scim.md#turn-it-on).
   After approved preflight, install the base profile with `migrations.run=false`,
   `replicaCount=0`, **`cnpg.pooler.enabled=false`**, namespace passed via Helm
   `--namespace dev-omg`. No Pooler is created by that Helm install. Complete the new
   Cluster/PVC/PV identity checks and await managed runtime-role LOGIN/password readiness
   **before creating the migration Job**. Then explicitly render only the initial-install
   Job with the exact chart path and arguments below for separate operator-reviewed
   application in this initial setup (no `--is-upgrade`):

   ```sh
   helm template omg-scim60 deploy/helm/open-model-gateway --namespace dev-omg \
     -f deploy/staging/scim-values.yaml --set migrations.run=true \
     --show-only templates/job-migrate.yaml
   ```

   An all-at-once install with migrations enabled would start that Job concurrently and
   cannot enforce these intervening gates. Await successful `wait-for-database`, `migrate`,
   `grants apply`; a CNPG/Job/grants error stops the sequence. Keep stored Helm values at
   migrations false, chart Pooler creation false and zero replicas throughout; track the
   separately applied Job for explicit teardown.
4. **Create the required pinned standalone Pooler.** After migration/grants success,
   recheck the selected namespace, fresh Pooler name and `omg-scim60-db` target, then
   explicitly apply only `deploy/staging/scim-pooler.yaml` under the operator-reviewed
   plan. Record its separate ownership/UID; Helm does not manage its lifecycle. Wait for
   CNPG Pooler readiness and confirm the actual `pgbouncer` image selects the published
   root-index digest above. Do not substitute the operator default, a mutable tag or a
   different database. A failure stops the sequence at zero gateway replicas. The Pooler
   **must be ready before the full peer privilege verifier**, so CNPG's Pooler `auth_query`
   reconciliation effects are included in that verification.
5. **Verify grants before bootstrap.** Run the matching image/source's official
   `deploy/staging/verify-privileges.sql` with fail-on-error (`psql -v ON_ERROR_STOP=1`)
   over the CNPG primary's local Unix socket as peer-authenticated database `postgres`,
   selecting **only `omg_scim60`**. Its rollback-only probes require `SET ROLE gateway_runtime`;
   the ordinary migrator owner is not assumed able to do that. Do not grant owner SET ROLE
   membership, omit failing probes or waive verification. Confirm schema family/checksums
   and runtime refusal cases against the matching pinned image. This is a future operator
   step, not an extra manifest or an execution claim in this preparation.
6. **Explicit bootstrap, then separate activation.** Only after migration/grants/verifier
   success manually run [`scim-bootstrap-job.yaml`](../deploy/staging/scim-bootstrap-job.yaml).
   Its pinned `wait-for-database --schema --timeout-seconds 600` init step waits/fails on an
   early apply without migrating; it does **not** replace grants verification. Its only
   write command is `provision-user --email omg-scim60-bootstrap@bitop.invalid --platform-admin`.
   This CLI creates **bootstrap**, not manual, grant provenance and one-use first-link
   permission; it never runs `bootstrap-demo`, starts serving or auto-activates. Both
   containers use only migrator role refs on `omg-scim60-db-rw`, UID/GID 10001, read-only
   root, dropped capabilities, no service-account token; backoff is zero and deadline 900s.
   Require Job success. Then, only once standalone Pooler pin/readiness, real OIDC and
   endpoint readiness gates pass, make the separate reviewed activation use **one replica,
   `migrations.run=false` and `cnpg.pooler.enabled=false`**; the standalone Pooler remains
   required and runtime connections remain pooled.
   Never mount the migrator credential into runtime or bootstrap through `serve`.
7. **Gate publication separately.** Keep the public route disabled until private-path
   application readiness, matching image/schema/runtime grants, the **full peer privilege
   verifier** (after Pooler reconciliation), and SCIM missing/invalid-token and unsupported
   operation refusals pass. Verify ownership, the sole direct target
   `omg-scim60.dev-omg.svc.cluster.local:8080`, and the intended TLS/Host configuration.
   Configuration is not certificate-issuance or public-connectivity proof. With SCIM
   synchronization still disabled, explicitly enable only the owned staging route, then
   verify its actual public certificate, canonical Host/origin, real issuer discovery/JWKS,
   callback and authentication/refusals. Disable that route on failure; never weaken TLS
   or identity checks. Enable synchronization only after these public checks pass.
   Approval is not execution evidence: no IdP, DNS or Kubernetes objects have been created
   by this preparation. Image provenance/execution qualification and all existing
   acceptance gates remain outstanding.

### Future Authentik setup and closeout gates

Prepare a **dedicated** ordinary OIDC application/provider and dedicated SCIM provider,
initially disabled/unassigned (no effective sync), with disposable users and an explicit
allowlist of test groups. Do not activate assignments or synchronization until the approved
endpoint/TLS path is ready. Register exactly:

- OIDC callback: `https://omg-scim60.bitop.dev/api/v1/auth/callback`
- SCIM base: `https://omg-scim60.bitop.dev/scim/v2`

Use authorization code + S256 PKCE, verified email, stable subject and the `groups` claim;
OIDC authentication alone grants no platform entitlement. Follow [Authentik mappings](scim.md#authentik):
`userName=request.user.email`; omit `emails` for reserved `.invalid`/`.test`/`.example`
test domains; raise context-provided `SkipObject` for every non-allowlisted group (the
provider's user filter alone does **not** filter groups). Never import the unsupported
`SkipObjectException`. Bootstrap's reserved-domain address needs that omission too.
Review dry-run output privately before enabling a push; never publish tokens or identity payloads.

Future acceptance must cover denial/no-grant sign-in, first linking, mapped versus independent
bootstrap/manual grant provenance, group changes, deactivation/reactivation and revoked keys
staying revoked, including whole-request last-Admin refusal and audit/alert behavior. Use only
this disposable installation and identities for last-Admin/deactivation tests, **never production
`omg-acceptance`**. No such acceptance has run here.

This is not the auto-TTL demo. Assign an owner and explicit teardown deadline. Disable/unassign
the dedicated SCIM provider and disable the dedicated public route first, stop the gateway,
revoke test sessions/keys and references, then remove only proven-owned staging
release/IdP/routing objects, separately owned standalone Pooler and bootstrap/migration
Jobs (tracked explicitly outside Helm), and deliberately selected
PVC/PVs under the approved retention policy; never delete `dev-omg` or unrelated resources.
Retain required sanitized audit/history outcomes in the issue. On failure keep zero replicas
and sync/publication disabled, preserve state for diagnosis, and use a separately reviewed
recovery or fresh replacement plan: no automatic reseed, volume deletion, privilege bypass, silent retry
of bootstrap or production cutover. Record outcomes only after execution; status remains
**NOT RUN / NOT ACCEPTED**.

## 1. Preflight / stop conditions

- Run the staging image, migration/grant, HTTPS and restore checks in [staging.md](staging.md).
- Record the exact application image digest, migration versions, configuration version and operator responsible for the test. Never record secret values.
- Keep the staging ingress behind your approved pilot-access restriction. Verify certificate trust, time synchronization and outbound DNS/HTTPS.
- Use a test provider account/project with vendor-side spend alerts/limits where available. Gateway costs are configured estimates and cannot guarantee the provider invoice.
- Stop if runtime needs schema-owner privileges, unexpected users/grants appear, TLS must be disabled, or a test requires weakening Origin/CSRF/verified-email checks.

## 2. OIDC registration and identity boundary

Provide issuer URL, client ID, staging origin, public/confidential client type, and the approved bootstrap administrator's verified email. Transfer any client secret through the deployment's protected secret file/secret store, not chat or Git.

- Register `https://YOUR-STAGING-HOST/api/v1/auth/callback` exactly.
- Verify discovery, authorization code + PKCE, stable subject, issuer/audience, nonce, and verified email. No API inference token may substitute for an interactive management session.
- Provision only the approved first operator using the migrator CLI; confirm the runtime CLI cannot grant platform administration.
- Enable the OIDC overlay, restart, and sign in/out through the real IdP. Check Secure/HttpOnly session cookies, logout invalidation, correct canonical origin and exact-Origin CSRF.
- Verify new ordinary identity behavior and explicit linking for a pre-provisioned email. An email-only match must not bypass linking approval.
- Create a separate test organization/team/project. Verify platform admin, org admin, shared-workspace admin and member access, including negative API requests—not just hidden navigation.
- Verify personal workspace metadata/keys/activity remain owner-private, even from another operator.
- Exercise the issuer's signing-key rotation: publish a new key, sign in with it without restarting the gateway (one rate-limited JWKS refetch), and confirm Admin › Settings › Sign-in shows the refreshed keys. If SCIM is enabled, also run [SCIM](scim.md) provisioning and deactivation against the real provider.

## 3. One model/provider first

Existing adapters: OpenAI, Anthropic, AWS Bedrock. Requested OpenAI-compatible Chat/Responses, vLLM and SGLang adapters are a separate implementation phase; do not point an existing fixed-endpoint adapter at an arbitrary server or claim compatibility without its capability tests.

- Inject one test provider credential/reference, or a restricted Bedrock workload role. Confirm secret values are absent from APIs, browser storage, application logs and image layers.
- As platform admin, create the provider connection and one deployment; keep them disabled while configuring.
- Enter an accurate immutable price version and the model's real hard input/output ceilings. Enter prices and budgets in **USD** in the dashboard; conversion to integer micro-USD is internal.
- Set a low platform organization ceiling, concurrency 1 and a small monthly budget that still covers the conservative reservation of the entire configured hard input ceiling. A very small budget may correctly reject even a tiny prompt before dispatch.
- Assign one model to the test organization and delegate it to one test workspace. Ensure the test user is an actual organization/workspace member.
- Issue one short-lived workspace key, save it only in a private local file, and explicitly enable the one test connection/deployment.

The helper below sends **one** synthetic non-streaming request with an output cap of eight tokens. It requires an explicit paid-request opt-in, verifies TLS, rejects redirects, and prints only HTTP/protocol/usage-presence metadata. It never prints the key, prompt, response text, or upstream error body.

```sh
python3 scripts/provider-acceptance.py \
  --origin https://YOUR-STAGING-HOST \
  --model YOUR-ASSIGNED-ALIAS \
  --key-file /PRIVATE/PATH/short-lived-workspace-key \
  --protocol chat \
  --allow-paid-request
```

Choose `responses` or `messages` only with an explicitly compatible deployment. For localhost rehearsal, `--ca-file .local/staging/local-ca.crt` supplies the local CA; TLS verification is never disabled. Without `--allow-paid-request`, the helper exits before reading credentials or making a request.

## 4. Controlled functional checks

Each real inference test can incur charges. Agree a total request/spend envelope first; do not run a load test as an acceptance smoke.

- Verify model listing, a successful JSON request, bounded output and actual reported token usage. Check known cost, holds, terminal execution state and append-only ledger through authorized views.
- Test Chat SSE and downstream cancellation with a real client. Responses and Messages stream text incrementally and should be verified with real clients.
- Verify wrong workspace/organization/model access is denied before upstream dispatch, including service-account versus human-key membership rules.
- Verify the platform ceiling aggregates personal/team/project traffic. A blank local limit must not remove the platform ceiling. Do not create fake usage records to simulate this.
- Rotate the test key; old credentials stop working and consumed allowance/restrictions remain. Revoke the organization model entitlement and confirm child access is removed.
- Test provider failures/unknown usage conservatively; do not infer zero cost. Do not enable ambiguous failover until duplicate-charge risk is accepted.
- For Bedrock, test actual region/model permission and workload-credential refresh; the unit mocks do not certify IAM.

## 5. Close out

Disable test deployments, revoke test keys, remove test credential mounts when finished, and preserve accounting/audit history. Record only sanitized request IDs, status, observed usage/cost states, image/schema versions and pass/fail outcomes. Do not erase unknown holds or mutate old price versions to make the report look clean.

Production approval still requires security/abuse and multi-replica load testing, monitoring/alerts, off-host encrypted backups and restore/cutover rehearsal. Provider-invoice reconciliation, internal chargeback and customer billing are distinct from this gateway's configured-rate estimates; their scope must be agreed separately.
