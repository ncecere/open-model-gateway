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

/// Unknown claims are captured inside the signature-verified ID token, never from userinfo.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct SignedClaims {
    #[serde(flatten)]
    values: std::collections::BTreeMap<String, serde_json::Value>,
}
impl AdditionalClaims for SignedClaims {}
type EnterpriseTokenFields = IdTokenFields<
    SignedClaims,
    EmptyExtraTokenFields,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
>;
type EnterpriseClient<A = EndpointNotSet, T = EndpointNotSet, U = EndpointNotSet> = Client<
    SignedClaims,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    StandardTokenResponse<EnterpriseTokenFields, CoreTokenType>,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    A,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    T,
    U,
>;
type DiscoveredClient = EnterpriseClient<EndpointSet, EndpointMaybeSet, EndpointMaybeSet>;
type EnterpriseIdToken = IdToken<
    SignedClaims,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJwsSigningAlgorithm,
>;
type EnterpriseClaims = IdTokenClaims<SignedClaims, CoreGenderClaim>;

/// ID tokens must use an asymmetric signature advertised by the issuer. `none`
/// and shared-secret MACs (HS*) are never accepted, even if advertised.
const ASYMMETRIC_ALGS: [CoreJwsSigningAlgorithm; 9] = [
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha384,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha512,
    CoreJwsSigningAlgorithm::RsaSsaPssSha256,
    CoreJwsSigningAlgorithm::RsaSsaPssSha384,
    CoreJwsSigningAlgorithm::RsaSsaPssSha512,
    CoreJwsSigningAlgorithm::EcdsaP256Sha256,
    CoreJwsSigningAlgorithm::EcdsaP384Sha384,
    CoreJwsSigningAlgorithm::EdDsa,
];

impl SignedClaims {
    fn groups(&self, path: &str) -> Option<Vec<String>> {
        // Prefer the literal claim name (URI and dot-containing claim names are legal).
        let value = if let Some(value) = self.values.get(path) {
            value
        } else {
            let mut parts = path.split('.');
            let mut value = self.values.get(parts.next()?)?;
            for part in parts {
                value = value.as_object()?.get(part)?;
            }
            value
        };
        let values = value.as_array()?;
        if values.len() > 2048 {
            return None;
        }
        values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 1024)
                    .map(str::to_owned)
            })
            .collect()
    }
}

/// Intentionally not Debug: contains a confidential-client secret.
#[derive(Clone)]
pub struct IdentityConfig {
    public_origin: String,
    issuer: IssuerUrl,
    client_id: String,
    client_secret: Option<String>,
    allow_loopback_http: bool,
    secure_cookies: bool,
    groups_claim: String,
    trusted_proxies: throttle::TrustedProxies,
    login_rate_per_minute: u32,
    pending_login_cap: i64,
    pending_cap_latch: Duration,
    session_policy: SessionPolicy,
}

impl IdentityConfig {
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        fn read(name: &str) -> anyhow::Result<Option<String>> {
            match env::var(name) {
                Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
                Ok(_) => anyhow::bail!("{name} must not be empty"),
                Err(env::VarError::NotPresent) => Ok(None),
                Err(_) => anyhow::bail!("{name} is not valid Unicode"),
            }
        }
        let public = read("GATEWAY_PUBLIC_URL")?;
        let issuer = read("GATEWAY_OIDC_ISSUER")?;
        let client_id = read("GATEWAY_OIDC_CLIENT_ID")?;
        let secret = read("GATEWAY_OIDC_CLIENT_SECRET")?;
        // Checked even without OIDC, so a typo never goes unnoticed.
        let session_policy = SessionPolicy::parse(
            read("GATEWAY_SESSION_LIFETIME")?.as_deref(),
            read("GATEWAY_SESSION_IDLE_TIMEOUT")?.as_deref(),
        )?;
        if issuer.is_none() && client_id.is_none() && secret.is_none() {
            // A public URL may also be used by non-identity application features.
            if let Some(public) = public {
                Self::validate_origin(
                    &public,
                    env::var("GATEWAY_ENV").as_deref() == Ok("development"),
                )?;
            }
            return Ok(None);
        }
        let mut config = Self::parse(
            public.ok_or_else(|| anyhow::anyhow!("GATEWAY_PUBLIC_URL is required for OIDC"))?,
            issuer.ok_or_else(|| anyhow::anyhow!("GATEWAY_OIDC_ISSUER is required for OIDC"))?,
            client_id
                .ok_or_else(|| anyhow::anyhow!("GATEWAY_OIDC_CLIENT_ID is required for OIDC"))?,
            secret,
            env::var("GATEWAY_ENV").as_deref() == Ok("development"),
        )?;
        if let Some(path) = read("GATEWAY_OIDC_GROUPS_CLAIM")? {
            anyhow::ensure!(
                path.len() <= 512 && !path.chars().any(|c| c.is_whitespace() || c.is_control()),
                "Invalid OIDC groups claim path"
            );
            config.groups_claim = path;
        }
        if let Some(value) = read("GATEWAY_TRUSTED_PROXIES")? {
            config.trusted_proxies = throttle::TrustedProxies::parse(&value)?;
        }
        config.session_policy = session_policy;
        if let Some(value) = read("GATEWAY_LOGIN_RATE_PER_MINUTE")? {
            config.login_rate_per_minute = value
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=10_000).contains(n))
                .ok_or_else(|| {
                    anyhow::anyhow!("GATEWAY_LOGIN_RATE_PER_MINUTE must be 1 to 10000")
                })?;
        }
        Ok(Some(config))
    }

    fn validate_origin(value: &str, development: bool) -> anyhow::Result<Url> {
        let url = validate_url(value, development)?;
        anyhow::ensure!(
            url.path() == "/" && url.query().is_none(),
            "GATEWAY_PUBLIC_URL must be an origin without a path or query"
        );
        anyhow::ensure!(
            value == url.origin().ascii_serialization()
                || value == format!("{}/", url.origin().ascii_serialization()),
            "GATEWAY_PUBLIC_URL must be a canonical origin"
        );
        Ok(url)
    }

    fn parse(
        public: String,
        issuer: String,
        client_id: String,
        client_secret: Option<String>,
        development: bool,
    ) -> anyhow::Result<Self> {
        let public = Self::validate_origin(&public, development)?;
        let issuer_url = validate_url(&issuer, development)?;
        anyhow::ensure!(
            issuer_url.query().is_none(),
            "OIDC issuer must not contain a query"
        );
        anyhow::ensure!(
            !client_id.trim().is_empty(),
            "OIDC client ID must not be empty"
        );
        Ok(Self {
            public_origin: public.origin().ascii_serialization(),
            issuer: IssuerUrl::new(issuer).map_err(|_| anyhow::anyhow!("Invalid OIDC issuer"))?,
            client_id,
            client_secret,
            allow_loopback_http: development,
            secure_cookies: public.scheme() == "https",
            groups_claim: "groups".into(),
            trusted_proxies: throttle::TrustedProxies::default(),
            login_rate_per_minute: LOGIN_RATE_PER_MINUTE,
            pending_login_cap: PENDING_LOGIN_CAP,
            pending_cap_latch: Duration::from_secs(5),
            session_policy: SessionPolicy::default(),
        })
    }
}

