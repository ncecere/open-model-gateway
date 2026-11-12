//! Browser identity is deliberately separate from inference API-key authentication.
use std::{env, net::IpAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Query, Request, State, rejection::QueryRejection},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use openidconnect::{
    AdditionalClaims, AuthorizationCode, ClaimsVerificationError, Client, ClientId, ClientSecret,
    CsrfToken, EmptyExtraTokenFields, EndpointMaybeSet, EndpointNotSet, EndpointSet, HttpRequest,
    HttpResponse, IdToken, IdTokenClaims, IdTokenFields, IdTokenVerifier, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, SignatureVerificationError,
    StandardErrorResponse, StandardTokenResponse, TokenResponse,
    core::{
        CoreAuthDisplay, CoreAuthPrompt, CoreAuthenticationFlow, CoreErrorResponseType,
        CoreGenderClaim, CoreJsonWebKey, CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreRevocableToken,
        CoreRevocationErrorResponse, CoreTokenIntrospectionResponse, CoreTokenType,
    },
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{lifecycle, store::Store};

#[path = "identity/jwks.rs"]
pub(crate) mod jwks;
use jwks::{JwksCache, JwksPolicy};
#[path = "identity/client_label.rs"]
mod client_label;
#[path = "identity/session_policy.rs"]
pub(crate) mod session_policy;
use session_policy::SessionPolicy;
/// Ended sessions (signed out or expired) are kept this long, then deleted (#80).
const ENDED_SESSION_RETENTION_DAYS: i32 = 30;
#[path = "identity/throttle.rs"]
pub(crate) mod throttle;
/// How often a live session's `last_seen_at` is written, at most.
const LAST_SEEN_EVERY_SECONDS: i32 = 5 * 60;

const SESSION: &str = "omg_session";
const CSRF: &str = "omg_csrf";
const BROWSER: &str = "omg_oidc";
const LOGIN_SECONDS: u32 = 10 * 60;
/// Unexpired sign-in attempts the installation holds at most (#78).
const PENDING_LOGIN_CAP: i64 = 10_000;
/// Default per-address rate of sign-in starts and of callbacks, each.
const LOGIN_RATE_PER_MINUTE: u32 = 60;

mod config;
mod login;
mod oidc;
mod session;

pub use config::IdentityConfig;
use config::validate_url;
pub use login::PeerIp;
use login::throttled;
use login::{auth_config, callback, login, logout};
use oidc::{ASYMMETRIC_ALGS, EnterpriseClient, OidcHttp, Provider, verify_id_token};
pub use session::{CurrentSession, authenticate_management};
use session::{verify_csrf, verify_session};

/// SCIM provisioning bound to the configured OIDC issuer (see `crate::scim`).
pub(crate) struct ScimRuntime {
    pub(crate) token_hash: [u8; 32],
    pub(crate) issuer: String,
    pub(crate) base_url: String,
}

#[derive(Clone)]
pub struct IdentityState {
    store: Store,
    provider: Option<Arc<Provider>>,
    scim: Option<Arc<ScimRuntime>>,
    /// Lifetime and idle timeout of new sessions and of recorded activity.
    policy: SessionPolicy,
    /// #37 PR2: admin-token request handling (IP allowlist resolution) is
    /// independent of whether OIDC sign-in is configured at all, so this is
    /// read directly from `GATEWAY_TRUSTED_PROXIES` rather than through
    /// `IdentityConfig` (which only exists when OIDC is configured).
    trusted_proxies: throttle::TrustedProxies,
    /// Bounded per-replica rate limiting of admin-token authentication
    /// attempts (malformed credentials, unknown/expired/revoked tokens):
    /// these are never individually audited (coordinator decision), only
    /// aggregated and rate-limited, so a flood of garbage bearer tokens
    /// cannot grow the audit log or the key-lookup load without bound.
    admin_token_limiter: Arc<throttle::Limiter>,
    /// #37 PR2 finding 3: a second, independent admission bound on top of
    /// the per-IP limiter above, checked *after* a token has successfully
    /// authenticated and before the attempt is started or dispatched --
    /// bounds how much one compromised or misbehaving token can do even
    /// from many different client addresses (which the per-IP limiter
    /// alone cannot see).
    admin_token_per_token_limiter: Arc<throttle::TokenLimiter>,
    /// #37 PR2 finding 3: one shared, replica-wide admission bound, checked
    /// *before* the expensive (one database round trip) authentication
    /// lookup -- bounds distributed floods of invalid-shape or unknown-id
    /// credentials spread across many client addresses, each individually
    /// under the per-IP limit, before any of that database work happens.
    admin_token_replica_limiter: Arc<throttle::ReplicaLimiter>,
}
/// Default rate of admin-token authentication attempts believed per client
/// address, per replica (same shape as [`LOGIN_RATE_PER_MINUTE`]). Higher
/// than the human sign-in rate: a legitimate automation caller using a
/// single admin token can reasonably issue many requests a minute, and only
/// requests that at least look like an admin token (not merely bearing some
/// Authorization header) ever consume this budget.
const ADMIN_TOKEN_AUTH_RATE_PER_MINUTE: u32 = 1_000;
/// #37 PR2 finding 3: per admitted (authenticated) admin token, per minute,
/// per replica. Same shape and magnitude as the per-IP rate above, but keyed
/// on the token's own id instead of a client address, so it still bounds a
/// single token's admission load even when spread across many addresses.
const ADMIN_TOKEN_PER_TOKEN_RATE_PER_MINUTE: u32 = 1_000;
/// #37 PR2 finding 3: every admin-token authentication attempt on one
/// replica, successful or not, aggregated into a single shared bound. Ten
/// times the per-IP/per-token rate: generous for one legitimate caller, but
/// still a hard ceiling on the database work a distributed flood (many
/// addresses, each individually under the per-IP limit) can cause on this
/// replica before a single request is even authenticated.
const ADMIN_TOKEN_REPLICA_RATE_PER_MINUTE: u32 = 10_000;

impl IdentityState {
    /// Discovery/issuer/JWKS validation is fail-closed: configured OIDC failure prevents startup.
    pub async fn new(store: Store, config: Option<IdentityConfig>) -> anyhow::Result<Self> {
        Self::build(
            store,
            config,
            JwksPolicy::default(),
            Arc::new(std::time::Instant::now),
        )
        .await
    }

    #[cfg_attr(not(test), expect(clippy::too_many_lines, reason = "#499"))]
    async fn build(
        store: Store,
        config: Option<IdentityConfig>,
        policy: JwksPolicy,
        clock: jwks::Clock,
    ) -> anyhow::Result<Self> {
        let provider = if let Some(config) = config {
            let http = OidcHttp::new(config.allow_loopback_http)?;
            // The issuer must pass the egress policy (#33); say which setting
            // to change instead of a bare discovery failure.
            let egress = crate::egress::policy().permit_loopback(config.allow_loopback_http);
            let issuer = Url::parse(config.issuer.as_str())
                .map_err(|_| anyhow::anyhow!("Invalid OIDC issuer"))?;
            crate::egress::check_url(&egress, crate::egress::Purpose::Oidc, &issuer)?;
            if let Some(host) = issuer
                .host_str()
                .filter(|h| !h.starts_with('[') && h.parse::<IpAddr>().is_err())
            {
                crate::egress::resolve(
                    &egress,
                    crate::egress::Purpose::Oidc,
                    host,
                    issuer.port_or_known_default().unwrap_or(443),
                )
                .await?;
            }
            let transport = |request| http.clone().send(request);
            let metadata = CoreProviderMetadata::discover_async(config.issuer.clone(), &transport)
                .await
                .map_err(|_| anyhow::anyhow!("OIDC discovery validation failed"))?;
            validate_url(
                metadata.authorization_endpoint().as_str(),
                config.allow_loopback_http,
            )?;
            let token_endpoint = metadata
                .token_endpoint()
                .ok_or_else(|| anyhow::anyhow!("OIDC provider has no token endpoint"))?;
            validate_url(token_endpoint.as_str(), config.allow_loopback_http)?;
            // Discovered endpoints may be IP literals, which the resolver never sees.
            crate::egress::check_url(
                &egress,
                crate::egress::Purpose::Oidc,
                &Url::parse(token_endpoint.as_str())?,
            )?;
            let algs: Vec<CoreJwsSigningAlgorithm> = metadata
                .id_token_signing_alg_values_supported()
                .iter()
                .filter(|alg| ASYMMETRIC_ALGS.contains(alg))
                .cloned()
                .collect();
            anyhow::ensure!(
                !algs.is_empty(),
                "OIDC provider advertises no supported asymmetric ID-token signing algorithm"
            );
            let jwks_uri = metadata.jwks_uri().url().to_string();
            validate_url(&jwks_uri, config.allow_loopback_http)?;
            crate::egress::check_url(
                &egress,
                crate::egress::Purpose::Oidc,
                &Url::parse(&jwks_uri)?,
            )?;
            let fetch_http = http.clone();
            let fetcher: jwks::Fetcher = Arc::new(move || {
                let (http, uri) = (fetch_http.clone(), jwks_uri.clone());
                Box::pin(http.fetch_jwks(uri))
            });
            let jwks = JwksCache::load(fetcher, policy, clock)
                .await
                .map_err(|_| anyhow::anyhow!("OIDC JWKS validation failed"))?;
            let redirect =
                RedirectUrl::new(format!("{}/api/v1/auth/callback", config.public_origin))
                    .map_err(|_| anyhow::anyhow!("Invalid OIDC callback URL"))?;
            let client = EnterpriseClient::from_provider_metadata(
                metadata,
                ClientId::new(config.client_id.clone()),
                config.client_secret.clone().map(ClientSecret::new),
            )
            .set_redirect_uri(redirect);
            let (login_limiter, callback_limiter) = (
                throttle::Limiter::new(config.login_rate_per_minute),
                throttle::Limiter::new(config.login_rate_per_minute),
            );
            Some(Arc::new(Provider {
                login_limiter,
                callback_limiter,
                login_full_until: std::sync::Mutex::default(),
                config,
                client,
                http,
                jwks,
                algs,
            }))
        } else {
            None
        };
        let policy = provider
            .as_ref()
            .map_or_else(SessionPolicy::default, |p| p.config.session_policy);
        // Reuses whatever the OIDC provider already parsed from the same
        // env var when configured; parsed independently otherwise, so
        // admin-token IP resolution works even with no OIDC configured.
        let trusted_proxies = match &provider {
            Some(provider) => provider.config.trusted_proxies.clone(),
            None => match env::var("GATEWAY_TRUSTED_PROXIES") {
                Ok(value) if !value.trim().is_empty() => throttle::TrustedProxies::parse(&value)?,
                Ok(_) => anyhow::bail!("GATEWAY_TRUSTED_PROXIES must not be empty"),
                Err(env::VarError::NotPresent) => throttle::TrustedProxies::default(),
                Err(_) => anyhow::bail!("GATEWAY_TRUSTED_PROXIES is not valid Unicode"),
            },
        };
        Ok(Self {
            store,
            provider,
            scim: None,
            policy,
            trusted_proxies,
            admin_token_limiter: Arc::new(throttle::Limiter::new(ADMIN_TOKEN_AUTH_RATE_PER_MINUTE)),
            admin_token_per_token_limiter: Arc::new(throttle::TokenLimiter::new(
                ADMIN_TOKEN_PER_TOKEN_RATE_PER_MINUTE,
            )),
            admin_token_replica_limiter: Arc::new(throttle::ReplicaLimiter::new(
                ADMIN_TOKEN_REPLICA_RATE_PER_MINUTE,
            )),
        })
    }

    /// Enable SCIM provisioning. SCIM groups feed the configured issuer's group
    /// mappings, so SCIM requires OIDC.
    pub fn with_scim(mut self, scim: Option<crate::scim::ScimConfig>) -> anyhow::Result<Self> {
        self.scim = match (scim, &self.provider) {
            (None, _) => None,
            (Some(_), None) => anyhow::bail!("GATEWAY_SCIM_TOKEN_ENV requires OIDC sign-in"),
            (Some(scim), Some(provider)) => Some(Arc::new(ScimRuntime {
                token_hash: scim.token_hash(),
                issuer: provider.config.issuer.as_str().to_owned(),
                base_url: format!("{}/scim/v2", provider.config.public_origin),
            })),
        };
        Ok(self)
    }

    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    /// Test-only: a copy with an explicit `GATEWAY_TRUSTED_PROXIES` policy,
    /// for admin-token IP-allowlist tests that need a trusted proxy chain
    /// (`management::authz_matrix_tests`, outside this module's own tree).
    #[cfg(test)]
    pub(crate) fn with_trusted_proxies(mut self, proxies: throttle::TrustedProxies) -> Self {
        self.trusted_proxies = proxies;
        self
    }

    /// Replace the per-token and replica admin-token limiters with tiny,
    /// test-chosen rates (#37 PR2 finding 3): deterministic small-limit
    /// tests exhaust a limiter of 1-2 requests a minute within a handful of
    /// in-process requests, with no real-minute wait and no injected clock
    /// (the limiters still read the wall clock, but a burst this small is
    /// exhausted well inside one test's execution). Independent `Arc`s, so
    /// a clone of this state still shares its own limiters correctly
    /// (`management::authz_admin_token_tests` clones `IdentityState`
    /// freely); this only ever replaces the limiter this value points at
    /// before any clone is made, never a production default.
    #[cfg(test)]
    pub(crate) fn with_admin_token_limits(
        mut self,
        per_token_per_minute: u32,
        replica_per_minute: u32,
    ) -> Self {
        self.admin_token_per_token_limiter =
            Arc::new(throttle::TokenLimiter::new(per_token_per_minute));
        self.admin_token_replica_limiter =
            Arc::new(throttle::ReplicaLimiter::new(replica_per_minute));
        self
    }

    pub(crate) fn scim(&self) -> Option<Arc<ScimRuntime>> {
        self.scim.clone()
    }

    /// Read-only sign-in configuration for Admin > Settings > Sign-in:
    /// public values only (never the client secret).
    pub fn sign_in_summary(&self) -> serde_json::Value {
        match &self.provider {
            Some(provider) => {
                let status = provider.jwks.status();
                serde_json::json!({
                "enabled": true,
                "issuer": provider.config.issuer.as_str(),
                "client_id": provider.config.client_id,
                "client_type": if provider.config.client_secret.is_some() { "confidential" } else { "public" },
                "groups_claim": provider.config.groups_claim,
                "public_url": provider.config.public_origin,
                "callback_url": format!("{}/api/v1/auth/callback", provider.config.public_origin),
                "secure_cookies": provider.config.secure_cookies,
                "jwks": {
                    "keys": status.keys,
                    "refreshed_at": status.refreshed_at,
                    "fresh_until": status.fresh_until,
                    "last_failure_at": status.last_failure_at,
                    "state": match status.state {
                        jwks::Freshness::Fresh => "fresh",
                        jwks::Freshness::Stale => "stale",
                        jwks::Freshness::Unavailable => "unavailable",
                    },
                },
                })
            }
            None => serde_json::json!({ "enabled": false }),
        }
    }
}

#[derive(Clone)]
pub struct BrowserPrincipal {
    pub user_id: Uuid,
    /// Normalized, signature-verified email claim from this browser session, not users.email.
    pub email: String,
    pub platform_admin: bool,
    pub platform_auditor: bool,
}

/// A live admin-token request context (#37 PR2): everything
/// `authenticate_admin_token` returned, plus the request's resolved client
/// address (identical resolution as [`throttle::client_ip`]), needed to
/// enforce the token's optional IP allowlist and to record last use.
#[derive(Clone)]
pub struct AdminTokenContext {
    pub token_id: Uuid,
    pub owner_user_id: Uuid,
    pub scopes: Vec<String>,
    pub ip_allowlist: Option<Vec<String>>,
    pub client_address: Option<IpAddr>,
}

/// Exactly one credential family authenticated a management request (#37
/// PR2): a browser session, or an admin API token. Never both -- selection
/// happens once, in [`authenticate_management`], before extraction, and a
/// `BrowserPrincipal` is never manufactured for a token (a token's
/// authority is its own: an owner id, scopes and an optional allowlist, not
/// a verified email or session claim).
#[derive(Clone)]
pub enum ManagementPrincipal {
    Browser(BrowserPrincipal),
    AdminToken(AdminTokenContext),
}
impl ManagementPrincipal {
    /// The user whose authority governs this request: the signed-in user,
    /// or the token's (immutable, currently-effective-Admin) owner.
    pub fn user_id(&self) -> Uuid {
        match self {
            Self::Browser(b) => b.user_id,
            Self::AdminToken(t) => t.owner_user_id,
        }
    }
    /// The admin token that authenticated this request, for audit
    /// attribution; `None` for a browser session.
    pub fn token_id(&self) -> Option<Uuid> {
        match self {
            Self::Browser(_) => None,
            Self::AdminToken(t) => Some(t.token_id),
        }
    }
    pub fn as_browser(&self) -> Option<&BrowserPrincipal> {
        match self {
            Self::Browser(b) => Some(b),
            Self::AdminToken(_) => None,
        }
    }
    pub fn as_admin_token(&self) -> Option<&AdminTokenContext> {
        match self {
            Self::Browser(_) => None,
            Self::AdminToken(t) => Some(t),
        }
    }
}

pub fn router(state: IdentityState) -> Router<Store> {
    let scim = crate::scim::router(crate::scim::ScimState::new(
        state.store.clone(),
        state.scim.clone(),
    ));
    Router::new()
        .route("/api/v1/auth/config", get(auth_config))
        .route("/api/v1/auth/login", get(login))
        .route("/api/v1/auth/callback", get(callback))
        .route("/api/v1/auth/logout", post(logout))
        .with_state(state)
        .merge(scim)
}

/// A refused authentication or sign-in step. The optional message overrides
/// the status's default text (storage failures, #439).
#[derive(Debug)]
struct AuthError(StatusCode, Option<&'static str>);
impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let message = self.1.unwrap_or(match self.0 {
            StatusCode::UNAUTHORIZED => "Authentication required",
            StatusCode::FORBIDDEN => "Request verification failed",
            StatusCode::CONFLICT => "Account linking requires operator approval",
            StatusCode::SERVICE_UNAVAILABLE => "Login unavailable",
            // Only a failed OS RNG during sign-in is a 500 now; storage failures
            // carry the same text with 503 (`storage_unavailable`, #439).
            StatusCode::INTERNAL_SERVER_ERROR => "Authentication service unavailable",
            _ => "Login verification failed",
        });
        private((self.0, Json(serde_json::json!({"error": message}))).into_response())
    }
}
/// Storage failed while authenticating or signing in: the retryable 503 every
/// other management storage failure answers (#439), never details. A failed
/// The lower-cased email an IdP asserts: at most 320 characters, the bound every
/// management email uses (#405), so a provisioned account can sign in.
fn sign_in_email_valid(email: &str) -> bool {
    !email.is_empty()
        && crate::text::chars_at_most(email, 320)
        && email.contains('@')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
}
/// OS RNG stays a 500 (`random_token`).
fn internal(error: sqlx::Error) -> AuthError {
    // The sanitized management storage diagnostic: kind, SQLSTATE and object
    // names, never row data or bound values.
    crate::management::log_storage_error(&error);
    storage_unavailable()
}
fn storage_unavailable() -> AuthError {
    AuthError(
        StatusCode::SERVICE_UNAVAILABLE,
        Some("Authentication service unavailable"),
    )
}
fn invalid() -> AuthError {
    AuthError(StatusCode::BAD_REQUEST, None)
}
fn private(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}
/// 256-bit OS CSPRNG token; an RNG failure fails the request closed (500, no
/// state stored, nothing signed in).
fn random_token() -> Result<String, AuthError> {
    crate::entropy::hex_token().map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR, None))
}
fn hash(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}
fn is_token(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// No decoding, prefix matching, or first/last-wins ambiguity for authentication cookies.
fn cookie(headers: &HeaderMap, name: &str) -> Result<Option<String>, AuthError> {
    let mut found = None;
    for line in headers.get_all(header::COOKIE) {
        let line = line.to_str().map_err(|_| invalid())?;
        for part in line.split(';') {
            let Some((key, value)) = part.trim().split_once('=') else {
                if part.trim() == name {
                    return Err(invalid());
                }
                continue;
            };
            if key.trim() == name {
                if found.is_some() || !is_token(value) {
                    return Err(invalid());
                }
                found = Some(value.to_owned());
            }
        }
    }
    Ok(found)
}
fn set_cookie(
    response: &mut Response,
    name: &str,
    value: &str,
    max_age: u32,
    secure: bool,
    http_only: bool,
) {
    let same_site = if name == CSRF { "Strict" } else { "Lax" };
    let value = format!(
        "{name}={value}; Path=/; Max-Age={max_age}; SameSite={same_site}{}{}",
        if secure { "; Secure" } else { "" },
        if http_only { "; HttpOnly" } else { "" }
    );
    // All fields are constants, booleans, numbers or generated hexadecimal tokens.
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&value).expect("safe generated cookie"),
    );
}

