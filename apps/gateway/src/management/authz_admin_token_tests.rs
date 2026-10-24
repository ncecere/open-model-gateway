//! #37 PR2: admin-token authentication, scope/IP enforcement and the
//! per-attempt audit trail, on the real served router and real middleware
//! (not injected principals) wherever the test needs the full stack, and on
//! `isolated::stub_routes()` (every declared route, the same two middleware
//! layers, no handler logic) for the exhaustive route/scope matrix.
use super::*;
use crate::{
    admin_tokens,
    governance::locks,
    identity::{self, throttle},
    route_policy::{TokenPolicy, TokenScope},
};
use isolated::stub_routes;

/// One admin token for `owner` (who must already be a currently effective
/// Platform Admin), with exactly `scopes` and optionally an IP allowlist.
/// Mirrors the browser handler's own contract: the owner's user-scope
/// authority lock exclusive, before the first write.
async fn mint(
    pool: &PgPool,
    owner: Uuid,
    scopes: &[&str],
    allowlist: Option<&[&str]>,
) -> (Uuid, String) {
    let mut tx = crate::db::begin(pool).await.unwrap();
    locks::exclusive(&mut tx, [locks::Scope::User(owner)])
        .await
        .unwrap();
    let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
    let allow: Option<Vec<String>> = allowlist.map(|a| a.iter().map(|s| s.to_string()).collect());
    let created = admin_tokens::create_admin_token(
        &mut tx,
        owner,
        "Test token",
        &scopes,
        30,
        allow.as_deref(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    (created.id, created.token)
}

/// The latest `start` attempt recorded for this exact token/route (never
/// ambiguous across probes made one at a time by these tests).
async fn last_outcome(
    pool: &PgPool,
    token_id: Uuid,
    method: &str,
    template: &str,
) -> Option<String> {
    sqlx::query_scalar(
        "SELECT outcome FROM admin_token_attempts WHERE token_id=$1 AND phase='start' AND route_method=$2 AND route_template=$3 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(token_id)
    .bind(method)
    .bind(template)
    .fetch_optional(pool)
    .await
    .unwrap()
}

fn concrete_path(path: &str, team: Uuid) -> String {
    let filler = Uuid::new_v4().to_string();
    path.replace("{ws}", &team.to_string())
        .replace("{kind}", "team")
        .replace("{role}", "user")
        .replace("{root}", "root-1")
        .replace("{*rest}", "unknown")
        .replace("{user}", &filler)
        .replace("{key}", &filler)
        .replace("{id}", &filler)
        .replace("{model}", &filler)
        .replace("{deployment}", &filler)
        .replace("{execution}", &filler)
        .replace("{hold}", &filler)
        .replace("{session}", &filler)
        .replace("{token}", &filler)
}

/// A synthetic, distinct-per-call client address (#37 PR2 finding 2
/// regression guard): the *existing*, unrelated per-IP admin-token limiter
/// in `identity.rs` buckets every in-process request with no `ConnectInfo`
/// under one shared "unknown peer" bucket, and the expanded matrix below
/// issues far more requests than that bucket's burst allows. Each probe
/// gets its own address so this test exercises the token-policy matrix,
/// not an unrelated rate limiter it isn't about.
fn synthetic_peer(seq: u32) -> std::net::SocketAddr {
    std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::from(0x0A00_0001u32.wrapping_add(seq))),
        0,
    )
}

/// Like [`authz_matrix_tests::request`](super::request), but for an admin
/// token only, with its own synthetic peer address per call (see
/// [`synthetic_peer`]).
async fn probe(app: &Router, method: &str, path: &str, token: &str, seq: u32) -> StatusCode {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("origin", ORIGIN)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from("{}"))
        .unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(synthetic_peer(seq)));
    app.clone().oneshot(req).await.unwrap().status()
}

fn stub_app(s: &Store, identity: identity::IdentityState) -> Router {
    stub_routes()
        .route_layer(middleware::from_fn_with_state(
            s.clone(),
            authorization::require_route_access,
        ))
        .route_layer(middleware::from_fn_with_state(
            identity,
            authenticate_management,
        ))
        .with_state(s.clone())
}