fn validate_url(value: &str, allow_loopback_http: bool) -> anyhow::Result<Url> {
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!("Invalid identity URL"))?;
    anyhow::ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "Identity URL must have a host and no credentials or fragment"
    );
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    anyhow::ensure!(
        url.scheme() == "https" || (allow_loopback_http && loopback && url.scheme() == "http"),
        "Identity URLs require HTTPS (HTTP is limited to development loopback)"
    );
    Ok(url)
}

#[derive(Clone)]
struct OidcHttp {
    client: reqwest::Client,
    allow_loopback_http: bool,
}
impl OidcHttp {
    async fn send(self, request: HttpRequest) -> Result<HttpResponse, std::io::Error> {
        // Apply the transport policy BEFORE every request, including the discovered JWKS URL.
        let fail = || std::io::Error::other("OIDC HTTP request failed");
        let url = request.uri().to_string();
        validate_url(&url, self.allow_loopback_http).map_err(|_| fail())?;
        // IP-literal hosts never reach the resolver: check them here (#33).
        let parsed = Url::parse(&url).map_err(|_| fail())?;
        crate::egress::check_url(
            &crate::egress::policy().permit_loopback(self.allow_loopback_http),
            crate::egress::Purpose::Oidc,
            &parsed,
        )
        .map_err(|_| fail())?;
        let request = reqwest::Request::try_from(request).map_err(|_| fail())?;
        let mut response = self.client.execute(request).await.map_err(|_| fail())?;
        let mut builder = axum::http::Response::builder().status(response.status());
        *builder.headers_mut().ok_or_else(fail)? = response.headers().clone();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| fail())? {
            if body.len() + chunk.len() > 1024 * 1024 {
                return Err(fail());
            }
            body.extend_from_slice(&chunk);
        }
        builder.body(body).map_err(|_| fail())
    }

    /// One bounded JWKS fetch under the same transport policy, with its Cache-Control max-age.
    async fn fetch_jwks(self, uri: String) -> Result<jwks::Fetched, ()> {
        let request = axum::http::Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::ACCEPT, "application/json")
            .body(Vec::new())
            .map_err(|_| ())?;
        let response = self.send(request).await.map_err(|_| ())?;
        if response.status() != StatusCode::OK {
            return Err(());
        }
        let max_age = jwks::max_age(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
        );
        let keys: jwks::Keys = serde_json::from_slice(response.body()).map_err(|_| ())?;
        Ok(jwks::Fetched { keys, max_age })
    }
}

struct Provider {
    config: IdentityConfig,
    client: DiscoveredClient,
    http: OidcHttp,
    jwks: JwksCache,
    algs: Vec<CoreJwsSigningAlgorithm>,
    /// Per-replica sign-in start and callback buckets (#78).
    login_limiter: throttle::Limiter,
    callback_limiter: throttle::Limiter,
    /// Until when this replica treats the pending-attempt cap as reached,
    /// so refusals during a flood don't each count the table.
    login_full_until: std::sync::Mutex<Option<std::time::Instant>>,
}

impl Provider {
    fn verifier(&self, keys: jwks::Keys) -> IdTokenVerifier<'static, CoreJsonWebKey> {
        // Always a public-client verifier: the client secret is never an ID-token key.
        IdTokenVerifier::new_public_client(
            ClientId::new(self.config.client_id.clone()),
            self.config.issuer.clone(),
            keys,
        )
        .set_allowed_algs(self.algs.iter().cloned())
    }
}

