# Live identity and provider acceptance

Use a dedicated staging hostname, database, identities and provider project. This checklist is **not yet live-provider/enterprise-IdP verification**. The automated suite uses fixtures; the staging smoke leaves OIDC disabled and sends no paid traffic.

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
- Exercise the issuer's signing-key rotation process in coordination with a gateway restart; automatic JWKS refresh is pending. Record that limitation in the pilot runbook.

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
- Test Chat SSE and downstream cancellation with a real client. Responses/Messages currently buffer bounded content; token-by-token native streaming must be separately verified when implemented.
- Verify wrong workspace/organization/model access is denied before upstream dispatch, including service-account versus human-key membership rules.
- Verify the platform ceiling aggregates personal/team/project traffic. A blank local limit must not remove the platform ceiling. Do not create fake usage records to simulate this.
- Rotate the test key; old credentials stop working and consumed allowance/restrictions remain. Revoke the organization model entitlement and confirm child access is removed.
- Test provider failures/unknown usage conservatively; do not infer zero cost. Do not enable ambiguous failover until duplicate-charge risk is accepted.
- For Bedrock, test actual region/model permission and workload-credential refresh; the unit mocks do not certify IAM.

## 5. Close out

Disable test deployments, revoke test keys, remove test credential mounts when finished, and preserve accounting/audit history. Record only sanitized request IDs, status, observed usage/cost states, image/schema versions and pass/fail outcomes. Do not erase unknown holds or mutate old price versions to make the report look clean.

Production approval still requires security/abuse and multi-replica load testing, monitoring/alerts, off-host encrypted backups and restore/cutover rehearsal. Provider-invoice reconciliation, internal chargeback and customer billing are distinct from this gateway's configured-rate estimates; their scope must be agreed separately.