/// TEST-FIRST (RED before this PR's authentication/authorization layer
/// existed, every one of these requests 404'd or 401'd with no scope
/// concept at all): every declared route's token policy is enforced
/// exactly as declared, for every route in the inventory -- not a sample,
/// and not only against one "a wrong scope" probe (finding 2: the original
/// version of this test took only the *first* mismatching scope via
/// `Iterator::find`, so a route that happened to also admit, say, the
/// second-tried scope by mistake would never have been caught). For a
/// `Scope(required)` route this now probes, individually, every one of the
/// five single-scope tokens (exactly one must admit it), a token holding
/// all five scopes together (still admitted: extra scopes never revoke the
/// one that matters), and a token holding every *other* scope but not the
/// required one (still refused: holding several wrong scopes at once must
/// not accidentally satisfy the check). `Denied`/`BrowserAdminOnly` routes
/// refuse all of that -- every single-scope token and the all-five token
/// alike -- regardless of how many scopes the token holds.
///
/// Two independent oracles, not one: the per-attempt audit row
/// (`admin_token_attempts.outcome`, the authorization decision alone) and
/// the actual HTTP response (which also depends on route readiness --
/// finding 1's HEAD/501 bug was invisible to the audit-only oracle alone,
/// since the outcome is decided before the readiness check runs).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn every_route_enforces_its_declared_token_policy(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = stub_app(&f.s, identity);
    let all_scope_strings: Vec<&str> = TokenScope::ALL.iter().map(|s| s.as_str()).collect();
    let mut tokens = std::collections::HashMap::new();
    for scope in TokenScope::ALL {
        let (id, token) = mint(&pool, f.admin.user_id, &[scope.as_str()], None).await;
        tokens.insert(scope.as_str(), (id, token));
    }
    let (all_five_id, all_five_token) =
        mint(&pool, f.admin.user_id, &all_scope_strings, None).await;
    // Keyed by the *excluded* scope: every other scope present, the
    // required one deliberately absent.
    let mut missing_one = std::collections::HashMap::new();
    for excluded in TokenScope::ALL {
        let rest: Vec<&str> = all_scope_strings
            .iter()
            .copied()
            .filter(|s| *s != excluded.as_str())
            .collect();
        missing_one.insert(
            excluded.as_str(),
            mint(&pool, f.admin.user_id, &rest, None).await,
        );
    }
    let mut checked = 0;
    for route in ROUTES {
        for method in route.methods.split_whitespace().flat_map(|m| {
            if m == "*" {
                vec!["GET", "POST", "PUT", "DELETE"]
            } else if m == "GET" {
                vec!["GET", "HEAD"]
            } else {
                vec![m]
            }
        }) {
            let path = concrete_path(route.path, f.team);
            // (token id, token, expected outcome, expected response is 501).
            let probes: Vec<(Uuid, &str, &'static str)> = match route.token_policy {
                TokenPolicy::Denied | TokenPolicy::BrowserAdminOnly => TokenScope::ALL
                    .iter()
                    .map(|s| {
                        let (id, token) = &tokens[s.as_str()];
                        (*id, token.as_str(), "denied")
                    })
                    .chain(std::iter::once((
                        all_five_id,
                        all_five_token.as_str(),
                        "denied",
                    )))
                    .collect(),
                TokenPolicy::Scope(required) => TokenScope::ALL
                    .iter()
                    .map(|s| {
                        let (id, token) = &tokens[s.as_str()];
                        let expected = if s.as_str() == required.as_str() {
                            "allowed"
                        } else {
                            "denied"
                        };
                        (*id, token.as_str(), expected)
                    })
                    .chain(std::iter::once((
                        all_five_id,
                        all_five_token.as_str(),
                        "allowed",
                    )))
                    .chain(std::iter::once({
                        let (id, token) = &missing_one[required.as_str()];
                        (*id, token.as_str(), "denied")
                    }))
                    .collect(),
            };
            for (token_id, token, expected) in probes {
                checked += 1;
                let status = probe(&app, method, &path, token, checked).await;
                let outcome = last_outcome(&pool, token_id, method, route.path).await;
                assert_eq!(
                    outcome.as_deref(),
                    Some(expected),
                    "{method} {} token={token_id}",
                    route.path
                );
                assert_ne!(
                    status,
                    StatusCode::NOT_IMPLEMENTED,
                    "{method} {} token={token_id}: an 'allowed' or 'denied' decision must \
                     never surface as a stub 501 (finding 1)",
                    route.path
                );
                if expected == "allowed" {
                    assert!(
                        !status.is_client_error() && !status.is_server_error(),
                        "{method} {} token={token_id}: expected a real success, got {status}",
                        route.path,
                    );
                } else {
                    assert_eq!(
                        status,
                        StatusCode::FORBIDDEN,
                        "{method} {} token={token_id}",
                        route.path
                    );
                }
            }
        }
    }
    assert!(
        checked > 900,
        "the token-policy matrix must exercise the complete route inventory across every scope"
    );
}

