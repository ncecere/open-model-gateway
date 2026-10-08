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
    AdditionalClaims, AuthorizationCode, Client, ClientId, ClientSecret, CsrfToken,
    EmptyExtraTokenFields, EndpointMaybeSet, EndpointNotSet, EndpointSet, HttpRequest,
    HttpResponse, IdTokenFields, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, StandardErrorResponse, StandardTokenResponse, TokenResponse,
    core::{
        CoreAuthDisplay, CoreAuthPrompt, CoreAuthenticationFlow, CoreErrorResponseType,
        CoreGenderClaim, CoreJsonWebKey, CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreRevocableToken,
        CoreRevocationErrorResponse, CoreTokenIntrospectionResponse, CoreTokenType,
    },
};
use rand::{RngCore, rngs::OsRng};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{lifecycle, store::Store};

const SESSION: &str = "omg_session";
const CSRF: &str = "omg_csrf";
const BROWSER: &str = "omg_oidc";
const SESSION_SECONDS: u32 = 12 * 60 * 60;
const LOGIN_SECONDS: u32 = 10 * 60;

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
        validate_url(&request.uri().to_string(), self.allow_loopback_http).map_err(|_| fail())?;
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
}

struct Provider {
    config: IdentityConfig,
    client: DiscoveredClient,
    http: OidcHttp,
}

#[derive(Clone)]
pub struct IdentityState {
    store: Store,
    provider: Option<Arc<Provider>>,
}

impl IdentityState {
    /// Discovery/issuer/JWKS validation is fail-closed: configured OIDC failure prevents startup.
    pub async fn new(store: Store, config: Option<IdentityConfig>) -> anyhow::Result<Self> {
        let provider = if let Some(config) = config {
            let http = OidcHttp {
                client: reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(Duration::from_secs(15))
                    .connect_timeout(Duration::from_secs(5))
                    .build()
                    .map_err(|_| anyhow::anyhow!("Cannot initialize OIDC HTTP client"))?,
                allow_loopback_http: config.allow_loopback_http,
            };
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
            let redirect =
                RedirectUrl::new(format!("{}/api/v1/auth/callback", config.public_origin))
                    .map_err(|_| anyhow::anyhow!("Invalid OIDC callback URL"))?;
            let client = EnterpriseClient::from_provider_metadata(
                metadata,
                ClientId::new(config.client_id.clone()),
                config.client_secret.clone().map(ClientSecret::new),
            )
            .set_redirect_uri(redirect);
            Some(Arc::new(Provider {
                config,
                client,
                http,
            }))
        } else {
            None
        };
        Ok(Self { store, provider })
    }

