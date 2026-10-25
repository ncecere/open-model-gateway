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
| `GATEWAY_TRUSTED_PROXIES` | Optional. Comma-separated addresses or CIDR networks of your reverse proxies, such as `10.42.0.0/16`. Only a request from one of them has its `X-Forwarded-For` believed, and then only the right-most entry that isn't itself a trusted proxy. Used for sign-in rate limits only. |
| `GATEWAY_LOGIN_RATE_PER_MINUTE` | Optional. Sign-in starts, and separately callbacks, allowed per client address per minute; default 60, allowed 1–10000. |
| `GATEWAY_SCIM_TOKEN_ENV` | Optional. Name of the variable holding the SCIM bearer token; enables [SCIM provisioning](scim.md). Requires OIDC. |

Register `${GATEWAY_PUBLIC_URL}/api/v1/auth/callback` with the origin's trailing slash removed. Admin › Settings › Sign-in shows the active issuer, client ID, client type, groups claim and this callback URL read-only (never the secret); see [settings](settings.md#sign-in). Use authorization code and S256 PKCE. ID tokens must contain `email`, boolean `email_verified:true`, and a valid string-array group claim, including `[]` when no groups match. The gateway requests the `openid email profile` scopes; an optional `name` claim is stored as the user's display name (trimmed, at most 200 characters, replaced at every sign-in and cleared at account cleanup). It is presentation only and never used for matching or authorization. The gateway does not fetch userinfo or retain upstream access/refresh tokens.

HTTPS is required for identity URLs. Only `GATEWAY_ENV=development` allows HTTP on localhost/literal loopback. Forwarded headers do not alter this policy. Cookies are Secure except on explicitly configured development-loopback HTTP. Missing all OIDC client settings disables login; partial/empty settings and configured discovery failures fail startup. Discovery/JWKS responses are bounded to 1 MiB, with redirects disabled, a 15-second timeout and five-second connection timeout. This is not provider certification.

### Sign-in abuse controls

- **Per address.** Each replica allows `GATEWAY_LOGIN_RATE_PER_MINUTE` sign-in starts and, separately, as many callbacks per client address per minute, as a burst of that size refilling evenly. IPv6 clients are counted per /64. Past it, the step gets 429 with `Retry-After`, stores nothing and sets no cookie. A throttled callback leaves its sign-in attempt usable. The limits are in memory, so with N replicas a client can make up to N times as many requests; the cap below bounds the total.
- **Client address.** The connection's address, unless it is in `GATEWAY_TRUSTED_PROXIES`. Then it's the right-most `X-Forwarded-For` entry that isn't itself a trusted proxy; a port on an entry is ignored, and an unparsable entry ends the walk (the request counts as the proxy). So list every layer-7 hop in front of the gateway: CDN, cloud load balancer, then the ingress controller (Helm: `gateway.trustedProxies`, for example the cluster pod CIDR). Without it, every user counts as the nearest proxy and shares one budget. A layer-4 load balancer hides client addresses unless it preserves them (`externalTrafficPolicy: Local`, or PROXY protocol to the ingress). An untrusted client's own `X-Forwarded-For` is ignored; a trusted network's members (for example any pod, when you trust the pod CIDR) can choose their bucket, which only affects these limits.
- **Installation-wide.** About 10,000 unexpired sign-in attempts at most (inserts racing at the boundary can add one per database connection). Beyond that, sign-in returns 503 with `Retry-After: 60` until attempts expire (10 minutes) or complete; a replica that hits the cap refuses for 5 seconds without querying again. Existing sessions keep working.
- **No account lockout.** The gateway never sees passwords; your IdP handles credential attacks. A per-account lockout would let anyone lock a known user out.
- **Metrics.** `gateway_auth_throttled_total{endpoint="login|callback",reason="client_rate|pending_cap"}`, with no addresses or identities. Alert on a sustained `pending_cap` rate (sign-in is refused for everyone), for example `sum(rate(gateway_auth_throttled_total{reason="pending_cap"}[5m])) > 0` for 5 minutes.

### Signing keys (JWKS)

The issuer's JWKS is fetched at startup (failure stops startup) and cached:

- **TTL:** `Cache-Control: max-age`, clamped to 5 minutes–24 hours; one hour when the issuer sends none. Expired keys are refreshed at the next sign-in.
- **Rotation:** a token naming an unknown `kid` triggers one refetch and one retry. Refetches for unknown keys happen at most once a minute per installation, and concurrent callers share one fetch, so random key IDs cannot cause a stampede.
- **Outage:** a failed or empty fetch keeps the last good keys for up to 6 hours past their expiry, retrying at most every 30 seconds. After that, sign-in fails closed with 503 until a fetch succeeds.
- **Algorithms:** only asymmetric algorithms that the issuer advertises (RS/PS 256–512, ES256/ES384, EdDSA). `none` and HMAC (`HS*`) are never accepted, even if advertised, and the client secret is never used as an ID-token key. Startup fails if the issuer advertises no supported algorithm.

