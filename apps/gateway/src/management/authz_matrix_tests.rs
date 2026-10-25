//! Exercise the served router, including live cookies/entitlement/CSRF, not injected principals.
use super::*;
use crate::{
    identity::tests::MockProvider,
    route_policy::{Access, ROUTES},
};
use sha2::{Digest, Sha256};

#[path = "authz_admin_token_tests.rs"]
mod admin_token_tests;
#[path = "authz_middleware_tests.rs"]
mod isolated;
#[path = "authz_matrix_fixtures.rs"]
mod targets;
const ORIGIN: &str = "http://127.0.0.1:3000";
struct Session {
    cookie: String,
    csrf: String,
}
async fn session(pool: &PgPool, u: &BrowserPrincipal) -> Session {
    let cookie = crate::entropy::hex_token().unwrap();
    let csrf = crate::entropy::hex_token().unwrap();
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email,last_seen_at) VALUES($1,$2,$3,now()+interval '1 hour',$4,now())")
        .bind(Sha256::digest(cookie.as_bytes()).as_slice()).bind(u.user_id)
        .bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(&u.email)
        .execute(pool).await.unwrap();
    Session { cookie, csrf }
}
async fn request(
    app: &Router,
    method: &str,
    path: &str,
    session: Option<&Session>,
    bearer: Option<&str>,
    csrf: bool,
    origin: &str,
) -> StatusCode {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("origin", origin);
    if let Some(s) = session {
        req = req.header("cookie", format!("omg_session={}", s.cookie));
        if csrf {
            req = req.header("x-csrf-token", &s.csrf);
        }
    }
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    app.clone()
        .oneshot(req.body(Body::from("{}")).unwrap())
        .await
        .unwrap()
        .status()
}

