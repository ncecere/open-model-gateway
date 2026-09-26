# Browser identity

Browser management sessions and inference bearer API keys are separate credentials. A bearer key never authenticates a management request. There is no development-login endpoint or default administrator.

## Configuration

Set all three required variables to enable login:

| Variable | Meaning |
| --- | --- |
| `GATEWAY_PUBLIC_URL` | Canonical external origin, e.g. `https://gateway.example.org`. An optional trailing `/` is accepted; paths, queries, fragments, user information, and noncanonical origins are rejected. |
| `GATEWAY_OIDC_ISSUER` | Exact provider issuer, including its path if applicable. Discovery must return the same issuer. |
| `GATEWAY_OIDC_CLIENT_ID` | Registered OIDC client ID. |
| `GATEWAY_OIDC_CLIENT_SECRET` | Optional secret for confidential clients. Omit for public clients. |

Register exactly `${GATEWAY_PUBLIC_URL}/api/v1/auth/callback` as the provider callback, removing any trailing slash from the public origin first. Enable authorization-code flow and S256 PKCE. The provider must include `email` and boolean `email_verified: true` in its signed ID token; the gateway does not fetch userinfo or request offline access.

HTTPS is required for the public URL, issuer, authorization/token endpoints, and every discovery/JWKS request. Only `GATEWAY_ENV=development` permits HTTP, and then only for `localhost` or literal loopback IPs. Cookies omit `Secure` only when the public origin itself is development-loopback HTTP. Forwarded host/protocol headers never override this policy.

Absent OIDC issuer, client ID and secret disables login. A public URL alone does not enable it. Partial/empty OIDC configuration fails startup. Configured discovery failure also fails startup; there is no permissive fallback. Discovery and JWKS are loaded with `openidconnect` 4. HTTP redirects are disabled; requests have a 15-second timeout, a 5-second connect timeout and a 1 MiB response limit. TLS certificate validation remains enabled.

The provider configuration is trusted operator input. Restrict outbound access independently if administrators must not be able to select arbitrary HTTPS endpoints. JWKS is a startup snapshot: restart the gateway to refresh keys after rotation. Unknown/new keys fail closed until refreshed.

## Endpoints

- `GET /api/v1/auth/config`: returns `{ "enabled": true|false }`.
- `GET /api/v1/auth/login`: starts an authorization-code login with S256 PKCE; returns 503 when unconfigured.
- `GET /api/v1/auth/callback`: verifies the login and redirects only to `/`. No return-to parameter is supported.
- `POST /api/v1/auth/logout`: requires a session, exact Origin and CSRF header; revokes that session and clears cookies.

Auth responses use `Cache-Control: no-store` and `Referrer-Policy: no-referrer`. Error messages do not contain provider responses, authorization codes, tokens, secrets or account details. Reverse proxies and access loggers must **not log callback query strings or authentication headers/cookies**. Avoid logging full request URLs on these endpoints.

## Login and account linking

Each login stores hashes of a random state and a separate browser-binding secret, together with a nonce and PKCE verifier. The `omg_oidc` binding cookie and database attempt expire after ten minutes. Callback atomically deletes and commits the matching unexpired attempt **before** contacting the token endpoint. A wrong browser cannot use the attempt. A replay, including one following a failed token exchange, fails. Starting another login replaces the binding cookie, invalidating use of earlier attempts from that browser.

The maintained OIDC library verifies the ID-token signature, issuer, audience, expiration and nonce. The gateway additionally checks an optional authorized-party claim against the client ID and requires a verified email. Upstream access and refresh tokens exist only transiently during exchange and are never retained or sent to the browser.

Users are resolved by `(issuer, subject)`, not by email after a link exists:

1. An existing identity authenticates its active user. Provider email changes do not change the local email or switch users.
2. A new identity with an existing case-insensitive email requires an active user with `oidc_link_allowed=true` and **no existing identity**. Otherwise linking returns 409 (disabled users return 401). Successful linking consumes the flag in the same transaction.
3. A genuinely new email creates a lowercase-email user with no memberships or platform-admin privilege.