/// Positive controls: a real route per scope, through the real router (no
/// stub), returns a real success -- the architecture is not merely "not
/// 403", it genuinely serves the request. Also proves mutations need no
/// CSRF token or Origin header for an admin token (unlike a browser
/// session, exercised everywhere else in this file).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn positive_controls_per_scope_reach_a_real_handler(pool: PgPool) {
    let mut f = fixture(&pool).await;
    f.s = f.s.with_secret_allowlists(
        crate::providers::secrets::EnvSecrets::from_lookup(["OPENAI_API_KEY".into()], |_| None),
        crate::providers::secrets::EnvSecrets::default(),
    );
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    for (scope, method, path, body) in [
        (
            "platform:read",
            "GET",
            "/api/v1/platform/providers".to_owned(),
            Value::Null,
        ),
        (
            "catalog:write",
            "POST",
            "/api/v1/platform/providers".to_owned(),
            json!({"name":"Token-created","provider":"openai","credential_ref":"env:OPENAI_API_KEY","enabled":false}),
        ),
        (
            "access:write",
            "POST",
            "/api/v1/platform/users".to_owned(),
            json!({"email":"token-created@example.test","platform_role":"user"}),
        ),
        (
            "governance:write",
            "POST",
            "/api/v1/platform/cost-centers".to_owned(),
            json!({"name":"Token cost center","code":"TOKC"}),
        ),
        (
            "settings:write",
            "PUT",
            "/api/v1/platform/settings/privacy".to_owned(),
            json!({"openrouter_data_collection":"allow"}),
        ),
    ] {
        let (_, token) = mint(&pool, f.admin.user_id, &[scope], None).await;
        let mut req = Request::builder()
            .method(method)
            .uri(&path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"));
        // Deliberately no Origin, no CSRF token, no cookie: an admin token
        // needs none of them.
        req = req.header("x-test-scope", scope);
        let response = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        assert!(
            status.is_success(),
            "{scope} {method} {path}: {status} {:?}",
            serde_json::from_slice::<Value>(
                &to_bytes(response.into_body(), 1 << 20).await.unwrap()
            )
        );
    }
}

/// Admin tokens never reach a route an admin-token policy denies, even
/// through the full router, and an unmapped route denies them too (the
/// same "undeclared route" guarantee the browser-session matrix proves
/// above, now for a token).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn denied_and_undeclared_routes_refuse_a_token_through_the_full_router(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity.clone());
    let (_, token) = mint(&pool, f.admin.user_id, &admin_tokens::SCOPES, None).await;
    // Token CRUD: BrowserAdminOnly, even with every scope.
    for (method, path) in [
        ("GET", "/api/v1/platform/admin-tokens"),
        ("POST", "/api/v1/platform/admin-tokens"),
    ] {
        assert_eq!(
            request(&app, method, path, None, Some(&token), false, ORIGIN).await,
            StatusCode::FORBIDDEN,
            "{method} {path}"
        );
    }
    // Workspace-member/key routes: Denied regardless of scope.
    let key_path = format!("/api/v1/workspaces/{}/keys", f.team);
    assert_eq!(
        request(&app, "GET", &key_path, None, Some(&token), false, ORIGIN).await,
        StatusCode::FORBIDDEN
    );
    // An undeclared route behind the same middleware pair denies a token
    // exactly like it denies a browser session.
    let undeclared = Router::new()
        .route(
            "/api/v1/platform/config/update",
            post(|| async { StatusCode::OK }),
        )
        .route_layer(middleware::from_fn_with_state(
            f.s.clone(),
            authorization::require_route_access,
        ))
        .route_layer(middleware::from_fn_with_state(
            identity,
            authenticate_management,
        ))
        .with_state(f.s.clone());
    assert_eq!(
        request(
            &undeclared,
            "POST",
            "/api/v1/platform/config/update",
            None,
            Some(&token),
            false,
            ORIGIN
        )
        .await,
        StatusCode::FORBIDDEN
    );
}