/// Signature, issuer, audience, expiry, nonce and algorithm policy. A token naming an
/// unknown key triggers one rate-limited, single-flight JWKS refetch and one retry.
async fn verify_id_token<'t>(
    provider: &Provider,
    token: &'t EnterpriseIdToken,
    nonce: &Nonce,
) -> Result<&'t EnterpriseClaims, AuthError> {
    let unavailable = || AuthError(StatusCode::SERVICE_UNAVAILABLE);
    let keys = provider.jwks.current().await.ok_or_else(unavailable)?;
    match token.claims(&provider.verifier(keys), nonce) {
        Ok(claims) => Ok(claims),
        Err(ClaimsVerificationError::SignatureVerification(
            SignatureVerificationError::NoMatchingKey,
        )) => {
            let keys = provider
                .jwks
                .refresh_for_unknown_kid()
                .await
                .ok_or_else(unavailable)?;
            token
                .claims(&provider.verifier(keys), nonce)
                .map_err(|_| invalid())
        }
        Err(_) => Err(invalid()),
    }
}

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
}
/// Default rate of admin-token authentication attempts believed per client
/// address, per replica (same shape as [`LOGIN_RATE_PER_MINUTE`]). Higher
/// than the human sign-in rate: a legitimate automation caller using a
/// single admin token can reasonably issue many requests a minute, and only
/// requests that at least look like an admin token (not merely bearing some
/// Authorization header) ever consume this budget.
const ADMIN_TOKEN_AUTH_RATE_PER_MINUTE: u32 = 1_000;

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

    async fn build(
        store: Store,
        config: Option<IdentityConfig>,
        policy: JwksPolicy,
        clock: jwks::Clock,
    ) -> anyhow::Result<Self> {
        let provider = if let Some(config) = config {
            let http = OidcHttp {
                // No ambient HTTP(S)_PROXY/ALL_PROXY and no implicit retries,
                // like every provider client (#144): identity traffic goes
                // straight to the issuer, once.
                client: reqwest::Client::builder()
                    .no_proxy()
                    // Discovery, token and JWKS addresses pass the egress policy
                    // (#33). `allow_loopback_http` is GATEWAY_ENV=development, as
                    // in the global policy; passing it keeps tests (strict global
                    // default) and production identical.
                    .dns_resolver(crate::egress::resolver_with(
                        crate::egress::Purpose::Oidc,
                        crate::egress::policy().permit_loopback(config.allow_loopback_http),
                    ))
                    .retry(reqwest::retry::never())
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(15))
                    .connect_timeout(Duration::from_secs(5))
                    .build()
                    .map_err(|_| anyhow::anyhow!("Cannot initialize OIDC HTTP client"))?,
                allow_loopback_http: config.allow_loopback_http,
            };
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

#[derive(Debug)]
struct AuthError(StatusCode);
impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let message = match self.0 {
            StatusCode::UNAUTHORIZED => "Authentication required",
            StatusCode::FORBIDDEN => "Request verification failed",
            StatusCode::CONFLICT => "Account linking requires operator approval",
            StatusCode::SERVICE_UNAVAILABLE => "Login unavailable",
            StatusCode::INTERNAL_SERVER_ERROR => "Authentication service unavailable",
            _ => "Login verification failed",
        };
        private((self.0, Json(serde_json::json!({"error": message}))).into_response())
    }
}
fn internal(_: sqlx::Error) -> AuthError {
    AuthError(StatusCode::INTERNAL_SERVER_ERROR)
}
fn invalid() -> AuthError {
    AuthError(StatusCode::BAD_REQUEST)
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
    crate::entropy::hex_token().map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR))
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

/// Public sign-in configuration: whether SSO is configured and the uploaded
/// installation logo (`{url, updated_at}` or null) with the installation name
/// as its alt text (only when a logo is set). Both are null on a storage
/// error, so the sign-in page never depends on them.
async fn auth_config(
    State(state): State<IdentityState>,
    files: Option<axum::Extension<crate::filestore::FileStoreRuntime>>,
) -> Response {
    let files = files.map(|axum::Extension(r)| r).unwrap_or_default();
    let (logo, name) = crate::management::branding::sign_in_branding(&state.store.pool, &files)
        .await
        .unwrap_or_default();
    private(
        Json(serde_json::json!({
            "enabled": state.provider.is_some(),
            "logo": logo,
            "installation_name": name,
        }))
        .into_response(),
    )
}