Account linking never grants roles. An operator must explicitly authorize linking for preprovisioned users through `cargo run -p open-model-gateway -- provision-user --email user@example.org` using trusted database access. Add `--platform-admin` only to deliberately grant platform administration. The command preserves existing privileges and does not rebind an existing OIDC identity. Preprovisioned administrator privileges are preserved, not inferred from email/provider claims. Unique constraints, transaction-level locks and row locks prevent simultaneous first-login/link races.

## Sessions and CSRF

A successful login creates independent 32-byte random session and CSRF secrets, encoded as lowercase hexadecimal. Only SHA-256 hashes are stored. Sessions have an absolute twelve-hour expiry without sliding extension.

| Cookie | Attributes |
| --- | --- |
| `omg_session` | `HttpOnly; Path=/; SameSite=Lax`, twelve hours |
| `omg_csrf` | JavaScript-readable; `Path=/; SameSite=Strict`, twelve hours |
| `omg_oidc` | `HttpOnly; Path=/; SameSite=Lax`, ten minutes |

All are host-only (no Domain attribute), and use `Secure` except for explicit development-loopback HTTP. Logout expires all three cookies. Duplicate/malformed authentication cookies are rejected rather than selecting the first or last value.

For every protected request, the middleware checks the session record, its revocation and expiry, and the user's current disabled status. It obtains the current email and platform-admin flag from the database; no role-bearing session cache exists. A missing/invalid session returns 401, even when OIDC is unconfigured. Tests can seed session records directly; production has no endpoint for doing so.

For every method other than GET/HEAD, including OPTIONS, send both:

```text
Origin: https://gateway.example.org
X-CSRF-Token: <current omg_csrf cookie value>
```

Origin must exactly match the configured canonical public origin. Missing, null, repeated or mismatched Origin/CSRF headers fail with 403. The hash of the supplied token is compared to the session's stored CSRF hash in constant time. Host/Referer and the CSRF cookie alone are not accepted as substitutes. With no OIDC configuration, mutating requests fail closed because no trusted public origin exists.

## Integration API

```rust
IdentityConfig::from_env() -> anyhow::Result<Option<IdentityConfig>>
IdentityState::new(store: Store, config: Option<IdentityConfig>) // async Result<Self>
identity::router(state: IdentityState) -> Router<Store>
identity::require_session(State<IdentityState>, Request, Next) // async Response
```

`IdentityState` is cloneable. Its internal configuration/provider/store fields are deliberately private. Protect management routes with `middleware::from_fn_with_state(identity.clone(), identity::require_session)`. The middleware inserts `Extension<BrowserPrincipal>` with public `user_id: Uuid`, `email: String`, and `platform_admin: bool`. Authorization and tenant scoping remain the management handlers' responsibility. Do not apply this middleware to inference routes or public login/callback routes.

Required dependency: `openidconnect = { version = "4", default-features = false, features = ["reqwest", "rustls-tls"] }`; other dependencies are already used by the gateway.

## Testing and operations

From the repository root:

```sh
cargo test -p open-model-gateway --lib identity::tests
DATABASE_URL=postgres://gateway:gateway@127.0.0.1:54329/gateway \
  cargo test -p open-model-gateway --features integration-tests --lib identity::tests
```

The integration suite uses `sqlx::test` and isolated real PostgreSQL databases. Crate API usage was checked against `ramosbugs/openidconnect-rs` tag `4.0.1` (`b639b5d39eac6903238867aeb2b29326502e6b26`), particularly `src/lib.rs`, `src/client.rs` and `src/verification/mod.rs`. The database role must be able to create databases. OIDC tests use only a local mock issuer and a public test-only signing key; no real provider or paid API is called.

Coverage includes discovery/URL policy, issuer mismatch, duplicate cookies, CSRF/origin enforcement, bearer rejection, expired/revoked/disabled sessions, linking approval/consumption, absence of default roles, one-use browser-bound attempts, complete signed OIDC/PKCE login, invalid signatures/claims and failed-exchange replay rejection. Identity tests run through the normal Cargo commands above, including isolated PostgreSQL and signed mock-issuer tests.

Operational follow-up: rate-limit login at the edge, periodically delete expired login attempts and expired/revoked sessions, and monitor generic failure counts without recording identity secrets. This module does not supply a cleanup scheduler, live JWKS refresh, upstream single logout, cross-device logout, provider-specific claim mapping, or real-provider interoperability certification. A loopback-issuer browser smoke is recorded in [verification](verification.md).