Admin › Settings › Sign-in shows the number of cached keys, the last refresh and whether keys are current, a cached copy (refresh failing) or unavailable.

## Authentication is not entitlement

Platform grants are `user`, `auditor`, and `admin`; Auditor/Admin include User access. A verified identity with no active platform grant cannot access the platform and receives no personal workspace. Workspace membership alone is not platform entitlement.

Platform Admins provision manual grants and generic issuer/group mappings through [management](management-api.md). Group targets are a platform role or a Team/Project `admin`/`member` membership; group ownership is not supported. Manual/bootstrap platform grants and manual workspace membership survive removal of an independent group grant. Multiple sources are not overwritten by a single effective role.

Synchronization runs at sign-in using the signature-verified claim. Valid empty groups remove no-longer-matching group grants. Missing/malformed groups deny sign-in **without treating the claim as an empty list or revoking existing grants**. Without SCIM, sign-in-only synchronization cannot discover group removal for someone who never signs in again; suspend manually when immediate loss of access matters. With [SCIM provisioning](scim.md), the identity provider pushes deactivation and group membership as they change. Membership of a group that SCIM has pushed then comes only from SCIM, not the token; other groups still sync at sign-in. Separately, changing/deleting a mapping definition through management immediately revokes that mapping's existing group grants, subject to last-admin safeguards; newly matching grants still require sign-in.

## Self-service membership requests