/// A same-origin dashboard path to return to after sign-in, or `None`.
/// Only relative paths are accepted: never a scheme, host, protocol-relative
/// `//`, backslash or control character, and never an API/inference/health
/// path (Axum owns those; returning there would show raw JSON).
pub(crate) fn safe_return_path(value: &str) -> Option<String> {
    let path = value.trim();
    if path.is_empty()
        || path.len() > 2048
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.chars().any(char::is_control)
    {
        return None;
    }
    // Same-origin check: resolving against a placeholder origin must keep it.
    let base = Url::parse("https://dashboard.invalid/").ok()?;
    let url = base.join(path).ok()?;
    if url.origin() != base.origin() {
        return None;
    }
    let route = url.path();
    if ["/api", "/v1", "/health"]
        .iter()
        .any(|p| route == *p || route.starts_with(&format!("{p}/")))
    {
        return None;
    }
    let mut out = route.to_owned();
    if let Some(query) = url.query() {
        out.push('?');
        out.push_str(query);
    }
    (out.len() <= 2048).then_some(out)
}

/** A display name worth showing: trimmed, 1–200 characters, no control characters; anything else is dropped. */
pub(crate) fn display_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    (!name.is_empty() && name.chars().count() <= 200 && !name.chars().any(char::is_control))
        .then(|| name.to_owned())
}