#[derive(Deserialize, Default)]
struct LoginQuery {
    return_to: Option<String>,
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

async fn login(
    State(state): State<IdentityState>,
    PeerIp(peer): PeerIp,
    headers: HeaderMap,
    query: Result<Query<LoginQuery>, QueryRejection>,
) -> Result<Response, AuthError> {
    let provider = state
        .provider
        .as_ref()
        .ok_or(AuthError(StatusCode::SERVICE_UNAVAILABLE))?;
    let client = throttle::client_ip(peer, &headers, &provider.config.trusted_proxies);
    if let Err(wait) = provider
        .login_limiter
        .check(client, std::time::Instant::now())
    {
        return Ok(throttled(
            "login",
            "client_rate",
            StatusCode::TOO_MANY_REQUESTS,
            wait,
        ));
    }
    let full = || {
        provider
            .login_full_until
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    };
    if full().is_some_and(|until| std::time::Instant::now() < until) {
        return Ok(throttled(
            "login",
            "pending_cap",
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(60),
        ));
    }
    // An unsafe or malformed return path is dropped (sign-in still works and lands on Home).
    let return_to = query
        .ok()
        .and_then(|Query(q)| q.return_to)
        .as_deref()
        .and_then(safe_return_path)
        .filter(|p| p != "/");
    // Every secret first, so an RNG failure stores and redirects nothing. The
    // PKCE verifier is 32 OS CSPRNG bytes as well (RFC 7636 base64url).
    let (state_token, nonce_token, browser) = (random_token()?, random_token()?, random_token()?);
    let verifier = {
        use base64::Engine as _;
        let bytes = crate::entropy::bytes::<32>()
            .map_err(|_| AuthError(StatusCode::INTERNAL_SERVER_ERROR))?;
        PkceCodeVerifier::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    };
    let challenge = PkceCodeChallenge::from_code_verifier_sha256(&verifier);
    let (url, oauth_state, nonce) = provider
        .client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            move || CsrfToken::new(state_token),
            move || Nonce::new(nonce_token),
        )
        .add_scope(Scope::new("email".into()))
        // `profile` carries the optional display name; it is presentation only.
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(challenge)
        .url();
    // Installation-wide bound on pending attempts: past it nothing is stored.
    // The count stops at the cap, so it costs at most `cap` index entries.
    // Concurrent inserts don't see each other (READ COMMITTED), so the cap can
    // be exceeded by the inserts in flight at once (at most one per database
    // connection); that still bounds the table. A strict cap would need a lock
    // every sign-in waits on.
    let stored = sqlx::query("INSERT INTO oidc_login_attempts(state_hash,browser_hash,nonce,pkce_verifier,expires_at,return_to) SELECT $1,$2,$3,$4,now()+interval '10 minutes',$5 WHERE (SELECT count(*) FROM (SELECT 1 FROM oidc_login_attempts WHERE expires_at>now() LIMIT $6) pending)<$6")
        .bind(hash(oauth_state.secret())).bind(hash(&browser)).bind(nonce.secret()).bind(verifier.secret()).bind(return_to).bind(provider.config.pending_login_cap)
        .execute(&state.store.pool).await.map_err(internal)?;
    if stored.rows_affected() == 0 {
        *full() = Some(std::time::Instant::now() + provider.config.pending_cap_latch);
        return Ok(throttled(
            "login",
            "pending_cap",
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(60),
        ));
    }
    let mut response = private(Redirect::to(url.as_str()).into_response());
    set_cookie(
        &mut response,
        BROWSER,
        &browser,
        LOGIN_SECONDS,
        provider.config.secure_cookies,
        true,
    );
    Ok(response)
}

/// The connection's peer address, when the server records it
/// (`into_make_service_with_connect_info`); in-process tests have none.
pub struct PeerIp(Option<IpAddr>);
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for PeerIp {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0.ip()),
        ))
    }
}