/// Credential ambiguity, in both directions, before either credential is
/// examined: no fallback.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn credential_selection_refuses_ambiguity_with_no_fallback(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = stub_app(&f.s, identity);
    let (_, token) = mint(&pool, f.admin.user_id, &["platform:read"], None).await;
    let path = "/api/v1/platform/providers";

    // Duplicate Authorization headers.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Authorization plus a *valid* browser session cookie: refused, even
    // though the cookie alone would have authenticated a browser session.
    let valid_session = session(&pool, &f.admin).await;
    assert_eq!(
        request(
            &app,
            "GET",
            path,
            Some(&valid_session),
            Some(&token),
            false,
            ORIGIN
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    // Authorization plus an *invalid* (unknown, garbage) omg_session
    // cookie: refused too -- "even an invalid cookie" means this check
    // never validates the cookie's own shape before refusing.
    let garbage = Session {
        cookie: "0".repeat(64),
        csrf: "0".repeat(64),
    };
    assert_eq!(
        request(
            &app,
            "GET",
            path,
            Some(&garbage),
            Some(&token),
            false,
            ORIGIN
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    // A malformed (non-hex, wrong length) omg_session cookie value is also
    // "present" for this check, even though it could never be a valid
    // session token.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("cookie", "omg_session=not-a-valid-session-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // The reverse direction: an inference-key-shaped bearer credential on a
    // management route is refused before any database access, never
    // treated as a (malformed) admin token.
    let inference_shaped = crate::auth::NewApiKey::generate().unwrap().token;
    assert_eq!(
        request(
            &app,
            "GET",
            path,
            None,
            Some(&inference_shaped),
            false,
            ORIGIN
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    // And the other reverse direction: an admin-token-shaped bearer
    // credential is refused on an inference route, never accepted as an
    // inference key (checked on the real inference router, not the stub).
    let inference_app = crate::http::router(f.s.clone());
    let response = inference_app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A malformed credential (not `Bearer <token>` at all) is refused the
    // same way, before any parsing of what follows.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", "Basic dXNlcjpwYXNz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// A real workspace-scoped mutation through an admin token with the
/// matching scope succeeds with neither a CSRF token nor a matching Origin
/// header -- the explicit exemption -- while the same route still enforces
/// exact CSRF for a browser session, proving the exemption is narrow (only
/// a successfully authenticated token), not a general relaxation.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn csrf_exemption_is_only_for_an_authenticated_admin_token(pool: PgPool) {
    let mut f = fixture(&pool).await;
    f.s = f.s.with_secret_allowlists(
        crate::providers::secrets::EnvSecrets::from_lookup(["OPENAI_API_KEY".into()], |_| None),
        crate::providers::secrets::EnvSecrets::default(),
    );
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let (_, token) = mint(&pool, f.admin.user_id, &["catalog:write"], None).await;
    let body = json!({"name":"CSRF probe","provider":"openai","credential_ref":"env:OPENAI_API_KEY","enabled":false});
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/platform/providers")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "admin token needs no CSRF"
    );
    // The exact same route, same admin, as a browser session with no CSRF
    // token: refused, unchanged behavior.
    let admin_session = session(&pool, &f.admin).await;
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/platform/providers",
            Some(&admin_session),
            None,
            false,
            ORIGIN
        )
        .await,
        StatusCode::FORBIDDEN,
        "browser session still needs CSRF"
    );
}

/// The resolved client address matches `throttle::client_ip`'s own rule:
/// the socket peer, unless it is a trusted proxy, then the right-most
/// untrusted `X-Forwarded-For` entry. Exercised on a real TCP listener so
/// there is a genuine peer address, with `GATEWAY_TRUSTED_PROXIES`
/// configured on this test's own `IdentityState`.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn ip_allowlist_follows_the_trusted_proxy_chain_and_fails_closed(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock
        .state(pool.clone())
        .await
        .with_trusted_proxies(throttle::TrustedProxies::parse("127.0.0.1/32").unwrap());
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let base = format!("http://{address}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // No allowlist: unrestricted, through a trusted-proxy forwarded chain.
    let (_, unrestricted) = mint(&pool, f.admin.user_id, &["platform:read"], None).await;
    let response = client
        .get(format!("{base}/api/v1/platform/providers"))
        .header("authorization", format!("Bearer {unrestricted}"))
        .header("x-forwarded-for", "203.0.113.9")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // Allowlisted to the forwarded address: admitted.
    let (_, matching) = mint(
        &pool,
        f.admin.user_id,
        &["platform:read"],
        Some(&["203.0.113.0/24"]),
    )
    .await;
    let response = client
        .get(format!("{base}/api/v1/platform/providers"))
        .header("authorization", format!("Bearer {matching}"))
        .header("x-forwarded-for", "203.0.113.9")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // A different forwarded address: refused.
    let response = client
        .get(format!("{base}/api/v1/platform/providers"))
        .header("authorization", format!("Bearer {matching}"))
        .header("x-forwarded-for", "198.51.100.7")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

    // Direct connection (no X-Forwarded-For): the peer is 127.0.0.1, which
    // is not in the allowlist either.
    let response = client
        .get(format!("{base}/api/v1/platform/providers"))
        .header("authorization", format!("Bearer {matching}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);

    server.abort();
}

/// Every admitted token attempt (allowed or denied) gets an immutable
/// start record before dispatch and a completion record before returning,
/// correlated by request id, carrying only the typed, non-sensitive facts
/// the spec names -- never the raw path, a body, a name or a credential.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn audit_records_capture_the_declared_facts_only(pool: PgPool) {
    let mut f = fixture(&pool).await;
    f.s = f.s.with_secret_allowlists(
        crate::providers::secrets::EnvSecrets::from_lookup(["OPENAI_API_KEY".into()], |_| None),
        crate::providers::secrets::EnvSecrets::default(),
    );
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let (token_id, token) = mint(&pool, f.admin.user_id, &["catalog:write"], None).await;
    let body = json!({"name":"Audit probe","provider":"openai","credential_ref":"env:OPENAI_API_KEY","enabled":false});
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/platform/providers")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let request_id: Uuid = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    type AttemptRow = (
        String,
        Uuid,
        Uuid,
        String,
        String,
        Option<String>,
        String,
        Option<i16>,
    );
    let rows: Vec<AttemptRow> = sqlx::query_as(
        "SELECT phase,token_id,owner_user_id,route_method,route_template,required_scope,outcome,status_code FROM admin_token_attempts WHERE request_id=$1 ORDER BY phase",
    )
    .bind(request_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "a start and a completion row");
    let (start, completion) = (&rows[1], &rows[0]); // 'completion' < 'start' alphabetically
    assert_eq!(start.0, "start");
    assert_eq!(completion.0, "completion");
    for row in [start, completion] {
        assert_eq!(row.1, token_id);
        assert_eq!(row.2, f.admin.user_id);
        assert_eq!(row.3, "POST");
        assert_eq!(row.4, "/api/v1/platform/providers");
        assert_eq!(row.5.as_deref(), Some("catalog:write"));
        assert_eq!(row.6, "allowed");
    }
    assert_eq!(start.7, None, "no status before dispatch");
    assert_eq!(completion.7, Some(200));
    // Never the raw body, name or credential anywhere in the row set.
    let serialized = format!("{rows:?}");
    assert!(!serialized.contains("Audit probe"));
    assert!(!serialized.contains(&token));

    // A denied attempt (wrong scope) is audited too, with no status code
    // (the start row IS the completion for a refusal: no dispatch ever
    // happens, so no second row exists).
    let (wrong_id, wrong) = mint(&pool, f.admin.user_id, &["platform:read"], None).await;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/platform/providers")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {wrong}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let denied_request_id: Uuid = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let denied_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT phase,outcome FROM admin_token_attempts WHERE request_id=$1 AND token_id=$2 ORDER BY phase",
    )
    .bind(denied_request_id)
    .bind(wrong_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        denied_rows.len(),
        2,
        "a denial is still a start and a completion"
    );
    assert!(denied_rows.iter().all(|(_, outcome)| outcome == "denied"));
}

/// Audit failure prevents dispatch and commit: when the start record
/// cannot be written, the request never reaches the handler (the business
/// row is never created), and the caller sees a transparent failure, not a
/// silently unaudited success.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn audit_failure_prevents_dispatch(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let (_, token) = mint(&pool, f.admin.user_id, &["catalog:write"], None).await;
    // Simulate an unavailable audit sink: drop the append-only table itself
    // (a disposable per-test database; this models the same "cannot
    // record it" condition a storage outage would).
    sqlx::query("DROP TABLE admin_token_attempts")
        .execute(&pool)
        .await
        .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_connections")
        .fetch_one(&pool)
        .await
        .unwrap();
    let body = json!({"name":"Should never exist","provider":"openai","credential_ref":"env:OPENAI_API_KEY","enabled":false});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/platform/providers")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_connections")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, after, "the handler must never have run");
}