/// Every declared workspace route must hide a foreign personal workspace just
/// like an unknown UUID, before handler extraction (including mutations).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn every_workspace_route_hides_foreign_personal_like_unknown(pool: PgPool) {
    let f = fixture(&pool).await;
    let owner = session(&pool, &f.owner).await;
    let (status, key_value) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let targets = targets::Targets::new(&pool, &f, id(&key_value), &owner).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let unknown = Uuid::new_v4();
    let sessions = [
        (session(&pool, &f.outsider).await, "user"),
        (session(&pool, &f.auditor).await, "auditor"),
        (session(&pool, &f.admin).await, "admin"),
    ];
    let mut checked = std::collections::BTreeSet::new();
    let mut failures = Vec::new();
    for route in ROUTES.iter().filter(|r| {
        r.path == "/api/v1/workspaces/{ws}" || r.path.starts_with("/api/v1/workspaces/{ws}/")
    }) {
        let concrete = targets.concrete(route.path, &f);
        let personal_path = concrete.replace(&f.team.to_string(), &f.personal.to_string());
        let unknown_path = concrete.replace(&f.team.to_string(), &unknown.to_string());
        assert!(!personal_path.contains('{'), "unexpanded {}", route.path);
        for method in route.methods.split_whitespace().flat_map(|m| {
            if m == "GET" {
                vec!["GET", "HEAD"]
            } else {
                vec![m]
            }
        }) {
            for (session, label) in &sessions {
                let mut replies = Vec::new();
                for path in [&personal_path, &unknown_path] {
                    let response = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method(method)
                                .uri(path)
                                .header("content-type", "application/json")
                                .header("origin", ORIGIN)
                                .header("cookie", format!("omg_session={}", session.cookie))
                                .header("x-csrf-token", &session.csrf)
                                .body(Body::from("{}"))
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    let status = response.status();
                    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                    replies.push((status, body));
                }
                // Compare the complete body, not only its keys: a distinguishing
                // message or reason would leave the existence oracle intact.
                // Preserve the existing role-first refusal without turning any
                // other unknown-workspace answer into 403 to equalize statuses.
                let expected_status =
                    if route.access == Access::WorkspacePlatformWrite && *label != "admin" {
                        StatusCode::FORBIDDEN
                    } else {
                        StatusCode::NOT_FOUND
                    };
                if replies[0] != replies[1] || replies[1].0 != expected_status {
                    failures.push(format!(
                        "{method} {} {label}: personal={:?}, unknown={:?}",
                        route.path, replies[0], replies[1]
                    ));
                }
                if method == "HEAD" {
                    assert!(replies.iter().all(|(_, body)| body.is_empty()));
                } else {
                    for (_, body) in &replies {
                        let error: Value = serde_json::from_slice(body).unwrap();
                        assert!(
                            error["error"].is_object(),
                            "{method} {} {label}: {error}",
                            route.path
                        );
                    }
                }
                checked.insert((route.path, method, *label));
            }
        }
    }
    let expected: usize = ROUTES
        .iter()
        .filter(|r| {
            r.path == "/api/v1/workspaces/{ws}" || r.path.starts_with("/api/v1/workspaces/{ws}/")
        })
        .map(|r| {
            r.methods
                .split_whitespace()
                .map(|m| if m == "GET" { 2 } else { 1 })
                .sum::<usize>()
        })
        .sum();
    // Positive controls: the same real middleware still admits the personal
    // owner for a representative read and a write, not a blanket refusal.
    assert_eq!(
        request(
            &app,
            "GET",
            &format!("/api/v1/workspaces/{}", f.personal),
            Some(&owner),
            None,
            true,
            ORIGIN
        )
        .await,
        StatusCode::OK
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/workspaces/{}/keys", f.personal))
                .header("content-type", "application/json")
                .header("origin", ORIGIN)
                .header("cookie", format!("omg_session={}", owner.cookie))
                .header("x-csrf-token", &owner.csrf)
                .body(Body::from(
                    json!({"name":"Owner control", "expires_in_days":1}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(expected > 0, "workspace route inventory must not be empty");
    assert_eq!(checked.len(), expected * sessions.len());
    assert!(
        failures.is_empty(),
        "workspace existence oracle:\n{}",
        failures.join("\n")
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_expiry_history_and_feed_use_real_router_authorization(pool: PgPool) {
    let f = fixture(&pool).await;
    let ws_admin = user(&pool, "expiry-workspace-admin", "user").await;
    member(&pool, f.team, ws_admin.user_id, "admin").await;
    for (u, ws) in [
        (&f.member, f.team),
        (&f.owner, f.team),
        (&f.owner, f.personal),
    ] {
        let (status, body) = key(&f, u, ws, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    sqlx::query("UPDATE api_keys SET expires_at=now()+interval '3 days'")
        .execute(&pool)
        .await
        .unwrap();
    let report = crate::alerts::evaluate_once(&f.s).await.unwrap().unwrap();
    assert_eq!((report.fired, report.failed_rules), (3, 0));
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    // Exercise a running gateway over loopback as well as its full middleware.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let app = format!("http://{address}");
    async fn get(app: &str, s: &Session, path: &str) -> (StatusCode, Value) {
        let response = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(format!("{app}{path}"))
            .header("cookie", format!("omg_session={}", s.cookie))
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap())
    }
    let history = format!(
        "/api/v1/workspaces/{}/alerts/events?kind=key_expiring",
        f.team
    );
    let holder = session(&pool, &f.member).await;
    assert_eq!(get(&app, &holder, &history).await.0, StatusCode::FORBIDDEN);
    let (status, feed) = get(&app, &holder, "/api/v1/me/notifications").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        feed["data"].as_array().unwrap().len(),
        1,
        "only the holder's own reminder"
    );
    let holder_event: Uuid = sqlx::query_scalar("SELECT e.id FROM alert_events e JOIN api_keys k ON k.id=e.api_key_id WHERE k.issued_to_user_id=$1")
        .bind(f.member.user_id).fetch_one(&pool).await.unwrap();
    assert_eq!(feed["data"][0]["id"], holder_event.to_string());
    let admin = session(&pool, &ws_admin).await;
    let (status, history_rows) = get(&app, &admin, &history).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        history_rows["data"].as_array().unwrap().len(),
        2,
        "workspace admin sees both shared reminders"
    );
    for u in [&f.admin, &f.auditor] {
        let s = session(&pool, u).await;
        assert_eq!(
            get(
                &app,
                &s,
                &format!(
                    "/api/v1/workspaces/{}/alerts/events?kind=key_expiring",
                    f.personal
                )
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        for path in [
            "/api/v1/me/notifications",
            "/api/v1/platform/alerts/events?kind=key_expiring",
            &history,
        ] {
            let (status, rows) = get(&app, &s, path).await;
            assert_eq!(status, StatusCode::OK, "{path}: {rows}");
            assert_eq!(
                rows["data"],
                json!([]),
                "platform authority never grants reminder visibility"
            );
        }
    }
    server.abort();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn every_route_refuses_lower_principals_and_unverified_mutations(pool: PgPool) {
    let f = fixture(&pool).await;
    // An entitled member of another workspace is not a member of this one.
    member(&pool, f.project, f.outsider.user_id, "member").await;
    let ws_admin = user(&pool, "workspace-admin", "user").await;
    member(&pool, f.team, ws_admin.user_id, "admin").await;
    let unentitled = user(&pool, "unentitled", "user").await;
    let unentitled_session = session(&pool, &unentitled).await;
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(unentitled.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let owner = session(&pool, &f.owner).await;
    let member_session = session(&pool, &f.member).await;
    let outsider = session(&pool, &f.outsider).await;
    let auditor = session(&pool, &f.auditor).await;
    let admin = session(&pool, &f.admin).await;
    let workspace_admin = session(&pool, &ws_admin).await;
    let (status, key_value) = key(&f, &f.owner, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let inference = key_value["token"].as_str().unwrap();
    let targets = targets::Targets::new(&pool, &f, id(&key_value), &owner).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let scim_value = crate::entropy::hex_token().unwrap();
    let scim = crate::scim::ScimConfig::from_lookup(|name| match name {
        "GATEWAY_SCIM_TOKEN_ENV" => Some("AUTHZ_MATRIX_SCIM".to_owned()),
        "AUTHZ_MATRIX_SCIM" => Some(scim_value.clone()),
        _ => None,
    })
    .unwrap();
    let identity = identity.with_scim(scim).unwrap();
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity.clone());
    // A newly registered management route is forbidden even for an admin until declared.
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
            Some(&admin),
            None,
            true,
            ORIGIN
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let sessions = [
        (&owner, "owner"),
        (&member_session, "member"),
        (&outsider, "other-workspace-member"),
        (&auditor, "auditor"),
        (&admin, "admin"),
        (&workspace_admin, "workspace-admin"),
    ];
    let mut checked = 0;
    for route in ROUTES {
        for method in route.methods.split_whitespace().flat_map(|m| {
            if m == "*" {
                vec![
                    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
                ]
            } else if m == "GET" {
                vec!["GET", "HEAD"]
            } else {
                vec![m]
            }
        }) {
            let path = targets.concrete(route.path, &f);
            let access = route.access;
            match access {
                Access::Anonymous | Access::Health => {
                    let public_app = if route.path == "/metrics" {
                        crate::metrics::router(f.s.clone())
                    } else {
                        app.clone()
                    };
                    let status =
                        request(&public_app, method, &path, None, None, false, ORIGIN).await;
                    assert!(
                        !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
                        "public {method} {}: {status}",
                        route.path
                    );
                    continue;
                }
                Access::InferenceKey => {
                    for (s, label) in sessions {
                        let status =
                            request(&app, method, &path, Some(s), None, true, ORIGIN).await;
                        assert_eq!(
                            status,
                            StatusCode::UNAUTHORIZED,
                            "{method} {} browser {label}",
                            route.path
                        );
                    }
                    assert_eq!(
                        request(&app, method, &path, None, None, false, ORIGIN).await,
                        StatusCode::UNAUTHORIZED,
                        "{method} {} anonymous",
                        route.path
                    );
                    assert_eq!(
                        request(&app, method, &path, None, Some(&scim_value), false, ORIGIN).await,
                        StatusCode::UNAUTHORIZED,
                        "{method} {} scim",
                        route.path
                    );
                    continue;
                }
                Access::Scim => {
                    assert_eq!(
                        request(&app, method, &path, None, None, false, ORIGIN).await,
                        StatusCode::UNAUTHORIZED,
                        "{method} {} anonymous",
                        route.path
                    );
                    assert_eq!(
                        request(&app, method, &path, None, Some(inference), false, ORIGIN).await,
                        StatusCode::UNAUTHORIZED,
                        "{method} {} inference",
                        route.path
                    );
                    for (s, label) in sessions {
                        assert_eq!(
                            request(&app, method, &path, Some(s), None, true, ORIGIN).await,
                            StatusCode::UNAUTHORIZED,
                            "{method} {} {label}",
                            route.path
                        );
                    }
                    continue;
                }
                _ => (),
            }
            for (credential, label) in [
                (None, "anonymous"),
                (Some(inference), "inference"),
                (Some(scim_value.as_str()), "scim"),
            ] {
                assert_eq!(
                    request(&app, method, &path, None, credential, false, ORIGIN).await,
                    StatusCode::UNAUTHORIZED,
                    "{method} {} {label}",
                    route.path
                );
            }
            assert_eq!(
                request(
                    &app,
                    method,
                    &path,
                    Some(&unentitled_session),
                    None,
                    true,
                    ORIGIN
                )
                .await,
                StatusCode::UNAUTHORIZED,
                "{method} {} unentitled",
                route.path
            );
            for (s, label) in sessions {
                let lower = match access {
                    Access::PlatformRead => matches!(
                        label,
                        "owner" | "member" | "other-workspace-member" | "workspace-admin"
                    ),
                    Access::PlatformWrite | Access::WorkspacePlatformWrite => label != "admin",
                    Access::WorkspaceMetadata => label == "other-workspace-member",
                    Access::WorkspaceMember => {
                        matches!(label, "other-workspace-member" | "auditor" | "admin")
                            || (label == "member"
                                && matches!(
                                    route.path,
                                    "/api/v1/workspaces/{ws}/files/{id}"
                                        | "/api/v1/workspaces/{ws}/files/{id}/content"
                                        | "/api/v1/workspaces/{ws}/batches/{id}"
                                        | "/api/v1/workspaces/{ws}/batches/{id}/cancel"
                                        | "/api/v1/workspaces/{ws}/requests/{root}"
                                        | "/api/v1/workspaces/{ws}/sessions/{session}"
                                ))
                    }
                    Access::WorkspaceKey => !matches!(label, "owner" | "workspace-admin"),
                    Access::WorkspaceKeyRotate => label != "owner",
                    Access::WorkspaceAdmin => !matches!(label, "owner" | "workspace-admin"),
                    Access::WorkspaceAdminOrReader => {
                        matches!(label, "member" | "other-workspace-member")
                    }
                    Access::Browser => {
                        route.path == "/api/v1/me/sessions/{session}" && label != "owner"
                    }
                    Access::BrowserLogout => false,
                    _ => unreachable!(),
                };
                if lower {
                    let status = request(&app, method, &path, Some(s), None, true, ORIGIN).await;
                    let hidden_object = (access == Access::WorkspaceKey && label == "member")
                        || (access == Access::WorkspaceKeyRotate && label == "member")
                        || (access == Access::WorkspaceMember && label == "member")
                        || (access == Access::Browser
                            && route.path == "/api/v1/me/sessions/{session}");
                    assert_eq!(
                        status,
                        if hidden_object {
                            StatusCode::NOT_FOUND
                        } else {
                            StatusCode::FORBIDDEN
                        },
                        "{method} {} {label}",
                        route.path
                    );
                }
            }
            if matches!(
                access,
                Access::WorkspaceMetadata
                    | Access::WorkspaceMember
                    | Access::WorkspaceKey
                    | Access::WorkspaceKeyRotate
                    | Access::WorkspaceAdmin
                    | Access::WorkspaceAdminOrReader
                    | Access::WorkspacePlatformWrite
            ) {
                let personal_path = path.replace(&f.team.to_string(), &f.personal.to_string());
                for (s, label) in sessions.into_iter().filter(|(_, label)| *label != "owner") {
                    assert_eq!(
                        request(&app, method, &personal_path, Some(s), None, true, ORIGIN).await,
                        if access == Access::WorkspacePlatformWrite && label != "admin" {
                            StatusCode::FORBIDDEN
                        } else {
                            StatusCode::NOT_FOUND
                        },
                        "{method} {} foreign personal {label}",
                        route.path
                    );
                }
                // Team membership/adminship does not grant the sibling Project.
                let project_path = path.replace(&f.team.to_string(), &f.project.to_string());
                for s in [&member_session, &workspace_admin] {
                    assert_eq!(
                        request(&app, method, &project_path, Some(s), None, true, ORIGIN).await,
                        StatusCode::FORBIDDEN,
                        "{method} {} sibling project",
                        route.path
                    );
                }
            }
            if !matches!(method, "GET" | "HEAD") {
                let required = match access {
                    Access::PlatformRead
                    | Access::PlatformWrite
                    | Access::WorkspacePlatformWrite => &admin,
                    _ => &owner,
                };
                assert_eq!(
                    request(&app, method, &path, Some(required), None, false, ORIGIN).await,
                    StatusCode::FORBIDDEN,
                    "{method} {} no CSRF",
                    route.path
                );
                assert_eq!(
                    request(
                        &app,
                        method,
                        &path,
                        Some(required),
                        None,
                        true,
                        "https://wrong.example"
                    )
                    .await,
                    StatusCode::FORBIDDEN,
                    "{method} {} wrong Origin",
                    route.path
                );
            }
            checked += 1;
        }
    }
    assert!(
        checked > 200,
        "the matrix must exercise the complete management surface"
    );
    // A non-owner workspace admin cannot remove the actual owner.
    assert_eq!(
        request(
            &app,
            "DELETE",
            &format!("/api/v1/workspaces/{}/members/{}", f.team, f.owner.user_id),
            Some(&workspace_admin),
            None,
            true,
            ORIGIN
        )
        .await,
        StatusCode::FORBIDDEN
    );
    // Positive cases for each boundary: real handler responses, never blanket denial.
    for (method, path, s, expected) in [
        ("GET", "/api/v1/me", &owner, StatusCode::OK),
        (
            "POST",
            "/api/v1/auth/logout",
            &outsider,
            StatusCode::NO_CONTENT,
        ),
        ("GET", "/api/v1/platform/users", &auditor, StatusCode::OK),
        (
            "GET",
            "/api/v1/platform/installation/policy",
            &auditor,
            StatusCode::GONE,
        ),
        (
            "PUT",
            "/api/v1/platform/installation/policy",
            &admin,
            StatusCode::GONE,
        ),
        (
            "POST",
            "/api/v1/platform/models",
            &admin,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("GET", "/api/v1/workspaces/{ws}", &auditor, StatusCode::OK),
        (
            "POST",
            "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
            &admin,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "GET",
            "/api/v1/workspaces/{ws}/keys",
            &member_session,
            StatusCode::OK,
        ),
        (
            "GET",
            "/api/v1/workspaces/{ws}/keys/{key}",
            &owner,
            StatusCode::OK,
        ),
        (
            "POST",
            "/api/v1/workspaces/{ws}/keys/{key}/rotate",
            &owner,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "GET",
            "/api/v1/workspaces/{ws}/service-accounts",
            &workspace_admin,
            StatusCode::OK,
        ),
        (
            "GET",
            "/api/v1/workspaces/{ws}/members",
            &auditor,
            StatusCode::OK,
        ),
    ] {
        assert_eq!(
            request(
                &app,
                method,
                &targets.concrete(path, &f),
                Some(s),
                None,
                true,
                ORIGIN
            )
            .await,
            expected,
            "positive {method} {path}"
        );
    }
    assert_eq!(
        request(
            &app,
            "GET",
            "/v1/models",
            None,
            Some(inference),
            false,
            ORIGIN
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/scim/v2/ServiceProviderConfig",
            None,
            Some(&scim_value),
            false,
            ORIGIN
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        request(&app, "GET", "/health/live", None, None, false, ORIGIN).await,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &app,
            "GET",
            "/api/v1/auth/config",
            None,
            None,
            false,
            ORIGIN
        )
        .await,
        StatusCode::OK
    );
}