An active, entitled user can request joining an explicitly discoverable Team/Project as `member` through the [membership request API](management-api.md#self-service-membership-requests-83-slice-a). Discoverability defaults off and is controlled only by Platform Admins. Requests never grant platform entitlement or expose personal/hidden workspaces. Model requests, notifications and UI follow in later slices.

An independent live workspace owner/admin, or a Platform Admin on the platform route, can approve. No one can approve their own request, including a Platform Admin. Approval atomically creates a linked **manual** member grant; group provenance and stronger existing roles are never overwritten. Current requester entitlement, workspace state/discoverability and approver authority are checked under canonical locks. Any grant or revocation after submission supersedes a pending request, including a grant later removed; withdraw and resubmit rather than reviving it. Returning entitlement never replays an approved request or restores a revoked key/grant. The approval queue shows the requester's display name/email, matching what that owner/admin, or that Platform Admin/Auditor, already sees for any entitled user through the Members picker or the platform user directory; the requester's own history never gets another user's identity. Both fields redact to null the moment the requester stops being eligible for that picker (disabled, cleaned or no current platform entitlement), even for a request submitted earlier while they were still eligible.

The requester's own history ("My requests") shows the target's name, never a raw workspace ID, but as `workspace_name_at_request`: the name captured once, server-side, at submission. It never updates afterward, so a later rename or the workspace becoming non-discoverable is never disclosed through a past request.

Requests expire after seven days using database time (synchronous checks and bounded lifecycle maintenance). Reasons are retained plain text, included in the requester's subject export and platform-visible subject export, never audit/log content. Account cleanup clears them without giving runtime permission to edit request reasons. A user legal hold prevents cleanup; a workspace hold retains that workspace's reasons. Once holds end, bounded lifecycle maintenance clears up to 100 retained reasons per pass for already-cleaned users, without reopening or changing request outcomes. Pseudonymous request, decision and immutable application IDs remain as history.

## Linking, suspension and cleanup

Identity binding uses `(issuer,subject)`, not a mutable email. An existing binding does not switch accounts when the provider email changes. A new binding can link to a preprovisioned active email only with one-use `oidc_link_allowed` and no existing binding. Conflicting links fail closed.

Trusted bootstrap/recovery can provision an explicit User or Admin grant:

```sh
cargo run -p open-model-gateway -- provision-user --email operator@example.org --platform-admin
```

This requires trusted database access and the enterprise lineage. It is not a public role-assignment endpoint and does not create a personal workspace before sign-in. Omit `--platform-admin` for a User grant. Existing identity bindings are not silently replaced.

When synchronization or explicit role removal detects loss of all entitlement, access is disabled and sessions/user-owned keys are revoked. Account/access attribution remains during a **30-day grace period**. Group re-entitlement within that period can reactivate entitlement-loss suspension, not administrative or SCIM suspension (`scim_deactivated`, lifted only by SCIM `active: true`; see [SCIM](scim.md#users)). Revoked keys stay revoked; issue new credentials. Shared service-account keys are independent of the departed person.

The serving process runs bounded inactive-account cleanup every minute (up to 100 accounts per transaction). Cleanup revokes retained human grants, disables the old personal workspace, clears email/linking permission and keeps a user/identity tombstone. It does not delete immutable financial/audit records or shared service accounts. After cleaned entitlement-loss accounts regain a valid mapped platform role, authenticated callback can bind a new user UUID; old grants, personal workspace and keys are not resurrected. Administrative-suspension tombstones do not automatically rebind. Management has last-admin/last-shared-owner protections; external entitlement loss must not preserve unauthorized access merely to keep an owner active.

### What cleanup removes and keeps (#76)

Cleanup runs 30 days after access ends, or at once when a Platform Admin chooses **Remove personal data now…** on a suspended person's page (`POST /api/v1/platform/users/{id}/remove-personal-data`; `409` while the person is active, already cleaned or on [legal hold](platform-administration.md#legal-holds); audited as `user.personal_data_removed`, and `user.cleaned` by the cleanup).

- **Removed:** email and display name; sessions' verified email (sessions revoked); SCIM user name, external id, given and family names, and group memberships; the person's address on old invitations (replaced with `removed@removed.invalid`) and in alert rule recipients; the names of files they uploaded; access-request reasons (except in a workspace on legal hold). Keys, platform role and workspace grants, sessions and pending invitations are revoked; the personal workspace is disabled.
- **Matched:** copies under the person's current address only; an address changed earlier (by an admin or SCIM) isn't found. Inside a team or project on legal hold, invitations, alert recipients and filenames stay.
- **Kept:** the names they gave their API keys (revoke or ask them to rename first if that matters); the user id, as the pseudonymous reference that ledger, price, execution and audit history point to (none of that history is rewritten); the IdP binding (`issuer`, `subject`) as a tombstone, so the account can't silently relink; request metadata, which follows [request log retention](settings.md) (#77); file contents until the files expire.

## HTTP sessions

- `GET /api/v1/auth/config` returns `{ "enabled": true|false, "logo": {url, updated_at}|null, "installation_name": string|null }`: the uploaded installation logo for the sign-in page and its alt text (the name only when a logo is set). See [management API](management-api.md#public-installation-logo).
- `GET /api/v1/auth/login` starts login; unconfigured login returns 503. An optional `return_to` is the dashboard path to land on after sign-in. It must be a same-origin relative path (no scheme, host, `//`, backslash or control characters, and never `/api`, `/v1` or `/health`). The server drops anything else, stores the accepted path with the login attempt (migration `0006_login_return_path`, checked again by a database constraint) and validates it again before the callback redirects there; otherwise the callback redirects to `/`.
- `GET /api/v1/auth/callback` redirects to `/`; no return-to parameter.
- `POST /api/v1/auth/logout` revokes the current session and clears cookies.

### Session lifetime and idle timeout

| Variable | Default | Allowed |
| --- | --- | --- |
| `GATEWAY_SESSION_LIFETIME` | `12h` | `15m` to `7d` (`30m`, `12h`, `7d`) |
| `GATEWAY_SESSION_IDLE_TIMEOUT` | `2h` | `off`, or `15m` up to the lifetime |

A session ends at whichever comes first: its lifetime after sign-in, or the idle timeout after its last recorded activity. Activity is recorded at most every 5 minutes, so a session can end up to 5 minutes earlier than the idle timeout after its last request. Both deadlines are stored on the session when it is created or active and compared with the database clock, so replicas agree and a changed setting never brings back a session that already ended: a longer lifetime applies to new sign-ins, a different idle timeout from a live session's next activity. The session cookie lasts as long as the lifetime. A shorter setting doesn't shorten sessions that already exist; to end them now, sign people out (Admin › Users › Sign out all sessions). Without SCIM, a removed IdP group takes effect at the next sign-in, so a longer lifetime with the idle timeout `off` keeps that access longer. Helm: `gateway.sessionLifetime`, `gateway.sessionIdleTimeout`.

Maintenance deletes expired sign-in attempts every minute and sessions that ended (signed out, expired or idle) more than 30 days ago every hour, in bounded batches. "First signed in" and "Last sign-in" are kept on the user, so they don't change when old sessions are deleted. They are for display only, never used for access decisions. The hourly cleanup scans the sessions table; a large table from before this release is cleared at 10,000 rows an hour.

### Your data (#74)

Your profile › Your data downloads what the gateway holds about you as one JSON file (`GET /api/v1/me/export?from=YYYY-MM-DD&to=YYYY-MM-DD`, format `omg.subject-export.v1`): your profile and platform roles, memberships (including your personal workspace), API key metadata, live sessions, the request records of your keys and your own audit entries for the chosen dates (up to 93 days; default the last 30), and the list of files you uploaded. Key secrets, prompts and responses are never in it, and file contents aren't (download them through the Files API while they exist). Request records include their estimated cost and the client app and session id your tools sent. It is your own data wherever it was created: requests and audit entries from workspaces you have since left are included, under their workspace ids. Each list is capped (50,000 requests, 10,000 of anything else) and marked `truncated` when the cap is hit; choose a shorter range. The `X-OMG-Export-SHA256` response header is the file's checksum. Exports are generated on demand, never stored, audited (`subject.exported`), limited to 3 an hour (counted separately for the admin export), and at most two build at once per replica (otherwise `503`, try again).

A Platform Admin or Auditor can download a person's record from their page (**Export user record**, `GET /api/v1/platform/users/{id}/export`, audited as `user.exported`): only what admins already see, meaning profile, platform roles, team and project memberships, and platform-visible audit entries by or about the person (the newest 10,000). Never their personal workspace, keys, sessions or requests.

### Your sessions

Your profile › Sessions lists where you're signed in to the dashboard: a browser and OS family ("Firefox on macOS", from a fixed list; one of them alone when only one is recognised, "Unknown browser" when neither is), when it signed in and when it was last active. Last activity is recorded at most every 5 minutes. No IP address or raw user agent is stored. You can sign one session out, or every other session at once; a signed-out session's cookie stops working on its next request, on every replica. The session you're using is marked and is signed out with the regular Sign out. Signing sessions out never touches API keys.

**Admin API tokens are independent of sessions (#37).** Signing out sessions, including an Admin signing out all of a person's sessions, does not revoke their admin tokens. Any current Admin can revoke a token through the browser-session-only management API; a dashboard UI is not yet available. Losing the owner's final effective Admin grant, suspension, SCIM deactivation or cleanup permanently revokes owned tokens in the same transaction. Regaining Admin never restores them. Tokens also stop authorizing requests at their expiry.

Because they are high-risk, long-lived management credentials, the last address a token was used from is recorded (throttled to once per 5 minutes) to help investigate misuse. Revocation stops further activity updates but retains the last recorded address; account cleanup clears it immediately, the same way cleanup clears every other address/identity copy on the account.

A Platform Admin sees how many sessions a person has (user detail, never which devices) and can sign them all out without suspending them (audited `user.sessions_revoked` with the count). Users signing out their own sessions is audited as `session.revoked` (one) or `session.others_revoked` (all others), with the count. See [management API](management-api.md#your-sessions).

Login attempts are browser-bound, one-use and ten minutes long. Callback consumes the attempt before token exchange. Signature, issuer, audience, expiry, nonce, authorized-party policy and verified email are checked. An entitlement-loss callback commits revocation before denying access. After full verification, a denied browser (`Accept: text/html`) is redirected to `/?auth_error=access_denied` with session cookies cleared; non-browser callers receive the status code. Unverified or malformed callbacks never clear an existing session.

Session/CSRF secrets are independent random values; only hashes are stored. Sessions retain the normalized signature-verified email separately from the editable directory email. Invitation acceptance uses this verified session claim; sessions lacking proof cannot authenticate. Cleanup clears retained session email. Sessions end at their lifetime or idle timeout (see [Session lifetime and idle timeout](#session-lifetime-and-idle-timeout)). `omg_session` is HttpOnly/SameSite=Lax; readable `omg_csrf` is SameSite=Strict; `omg_oidc` is HttpOnly/SameSite=Lax. Cookies are host-only with Path `/`. Duplicate/malformed authentication cookies fail.

Every protected request reads current active entitlement. Every non-GET/HEAD request must also send:

```text
Origin: https://gateway.example.org
X-CSRF-Token: <current omg_csrf value>
```

Origin must exactly match configuration; repeated, missing or mismatched headers fail. Cookie-only, Referer and inference bearer authentication are not substitutes. Management handlers independently recheck live authority under locks.

Auth responses are no-store/no-referrer. Edge logs must not retain callback queries, cookies or auth headers. Rate-limit login and manage expired attempts/sessions operationally. Upstream single logout, cross-device logout and real-provider certification remain separate work. Tests in `identity/tests.rs` (including a local mock issuer rotating keys), `identity/jwks.rs`, `scim/tests.rs`, `lifecycle/tests.rs` and `auth/enterprise_tests.rs` are source coverage, not a fresh execution claim; historical browser checks are in [verification](verification.md).