/// A throttled sign-in step: nothing stored, no cookie, a `Retry-After`, and
/// a counter labelled only by step and reason (never an address or identity).
fn throttled(
    endpoint: &'static str,
    reason: &'static str,
    status: StatusCode,
    wait: Duration,
) -> Response {
    crate::metrics::METRICS.auth_throttled(endpoint, reason);
    let mut response = private(
        (
            status,
            Json(serde_json::json!({"error": "Too many sign-in attempts; try again shortly"})),
        )
            .into_response(),
    );
    if let Ok(value) = wait.as_secs().max(1).to_string().parse() {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

#[derive(Deserialize)]
struct CallbackQuery {
    state: String,
    code: Option<String>,
    error: Option<String>,
}

async fn consume_attempt(
    store: &Store,
    oauth_state: &str,
    browser: &str,
) -> Result<(String, String, Option<String>), AuthError> {
    let mut tx = crate::db::begin(&store.pool).await.map_err(internal)?;
    let attempt = sqlx::query_as::<_, (String, String, Option<String>)>("DELETE FROM oidc_login_attempts WHERE state_hash=$1 AND browser_hash=$2 AND expires_at>now() RETURNING nonce,pkce_verifier,return_to")
        .bind(hash(oauth_state)).bind(hash(browser)).fetch_optional(&mut *tx).await.map_err(internal)?;
    // Commit before ANY token endpoint request: replay is impossible even if exchange fails.
    tx.commit().await.map_err(internal)?;
    attempt.ok_or_else(invalid)
}

/** A display name worth showing: trimmed, 1–200 characters, no control characters; anything else is dropped. */
pub(crate) fn display_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    (!name.is_empty() && name.chars().count() <= 200 && !name.chars().any(char::is_control))
        .then(|| name.to_owned())
}
async fn callback(
    State(state): State<IdentityState>,
    PeerIp(peer): PeerIp,
    headers: HeaderMap,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Result<Response, AuthError> {
    let provider = state
        .provider
        .as_ref()
        .ok_or(AuthError(StatusCode::SERVICE_UNAVAILABLE))?;
    // Before the attempt is consumed: a throttled callback leaves it usable.
    let client = throttle::client_ip(peer, &headers, &provider.config.trusted_proxies);
    if let Err(wait) = provider
        .callback_limiter
        .check(client, std::time::Instant::now())
    {
        return Ok(throttled(
            "callback",
            "client_rate",
            StatusCode::TOO_MANY_REQUESTS,
            wait,
        ));
    }
    let Query(query) = query.map_err(|_| invalid())?;
    if !is_token(&query.state) {
        return Err(invalid());
    }
    let browser = cookie(&headers, BROWSER)?.ok_or_else(invalid)?;
    let (nonce, verifier, return_to) =
        consume_attempt(&state.store, &query.state, &browser).await?;
    if query.error.is_some() {
        return Err(invalid());
    }
    let code = query
        .code
        .filter(|code| !code.is_empty() && code.len() <= 8192)
        .ok_or_else(invalid)?;
    let transport = |request| provider.http.clone().send(request);
    let tokens = provider
        .client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|_| invalid())?
        .set_pkce_verifier(PkceCodeVerifier::new(verifier))
        .request_async(&transport)
        .await
        .map_err(|_| invalid())?;
    let id_token = tokens.id_token().ok_or_else(invalid)?;
    // Signature (cached, rotating JWKS; asymmetric algorithms only), issuer, audience,
    // expiration and the original nonce.
    let claims = verify_id_token(provider, id_token, &Nonce::new(nonce)).await?;
    // The library deliberately leaves `azp` policy to relying parties.
    if claims
        .authorized_party()
        .is_some_and(|party| party.as_str() != provider.config.client_id)
        || (claims.audiences().len() > 1 && claims.authorized_party().is_none())
        || claims.email_verified() != Some(true)
    {
        return Err(invalid());
    }
    let email = claims
        .email()
        .map(|email| email.as_str().to_lowercase())
        .filter(|email| {
            !email.is_empty()
                && email.len() <= 320
                && email.contains('@')
                && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        })
        .ok_or_else(invalid)?;
    let subject = claims.subject().as_str();
    if subject.is_empty() || subject.len() > 2048 || subject.chars().any(char::is_control) {
        return Err(invalid());
    }
    let groups = claims
        .additional_claims()
        .groups(&provider.config.groups_claim)
        .ok_or(AuthError(StatusCode::FORBIDDEN))?;
    // Presentation only: the verified token's display name (or none) replaces the stored one.
    let display_name = claims
        .name()
        .and_then(|name| name.get(None))
        .and_then(|name| display_name(name.as_str()));
    let user_id = match resolve_identity_with(
        &state.store,
        provider.config.issuer.as_str(),
        subject,
        &email,
        &groups,
        state.scim.is_some(),
    )
    .await
    {
        Ok(id) => id,
        Err(AuthError(status)) if status == StatusCode::FORBIDDEN => {
            // A denied sign-in (no platform role) still records who tried under the
            // linked identity, so Admin › Users shows a name rather than a bare row.
            sqlx::query("UPDATE users u SET display_name=$3 FROM oidc_identities i WHERE i.issuer=$1 AND i.subject=$2 AND u.id=i.user_id AND u.cleaned_at IS NULL AND u.display_name IS DISTINCT FROM $3")
                .bind(provider.config.issuer.as_str())
                .bind(subject)
                .bind(&display_name)
                .execute(&state.store.pool)
                .await
                .map_err(internal)?;
            if wants_html(&headers) {
                return denied_browser_callback(&state, &headers, provider.config.secure_cookies)
                    .await;
            }
            return Err(AuthError(status));
        }
        Err(error) => return Err(error),
    };
    sqlx::query("UPDATE users SET display_name=$2 WHERE id=$1 AND cleaned_at IS NULL AND display_name IS DISTINCT FROM $2")
        .bind(user_id)
        .bind(display_name)
        .execute(&state.store.pool)
        .await
        .map_err(internal)?;
    // Upstream access/refresh tokens are never persisted or forwarded.
    drop(tokens);
    let session = random_token()?;
    let csrf = random_token()?;
    // Device summary for "Your sessions": a fixed-vocabulary browser/OS family only.
    let label = client_label::client_label(
        headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    );
    // Both deadlines are fixed now, on the database clock (#80).
    // Sign-in times live on the user, in the same statement, so deleting
    // ended sessions (#80) never changes "First signed in" or "Last sign-in".
    let inserted: i64 = sqlx::query_scalar("WITH s AS (INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,idle_expires_at,verified_email,client_label,last_seen_at) SELECT $1,id,$3,now()+make_interval(secs=>$6),now()+make_interval(secs=>$7::float8),$4,$5,now() FROM users WHERE id=$2 AND disabled_at IS NULL AND cleaned_at IS NULL AND EXISTS(SELECT 1 FROM effective_platform_roles p WHERE p.user_id=users.id) RETURNING user_id), u AS (UPDATE users SET last_signed_in_at=now(),first_signed_in_at=coalesce(first_signed_in_at,now()) WHERE id IN (SELECT user_id FROM s)) SELECT count(*) FROM s")
        .bind(hash(&session)).bind(user_id).bind(hash(&csrf)).bind(&email).bind(label)
        .bind(f64::from(state.policy.lifetime_seconds())).bind(state.policy.idle_seconds())
        .fetch_one(&state.store.pool).await.map_err(internal)?;
    if inserted != 1 {
        if wants_html(&headers) {
            return denied_browser_callback(&state, &headers, provider.config.secure_cookies).await;
        }
        return Err(AuthError(StatusCode::UNAUTHORIZED));
    }
    // Validated again on the way out; anything else lands on "/" (Home).
    let target = return_to
        .as_deref()
        .and_then(safe_return_path)
        .unwrap_or_else(|| "/".into());
    let mut response = private(Redirect::to(&target).into_response());
    set_cookie(
        &mut response,
        SESSION,
        &session,
        state.policy.lifetime_seconds(),
        provider.config.secure_cookies,
        true,
    );
    set_cookie(
        &mut response,
        CSRF,
        &csrf,
        state.policy.lifetime_seconds(),
        provider.config.secure_cookies,
        false,
    );
    set_cookie(
        &mut response,
        BROWSER,
        "",
        0,
        provider.config.secure_cookies,
        true,
    );
    Ok(response)
}

async fn advisory_lock(tx: &mut Transaction<'_, Postgres>, key: &str) -> Result<(), AuthError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(key)
        .execute(&mut **tx)
        .await
        .map_err(internal)?;
    Ok(())
}

#[cfg(all(test, feature = "integration-tests"))]
async fn resolve_identity(
    store: &Store,
    issuer: &str,
    subject: &str,
    email: &str,
    groups: &[String],
) -> Result<Uuid, AuthError> {
    resolve_identity_with(store, issuer, subject, email, groups, false).await
}

/// With SCIM enabled, SCIM-managed groups take their membership from SCIM, not the
/// token: the claim is merged as (token groups not managed by SCIM) ∪ (SCIM groups).
async fn resolve_identity_with(
    store: &Store,
    issuer: &str,
    subject: &str,
    email: &str,
    groups: &[String],
    scim: bool,
) -> Result<Uuid, AuthError> {
    let email = email.to_lowercase();
    let mut tx = crate::db::begin(&store.pool).await.map_err(internal)?;
    lifecycle::lock(&mut tx).await.map_err(internal)?;
    advisory_lock(&mut tx, &format!("oidc:{}:{issuer}{subject}", issuer.len())).await?;
    // Every account this sign-in may change (the linked one, which may be
    // cleaned up, and the one its email resolves to): authority scope locks
    // up front, in canonical order (`lifecycle::lock_users`). New accounts
    // (the id an account created here gets) have no keys or workspaces yet.
    let new_user = Uuid::new_v4();
    let mut candidates: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM oidc_identities WHERE issuer=$1 AND subject=$2 UNION SELECT id FROM users WHERE lower(email)=$3")
        .bind(issuer).bind(subject).bind(&email).fetch_all(&mut *tx).await.map_err(internal)?;
    candidates.push(new_user);
    lifecycle::lock_users(&mut tx, &candidates)
        .await
        .map_err(internal)?;
    let existing = sqlx::query_as::<_, (Uuid, bool, bool, Option<String>)>(
        "SELECT u.id,u.cleaned_at IS NOT NULL,coalesce(u.cleanup_due_at<=now(),false),u.disable_reason FROM oidc_identities i JOIN users u ON u.id=i.user_id WHERE i.issuer=$1 AND i.subject=$2 FOR UPDATE OF u",
    ).bind(issuer).bind(subject).fetch_optional(&mut *tx).await.map_err(internal)?;
    let mut rebind = false;
    let existing_id = if let Some((id, cleaned, expired, reason)) = existing {
        if cleaned || expired {
            // On legal hold (#75): nothing is removed and the account stays
            // closed until the hold is released.
            if !cleaned
                && !lifecycle::cleanup_user(&mut tx, id)
                    .await
                    .map_err(internal)?
            {
                tx.rollback().await.map_err(internal)?;
                return Err(AuthError(StatusCode::FORBIDDEN));
            }
            if reason.as_deref() != Some("entitlement_loss") {
                tx.commit().await.map_err(internal)?;
                return Err(AuthError(StatusCode::FORBIDDEN));
            }
            let reentitled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oidc_group_mappings WHERE issuer=$1 AND enabled AND target_kind='platform' AND group_value=ANY($2))")
                .bind(issuer).bind(groups).fetch_one(&mut *tx).await.map_err(internal)?;
            if !reentitled {
                tx.commit().await.map_err(internal)?;
                return Err(AuthError(StatusCode::FORBIDDEN));
            }
            rebind = true;
            None
        } else {
            Some(id)
        }
    } else {
        None
    };
    let id = if let Some(id) = existing_id {
        id
    } else {
        advisory_lock(&mut tx, &format!("email:{email}")).await?;
        let created = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO users(id,email) VALUES($1,$2) ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(new_user)
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?;
        let id = if let Some(id) = created {
            id
        } else {
            let (id, active, allowed) = sqlx::query_as::<_, (Uuid, bool, bool)>("SELECT id,disabled_at IS NULL AND cleaned_at IS NULL,oidc_link_allowed FROM users WHERE lower(email)=$1 FOR UPDATE")
                .bind(&email).fetch_one(&mut *tx).await.map_err(internal)?;
            if !active {
                return Err(AuthError(StatusCode::FORBIDDEN));
            }
            let linked: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oidc_identities WHERE user_id=$1)")
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(internal)?;
            if !allowed || linked {
                return Err(AuthError(StatusCode::CONFLICT));
            }
            sqlx::query("UPDATE users SET oidc_link_allowed=false WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            id
        };
        if rebind {
            sqlx::query("UPDATE oidc_identities SET user_id=$3 WHERE issuer=$1 AND subject=$2")
                .bind(issuer)
                .bind(subject)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id) VALUES($1,$2,'identity.rebound','user',$2)")
                .bind(Uuid::new_v4()).bind(id).execute(&mut *tx).await.map_err(internal)?;
        } else {
            sqlx::query("INSERT INTO oidc_identities(issuer,subject,user_id) VALUES($1,$2,$3)")
                .bind(issuer)
                .bind(subject)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }
        id
    };
    let groups = if scim {
        crate::scim::effective_groups(&mut tx, id, groups)
            .await
            .map_err(internal)?
    } else {
        groups.to_vec()
    };
    let active = lifecycle::synchronize_groups(&mut tx, id, issuer, &groups)
        .await
        .map_err(internal)?;
    // Entitlement loss MUST commit session/key revocation before returning the 403.
    tx.commit().await.map_err(internal)?;
    if !active {
        return Err(AuthError(StatusCode::FORBIDDEN));
    }
    Ok(id)
}