    /// Read-only sign-in configuration for Admin > Settings > Sign-in:
    /// public values only (never the client secret).
    pub fn sign_in_summary(&self) -> serde_json::Value {
        match &self.provider {
            Some(provider) => serde_json::json!({
                "enabled": true,
                "issuer": provider.config.issuer.as_str(),
                "client_id": provider.config.client_id,
                "client_type": if provider.config.client_secret.is_some() { "confidential" } else { "public" },
                "groups_claim": provider.config.groups_claim,
                "public_url": provider.config.public_origin,
                "callback_url": format!("{}/api/v1/auth/callback", provider.config.public_origin),
                "secure_cookies": provider.config.secure_cookies,
            }),
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

pub fn router(state: IdentityState) -> Router<Store> {
    Router::new()
        .route("/api/v1/auth/config", get(auth_config))
        .route("/api/v1/auth/login", get(login))
        .route("/api/v1/auth/callback", get(callback))
        .route("/api/v1/auth/logout", post(logout))
        .with_state(state)
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
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
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

async fn auth_config(State(state): State<IdentityState>) -> Response {
    private(Json(serde_json::json!({ "enabled": state.provider.is_some() })).into_response())
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
    query: Result<Query<LoginQuery>, QueryRejection>,
) -> Result<Response, AuthError> {
    let provider = state
        .provider
        .as_ref()
        .ok_or(AuthError(StatusCode::SERVICE_UNAVAILABLE))?;
    // An unsafe or malformed return path is dropped (sign-in still works and lands on Home).
    let return_to = query
        .ok()
        .and_then(|Query(q)| q.return_to)
        .as_deref()
        .and_then(safe_return_path)
        .filter(|p| p != "/");
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, oauth_state, nonce) = provider
        .client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            || CsrfToken::new(random_token()),
            || Nonce::new(random_token()),
        )
        .add_scope(Scope::new("email".into()))
        // `profile` carries the optional display name; it is presentation only.
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(challenge)
        .url();
    let browser = random_token();
    sqlx::query("INSERT INTO oidc_login_attempts(state_hash,browser_hash,nonce,pkce_verifier,expires_at,return_to) VALUES($1,$2,$3,$4,now()+interval '10 minutes',$5)")
        .bind(hash(oauth_state.secret())).bind(hash(&browser)).bind(nonce.secret()).bind(verifier.secret()).bind(return_to)
        .execute(&state.store.pool).await.map_err(internal)?;
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
    let mut tx = store.pool.begin().await.map_err(internal)?;
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
    headers: HeaderMap,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Result<Response, AuthError> {
    let provider = state
        .provider
        .as_ref()
        .ok_or(AuthError(StatusCode::SERVICE_UNAVAILABLE))?;
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
    // Library verifies signature, issuer, audience, expiration and the original nonce.
    let claims = id_token
        .claims(&provider.client.id_token_verifier(), &Nonce::new(nonce))
        .map_err(|_| invalid())?;
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
    let user_id = match resolve_identity(
        &state.store,
        provider.config.issuer.as_str(),
        subject,
        &email,
        &groups,
    )
    .await
    {
        Ok(id) => id,
        Err(AuthError(status)) if status == StatusCode::FORBIDDEN && wants_html(&headers) => {
            return denied_browser_callback(&state, &headers, provider.config.secure_cookies).await;
        }
        Err(error) => return Err(error),
    };
    // Presentation only: the verified token's display name (or none) replaces the stored one.
    let display_name = claims
        .name()
        .and_then(|name| name.get(None))
        .and_then(|name| display_name(name.as_str()));
    sqlx::query("UPDATE users SET display_name=$2 WHERE id=$1 AND cleaned_at IS NULL AND display_name IS DISTINCT FROM $2")
        .bind(user_id)
        .bind(display_name)
        .execute(&state.store.pool)
        .await
        .map_err(internal)?;
    // Upstream access/refresh tokens are never persisted or forwarded.
    drop(tokens);
    let session = random_token();
    let csrf = random_token();
    let inserted = sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) SELECT $1,id,$3,now()+interval '12 hours',$4 FROM users WHERE id=$2 AND disabled_at IS NULL AND cleaned_at IS NULL AND EXISTS(SELECT 1 FROM effective_platform_roles p WHERE p.user_id=users.id)")
        .bind(hash(&session)).bind(user_id).bind(hash(&csrf)).bind(&email).execute(&state.store.pool).await.map_err(internal)?;
    if inserted.rows_affected() != 1 {
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
        SESSION_SECONDS,
        provider.config.secure_cookies,
        true,
    );
    set_cookie(
        &mut response,
        CSRF,
        &csrf,
        SESSION_SECONDS,
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

async fn resolve_identity(
    store: &Store,
    issuer: &str,
    subject: &str,
    email: &str,
    groups: &[String],
) -> Result<Uuid, AuthError> {
    let email = email.to_lowercase();
    let mut tx = store.pool.begin().await.map_err(internal)?;
    lifecycle::lock(&mut tx).await.map_err(internal)?;
    advisory_lock(&mut tx, &format!("oidc:{}:{issuer}{subject}", issuer.len())).await?;
    let existing = sqlx::query_as::<_, (Uuid, bool, bool, Option<String>)>(
        "SELECT u.id,u.cleaned_at IS NOT NULL,coalesce(u.cleanup_due_at<=now(),false),u.disable_reason FROM oidc_identities i JOIN users u ON u.id=i.user_id WHERE i.issuer=$1 AND i.subject=$2 FOR UPDATE OF u",
    ).bind(issuer).bind(subject).fetch_optional(&mut *tx).await.map_err(internal)?;
    let mut rebind = false;
    let existing_id = if let Some((id, cleaned, expired, reason)) = existing {
        if cleaned || expired {
            if !cleaned {
                lifecycle::cleanup_user(&mut tx, id)
                    .await
                    .map_err(internal)?;
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
        .bind(Uuid::new_v4())
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
    let active = lifecycle::synchronize_groups(&mut tx, id, issuer, groups)
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
}
async fn verify_session(
    state: &IdentityState,
    headers: &HeaderMap,
) -> Result<VerifiedSession, AuthError> {
    let token = cookie(headers, SESSION)
        .map_err(|_| AuthError(StatusCode::UNAUTHORIZED))?
        .ok_or(AuthError(StatusCode::UNAUTHORIZED))?;
    let token_hash = hash(&token);
    let row = sqlx::query_as::<_, (Uuid, String, String, Vec<u8>)>("SELECT u.id,s.verified_email,p.role,s.csrf_hash FROM browser_sessions s JOIN users u ON u.id=s.user_id JOIN effective_platform_roles p ON p.user_id=u.id WHERE s.token_hash=$1 AND s.verified_email IS NOT NULL AND s.revoked_at IS NULL AND s.expires_at>now() AND u.disabled_at IS NULL AND u.cleaned_at IS NULL")
        .bind(&token_hash).fetch_optional(&state.store.pool).await.map_err(internal)?.ok_or(AuthError(StatusCode::UNAUTHORIZED))?;
    Ok(VerifiedSession {
        principal: BrowserPrincipal {
            user_id: row.0,
            email: row.1,
            platform_admin: row.2 == "admin",
            platform_auditor: matches!(row.2.as_str(), "admin" | "auditor"),
        },
        token_hash,
        csrf_hash: row.3,
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

/// Apply to every management route (not inference). Never accepts an Authorization bearer key.
pub async fn require_session(
    State(state): State<IdentityState>,
    mut request: Request,
    next: Next,
) -> Response {
    let session = match verify_session(&state, request.headers()).await {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    if request.method() != Method::GET
        && request.method() != Method::HEAD
        && let Err(error) = verify_csrf(&state, request.headers(), &session.csrf_hash)
    {
        return error.into_response();
    }
    request.extensions_mut().insert(session.principal);
    private(next.run(request).await)
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

#[cfg(test)]
#[path = "identity/tests.rs"]
mod tests;