/// Delete expired sign-in attempts, at most `limit` per call (#80). They are
/// useless once expired: `consume_attempt` only accepts unexpired ones. No
/// row locks: one maintenance lease runs it, and runtime has no UPDATE here.
pub async fn prune_login_attempts_fenced(
    store: &Store,
    limit: i64,
    fence: Option<&crate::leases::Fence>,
) -> Result<u64, sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    let pruned = sqlx::query("DELETE FROM oidc_login_attempts WHERE state_hash IN (SELECT state_hash FROM oidc_login_attempts WHERE expires_at<now() ORDER BY expires_at LIMIT $1)")
        .bind(limit)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(pruned)
}

/// Delete sessions that ended (signed out, expired or idle-expired) more than
/// 30 days ago, at most `limit` per call (#80). A live session is never
/// deleted; an ended one can never authenticate again, so deleting it changes
/// nothing a request can observe.
pub async fn prune_sessions_fenced(
    store: &Store,
    limit: i64,
    fence: Option<&crate::leases::Fence>,
) -> Result<u64, sqlx::Error> {
    let mut tx = crate::db::begin(&store.pool).await?;
    crate::leases::fence(&mut tx, fence).await?;
    let pruned = sqlx::query("DELETE FROM browser_sessions WHERE token_hash IN (SELECT token_hash FROM browser_sessions WHERE least(coalesce(revoked_at,'infinity'),expires_at,coalesce(idle_expires_at,'infinity'))<now()-make_interval(days=>$2) LIMIT $1)")
        .bind(limit)
        .bind(ENDED_SESSION_RETENTION_DAYS)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok(pruned)
}

// Test-only imports sit here, after all production code: source scans such as the
// entropy guard (`entropy.rs`) read a file only up to its first test gate (#506).
#[cfg(all(test, feature = "integration-tests"))]
use login::{CallbackQuery, LoginQuery, consume_attempt, resolve_identity, resolve_identity_with};
#[cfg(test)]
use oidc::{EnterpriseIdToken, SignedClaims};
#[cfg(test)]
#[path = "identity/tests.rs"]
pub(crate) mod tests;