struct VerifiedSession {
    principal: BrowserPrincipal,
    token_hash: Vec<u8>,
    csrf_hash: Vec<u8>,
    id: Uuid,
}

/// The public ID of the session a management request came in on (#79):
/// lets "Your sessions" mark it and "Sign out other sessions" keep it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CurrentSession(pub Uuid);
async fn verify_session(
    state: &IdentityState,
    headers: &HeaderMap,
) -> Result<VerifiedSession, AuthError> {
    let token = cookie(headers, SESSION)
        .map_err(|_| AuthError(StatusCode::UNAUTHORIZED))?
        .ok_or(AuthError(StatusCode::UNAUTHORIZED))?;
    let token_hash = hash(&token);
    let row = sqlx::query_as::<_, (Uuid, String, String, Vec<u8>, Uuid, bool)>("SELECT u.id,s.verified_email,p.role,s.csrf_hash,s.id,s.last_seen_at IS NULL OR s.last_seen_at<now()-make_interval(secs=>$2) FROM browser_sessions s JOIN users u ON u.id=s.user_id JOIN effective_platform_roles p ON p.user_id=u.id WHERE s.token_hash=$1 AND s.verified_email IS NOT NULL AND s.revoked_at IS NULL AND s.expires_at>now() AND (s.idle_expires_at IS NULL OR s.idle_expires_at>now()) AND u.disabled_at IS NULL AND u.cleaned_at IS NULL")
        .bind(&token_hash).bind(f64::from(LAST_SEEN_EVERY_SECONDS)).fetch_optional(&state.store.pool).await.map_err(internal)?.ok_or(AuthError(StatusCode::UNAUTHORIZED))?;
    if row.5 {
        // At most one write per session per interval, not one per request.
        // Best effort: a failed write (say, a primary briefly read-only during
        // failover) must not fail an otherwise valid session.
        // Activity also moves the idle deadline, by the policy in force now;
        // an already idle-expired session is never extended (#80).
        if let Err(error) = sqlx::query("UPDATE browser_sessions SET last_seen_at=now(),idle_expires_at=now()+make_interval(secs=>$3::float8) WHERE token_hash=$1 AND revoked_at IS NULL AND expires_at>now() AND (idle_expires_at IS NULL OR idle_expires_at>now()) AND (last_seen_at IS NULL OR last_seen_at<now()-make_interval(secs=>$2))")
            .bind(&token_hash).bind(f64::from(LAST_SEEN_EVERY_SECONDS)).bind(state.policy.idle_seconds()).execute(&state.store.pool).await
        {
            tracing::warn!(%error, "could not record session activity");
        }
    }
    Ok(VerifiedSession {
        principal: BrowserPrincipal {
            user_id: row.0,
            email: row.1,
            platform_admin: row.2 == "admin",
            platform_auditor: matches!(row.2.as_str(), "admin" | "auditor"),
        },
        token_hash,
        csrf_hash: row.3,
        id: row.4,
    })
}
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(first)
}
fn verify_csrf(
    state: &IdentityState,
    headers: &HeaderMap,
    expected: &[u8],
) -> Result<(), AuthError> {
    let forbidden = || AuthError(StatusCode::FORBIDDEN);
    let origin = state
        .provider
        .as_ref()
        .map(|provider| provider.config.public_origin.as_str())
        .ok_or_else(forbidden)?;
    if single_header(headers, "origin") != Some(origin) {
        return Err(forbidden());
    }
    let token = single_header(headers, "x-csrf-token")
        .filter(|token| is_token(token))
        .ok_or_else(forbidden)?;
    if !bool::from(hash(token).as_slice().ct_eq(expected)) {
        return Err(forbidden());
    }
    Ok(())
}

