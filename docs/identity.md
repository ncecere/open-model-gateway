# Browser identity

Browser sessions and inference keys are separate credentials. A bearer key cannot authorize management. There is no default administrator or development-login endpoint.

## Configure generic OIDC

| Variable | Meaning |
| --- | --- |
| `GATEWAY_PUBLIC_URL` | Canonical external origin, such as `https://gateway.example.org`; no path/query/fragment or credentials. A trailing `/` is accepted. |
| `GATEWAY_OIDC_ISSUER` | Exact discovered issuer, including any issuer path. |
| `GATEWAY_OIDC_CLIENT_ID` | Registered authorization-code client. |
| `GATEWAY_OIDC_CLIENT_SECRET` | Optional confidential-client secret. |
| `GATEWAY_OIDC_GROUPS_CLAIM` | Signed ID-token claim name/path; default `groups`. Literal names take precedence over dotted nested paths. |

Register `${GATEWAY_PUBLIC_URL}/api/v1/auth/callback` with the origin's trailing slash removed. Admin › Settings › Sign-in shows the active issuer, client ID, client type, groups claim and this callback URL read-only (never the secret); see [settings](settings.md#sign-in). Use authorization code and S256 PKCE. ID tokens must contain `email`, boolean `email_verified:true`, and a valid string-array group claim, including `[]` when no groups match. The gateway requests the `openid email profile` scopes; an optional `name` claim is stored as the user's display name (trimmed, at most 200 characters, replaced at every sign-in and cleared at account cleanup). It is presentation only and never used for matching or authorization. The gateway does not fetch userinfo or retain upstream access/refresh tokens.

HTTPS is required for identity URLs. Only `GATEWAY_ENV=development` allows HTTP on localhost/literal loopback. Forwarded headers do not alter this policy. Cookies are Secure except on explicitly configured development-loopback HTTP. Missing all OIDC client settings disables login; partial/empty settings and configured discovery failures fail startup. Discovery/JWKS responses are bounded to 1 MiB, with redirects disabled, a 15-second timeout and five-second connection timeout. JWKS is a startup snapshot; restart after key rotation. This is not live JWKS refresh or provider certification.

## Authentication is not entitlement

Platform grants are `user`, `auditor`, and `admin`; Auditor/Admin include User access. A verified identity with no active platform grant cannot access the platform and receives no personal workspace. Workspace membership alone is not platform entitlement.

Platform Admins provision manual grants and generic issuer/group mappings through [management](management-api.md). Group targets are a platform role or a Team/Project `admin`/`member` membership; group ownership is not supported. Manual/bootstrap platform grants and manual workspace membership survive removal of an independent group grant. Multiple sources are not overwritten by a single effective role.

Synchronization runs at sign-in using the signature-verified claim. Valid empty groups remove no-longer-matching group grants. Missing/malformed groups deny sign-in **without treating the claim as an empty list or revoking existing grants**. Sign-in-only synchronization cannot discover group removal for someone who never signs in again. That accepted delay requires explicit manual suspension when immediate loss of access matters. There is no background SCIM/provisioning guarantee. Separately, changing/deleting a mapping definition through management immediately revokes that mapping's existing group grants, subject to last-admin safeguards; newly matching grants still require sign-in.

## Linking, suspension and cleanup

Identity binding uses `(issuer,subject)`, not a mutable email. An existing binding does not switch accounts when the provider email changes. A new binding can link to a preprovisioned active email only with one-use `oidc_link_allowed` and no existing binding. Conflicting links fail closed.

Trusted bootstrap/recovery can provision an explicit User or Admin grant:

```sh
cargo run -p open-model-gateway -- provision-user --email operator@example.org --platform-admin
```

This requires trusted database access and the enterprise lineage. It is not a public role-assignment endpoint and does not create a personal workspace before sign-in. Omit `--platform-admin` for a User grant. Existing identity bindings are not silently replaced.

When synchronization or explicit role removal detects loss of all entitlement, access is disabled and sessions/user-owned keys are revoked. Account/access attribution remains during a **30-day grace period**. Group re-entitlement within that period can reactivate entitlement-loss suspension, not administrative suspension. Revoked keys stay revoked; issue new credentials. Shared service-account keys are independent of the departed person.

The serving process runs bounded inactive-account cleanup every minute (up to 100 accounts per transaction). Cleanup revokes retained human grants, disables the old personal workspace, clears email/linking permission and keeps a user/identity tombstone. It does not delete immutable financial/audit records or shared service accounts. After cleaned entitlement-loss accounts regain a valid mapped platform role, authenticated callback can bind a new user UUID; old grants, personal workspace and keys are not resurrected. Administrative-suspension tombstones do not automatically rebind. Management has last-admin/last-shared-owner protections; external entitlement loss must not preserve unauthorized access merely to keep an owner active.

## HTTP sessions

- `GET /api/v1/auth/config` returns `{ "enabled": true|false }`.
- `GET /api/v1/auth/login` starts login; unconfigured login returns 503. An optional `return_to` is the dashboard path to land on after sign-in. It must be a same-origin relative path (no scheme, host, `//`, backslash or control characters, and never `/api`, `/v1` or `/health`). The server drops anything else, stores the accepted path with the login attempt (migration `0006_login_return_path`, checked again by a database constraint) and validates it again before the callback redirects there; otherwise the callback redirects to `/`.
- `GET /api/v1/auth/callback` redirects to `/`; no return-to parameter.
- `POST /api/v1/auth/logout` revokes the current session and clears cookies.

Login attempts are browser-bound, one-use and ten minutes long. Callback consumes the attempt before token exchange. Signature, issuer, audience, expiry, nonce, authorized-party policy and verified email are checked. An entitlement-loss callback commits revocation before denying access. After full verification, a denied browser (`Accept: text/html`) is redirected to `/?auth_error=access_denied` with session cookies cleared; non-browser callers receive the status code. Unverified or malformed callbacks never clear an existing session.

Session/CSRF secrets are independent random values; only hashes are stored. Sessions retain the normalized signature-verified email separately from the editable directory email. Invitation acceptance uses this verified session claim; sessions lacking proof cannot authenticate. Cleanup clears retained session email. Sessions expire absolutely after twelve hours. `omg_session` is HttpOnly/SameSite=Lax; readable `omg_csrf` is SameSite=Strict; `omg_oidc` is HttpOnly/SameSite=Lax. Cookies are host-only with Path `/`. Duplicate/malformed authentication cookies fail.

Every protected request reads current active entitlement. Every non-GET/HEAD request must also send:

```text
Origin: https://gateway.example.org
X-CSRF-Token: <current omg_csrf value>
```

Origin must exactly match configuration; repeated, missing or mismatched headers fail. Cookie-only, Referer and inference bearer authentication are not substitutes. Management handlers independently recheck live authority under locks.

Auth responses are no-store/no-referrer. Edge logs must not retain callback queries, cookies or auth headers. Rate-limit login and manage expired attempts/sessions operationally. Upstream single logout, cross-device logout and real-provider certification remain separate work. Tests in `identity/tests.rs`, `lifecycle/tests.rs` and `auth/enterprise_tests.rs` are source coverage, not a fresh execution claim; historical browser checks are in [verification](verification.md).
