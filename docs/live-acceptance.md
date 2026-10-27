# Live identity and provider acceptance

Use a dedicated staging hostname, database, identities and provider project. This checklist is **not yet live-provider/enterprise-IdP verification**. The automated suite uses fixtures; the staging smoke leaves OIDC disabled and sends no paid traffic.

## Isolated SCIM #60 preparation — NOT RUN / NOT ACCEPTED

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
  One transaction-mode Pooler remains enabled: chart runtime URLs always target it;
  LISTEN and migrator URLs target `omg-scim60-db-rw` directly.
- **Unresolved Pooler image gate:** the existing chart has no Pooler image override.
  Before the initial install (which starts the Pooler even at zero gateway replicas),
  the operator must verify the operator-selected Pooler image is published and selected
  by digest, and verify the actual selection before gateway startup/admission.
  A tag-only/unpublished selection is a blocker;
  do not add an ignored values key, fabricate a digest, disable the Pooler or silently
  change cluster-wide operator configuration.

Local-only checks (no cluster or new dependencies):

```sh
helm template omg-scim60 deploy/helm/open-model-gateway --namespace dev-omg \
  -f deploy/staging/scim-values.yaml
helm template omg-scim60 deploy/helm/open-model-gateway --namespace dev-omg \
  -f deploy/staging/scim-values.yaml --set migrations.run=true
helm lint deploy/helm/open-model-gateway -f deploy/staging/scim-values.yaml
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
in principle, not yet enacted. Routing/TLS/DNS changes outside `dev-omg` require separate
approval. The proposed origin **`https://omg-scim60.bitop.dev` is not DNS-registered or
route-approved**. Do not substitute production routing or alter another namespace/repository.

1. **Prove freshness and selection.** Check the explicitly selected record, Helm release,
   Cluster identity/UID, PVC ownership and bound PV claim references all belong to
   `dev-omg` / `omg-scim60-db`. This boundary is **not `dev-env-pg`**. Before creation,
   any existing release, Cluster, matching old PVC/PV, retained volume or unexpected
   resource is a stop condition, not permission to adopt/reset it. After creation, match
   the newly created Cluster UID through PVC owner references and PV bindings to the
   selected record before migrations. A failed lookup/permission error is not proof of
   absence. Never reuse, reset, migrate or delete an old installation implicitly.
2. **Verify reachability, without inventing migration policy.** The reviewed static
   homelab `kubernetes/platform/dev-areas/base/network-policies.yaml` has default-deny
   plus `allow-cluster-dns` and `allow-same-namespace` for **all pods**. These allow
   bootstrap/migrator DNS and direct same-namespace PostgreSQL. This is namespace-level
   development trust, **not per-Job isolation**: NetworkPolicies are additive, so adding
   a narrow policy would not tighten that existing allowance. The chart's three policies
   select only `component: gateway`; no migration-specific policy exists in this base.
   Bootstrap's release/app labels and `component: migrate` identify ownership/convention,
   **not a selector granting connectivity**. No extra egress is added here. The operator
   must verify the **deployed** namespace policy before apply; this preparation only read
   the static file and performed no cluster inspection. Real issuer DNS/discovery/JWKS
   and HTTPS reachability also need preflight: same-namespace allowance does not allow
   a cross-namespace IdP. If needed, separately review a namespace-local destination-scoped
   exception and gateway private-egress approval, not broad egress or an out-of-namespace
   routing change. Never disable TLS or identity checks to pass preflight.
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
   After approved preflight, install the stopped profile with `migrations.run=false`,
   `replicaCount=0`, namespace passed via Helm `--namespace dev-omg`. Complete the new
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
   cannot enforce these
   intervening gates. Await successful `wait-for-database`, `migrate`, `grants apply`;
   a CNPG/Job/grants error stops the sequence. Keep stored Helm values at migrations false
   and zero replicas throughout; track the separately applied Job for explicit teardown.
4. **Verify grants before bootstrap.** Run the matching image/source's official
   `deploy/staging/verify-privileges.sql` with fail-on-error (`psql -v ON_ERROR_STOP=1`)
   over the CNPG primary's local Unix socket as peer-authenticated database `postgres`,
   selecting **only `omg_scim60`**. Its rollback-only probes require `SET ROLE gateway_runtime`;
   the ordinary migrator owner is not assumed able to do that. Do not grant owner SET ROLE
   membership, omit failing probes or waive verification. Confirm schema family/checksums
   and runtime refusal cases against the matching pinned image. This is a future operator
   step, not an extra manifest or an execution claim in this preparation.
5. **Explicit bootstrap, then separate activation.** Only after migration/grants/verifier
   success manually run [`scim-bootstrap-job.yaml`](../deploy/staging/scim-bootstrap-job.yaml).
   Its pinned `wait-for-database --schema --timeout-seconds 600` init step waits/fails on an
   early apply without migrating; it does **not** replace grants verification. Its only
   write command is `provision-user --email omg-scim60-bootstrap@bitop.invalid --platform-admin`.
   This CLI creates **bootstrap**, not manual, grant provenance and one-use first-link
   permission; it never runs `bootstrap-demo`, starts serving or auto-activates. Both
   containers use only migrator role refs on `omg-scim60-db-rw`, UID/GID 10001, read-only
   root, dropped capabilities, no service-account token; backoff is zero and deadline 900s.
   Require Job success. Then, only once Pooler digest, real OIDC and endpoint readiness gates
   pass, make the separate reviewed activation use **one replica and `migrations.run=false`**.
   Never mount the migrator credential into runtime or bootstrap through `serve`.

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
the dedicated SCIM provider first, stop the gateway, revoke test sessions/keys and references,
then remove only proven-owned staging release/Job/IdP objects and deliberately selected
PVC/PVs under the approved retention policy; never delete `dev-omg` or unrelated resources.
Retain required sanitized audit/history outcomes in the issue. On failure keep zero replicas
and sync disabled, preserve state for diagnosis, and use a separately reviewed recovery or
fresh replacement plan: no automatic reseed, volume deletion, privilege bypass, silent retry
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