/// A request admitted after the owner's Admin status (and therefore the
/// token itself, per migration 0051's owner-invalidation trigger) is
/// revoked in another transaction must fail: there is no positive
/// authorization cache anywhere in this path. Real PostgreSQL, two
/// connections, no injected principal.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn revocation_committed_between_authentication_and_reuse_is_observed_immediately(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let (_, token) = mint(&pool, f.admin.user_id, &["platform:read"], None).await;
    let probe = || {
        let app = app.clone();
        let token = token.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .uri("/api/v1/platform/providers")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
        }
    };
    assert_eq!(
        probe().await,
        StatusCode::OK,
        "valid while the owner is Admin"
    );
    // The owner's only Admin grant is revoked in a fresh connection/
    // transaction, racing nothing on this one: migration 0051's deferred
    // constraint trigger revokes every token the owner holds at commit.
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(f.admin.user_id)
    .execute(&pool)
    .await
    .unwrap();
    // The token itself is now revoked (live, no cache): the very next
    // request with the identical bearer credential fails immediately,
    // with no further role check even needed.
    assert_eq!(
        probe().await,
        StatusCode::UNAUTHORIZED,
        "revoked immediately after the owner lost Admin, no cache"
    );
    assert!(
        f.s.authenticate_admin_token(&token)
            .await
            .unwrap()
            .is_none(),
        "the live lookup itself must already refuse it"
    );
}