/// A `Bearer` credential's token, from exactly one `Authorization` header
/// (the caller has already refused more than one). Strict: case-insensitive
/// `Bearer` scheme only, a non-empty token with no internal whitespace --
/// the same shape `http.rs`'s own `credential()` enforces for inference
/// keys, with no `x-api-key` fallback (management never accepts that header).
fn bearer_credential(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.bytes().any(|b| b.is_ascii_whitespace())
    {
        return None;
    }
    Some(token)
}

/// An inference key's exact shape (`omg_<32-hex>.<64-hex>`, 101 characters)
/// with the admin-token prefix excluded. Checked before any database access
/// so a well-formed inference key presented to a management route is
/// refused outright rather than merely failing the admin-token parser by
/// coincidence (the approved spec calls this out explicitly).
fn looks_like_inference_key(token: &str) -> bool {
    token.len() == 101 && token.starts_with("omg_") && !token.starts_with("omg_admin_")
}

/// Whether a (possibly malformed) `omg_session` cookie is present at all,
/// regardless of its value's validity: used only to refuse a request that
/// also carries an `Authorization` header (#37 PR2) -- "even an invalid
/// cookie" means this check must not itself validate the cookie's shape.
/// An undecodable `Cookie` header line is treated as present (fails closed):
/// this function only ever makes credential selection *stricter*.
fn raw_cookie_present(headers: &HeaderMap, name: &str) -> bool {
    for line in headers.get_all(header::COOKIE) {
        let Ok(line) = line.to_str() else {
            return true;
        };
        for part in line.split(';') {
            let key = part.split_once('=').map_or(part, |(k, _)| k);
            if key.trim() == name {
                return true;
            }
        }
    }
    false
}

