use super::*;
use axum::{body::Body, extract::Form};
use openidconnect::{
    PrivateSigningKey,
    core::{CoreEdDsaPrivateSigningKey, CoreIdToken, CoreIdTokenClaims, CoreJwsSigningAlgorithm},
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
        let jwks = json!({"keys": [signing_key().as_verification_key()]});
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = Arc::new(Mutex::new(json!({
            "iss": issuer, "sub": "subject-one", "aud": "test-client", "exp": now+300,
            "iat": now, "nonce": "not-yet-set", "email": "NewUser@Example.test", "email_verified": true
        })));
        let exchanges = Arc::new(AtomicUsize::new(0));
        let verifier = Arc::new(Mutex::new(None));
        let state = (claims.clone(), exchanges.clone(), verifier.clone());
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(move || { let metadata = metadata.clone(); async move { Json(metadata) } }))
            .route("/jwks", get(move || { let jwks = jwks.clone(); async move { Json(jwks) } }))
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
                    let claims: CoreIdTokenClaims = serde_json::from_value(values).unwrap();
                    let id_token = CoreIdToken::new(claims, &signing_key(), CoreJwsSigningAlgorithm::EdDsa, None, None).unwrap();
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
        sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at) VALUES($1,$2,$3,now()+interval '12 hours')").bind(hash(&session)).bind(id).bind(hash(&csrf)).execute(pool).await.unwrap();
        (id, session, csrf)
    }

    #[sqlx::test(migrations = "./migrations")]
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

    #[sqlx::test(migrations = "./migrations")]
    async fn linking_is_explicit_single_use_and_does_not_grant_roles(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let (id, _, _) = seed(&pool).await;
        let email = format!("{id}@example.test");
        assert_eq!(
            resolve_identity(&store, "https://issuer.test", "subject", &email)
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
                &email.to_uppercase()
            )
            .await
            .unwrap(),
            id
        );
        let flags: (bool, bool) =
            sqlx::query_as("SELECT oidc_link_allowed,platform_admin FROM users WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(flags, (false, false));
        assert_eq!(
            resolve_identity(
                &store,
                "https://issuer.test",
                "subject",
                "changed@example.test"
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
            resolve_identity(&store, "https://issuer.test", "other-subject", &email)
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        let fresh = resolve_identity(&store, "https://issuer.test", "fresh", "Fresh@Example.test")
            .await
            .unwrap();
        let row: (String, bool) =
            sqlx::query_as("SELECT email,platform_admin FROM users WHERE id=$1")
                .bind(fresh)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, ("fresh@example.test".into(), false));
        let memberships: i64 =
            sqlx::query_scalar("SELECT count(*) FROM organization_memberships WHERE user_id=$1")
                .bind(fresh)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(memberships, 0);
        sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            resolve_identity(&store, "https://issuer.test", "subject", &email)
                .await
                .unwrap_err()
                .0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn attempts_are_bound_expiring_and_single_use(pool: sqlx::PgPool) {
        let store = Store::new(pool.clone());
        let oauth_state = random_token();
        let browser = random_token();
        sqlx::query("INSERT INTO login_attempts VALUES($1,$2,'nonce','verifier',now()+interval '10 minutes')").bind(hash(&oauth_state)).bind(hash(&browser)).execute(&pool).await.unwrap();
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
            "INSERT INTO login_attempts VALUES($1,$2,'nonce','verifier',now()-interval '1 second')",
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

    #[sqlx::test(migrations = "./migrations")]
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
        let response = login(State(state.clone())).await.unwrap();
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

    #[sqlx::test(migrations = "./migrations")]
    async fn complete_oidc_flow_uses_pkce_and_fresh_hashed_sessions(pool: sqlx::PgPool) {
        let mock = MockProvider::start(None).await;
        let state = mock.state(pool.clone()).await;
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

    #[sqlx::test(migrations = "./migrations")]
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
}