/// Demotion racing an in-flight admin-token request: even if the owner's
/// Admin grant is revoked *after* `authenticate_admin_token` already
/// returned a live principal for this request but *before*
/// `database_access`'s own independent live role check runs, the second
/// check still denies -- nothing upstream of it is trusted as a cache.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn demotion_after_authentication_but_before_the_authority_check_still_denies(pool: PgPool) {
    let f = fixture(&pool).await;
    let (_, token) = mint(&pool, f.admin.user_id, &["platform:read"], None).await;
    // Authenticate once, exactly as identity::authenticate_management does,
    // getting back a live principal (owner still Admin at this instant).
    let principal = f.s.authenticate_admin_token(&token).await.unwrap().unwrap();
    assert_eq!(principal.owner_user_id, f.admin.user_id);
    // Demotion commits in a separate transaction, strictly after that
    // authentication and before the authorization check below -- the
    // in-flight-race shape the approved spec calls out explicitly.
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(f.admin.user_id)
    .execute(&pool)
    .await
    .unwrap();
    // The authorization layer's own independent check (resources::
    // platform_role_snapshot via database_access), run against the
    // already-authenticated principal: it must deny, because it re-reads
    // the database live rather than trusting the principal it was handed.
    let decision = resources::platform_role_snapshot(
        &mut f.s.snapshot().await.unwrap(),
        principal.owner_user_id,
    )
    .await;
    assert!(
        decision.is_err() || decision.unwrap() != "admin",
        "a demoted owner must not still read back as admin"
    );
}