/// Apply to every management route (not inference). Selects exactly one
/// credential family -- an admin API token (`Authorization: Bearer`) or a
/// browser session (`omg_session` cookie) -- never both, with no fallback
/// in either direction (#37 PR2): a duplicate `Authorization` header, or any
/// request carrying both an `Authorization` header and an `omg_session`
/// cookie (even an invalid one), is refused before either credential is
/// examined.
pub async fn authenticate_management(
    State(state): State<IdentityState>,
    PeerIp(peer): PeerIp,
    mut request: Request,
    next: Next,
) -> Response {
    let headers = request.headers().clone();
    let authorization_count = headers.get_all(header::AUTHORIZATION).iter().count();
    if authorization_count > 0 {
        if authorization_count > 1 || raw_cookie_present(&headers, SESSION) {
            return AuthError(StatusCode::UNAUTHORIZED).into_response();
        }
        return authenticate_admin_token_request(&state, peer, &headers, request, next).await;
    }
    let session = match verify_session(&state, &headers).await {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    if request.method() != Method::GET
        && request.method() != Method::HEAD
        && let Err(error) = verify_csrf(&state, &headers, &session.csrf_hash)
    {
        return error.into_response();
    }
    request
        .extensions_mut()
        .insert(ManagementPrincipal::Browser(session.principal.clone()));
    request.extensions_mut().insert(session.principal);
    request.extensions_mut().insert(CurrentSession(session.id));
    private(next.run(request).await)
}

/// The admin-token half of [`authenticate_management`]: a pre-authentication
/// failure (malformed credential, inference-key shape, or an unknown/
/// expired/revoked/wrong-owner token -- `authenticate_admin_token` itself
/// re-reads the primary database live, no cache) is rate-limited and
/// aggregated only, never individually audited (coordinator decision: there
/// is no token id yet to attribute it to, and auditing unauthenticated noise
/// would itself be a flood vector). Only after the token authenticates does
/// an attempt exist to audit, which `management::authorization` does for
/// every request it admits or refuses from here on.
async fn authenticate_admin_token_request(
    state: &IdentityState,
    peer: Option<IpAddr>,
    headers: &HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    let pre_auth_fail = |reason: &'static str| {
        crate::metrics::METRICS.auth_throttled("admin_token", reason);
        AuthError(StatusCode::UNAUTHORIZED).into_response()
    };
    // Free, format-only rejections (not even shaped like `omg_admin_...`)
    // never consume the rate limiter below: there is nothing here for a
    // flood of these to protect (no database access, no admin-token
    // parsing), and reserving the limiter's budget for credentials that at
    // least look like an admin token keeps a mixed flood (garbage plus
    // real-looking attempts) from starving genuine admin-token traffic.
    let Some(token) = bearer_credential(headers) else {
        return pre_auth_fail("malformed_credential");
    };
    if looks_like_inference_key(token) {
        return pre_auth_fail("inference_key_shape");
    }
    let client = throttle::client_ip(peer, headers, &state.trusted_proxies);
    if let Err(wait) = state
        .admin_token_limiter
        .check(client, std::time::Instant::now())
    {
        return throttled(
            "admin_token",
            "client_rate",
            StatusCode::TOO_MANY_REQUESTS,
            wait,
        );
    }
    match state.store.authenticate_admin_token(token).await {
        Ok(Some(principal)) => {
            let context = AdminTokenContext {
                token_id: principal.token_id,
                owner_user_id: principal.owner_user_id,
                scopes: principal.scopes,
                ip_allowlist: principal.ip_allowlist,
                client_address: client,
            };
            request
                .extensions_mut()
                .insert(ManagementPrincipal::AdminToken(context));
            // Exempt from CSRF: the credential is a Bearer secret a
            // cross-site form or script can never already hold, unlike a
            // cookie a browser attaches automatically (#37 PR2 coordinator
            // decision). Browser-session mutations keep exact Origin and
            // CSRF-token validation above, unchanged.
            private(next.run(request).await)
        }
        Ok(None) => pre_auth_fail("invalid_credential"),
        Err(error) => {
            tracing::error!(%error, "admin token authentication database lookup failed");
            AuthError(StatusCode::INTERNAL_SERVER_ERROR).into_response()
        }
    }
}

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| {
            h.split(',')
                .any(|part| part.trim().split(';').next() == Some("text/html"))
        })
}

// Only called after nonce-bound signature verification and a live access denial.
// Do not turn malformed/unverified callbacks into a cross-site logout primitive.
async fn denied_browser_callback(
    state: &IdentityState,
    headers: &HeaderMap,
    secure: bool,
) -> Result<Response, AuthError> {
    if let Some(token) = cookie(headers, SESSION)? {
        sqlx::query(
            "UPDATE browser_sessions SET revoked_at=coalesce(revoked_at,now()) WHERE token_hash=$1",
        )
        .bind(hash(&token))
        .execute(&state.store.pool)
        .await
        .map_err(internal)?;
    }
    let mut response = private(Redirect::to("/?auth_error=access_denied").into_response());
    set_cookie(&mut response, SESSION, "", 0, secure, true);
    set_cookie(&mut response, CSRF, "", 0, secure, false);
    set_cookie(&mut response, BROWSER, "", 0, secure, true);
    Ok(response)
}

async fn logout(
    State(state): State<IdentityState>,
    headers: HeaderMap,
) -> Result<Response, AuthError> {
    let session = verify_session(&state, &headers).await?;
    verify_csrf(&state, &headers, &session.csrf_hash)?;
    sqlx::query(
        "UPDATE browser_sessions SET revoked_at=now() WHERE token_hash=$1 AND revoked_at IS NULL",
    )
    .bind(session.token_hash)
    .execute(&state.store.pool)
    .await
    .map_err(internal)?;
    let secure = state
        .provider
        .as_ref()
        .is_none_or(|provider| provider.config.secure_cookies);
    let mut response = private(StatusCode::NO_CONTENT.into_response());
    set_cookie(&mut response, SESSION, "", 0, secure, true);
    set_cookie(&mut response, CSRF, "", 0, secure, false);
    set_cookie(&mut response, BROWSER, "", 0, secure, true);
    Ok(response)
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

#[cfg(test)]
#[path = "identity/tests.rs"]
pub(crate) mod tests;
