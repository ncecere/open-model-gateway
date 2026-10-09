use super::*;
use axum::{body::Body, extract::Form};
use openidconnect::{
    IdToken, IdTokenClaims, JsonWebKeyId, PrivateSigningKey,
    core::{CoreEdDsaPrivateSigningKey, CoreJwsSigningAlgorithm},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tower::ServiceExt;

// Public upstream test-only key; NEVER used outside this mock provider.
const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEICWeYPLxoZKHZlQ6rkBi11E9JwchynXtljATLqym/XS9\n-----END PRIVATE KEY-----";
fn signing_key() -> CoreEdDsaPrivateSigningKey {
    CoreEdDsaPrivateSigningKey::from_ed25519_pem(TEST_KEY, None).unwrap()
}

#[cfg_attr(not(feature = "integration-tests"), allow(dead_code))]
struct MockProvider {
    issuer: String,
    claims: Arc<Mutex<Value>>,
    exchanges: Arc<AtomicUsize>,
    verifier: Arc<Mutex<Option<String>>>,
    /// Published key set, request count, Cache-Control header and outage switch.
    jwks: Arc<Mutex<Value>>,
    jwks_hits: Arc<AtomicUsize>,
    jwks_cache_control: Arc<Mutex<Option<String>>>,
    jwks_fail: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for MockProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl MockProvider {
    async fn start(change: Option<(&str, Value)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let mut metadata = json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["EdDSA"],
            "code_challenge_methods_supported": ["S256"]
        });
        if let Some((key, value)) = change {
            metadata[key] = value;
        }
        let jwks = Arc::new(Mutex::new(
            json!({"keys": [signing_key().as_verification_key()]}),
        ));
        let jwks_hits = Arc::new(AtomicUsize::new(0));
        let jwks_cache_control = Arc::new(Mutex::new(None::<String>));
        let jwks_fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let jwks_state = (
            jwks.clone(),
            jwks_hits.clone(),
            jwks_cache_control.clone(),
            jwks_fail.clone(),
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = Arc::new(Mutex::new(json!({
            "iss": issuer, "sub": "subject-one", "aud": "test-client", "exp": now+300,
            "iat": now, "nonce": "not-yet-set", "email": "NewUser@Example.test", "email_verified": true,
            "groups": ["entitled"]
        })));
        let exchanges = Arc::new(AtomicUsize::new(0));
        let verifier = Arc::new(Mutex::new(None));
        let state = (claims.clone(), exchanges.clone(), verifier.clone());
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(move || { let metadata = metadata.clone(); async move { Json(metadata) } }))
            .route("/jwks", get(move || {
                let (jwks, hits, cache_control, fail) = jwks_state.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    if fail.load(Ordering::SeqCst) {
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    let mut headers = HeaderMap::new();
                    if let Some(value) = cache_control.lock().unwrap().clone() {
                        headers.insert(header::CACHE_CONTROL, value.parse().unwrap());
                    }
                    let body = jwks.lock().unwrap().clone();
                    (headers, Json(body)).into_response()
                }
            }))
            .route("/token", post(move |Form(form): Form<HashMap<String,String>>| {
                let (claims, exchanges, verifier) = state.clone();
                async move {
                    exchanges.fetch_add(1, Ordering::SeqCst);
                    *verifier.lock().unwrap() = form.get("code_verifier").cloned();
                    if form.get("code").map(String::as_str) != Some("good") {
                        return (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid_grant"})));
                    }
                    let values = claims.lock().unwrap().clone();
                    let corrupt_signature = values["test_bad_signature"] == json!(true);
                    let claims: IdTokenClaims<SignedClaims, CoreGenderClaim> = serde_json::from_value(values).unwrap();
                    let id_token: IdToken<SignedClaims, CoreGenderClaim, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm> = IdToken::new(claims, &signing_key(), CoreJwsSigningAlgorithm::EdDsa, None, None).unwrap();
                    let mut encoded = id_token.to_string();
                    if corrupt_signature {
                        let index = encoded.rfind('.').unwrap() + 1;
                        let replacement = if encoded.as_bytes()[index] == b'A' { "B" } else { "A" };
                        encoded.replace_range(index..index+1, replacement);
                    }
                    (StatusCode::OK, Json(json!({"access_token":"discard-me","refresh_token":"discard-me-too","token_type":"Bearer","id_token":encoded})))
                }
            }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            issuer,
            claims,
            exchanges,
            verifier,
            jwks,
            jwks_hits,
            jwks_cache_control,
            jwks_fail,
            task,
        }
    }
    fn config(&self) -> IdentityConfig {
        IdentityConfig::parse(
            "http://127.0.0.1:3000".into(),
            self.issuer.clone(),
            "test-client".into(),
            None,
            true,
        )
        .unwrap()
    }
    async fn state(&self, pool: sqlx::PgPool) -> IdentityState {
        IdentityState::new(Store::new(pool), Some(self.config()))
            .await
            .unwrap()
    }
}
fn lazy_pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
        .unwrap()
}
fn cookie_header(name: &str, token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, format!("{name}={token}").parse().unwrap());
    headers
}
#[cfg(feature = "integration-tests")]
fn response_cookie(response: &Response, name: &str) -> String {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|header| {
            let (key, value) = header
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
        .unwrap()
}

#[test]
fn transport_and_origin_configuration_is_strict() {
    for bad in [
        "http://example.com",
        "https://user:password@example.com",
        "https://example.com/#fragment",
        "https://example.com/path",
        "https://example.com/?q=1",
        "https://EXAMPLE.com",
        "https://example.com:443",
    ] {
        assert!(
            IdentityConfig::validate_origin(bad, false).is_err(),
            "{bad}"
        );
    }
    assert!(IdentityConfig::validate_origin("https://example.com/", false).is_ok());
    for local in [
        "http://localhost:3000",
        "http://127.0.0.1:3000",
        "http://[::1]:3000",
    ] {
        assert!(IdentityConfig::validate_origin(local, true).is_ok());
        assert!(IdentityConfig::validate_origin(local, false).is_err());
    }
    for remote in [
        "http://localhost.evil.test",
        "http://10.0.0.1",
        "http://0.0.0.0",
    ] {
        assert!(validate_url(remote, true).is_err());
    }
}

#[test]
fn duplicate_and_malformed_cookies_are_rejected() {
    let token = random_token();
    assert!(is_token(&token));
    assert_eq!(hash(&token).len(), 32);
    let mut headers = cookie_header(SESSION, &token);
    assert_eq!(cookie(&headers, SESSION).unwrap(), Some(token.clone()));
    headers.append(
        header::COOKIE,
        format!("{SESSION}={token}").parse().unwrap(),
    );
    assert!(cookie(&headers, SESSION).is_err());
    for value in [
        format!("{SESSION}={token}; {SESSION}={token}"),
        format!("{SESSION}=\"{token}\""),
        format!("{SESSION}={token}; {SESSION}"),
        format!("{SESSION}=garbage"),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, value.parse().unwrap());
        assert!(cookie(&headers, SESSION).is_err());
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer inference-key".parse().unwrap(),
    );
    assert!(cookie(&headers, SESSION).unwrap().is_none());
}