/// Finding 1, end to end on the real served router (not the stub): a GET-
/// only token-ready route answers a real HEAD request with an empty body
/// and the completion row carries the real 200, never the stub-matrix's
/// NOT_IMPLEMENTED symptom this finding described. `route_policy::find`'s
/// own HEAD->GET fallback already resolved the Access/TokenPolicy decision
/// correctly before this fix; only the separate `TOKEN_READY_ROUTES`
/// readiness lookup had its own, un-mirrored exact-method comparison.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn head_on_a_get_only_ready_route_succeeds_with_a_correct_completion(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let (token_id, token) = mint(&pool, f.admin.user_id, &["platform:read"], None).await;
    let response = app
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/api/v1/platform/providers")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a token-ready GET route must answer HEAD too, not 501"
    );
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert!(body.is_empty(), "HEAD must return an empty body");
    let outcome = last_outcome(&pool, token_id, "HEAD", "/api/v1/platform/providers").await;
    assert_eq!(outcome.as_deref(), Some("allowed"));
    let completion_status: Option<i16> = sqlx::query_scalar(
        "SELECT status_code FROM admin_token_attempts WHERE token_id=$1 AND route_method='HEAD' AND phase='completion'",
    )
    .bind(token_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(completion_status, Some(200));
}

/// Finding 4: a database error from the live authorization recheck is
/// reported honestly instead of being collapsed into a misleading 403
/// "denied". Only the table `access_inner`'s live boundary check
/// additionally needs (`workspaces`) is dropped here, not
/// `effective_platform_roles`: dropping that would instead fail
/// *authentication* itself (identity.rs, before this layer ever runs),
/// which is a different fault than the one this test targets -- the
/// authentication step below must still succeed.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn authorization_infrastructure_errors_are_distinguished_from_denials(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let (token_id, token) = mint(&pool, f.admin.user_id, &["platform:read"], None).await;
    assert!(
        f.s.authenticate_admin_token(&token)
            .await
            .unwrap()
            .is_some(),
        "authentication must still work before the table is dropped"
    );
    sqlx::query("DROP TABLE workspaces CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        f.s.authenticate_admin_token(&token)
            .await
            .unwrap()
            .is_some(),
        "authentication never touches `workspaces`, so it must still succeed afterward"
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/workspaces/{}", f.team))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "an infrastructure failure must never be reported as a 403 denial"
    );
    let outcome = last_outcome(&pool, token_id, "GET", "/api/v1/workspaces/{ws}").await;
    assert_eq!(outcome.as_deref(), Some("error"));
}

/// Finding 3: a per-token admission bound holds even when the same token
/// is presented from many different client addresses, which the existing
/// per-IP limiter alone cannot see. A tiny, test-injected per-token rate
/// (no real-minute wait, no injected clock) exhausts deterministically
/// within a handful of in-process requests; the replica-wide bound is set
/// generously high here so only the per-token bound is under test.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn per_token_limiter_bounds_one_token_even_across_many_client_addresses(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock
        .state(pool.clone())
        .await
        .with_admin_token_limits(2, 1_000);
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let (token_id, token) = mint(&pool, f.admin.user_id, &["platform:read"], None).await;
    let mut statuses = Vec::new();
    for i in 0..4u8 {
        let addr: std::net::SocketAddr = format!("203.0.113.{i}:12345").parse().unwrap();
        let mut req = Request::builder()
            .uri("/api/v1/platform/providers")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(addr));
        statuses.push(app.clone().oneshot(req).await.unwrap().status());
    }
    assert_eq!(&statuses[..2], [StatusCode::OK, StatusCode::OK]);
    assert_eq!(
        statuses[2],
        StatusCode::TOO_MANY_REQUESTS,
        "a varying peer address must not bypass the per-token bound"
    );
    assert_eq!(statuses[3], StatusCode::TOO_MANY_REQUESTS);
    // The two throttled attempts never reached `management::authorization`
    // at all: exactly one start+completion pair per admitted request, and
    // nothing for the rejected ones.
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admin_token_attempts WHERE token_id=$1")
            .bind(token_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows, 4,
        "two admitted requests, one start and one completion row each"
    );
}

/// Finding 3: the aggregate per-replica bound applies before a single
/// request is even authenticated, across distinct valid tokens and distinct
/// client addresses alike -- the per-IP and per-token limiters cannot see
/// this shape (many different tokens, each from its own address, none
/// individually over its own limit).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn replica_limiter_bounds_the_aggregate_before_authentication(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock
        .state(pool.clone())
        .await
        .with_admin_token_limits(1_000, 2);
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let mut tokens = Vec::new();
    for _ in 0..3u8 {
        tokens.push(mint(&pool, f.admin.user_id, &["platform:read"], None).await);
    }
    let mut statuses = Vec::new();
    for (i, (_, token)) in tokens.iter().enumerate() {
        let addr: std::net::SocketAddr = format!("203.0.113.{i}:1").parse().unwrap();
        let mut req = Request::builder()
            .uri("/api/v1/platform/providers")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(addr));
        statuses.push(app.clone().oneshot(req).await.unwrap().status());
    }
    assert_eq!(&statuses[..2], [StatusCode::OK, StatusCode::OK]);
    assert_eq!(
        statuses[2],
        StatusCode::TOO_MANY_REQUESTS,
        "the shared replica bound applies across distinct tokens and addresses alike"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_token_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 4,
        "the third (throttled) token never authenticated, so it never produced an attempt row"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn completion_audit_failure_reports_uncertainty_after_committed_mutation(pool: PgPool) {
    let mut f = fixture(&pool).await;
    f.s = f.s.with_secret_allowlists(
        crate::providers::secrets::EnvSecrets::from_lookup(["OPENAI_API_KEY".into()], |_| None),
        crate::providers::secrets::EnvSecrets::default(),
    );
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let (token_id, token) = mint(&pool, f.admin.user_id, &["catalog:write"], None).await;
    // Fail only the post-dispatch insert, not the durable start or the
    // transaction's business audit. This models losing audit availability
    // after the handler has already committed its mutation.
    sqlx::raw_sql(
        "CREATE FUNCTION reject_completion_for_test() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
          IF NEW.phase='completion' THEN RAISE EXCEPTION 'simulated completion audit outage'; END IF;
          RETURN NEW;
         END $$;
         CREATE TRIGGER reject_completion_for_test BEFORE INSERT ON admin_token_attempts
         FOR EACH ROW EXECUTE FUNCTION reject_completion_for_test();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/platform/providers")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    json!({
                        "name":"Completion audit probe", "provider":"openai",
                        "credential_ref":"env:OPENAI_API_KEY", "enabled":false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let created: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provider_connections WHERE name='Completion audit probe'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(created, 1, "the business mutation already committed");
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE admin_token_id=$1 AND action='provider.created'",
    )
    .bind(token_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        audited, 1,
        "business audit remains atomic with the mutation"
    );
    let phases: Vec<String> = sqlx::query_scalar(
        "SELECT phase FROM admin_token_attempts WHERE token_id=$1 ORDER BY phase",
    )
    .bind(token_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        phases,
        ["start"],
        "an incomplete attempt must remain honest"
    );
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("may have completed"), "{message}");
    assert!(!message.contains("was not processed"), "{message}");
    assert!(!message.contains(&token));
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn completion_audit_failure_before_dispatch_reports_no_action(pool: PgPool) {
    let mut f = fixture(&pool).await;
    f.s = f.s.with_secret_allowlists(
        crate::providers::secrets::EnvSecrets::from_lookup(["OPENAI_API_KEY".into()], |_| None),
        crate::providers::secrets::EnvSecrets::default(),
    );
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let (token_id, token) = mint(&pool, f.admin.user_id, &["platform:read"], None).await;
    sqlx::raw_sql(
        "CREATE FUNCTION reject_completion_for_test() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
          IF NEW.phase='completion' THEN RAISE EXCEPTION 'simulated completion audit outage'; END IF;
          RETURN NEW;
         END $$;
         CREATE TRIGGER reject_completion_for_test BEFORE INSERT ON admin_token_attempts
         FOR EACH ROW EXECUTE FUNCTION reject_completion_for_test();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/platform/providers")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    json!({
                        "name":"Denied completion probe", "provider":"openai",
                        "credential_ref":"env:OPENAI_API_KEY", "enabled":false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let created: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provider_connections WHERE name='Denied completion probe'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(created, 0);
    let attempts: Vec<(String, String)> = sqlx::query_as(
        "SELECT phase,outcome FROM admin_token_attempts WHERE token_id=$1 ORDER BY phase",
    )
    .bind(token_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(attempts, [("start".to_owned(), "denied".to_owned())]);
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("was not processed"), "{message}");
    assert!(!message.contains("may have completed"), "{message}");
}