#[test]
fn cookie_attributes_are_host_only_and_explicit() {
    for secure in [true, false] {
        let mut response = StatusCode::OK.into_response();
        set_cookie(
            &mut response,
            SESSION,
            &random_token(),
            SESSION_SECONDS,
            secure,
            true,
        );
        set_cookie(
            &mut response,
            CSRF,
            &random_token(),
            SESSION_SECONDS,
            secure,
            false,
        );
        set_cookie(
            &mut response,
            BROWSER,
            &random_token(),
            LOGIN_SECONDS,
            secure,
            true,
        );
        for header in response.headers().get_all(header::SET_COOKIE) {
            let cookie = header.to_str().unwrap();
            assert!(cookie.contains("; Path=/;"));
            assert!(!cookie.contains("Domain="));
            assert_eq!(cookie.contains("; Secure"), secure);
            if cookie.starts_with("omg_csrf=") {
                assert!(!cookie.contains("HttpOnly"));
                assert!(cookie.contains("SameSite=Strict"));
            } else {
                assert!(cookie.contains("HttpOnly"));
                assert!(cookie.contains("SameSite=Lax"));
            }
        }
    }
}

#[tokio::test]
async fn discovery_and_csrf_origin_validation() {
    let mock = MockProvider::start(None).await;
    let state = mock.state(lazy_pool()).await;
    let token = random_token();
    let mut headers = HeaderMap::new();
    headers.insert("origin", "http://127.0.0.1:3000".parse().unwrap());
    headers.insert("x-csrf-token", token.parse().unwrap());
    assert!(verify_csrf(&state, &headers, &hash(&token)).is_ok());
    headers.append("origin", "http://127.0.0.1:3000".parse().unwrap());
    assert!(verify_csrf(&state, &headers, &hash(&token)).is_err());
    for origin in [
        "null",
        "http://127.0.0.1:3000/",
        "http://127.0.0.1:3001",
        "http://evil.test",
        "http://127.0.0.1:3000 http://evil.test",
    ] {
        headers.insert("origin", origin.parse().unwrap());
        assert_eq!(
            verify_csrf(&state, &headers, &hash(&token)).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
    }
    headers.insert("origin", "http://127.0.0.1:3000".parse().unwrap());
    assert!(verify_csrf(&state, &headers, &hash(&random_token())).is_err());
    headers.append("x-csrf-token", token.parse().unwrap());
    assert!(verify_csrf(&state, &headers, &hash(&token)).is_err());
    headers.remove("x-csrf-token");
    assert!(verify_csrf(&state, &headers, &hash(&token)).is_err());
}

#[tokio::test]
async fn discovery_rejects_issuer_mismatch_and_unsafe_endpoints() {
    for change in [
        ("issuer", json!("https://wrong.example.test")),
        ("jwks_uri", json!("http://example.test/jwks")),
        ("token_endpoint", json!("http://example.test/token")),
        ("authorization_endpoint", json!("http://example.test/auth")),
    ] {
        let mock = MockProvider::start(Some(change)).await;
        assert!(
            IdentityState::new(Store::new(lazy_pool()), Some(mock.config()))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn disabled_login_and_bearer_rejection_do_not_need_database() {
    let state = IdentityState::new(Store::new(lazy_pool()), None)
        .await
        .unwrap();
    let app = router(state.clone()).with_state(state.store.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer inference-key".parse().unwrap(),
    );
    assert!(matches!(
        verify_session(&state, &headers).await,
        Err(AuthError(StatusCode::UNAUTHORIZED))
    ));
}

/// Ed25519 PKCS#8 keys built from fixed test seeds (public test material only).
fn rotation_key(der_base64: &str, kid: &str) -> CoreEdDsaPrivateSigningKey {
    let pem = format!("-----BEGIN PRIVATE KEY-----\n{der_base64}\n-----END PRIVATE KEY-----");
    CoreEdDsaPrivateSigningKey::from_ed25519_pem(&pem, Some(JsonWebKeyId::new(kid.into()))).unwrap()
}
const KEY_B: &str = "MC4CAQAwBQYDK2VwBCIEIIUFxA5MJvcFlRFzvVZvw0RpOP86u8UZywrRtvXcdAD7";
const KEY_C: &str = "MC4CAQAwBQYDK2VwBCIEIK1PB7sZp+K3ReVinF8TAbYnSpyirzgr6lzAzk/Jy295";

fn token_claims(issuer: &str) -> IdTokenClaims<SignedClaims, CoreGenderClaim> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    serde_json::from_value(json!({
        "iss": issuer, "sub": "rotation", "aud": "test-client", "exp": now + 300, "iat": now,
        "nonce": "jwks-nonce", "email": "rotation@example.test", "email_verified": true, "groups": []
    }))
    .unwrap()
}

fn mint(key: &CoreEdDsaPrivateSigningKey, issuer: &str) -> EnterpriseIdToken {
    IdToken::new(
        token_claims(issuer),
        key,
        CoreJwsSigningAlgorithm::EdDsa,
        None,
        None,
    )
    .unwrap()
}

#[derive(Clone)]
struct TestClock {
    base: std::time::Instant,
    offset: Arc<std::sync::atomic::AtomicU64>,
}
impl TestClock {
    fn new() -> Self {
        Self {
            base: std::time::Instant::now(),
            offset: Arc::default(),
        }
    }
    fn advance(&self, seconds: u64) {
        self.offset.fetch_add(seconds, Ordering::SeqCst);
    }
    fn clock(&self) -> jwks::Clock {
        let this = self.clone();
        Arc::new(move || this.base + Duration::from_secs(this.offset.load(Ordering::SeqCst)))
    }
}

#[tokio::test]
async fn jwks_rotation_refetch_is_rate_limited_single_flight_and_survives_outages() {
    let mock = MockProvider::start(None).await;
    let (a, b, c) = (
        rotation_key(
            "MC4CAQAwBQYDK2VwBCIEICWeYPLxoZKHZlQ6rkBi11E9JwchynXtljATLqym/XS9",
            "kid-a",
        ),
        rotation_key(KEY_B, "kid-b"),
        rotation_key(KEY_C, "kid-c"),
    );
    *mock.jwks.lock().unwrap() = json!({"keys": [a.as_verification_key()]});
    *mock.jwks_cache_control.lock().unwrap() = Some("public, max-age=600".into());
    let clock = TestClock::new();
    let state = IdentityState::build(
        Store::new(lazy_pool()),
        Some(mock.config()),
        JwksPolicy::default(),
        clock.clock(),
    )
    .await
    .unwrap();
    let provider = state.provider.clone().unwrap();
    let nonce = Nonce::new("jwks-nonce".into());
    let hits = || mock.jwks_hits.load(Ordering::SeqCst);
    let start = hits();
    let (token_a, token_b, token_c) = (
        mint(&a, &mock.issuer),
        mint(&b, &mock.issuer),
        mint(&c, &mock.issuer),
    );
    assert!(verify_id_token(&provider, &token_a, &nonce).await.is_ok());
    assert_eq!(hits(), start, "cached keys are reused");
    let summary = state.sign_in_summary();
    assert_eq!(summary["jwks"]["keys"], 1);
    assert_eq!(summary["jwks"]["state"], "fresh");
    assert!(summary["jwks"]["refreshed_at"].is_string());

    // Rotation: kid-b is published; an unknown kid refetches at most once a minute.
    *mock.jwks.lock().unwrap() =
        json!({"keys": [a.as_verification_key(), b.as_verification_key()]});
    assert_eq!(
        verify_id_token(&provider, &token_b, &nonce)
            .await
            .unwrap_err()
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(hits(), start, "rate limited right after startup");
    clock.advance(60);
    assert!(verify_id_token(&provider, &token_b, &nonce).await.is_ok());
    assert!(verify_id_token(&provider, &token_a, &nonce).await.is_ok());
    assert_eq!(hits(), start + 1);

    // A burst of unknown key ids triggers one fetch, then nothing until the interval passes.
    clock.advance(60);
    let burst = futures_util::future::join_all(
        (0..16).map(|_| verify_id_token(&provider, &token_c, &nonce)),
    )
    .await;
    assert!(burst.iter().all(Result::is_err));
    assert_eq!(hits(), start + 2, "single flight");
    for _ in 0..5 {
        assert!(verify_id_token(&provider, &token_c, &nonce).await.is_err());
    }
    assert_eq!(hits(), start + 2);

    // max-age=600 is honoured: keys refresh once after expiry.
    clock.advance(600);
    assert!(verify_id_token(&provider, &token_a, &nonce).await.is_ok());
    assert_eq!(hits(), start + 3);

    // Outage: last good keys stay usable within the grace period, then sign-in fails closed.
    mock.jwks_fail.store(true, Ordering::SeqCst);
    clock.advance(600);
    assert!(verify_id_token(&provider, &token_b, &nonce).await.is_ok());
    assert_eq!(hits(), start + 4);
    let summary = state.sign_in_summary();
    assert_eq!(summary["jwks"]["state"], "stale");
    assert!(summary["jwks"]["last_failure_at"].is_string());
    assert!(verify_id_token(&provider, &token_a, &nonce).await.is_ok());
    assert_eq!(hits(), start + 4, "failure backoff");
    clock.advance(6 * 60 * 60);
    assert_eq!(
        verify_id_token(&provider, &token_a, &nonce)
            .await
            .unwrap_err()
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(state.sign_in_summary()["jwks"]["state"], "unavailable");
    mock.jwks_fail.store(false, Ordering::SeqCst);
    clock.advance(30);
    assert!(verify_id_token(&provider, &token_a, &nonce).await.is_ok());
    assert_eq!(state.sign_in_summary()["jwks"]["state"], "fresh");
    assert_eq!(state.sign_in_summary()["jwks"]["keys"], 2);
}

#[tokio::test]
async fn id_tokens_never_accept_none_or_shared_secret_algorithms() {
    for algs in [json!(["none"]), json!(["HS256"]), json!(["HS256", "none"])] {
        let mock = MockProvider::start(Some(("id_token_signing_alg_values_supported", algs))).await;
        assert!(
            IdentityState::new(Store::new(lazy_pool()), Some(mock.config()))
                .await
                .is_err()
        );
    }
    // Advertised HS256/none are ignored; a confidential client secret is never an ID-token key.
    let mock = MockProvider::start(Some((
        "id_token_signing_alg_values_supported",
        json!(["HS256", "none", "EdDSA"]),
    )))
    .await;
    let config = IdentityConfig::parse(
        "http://127.0.0.1:3000".into(),
        mock.issuer.clone(),
        "test-client".into(),
        Some("test-client-secret".into()),
        true,
    )
    .unwrap();
    let state = IdentityState::new(Store::new(lazy_pool()), Some(config))
        .await
        .unwrap();
    let provider = state.provider.clone().unwrap();
    assert_eq!(provider.algs, [CoreJwsSigningAlgorithm::EdDsa]);
    let nonce = Nonce::new("jwks-nonce".into());
    let hmac: EnterpriseIdToken = IdToken::new(
        token_claims(&mock.issuer),
        &openidconnect::core::CoreHmacKey::new("test-client-secret"),
        CoreJwsSigningAlgorithm::HmacSha256,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        verify_id_token(&provider, &hmac, &nonce)
            .await
            .unwrap_err()
            .0,
        StatusCode::BAD_REQUEST
    );
    // Same claims, `alg: none` and an empty signature.
    let payload = hmac.to_string().split('.').nth(1).unwrap().to_owned();
    let unsigned = format!("eyJhbGciOiJub25lIn0.{payload}.");
    if let Ok(token) = unsigned.parse::<EnterpriseIdToken>() {
        assert!(verify_id_token(&provider, &token, &nonce).await.is_err());
    }
    // The legitimate key still works.
    assert!(
        verify_id_token(
            &provider,
            &IdToken::new(
                token_claims(&mock.issuer),
                &signing_key(),
                CoreJwsSigningAlgorithm::EdDsa,
                None,
                None,
            )
            .unwrap(),
            &nonce
        )
        .await
        .is_ok()
    );
}

#[test]
fn generic_group_claim_is_strict_and_supports_custom_paths() {
    let claims: SignedClaims = serde_json::from_value(
        json!({"roles":{"platform":["one","two"]},"https://claims.test/groups":["uri"]}),
    )
    .unwrap();
    assert_eq!(
        claims.groups("roles.platform"),
        Some(vec!["one".into(), "two".into()])
    );
    assert_eq!(
        claims.groups("https://claims.test/groups"),
        Some(vec!["uri".into()])
    );
    assert_eq!(claims.groups("missing"), None);
    for value in [
        Value::Null,
        json!("admin"),
        json!(["good", 12]),
        json!({"value":["admin"]}),
        json!([""]),
    ] {
        let claims: SignedClaims = serde_json::from_value(json!({"groups":value})).unwrap();
        assert_eq!(claims.groups("groups"), None);
    }
    let claims: SignedClaims = serde_json::from_value(json!({"groups":[]})).unwrap();
    assert_eq!(claims.groups("groups"), Some(vec![]));
}

#[cfg(feature = "integration-tests")]
mod database {
    use super::*;
    use axum::{Extension, middleware};

    async fn seed(pool: &sqlx::PgPool) -> (Uuid, String, String) {
        let id = Uuid::new_v4();
        let session = random_token();
        let csrf = random_token();
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(id)
            .bind(format!("{id}@example.test"))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO platform_role_grants(id,user_id,role,source) VALUES($1,$2,'user','manual')").bind(Uuid::new_v4()).bind(id).execute(pool).await.unwrap();
        // Explicit simulated verified claim for session-only tests, not an OIDC sign-in.
        sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) VALUES($1,$2,$3,now()+interval '12 hours',$4)").bind(hash(&session)).bind(id).bind(hash(&csrf)).bind(format!("{id}@example.test")).execute(pool).await.unwrap();
        (id, session, csrf)
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn expiry_revocation_and_disabled_user_are_checked_every_request(pool: sqlx::PgPool) {
        let state = IdentityState::new(Store::new(pool.clone()), None)
            .await
            .unwrap();
        let (id, session, _) = seed(&pool).await;
        let headers = cookie_header(SESSION, &session);
        assert_eq!(
            verify_session(&state, &headers)
                .await
                .unwrap()
                .principal
                .user_id,
            id
        );
        sqlx::query("UPDATE browser_sessions SET expires_at=now()-interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            verify_session(&state, &headers).await,
            Err(AuthError(StatusCode::UNAUTHORIZED))
        ));
        sqlx::query(
            "UPDATE browser_sessions SET expires_at=now()+interval '1 hour',revoked_at=now()",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(verify_session(&state, &headers).await.is_err());
        sqlx::query("UPDATE browser_sessions SET revoked_at=NULL")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(verify_session(&state, &headers).await.is_err());
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn sessions_without_verified_email_cannot_authenticate(pool: sqlx::PgPool) {
        let state = IdentityState::new(Store::new(pool.clone()), None)
            .await
            .unwrap();
        let (_, session, _) = seed(&pool).await;
        sqlx::query("UPDATE browser_sessions SET verified_email=NULL WHERE token_hash=$1")
            .bind(hash(&session))
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            verify_session(&state, &cookie_header(SESSION, &session)).await,
            Err(AuthError(StatusCode::UNAUTHORIZED))
        ));
        let app = crate::management::router(state.clone()).with_state(state.store.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .header(header::COOKIE, format!("{SESSION}={session}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn linking_is_explicit_single_use_and_authentication_does_not_infer_roles(
        pool: sqlx::PgPool,
    ) {
        let store = Store::new(pool.clone());
        let (id, _, _) = seed(&pool).await;
        let email = format!("{id}@example.test");
        assert_eq!(
            resolve_identity(&store, "https://issuer.test", "subject", &email, &[])
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        sqlx::query("UPDATE users SET oidc_link_allowed=true WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            resolve_identity(
                &store,
                "https://issuer.test",
                "subject",
                &email.to_uppercase(),
                &[]
            )
            .await
            .unwrap(),
            id
        );
        let allowed: bool = sqlx::query_scalar("SELECT oidc_link_allowed FROM users WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!allowed);
        let role: String =
            sqlx::query_scalar("SELECT role FROM effective_platform_roles WHERE user_id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(role, "user");
        assert_eq!(
            resolve_identity(
                &store,
                "https://issuer.test",
                "subject",
                "changed@example.test",
                &[]
            )
            .await
            .unwrap(),
            id
        );
        sqlx::query("UPDATE users SET oidc_link_allowed=true WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            resolve_identity(&store, "https://issuer.test", "other-subject", &email, &[])
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            resolve_identity(
                &store,
                "https://issuer.test",
                "fresh",
                "Fresh@Example.test",
                &[]
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        let fresh: Uuid =
            sqlx::query_scalar("SELECT user_id FROM oidc_identities WHERE subject='fresh'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let row: (String, bool) =
            sqlx::query_as("SELECT email,disabled_at IS NOT NULL FROM users WHERE id=$1")
                .bind(fresh)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, ("fresh@example.test".into(), true));
        let counts: (i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM platform_role_grants WHERE user_id=$1),(SELECT count(*) FROM workspaces WHERE owner_user_id=$1)").bind(fresh).fetch_one(&pool).await.unwrap();
        assert_eq!(counts, (0, 0));
        sqlx::query(
            "UPDATE users SET disabled_at=now(),disable_reason='admin_suspension' WHERE id=$1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            resolve_identity(&store, "https://issuer.test", "subject", &email, &[])
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn attempts_are_bound_expiring_and_single_use(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let oauth_state = random_token();
        let browser = random_token();
        sqlx::query("INSERT INTO oidc_login_attempts VALUES($1,$2,'nonce','verifier',now()+interval '10 minutes')").bind(hash(&oauth_state)).bind(hash(&browser)).execute(&pool).await.unwrap();
        assert!(
            consume_attempt(&store, &oauth_state, &random_token())
                .await
                .is_err()
        );
        let (a, b) = tokio::join!(
            consume_attempt(&store, &oauth_state, &browser),
            consume_attempt(&store, &oauth_state, &browser)
        );
        assert_ne!(a.is_ok(), b.is_ok());
        sqlx::query(
            "INSERT INTO oidc_login_attempts VALUES($1,$2,'nonce','verifier',now()-interval '1 second')",
        )
        .bind(hash(&oauth_state))
        .bind(hash(&browser))
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            consume_attempt(&store, &oauth_state, &browser)
                .await
                .is_err()
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn middleware_and_logout_enforce_csrf_and_exact_origin(pool: sqlx::PgPool) {
        let mock = MockProvider::start(None).await;
        let state = mock.state(pool.clone()).await;
        let (id, session, csrf) = seed(&pool).await;
        let app =
            Router::new()
                .route(
                    "/manage",
                    get(|Extension(p): Extension<BrowserPrincipal>| async move {
                        p.user_id.to_string()
                    })
                    .post(|| async { StatusCode::NO_CONTENT }),
                )
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    require_session,
                ));
        for (method, origin, token, expected) in [
            ("GET", None, None, StatusCode::OK),
            ("POST", None, None, StatusCode::FORBIDDEN),
            (
                "POST",
                Some("http://evil.test"),
                Some(csrf.as_str()),
                StatusCode::FORBIDDEN,
            ),
            (
                "POST",
                Some("http://127.0.0.1:3000"),
                Some("wrong"),
                StatusCode::FORBIDDEN,
            ),
            (
                "POST",
                Some("http://127.0.0.1:3000"),
                Some(csrf.as_str()),
                StatusCode::NO_CONTENT,
            ),
        ] {
            let mut request = Request::builder()
                .uri("/manage")
                .method(method)
                .header(header::COOKIE, format!("{SESSION}={session}"));
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            if let Some(token) = token {
                request = request.header("x-csrf-token", token);
            }
            assert_eq!(
                app.clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                expected
            );
        }
        assert_eq!(
            verify_session(&state, &cookie_header(SESSION, &session))
                .await
                .unwrap()
                .principal
                .user_id,
            id
        );
        let auth = router(state.clone()).with_state(state.store.clone());
        let response = auth
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/logout")
                    .method("POST")
                    .header(header::COOKIE, format!("{SESSION}={session}"))
                    .header("origin", "http://127.0.0.1:3000")
                    .header("x-csrf-token", csrf)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .all(|cookie| cookie.to_str().unwrap().contains("Max-Age=0"))
        );
        assert!(
            verify_session(&state, &cookie_header(SESSION, &session))
                .await
                .is_err()
        );
    }

    async fn begin_login(state: &IdentityState, mock: &MockProvider) -> (String, String, String) {
        let response = login(State(state.clone()), Ok(Query(LoginQuery::default())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let url = Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let params: HashMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(params["code_challenge_method"], "S256");
        assert!(params["scope"].split(' ').any(|scope| scope == "openid"));
        mock.claims.lock().unwrap()["nonce"] = json!(params["nonce"]);
        (
            params["state"].clone(),
            response_cookie(&response, BROWSER),
            params["code_challenge"].clone(),
        )
    }
    async fn finish_login(
        state: &IdentityState,
        oauth_state: &str,
        browser: &str,
        code: &str,
    ) -> Result<Response, AuthError> {
        callback(
            State(state.clone()),
            cookie_header(BROWSER, browser),
            Ok(Query(CallbackQuery {
                state: oauth_state.into(),
                code: Some(code.into()),
                error: None,
            })),
        )
        .await
    }

    #[test]
    fn display_names_are_trimmed_bounded_and_printable() {
        assert_eq!(display_name("  Alex Example "), Some("Alex Example".into()));
        assert_eq!(display_name("   "), None);
        assert_eq!(display_name("Alex\u{7}"), None);
        assert_eq!(display_name(&"a".repeat(200)).map(|n| n.len()), Some(200));
        assert_eq!(display_name(&"a".repeat(201)), None);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn sign_in_records_the_verified_display_name_and_cleanup_clears_it(pool: sqlx::PgPool) {
        let mock = MockProvider::start(None).await;
        let state = mock.state(pool.clone()).await;
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,'entitled','platform','user')")
            .bind(Uuid::new_v4()).bind(&mock.issuer).execute(&pool).await.unwrap();
        mock.claims.lock().unwrap()["name"] = json!("  Alex Example ");
        let (oauth_state, browser, _) = begin_login(&state, &mock).await;
        finish_login(&state, &oauth_state, &browser, "good")
            .await
            .unwrap();
        let (id, name): (Uuid, Option<String>) =
            sqlx::query_as("SELECT id,display_name FROM users")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(name.as_deref(), Some("Alex Example"));
        // A later sign-in without the claim drops the stored name rather than keeping a stale one.
        mock.claims
            .lock()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("name");
        let (oauth_state, browser, _) = begin_login(&state, &mock).await;
        finish_login(&state, &oauth_state, &browser, "good")
            .await
            .unwrap();
        let name: Option<String> = sqlx::query_scalar("SELECT display_name FROM users WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(name, None);
        sqlx::query("UPDATE users SET display_name='Alex Example' WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        crate::lifecycle::cleanup_user(&mut tx, id).await.unwrap();
        tx.commit().await.unwrap();
        let (email, name): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT email,display_name FROM users WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((email, name), (None, None));
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn complete_oidc_flow_uses_pkce_and_fresh_hashed_sessions(pool: sqlx::PgPool) {
        let mock = MockProvider::start(None).await;
        let state = mock.state(pool.clone()).await;
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,'entitled','platform','user')")
            .bind(Uuid::new_v4()).bind(&mock.issuer).execute(&pool).await.unwrap();
        let (oauth_state, browser, challenge) = begin_login(&state, &mock).await;
        let response = finish_login(&state, &oauth_state, &browser, "good")
            .await
            .unwrap();
        assert_eq!(response.headers()[header::LOCATION], "/");
        assert!(
            finish_login(&state, &oauth_state, &browser, "good")
                .await
                .is_err()
        );
        assert_eq!(mock.exchanges.load(Ordering::SeqCst), 1);
        let verifier = mock.verifier.lock().unwrap().clone().unwrap();
        assert_eq!(
            PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(verifier)).as_str(),
            challenge
        );
        let token = response_cookie(&response, SESSION);
        let csrf = response_cookie(&response, CSRF);
        assert_ne!(token, csrf);
        assert!(is_token(&token));
        assert!(is_token(&csrf));
        let row: (Vec<u8>,Vec<u8>,bool) = sqlx::query_as("SELECT token_hash,csrf_hash,expires_at > now()+interval '11 hours' AND expires_at <= now()+interval '12 hours' FROM browser_sessions").fetch_one(&pool).await.unwrap();
        assert_eq!(row, (hash(&token), hash(&csrf), true));
        assert_eq!(
            verify_session(&state, &cookie_header(SESSION, &token))
                .await
                .unwrap()
                .principal
                .email,
            "newuser@example.test"
        );
    }

    // Exercises the public auth routes, signed token exchange, persisted session,
    // production require_session middleware and actual management handlers.
    async fn http_sign_in(app: &Router, mock: &MockProvider) -> (String, String) {
        let (session, csrf, _) = http_sign_in_from(app, mock, "/api/v1/auth/login").await;
        (session, csrf)
    }

    /// Signs in through `login` (which may carry `return_to`); also returns the callback's Location.
    async fn http_sign_in_from(
        app: &Router,
        mock: &MockProvider,
        login: &str,
    ) -> (String, String, String) {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(login).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let url = Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let params: HashMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        mock.claims.lock().unwrap()["nonce"] = json!(params["nonce"]);
        let browser = response_cookie(&response, BROWSER);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/auth/callback?state={}&code=good",
                        params["state"]
                    ))
                    .header(header::COOKIE, format!("{BROWSER}={browser}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        (
            response_cookie(&response, SESSION),
            response_cookie(&response, CSRF),
            response.headers()[header::LOCATION]
                .to_str()
                .unwrap()
                .to_owned(),
        )
    }

    #[test]
    fn return_paths_are_same_origin_dashboard_paths_only() {
        for (input, expected) in [
            ("/workspaces/abc/keys", Some("/workspaces/abc/keys")),
            (
                "/workspaces/abc/requests?status=failed&q=3cbf",
                Some("/workspaces/abc/requests?status=failed&q=3cbf"),
            ),
            (
                "/admin/models/x?tab=routes",
                Some("/admin/models/x?tab=routes"),
            ),
            ("/home", Some("/home")),
            ("/a/../admin", Some("/admin")),
        ] {
            assert_eq!(safe_return_path(input).as_deref(), expected, "{input}");
        }
        for bad in [
            "",
            "home",
            "//evil.example/x",
            "/\\evil.example",
            "/\\/evil.example",
            "https://evil.example/",
            "javascript:alert(1)",
            "/api/v1/me",
            "/api",
            "/v1/chat/completions",
            "/health/ready",
            "/a/../api/v1/me",
            "/x\ny",
            "/x\u{7}",
        ] {
            assert_eq!(safe_return_path(bad), None, "{bad:?}");
        }
        assert_eq!(safe_return_path(&format!("/{}", "a".repeat(2048))), None);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn sign_in_returns_to_the_validated_deep_link(pool: sqlx::PgPool) {
        let mock = MockProvider::start(None).await;
        let state = mock.state(pool.clone()).await;
        let app = router(state.clone())
            .merge(crate::management::router(state.clone()))
            .with_state(state.store.clone());
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,'entitled','platform','user')")
            .bind(Uuid::new_v4()).bind(&mock.issuer).execute(&pool).await.unwrap();
        let deep = "/workspaces/abc/keys?status=all";
        let (_, _, location) = http_sign_in_from(
            &app,
            &mock,
            &format!("/api/v1/auth/login?return_to={}", urlencoding(deep)),
        )
        .await;
        assert_eq!(location, deep);
        // Unsafe targets are dropped: sign-in still works and lands on "/".
        for bad in [
            "//evil.example/",
            "https://evil.example/",
            "/api/v1/me",
            "/\\evil",
        ] {
            let (_, _, location) = http_sign_in_from(
                &app,
                &mock,
                &format!("/api/v1/auth/login?return_to={}", urlencoding(bad)),
            )
            .await;
            assert_eq!(location, "/", "{bad}");
        }
        let (_, _, location) = http_sign_in_from(&app, &mock, "/api/v1/auth/login").await;
        assert_eq!(location, "/");
        // The database rejects an unsafe stored value even if the handler were bypassed.
        let rejected = sqlx::query("INSERT INTO oidc_login_attempts VALUES($1,$2,'n','v',now()+interval '1 minute','//evil.example')")
            .bind(hash(&random_token())).bind(hash(&random_token())).execute(&pool).await;
        assert!(rejected.is_err());
    }

    fn urlencoding(value: &str) -> String {
        let mut url = Url::parse("https://x.invalid/").unwrap();
        url.query_pairs_mut().append_pair("v", value);
        url.query().unwrap()[2..].to_owned()
    }

    async fn http_management(
        app: &Router,
        credentials: &(String, String),
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::COOKIE, format!("{SESSION}={}", credentials.0))
                    .header("origin", "http://127.0.0.1:3000")
                    .header("x-csrf-token", &credentials.1)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn fresh_signed_session_accepts_claim_email_not_admin_edited_profile_email(
        pool: sqlx::PgPool,
    ) {
        let mock = MockProvider::start(None).await;
        let state = mock.state(pool.clone()).await;
        let app = router(state.clone())
            .merge(crate::management::router(state.clone()))
            .with_state(state.store.clone());
        for (group, role) in [("entitled", "user"), ("administrators", "admin")] {
            sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,$3,'platform',$4)")
                .bind(Uuid::new_v4()).bind(&mock.issuer).bind(group).bind(role).execute(&pool).await.unwrap();
        }
        let original_claims = mock.claims.lock().unwrap().clone();
        let initial = http_sign_in(&app, &mock).await;
        let user = verify_session(&state, &cookie_header(SESSION, &initial.0))
            .await
            .unwrap()
            .principal
            .user_id;
        {
            let mut claims = mock.claims.lock().unwrap();
            claims["sub"] = json!("admin-subject");
            claims["email"] = json!("administrator@example.test");
            claims["groups"] = json!(["administrators"]);
        }
        let administrator = http_sign_in(&app, &mock).await;
        let shared = Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces(id,name,kind) VALUES($1,'Shared','team')")
            .bind(shared)
            .execute(&pool)
            .await
            .unwrap();
        // Invitations need actual workspace authority, not platform administration.
        let administrator_id = verify_session(&state, &cookie_header(SESSION, &administrator.0))
            .await
            .unwrap()
            .principal
            .user_id;
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'owner','manual')")
            .bind(shared)
            .bind(administrator_id)
            .execute(&pool)
            .await
            .unwrap();
        let path = format!("/api/v1/workspaces/{shared}/invitations");
        let mut invites = Vec::new();
        for email in ["NewUser@Example.test", "profile-only@example.test"] {
            let (status, invite) = http_management(
                &app,
                &administrator,
                "POST",
                &path,
                json!({"email":email,"role":"member"}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{invite}");
            invites.push(invite);
        }
        let (status, body) = http_management(
            &app,
            &administrator,
            "PATCH",
            &format!("/api/v1/platform/users/{user}"),
            json!({"email":"profile-only@example.test"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            http_management(&app, &initial, "GET", "/api/v1/me", json!({}))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );

        // A fresh signature-verified claim still says A, while the profile now says B.
        *mock.claims.lock().unwrap() = original_claims;
        let fresh = http_sign_in(&app, &mock).await;
        let principal = verify_session(&state, &cookie_header(SESSION, &fresh.0))
            .await
            .unwrap()
            .principal;
        assert_eq!(principal.user_id, user);
        assert_eq!(principal.email, "newuser@example.test");
        let profile: String = sqlx::query_scalar("SELECT email FROM users WHERE id=$1")
            .bind(user)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(profile, "profile-only@example.test");
        let proof: String =
            sqlx::query_scalar("SELECT verified_email FROM browser_sessions WHERE token_hash=$1")
                .bind(hash(&fresh.0))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(proof, "newuser@example.test");
        let binding: Uuid = sqlx::query_scalar(
            "SELECT user_id FROM oidc_identities WHERE issuer=$1 AND subject='subject-one'",
        )
        .bind(&mock.issuer)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(binding, user);
        let (status, me) = http_management(&app, &fresh, "GET", "/api/v1/me", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(me["user"]["email"], "newuser@example.test");
        assert_eq!(
            http_management(
                &app,
                &fresh,
                "POST",
                "/api/v1/invitations/accept",
                json!({"token":invites[1]["token"]})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let members: i64 = sqlx::query_scalar("SELECT count(*) FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2")
            .bind(shared).bind(user).fetch_one(&pool).await.unwrap();
        assert_eq!(members, 0);
        let (status, accepted) = http_management(
            &app,
            &fresh,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":invites[0]["token"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{accepted}");
        assert_eq!(accepted, json!({"workspace_id":shared}));
        let accepted_emails: Vec<String> = sqlx::query_scalar(
            "SELECT email FROM workspace_invitations WHERE accepted_at IS NOT NULL",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(accepted_emails, vec!["newuser@example.test"]);
        let role: String = sqlx::query_scalar(
            "SELECT role FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
        )
        .bind(shared)
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, "member");
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn invalid_tokens_and_failed_exchange_consume_attempt_without_session(
        pool: sqlx::PgPool,
    ) {
        let mock = MockProvider::start(None).await;
        let state = mock.state(pool.clone()).await;
        for (field, value) in [
            ("nonce", json!("wrong")),
            ("iss", json!("https://wrong.test")),
            ("aud", json!("other-client")),
            ("exp", json!(0)),
            ("email_verified", json!(false)),
            ("email_verified", Value::Null),
            ("email", Value::Null),
            ("azp", json!("other-client")),
            ("aud", json!(["test-client", "other-client"])),
            ("test_bad_signature", json!(true)),
        ] {
            let original = mock.claims.lock().unwrap().clone();
            let (oauth_state, browser, _) = begin_login(&state, &mock).await;
            mock.claims.lock().unwrap()[field] = value;
            assert!(
                finish_login(&state, &oauth_state, &browser, "good")
                    .await
                    .is_err(),
                "{field}"
            );
            let count = mock.exchanges.load(Ordering::SeqCst);
            assert!(
                finish_login(&state, &oauth_state, &browser, "good")
                    .await
                    .is_err()
            );
            assert_eq!(mock.exchanges.load(Ordering::SeqCst), count);
            *mock.claims.lock().unwrap() = original;
        }
        let (oauth_state, browser, _) = begin_login(&state, &mock).await;
        assert!(
            finish_login(&state, &oauth_state, &browser, "bad")
                .await
                .is_err()
        );
        assert!(
            finish_login(&state, &oauth_state, &browser, "good")
                .await
                .is_err()
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM browser_sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn browser_access_denial_redirects_without_callback_secrets_or_logout_csrf(
        pool: sqlx::PgPool,
    ) {
        let mock = MockProvider::start(None).await;
        let state = mock.state(pool.clone()).await;
        let (oauth, browser, _) = begin_login(&state, &mock).await;
        let mut headers = cookie_header(BROWSER, &browser);
        headers.insert(header::ACCEPT, HeaderValue::from_static("text/html"));
        let response = callback(
            State(state.clone()),
            headers,
            Ok(Query(CallbackQuery {
                state: oauth,
                code: Some("good".into()),
                error: None,
            })),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers()[header::LOCATION],
            "/?auth_error=access_denied"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .all(|v| v.to_str().unwrap().contains("Max-Age=0"))
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM workspaces")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM browser_sessions")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,'entitled','platform','user')").bind(Uuid::new_v4()).bind(&mock.issuer).execute(&pool).await.unwrap();
        let (oauth, browser, _) = begin_login(&state, &mock).await;
        let session = response_cookie(
            &finish_login(&state, &oauth, &browser, "good")
                .await
                .unwrap(),
            SESSION,
        );
        // An unverified callback cannot revoke the preceding valid session.
        let (oauth, browser, _) = begin_login(&state, &mock).await;
        mock.claims.lock().unwrap()["nonce"] = json!("incorrect");
        let mut headers = cookie_header(BROWSER, &browser);
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{BROWSER}={browser}; {SESSION}={session}")).unwrap(),
        );
        headers.insert(header::ACCEPT, HeaderValue::from_static("text/html"));
        assert_eq!(
            callback(
                State(state.clone()),
                headers,
                Ok(Query(CallbackQuery {
                    state: oauth,
                    code: Some("good".into()),
                    error: None
                }))
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::BAD_REQUEST
        );
        assert!(
            verify_session(&state, &cookie_header(SESSION, &session))
                .await
                .is_ok()
        );
        // A verified switch to an unauthorized account clears the old browser
        // session but must not provision a personal workspace for the new one.
        mock.claims.lock().unwrap()["sub"] = json!("different-unentitled-subject");
        mock.claims.lock().unwrap()["email"] = json!("denied@example.test");
        mock.claims.lock().unwrap()["groups"] = json!([]);
        let (oauth, browser, _) = begin_login(&state, &mock).await;
        let mut headers = cookie_header(BROWSER, &browser);
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{BROWSER}={browser}; {SESSION}={session}")).unwrap(),
        );
        headers.insert(header::ACCEPT, HeaderValue::from_static("text/html"));
        let response = callback(
            State(state.clone()),
            headers,
            Ok(Query(CallbackQuery {
                state: oauth,
                code: Some("good".into()),
                error: None,
            })),
        )
        .await
        .unwrap();
        assert_eq!(
            response.headers()[header::LOCATION],
            "/?auth_error=access_denied"
        );
        assert!(
            verify_session(&state, &cookie_header(SESSION, &session))
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM workspaces")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn custom_signed_groups_entitlement_loss_commits_before_forbidden_and_malformed_does_not_revoke(
        pool: sqlx::PgPool,
    ) {
        let mock = MockProvider::start(None).await;
        let mut config = mock.config();
        config.groups_claim = "entitlements.roles".into();
        let state = IdentityState::new(Store::new(pool.clone()), Some(config))
            .await
            .unwrap();
        mock.claims.lock().unwrap()["entitlements"] = json!({"roles":["staff"]});
        // Valid authentication alone cannot create a personal workspace.
        let (oauth, browser, _) = begin_login(&state, &mock).await;
        assert_eq!(
            finish_login(&state, &oauth, &browser, "good")
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM workspaces")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,'staff','platform','admin')")
            .bind(Uuid::new_v4()).bind(&mock.issuer).execute(&pool).await.unwrap();
        let (oauth, browser, _) = begin_login(&state, &mock).await;
        let response = finish_login(&state, &oauth, &browser, "good")
            .await
            .unwrap();
        let session = response_cookie(&response, SESSION);
        let principal = verify_session(&state, &cookie_header(SESSION, &session))
            .await
            .unwrap()
            .principal;
        assert!(principal.platform_admin && principal.platform_auditor);
        let ws: Uuid = sqlx::query_scalar("SELECT id FROM workspaces WHERE owner_user_id=$1")
            .bind(principal.user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let key = crate::auth::NewApiKey::generate();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'Mine',$4)")
            .bind(key.id).bind(ws).bind(principal.user_id).bind(key.digest.as_slice()).execute(&pool).await.unwrap();
        // Profile divergence and even another account owning the signed email must not
        // short-circuit authoritative empty-group revocation for this issuer/subject.
        sqlx::query("UPDATE users SET email='profile-only@example.test' WHERE id=$1")
            .bind(principal.user_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
            .bind(Uuid::new_v4())
            .bind(&principal.email)
            .execute(&pool)
            .await
            .unwrap();
        for malformed in [
            Value::Null,
            json!({"roles":"staff"}),
            json!({"roles":["staff",1]}),
        ] {
            mock.claims.lock().unwrap()["entitlements"] = malformed;
            let (oauth, browser, _) = begin_login(&state, &mock).await;
            assert_eq!(
                finish_login(&state, &oauth, &browser, "good")
                    .await
                    .unwrap_err()
                    .0,
                StatusCode::FORBIDDEN
            );
            assert!(
                verify_session(&state, &cookie_header(SESSION, &session))
                    .await
                    .is_ok()
            );
            assert!(
                state
                    .store
                    .authenticate(&key.token)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        mock.claims
            .lock()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("entitlements");
        let (oauth, browser, _) = begin_login(&state, &mock).await;
        assert_eq!(
            finish_login(&state, &oauth, &browser, "good")
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        assert!(
            state
                .store
                .authenticate(&key.token)
                .await
                .unwrap()
                .is_some()
        );
        mock.claims.lock().unwrap()["entitlements"] = json!({"roles":[]});
        let (oauth, browser, _) = begin_login(&state, &mock).await;
        assert_eq!(
            finish_login(&state, &oauth, &browser, "good")
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        assert!(
            verify_session(&state, &cookie_header(SESSION, &session))
                .await
                .is_err()
        );
        assert!(
            state
                .store
                .authenticate(&key.token)
                .await
                .unwrap()
                .is_none()
        );
        let disabled: bool = sqlx::query_scalar(
            "SELECT disabled_at IS NOT NULL AND cleanup_due_at IS NOT NULL FROM users WHERE id=$1",
        )
        .bind(principal.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(disabled);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn cleaned_identity_rebinds_to_new_uuid_without_resurrecting_old_access(
        pool: sqlx::PgPool,
    ) {
        let store = Store::new(pool.clone());
        let issuer = "https://id.test";
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,'staff','platform','user')").bind(Uuid::new_v4()).bind(issuer).execute(&pool).await.unwrap();
        let old = resolve_identity(
            &store,
            issuer,
            "subject",
            "user@test.invalid",
            &["staff".into()],
        )
        .await
        .unwrap();
        let old_ws: Uuid = sqlx::query_scalar("SELECT id FROM workspaces WHERE owner_user_id=$1")
            .bind(old)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            resolve_identity(&store, issuer, "subject", "user@test.invalid", &[])
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        sqlx::query("UPDATE users SET cleanup_due_at=now()-interval '1 second' WHERE id=$1")
            .bind(old)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            lifecycle::cleanup_inactive_accounts(&store).await.unwrap(),
            1
        );
        let new = resolve_identity(
            &store,
            issuer,
            "subject",
            "user@test.invalid",
            &["staff".into()],
        )
        .await
        .unwrap();
        assert_ne!(old, new);
        let binding: Uuid = sqlx::query_scalar(
            "SELECT user_id FROM oidc_identities WHERE issuer=$1 AND subject='subject'",
        )
        .bind(issuer)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(binding, new);
        let old_disabled: bool =
            sqlx::query_scalar("SELECT disabled_at IS NOT NULL FROM workspaces WHERE id=$1")
                .bind(old_ws)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(old_disabled);
        let inherited: i64=sqlx::query_scalar("SELECT count(*) FROM workspace_membership_grants WHERE user_id=$1 AND workspace_id=$2 AND revoked_at IS NULL").bind(new).bind(old_ws).fetch_one(&pool).await.unwrap();
        assert_eq!(inherited, 0);
        let user_count: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(user_count, 2);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn expired_grace_callback_cleans_old_account_even_when_worker_has_not_run(
        pool: sqlx::PgPool,
    ) {
        let store = Store::new(pool.clone());
        let issuer = "https://id.test";
        sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,'staff','platform','user')").bind(Uuid::new_v4()).bind(issuer).execute(&pool).await.unwrap();
        let old = resolve_identity(
            &store,
            issuer,
            "subject",
            "old@test.invalid",
            &["staff".into()],
        )
        .await
        .unwrap();
        assert_eq!(
            resolve_identity(&store, issuer, "subject", "old@test.invalid", &[])
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        sqlx::query("UPDATE users SET cleanup_due_at=now()-interval '1 second' WHERE id=$1")
            .bind(old)
            .execute(&pool)
            .await
            .unwrap();
        // A still-unentitled callback keeps the old binding tombstoned rather than creating a new user.
        assert_eq!(
            resolve_identity(&store, issuer, "subject", "old@test.invalid", &[])
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        let bound: Uuid =
            sqlx::query_scalar("SELECT user_id FROM oidc_identities WHERE subject='subject'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(bound, old);
        let new = resolve_identity(
            &store,
            issuer,
            "subject",
            "old@test.invalid",
            &["staff".into()],
        )
        .await
        .unwrap();
        assert_ne!(old, new);
        let cleaned: bool = sqlx::query_scalar(
            "SELECT cleaned_at IS NOT NULL AND disabled_at IS NOT NULL FROM users WHERE id=$1",
        )
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(cleaned);
        let old_active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM platform_role_grants WHERE user_id=$1 AND revoked_at IS NULL",
        )
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(old_active, 0);
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn sessions_require_current_entitlement_not_stale_admin_presentation(pool: sqlx::PgPool) {
        let state = IdentityState::new(Store::new(pool.clone()), None)
            .await
            .unwrap();
        let (user, session, _) = seed(&pool).await;
        sqlx::query("UPDATE platform_role_grants SET role='admin' WHERE user_id=$1")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            verify_session(&state, &cookie_header(SESSION, &session))
                .await
                .unwrap()
                .principal
                .platform_admin
        );
        sqlx::query("UPDATE platform_role_grants SET role='auditor' WHERE user_id=$1")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        let principal = verify_session(&state, &cookie_header(SESSION, &session))
            .await
            .unwrap()
            .principal;
        assert!(!principal.platform_admin && principal.platform_auditor);
        sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            verify_session(&state, &cookie_header(SESSION, &session))
                .await
                .is_err()
        );
    }

    #[sqlx::test(migrations = "./enterprise_migrations")]
    async fn scim_managed_groups_replace_token_membership_at_sign_in(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let issuer = "https://issuer.test";
        for group in ["Engineering", "Legacy"] {
            sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,$2,$3,'platform','user')")
                .bind(Uuid::new_v4()).bind(issuer).bind(group).execute(&pool).await.unwrap();
        }
        let (scim_member, group) = (Uuid::new_v4(), Uuid::new_v4());
        sqlx::query(
            "INSERT INTO users(id,email,oidc_link_allowed) VALUES($1,'member@example.test',true)",
        )
        .bind(scim_member)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO scim_groups(id,display_name,external_id) VALUES($1,'Engineering','grp-eng')")
            .bind(group).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO scim_group_members(group_id,user_id) VALUES($1,$2)")
            .bind(group)
            .bind(scim_member)
            .execute(&pool)
            .await
            .unwrap();
        let grants = |user: Uuid| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, String>("SELECT m.group_value FROM platform_role_grants g JOIN oidc_group_mappings m ON m.id=g.mapping_id WHERE g.user_id=$1 AND g.source='group' AND g.revoked_at IS NULL ORDER BY 1")
                    .bind(user).fetch_all(&pool).await.unwrap()
            }
        };
        // SCIM membership entitles even though the token claim is empty.
        assert_eq!(
            resolve_identity_with(&store, issuer, "member", "member@example.test", &[], true)
                .await
                .unwrap(),
            scim_member
        );
        assert_eq!(grants(scim_member).await, ["Engineering"]);
        // With SCIM disabled the signed empty claim is authoritative again.
        assert_eq!(
            resolve_identity_with(&store, issuer, "member", "member@example.test", &[], false)
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            resolve_identity_with(&store, issuer, "member", "member@example.test", &[], true)
                .await
                .unwrap(),
            scim_member
        );
        // A token claiming a SCIM-managed group (by name or external id) does not grant it;
        // groups SCIM does not manage still come from the token.
        assert_eq!(
            resolve_identity_with(
                &store,
                issuer,
                "outsider",
                "outsider@example.test",
                &["Engineering".into(), "grp-eng".into()],
                true
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        let outsider = resolve_identity_with(
            &store,
            issuer,
            "outsider",
            "outsider@example.test",
            &["Engineering".into(), "Legacy".into()],
            true,
        )
        .await
        .unwrap();
        assert_eq!(grants(outsider).await, ["Legacy"]);
    }
}
