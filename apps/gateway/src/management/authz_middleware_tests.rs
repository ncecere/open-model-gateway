//! Exact expectations for the two management middleware layers, independent of handlers.
use super::*;
use axum::routing::{MethodFilter, MethodRouter, any, on};

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
enum Actor {
    Anonymous,
    Inference,
    Unentitled,
    User,
    Auditor,
    Admin,
    NonMember,
    KeyHolder,
    OtherMember,
    WorkspaceAdmin,
    Owner,
    PersonalOwner,
    PersonalAuditor,
    PersonalAdmin,
    // An otherwise-authorized principal for this Access, with the token omitted.
    NoCsrf,
}
const ACTORS: [Actor; 15] = [
    Actor::Anonymous,
    Actor::Inference,
    Actor::Unentitled,
    Actor::User,
    Actor::Auditor,
    Actor::Admin,
    Actor::NonMember,
    Actor::KeyHolder,
    Actor::OtherMember,
    Actor::WorkspaceAdmin,
    Actor::Owner,
    Actor::PersonalOwner,
    Actor::PersonalAuditor,
    Actor::PersonalAdmin,
    Actor::NoCsrf,
];
// Each cell is (GET/HEAD, mutation). 204 proves the stub was reached.
const A: (u16, u16) = (204, 204);
const U: (u16, u16) = (401, 401);
const F: (u16, u16) = (403, 403);
const N: (u16, u16) = (404, 404);
const C: (u16, u16) = (204, 403);
// One independently written Access × Actor table. External route classes are
// intentionally forbidden if accidentally mounted behind management middleware;
// their real routers' credential families remain covered by the handler matrix.
const EXPECTED: &[(Access, [(u16, u16); 15])] = &[
    (
        Access::Anonymous,
        [U, U, U, F, F, F, F, F, F, F, F, F, F, F, F],
    ),
    (
        Access::Health,
        [U, U, U, F, F, F, F, F, F, F, F, F, F, F, F],
    ),
    (
        Access::InferenceKey,
        [U, U, U, F, F, F, F, F, F, F, F, F, F, F, F],
    ),
    (Access::Scim, [U, U, U, F, F, F, F, F, F, F, F, F, F, F, F]),
    (
        Access::BrowserLogout,
        [U, U, U, F, F, F, F, F, F, F, F, F, F, F, F],
    ),
    (
        Access::Browser,
        [U, U, U, A, A, A, A, A, A, A, A, A, A, A, C],
    ),
    (
        Access::PlatformRead,
        [U, U, U, F, A, A, F, F, F, F, F, F, A, A, C],
    ),
    (
        Access::PlatformWrite,
        [U, U, U, F, F, A, F, F, F, F, F, F, F, A, C],
    ),
    (
        Access::WorkspaceMetadata,
        [U, U, U, F, A, A, F, A, A, A, A, A, F, F, C],
    ),
    (
        Access::WorkspaceMember,
        [U, U, U, F, F, F, F, A, A, A, A, A, F, F, C],
    ),
    (
        Access::WorkspaceAdmin,
        [U, U, U, F, F, F, F, F, F, A, A, A, F, F, C],
    ),
    (
        Access::WorkspaceAdminOrReader,
        [U, U, U, F, A, A, F, F, F, A, A, A, F, F, C],
    ),
    (
        Access::WorkspacePlatformWrite,
        [U, U, U, F, F, A, F, F, F, F, F, F, F, F, C],
    ),
    (
        Access::WorkspaceKey,
        [U, U, U, F, F, F, F, A, N, A, A, A, F, F, C],
    ),
    (
        Access::WorkspaceKeyRotate,
        // Workspace admins and owners see every key: refused rotation is 403.
        [U, U, U, F, F, F, F, A, N, F, F, A, F, F, C],
    ),
];
fn expected(access: Access, actor: Actor, method: &str) -> StatusCode {
    let row = &EXPECTED
        .iter()
        .find(|(a, _)| *a == access)
        .expect("missing expected Access row")
        .1;
    let (safe, mutation) = row[actor as usize];
    StatusCode::from_u16(if matches!(method, "GET" | "HEAD") {
        safe
    } else {
        mutation
    })
    .unwrap()
}
fn methods(route: &crate::route_policy::Route) -> Vec<&str> {
    route
        .methods
        .split_whitespace()
        .flat_map(|m| match m {
            "*" => vec![
                "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "TRACE", "CONNECT",
                "PROPFIND",
            ],
            "GET" => vec!["GET", "HEAD"],
            _ => vec![m],
        })
        .collect()
}
fn stub_routes() -> Router<Store> {
    let mut app = Router::new();
    for route in ROUTES {
        for method in route.methods.split_whitespace() {
            let mr: MethodRouter<Store> = if method == "*" {
                any(|| async { StatusCode::NO_CONTENT })
            } else {
                on(
                    MethodFilter::try_from(method.parse::<axum::http::Method>().unwrap()).unwrap(),
                    || async { StatusCode::NO_CONTENT },
                )
            };
            app = app.route(route.path, mr);
        }
    }
    app
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_writer_checks_admin_before_missing_workspace(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = stub_routes()
        .route_layer(middleware::from_fn_with_state(
            f.s.clone(),
            authorization::require_route_access,
        ))
        .route_layer(middleware::from_fn_with_state(identity, require_session))
        .with_state(f.s.clone());
    let path = format!(
        "/api/v1/workspaces/{}/costs/{}/reconcile",
        Uuid::nil(),
        Uuid::nil()
    );
    for principal in [&f.owner, &f.member, &f.auditor] {
        let cookie = session(&pool, principal).await;
        assert_eq!(
            request(&app, "POST", &path, Some(&cookie), None, true, ORIGIN).await,
            StatusCode::FORBIDDEN
        );
    }
    let admin = session(&pool, &f.admin).await;
    assert_eq!(
        request(&app, "POST", &path, Some(&admin), None, true, ORIGIN).await,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn service_keys_require_admin_in_middleware(pool: PgPool) {
    let f = fixture(&pool).await;
    let (_, account) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/service-accounts", f.team),
        json!({"name":"Service"}),
    )
    .await;
    let (status, service) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({"name":"Service key","expires_in_days":1,"service_account_id":id(&account)}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let administrator = user(&pool, "service-key-admin", "user").await;
    member(&pool, f.team, administrator.user_id, "admin").await;
    let admin = session(&pool, &administrator).await;
    let plain = session(&pool, &f.member).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = stub_routes()
        .route_layer(middleware::from_fn_with_state(
            f.s.clone(),
            authorization::require_route_access,
        ))
        .route_layer(middleware::from_fn_with_state(identity, require_session))
        .with_state(f.s.clone());
    for route in ROUTES
        .iter()
        .filter(|r| matches!(r.access, Access::WorkspaceKey | Access::WorkspaceKeyRotate))
    {
        let path = route
            .path
            .replace("{ws}", &f.team.to_string())
            .replace("{key}", &id(&service).to_string());
        // As on main: a plain member gets 403 for rotation (the handler's
        // `manage`) and 404 on the key-policy routes (`lineage`).
        let refused = if route.access == Access::WorkspaceKeyRotate {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::NOT_FOUND
        };
        for method in methods(route) {
            for (cookie, expected) in [(&admin, StatusCode::NO_CONTENT), (&plain, refused)] {
                assert_eq!(
                    request(&app, method, &path, Some(cookie), None, true, ORIGIN).await,
                    expected,
                    "service key {method} {}",
                    route.path
                );
            }
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn snapshot_and_locked_workspace_authority_agree(pool: PgPool) {
    let f = fixture(&pool).await;
    let unentitled = user(&pool, "unentitled-parity", "user").await;
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(unentitled.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for disabled in [false, true] {
        if disabled {
            sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=ANY($1)")
                .bind(vec![f.team, f.personal])
                .execute(&pool)
                .await
                .unwrap();
        }
        for user in [
            &f.owner,
            &f.member,
            &f.outsider,
            &f.auditor,
            &f.admin,
            &unentitled,
        ] {
            for ws in [f.team, f.project, f.personal, Uuid::nil()] {
                for allow_disabled in [false, true] {
                    let locked = match if allow_disabled {
                        resources::workspace_read_tx(&f.s, user, ws).await
                    } else {
                        resources::workspace_tx(&f.s, user, ws).await
                    } {
                        Ok((tx, a)) => {
                            tx.rollback().await.unwrap();
                            Ok(a)
                        }
                        Err(e) => Err(e.0),
                    };
                    let mut tx = f.s.snapshot().await.unwrap();
                    let snapshot =
                        resources::workspace_boundary_snapshot(&mut tx, user, ws, allow_disabled)
                            .await
                            .map_err(|e| e.0);
                    tx.rollback().await.unwrap();
                    assert_eq!(
                        snapshot, locked,
                        "disabled={disabled}, allow_disabled={allow_disabled}, workspace={ws}"
                    );
                }
            }
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn every_route_middleware_has_exact_positive_and_negative_outcomes(pool: PgPool) {
    let f = fixture(&pool).await;
    member(&pool, f.project, f.outsider.user_id, "member").await;
    let plain = user(&pool, "plain-user", "user").await;
    let ws_admin = user(&pool, "workspace-admin", "user").await;
    member(&pool, f.team, ws_admin.user_id, "admin").await;
    let other_member = user(&pool, "other-member", "user").await;
    member(&pool, f.team, other_member.user_id, "member").await;
    let no_role = user(&pool, "no-role", "user").await;
    let no_role_session = session(&pool, &no_role).await;
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(no_role.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let owner = session(&pool, &f.owner).await;
    let holder = session(&pool, &f.member).await;
    let other = session(&pool, &other_member).await;
    let user_session = session(&pool, &plain).await;
    let outsider = session(&pool, &f.outsider).await;
    let auditor = session(&pool, &f.auditor).await;
    let admin = session(&pool, &f.admin).await;
    let administrator = session(&pool, &ws_admin).await;
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, holder_key) = key(&f, &f.member, f.team, Value::Null).await;
    let (_, personal_key) = key(&f, &f.owner, f.personal, Value::Null).await;
    let targets = targets::Targets::new(&pool, &f, id(&owner_key), &owner).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = stub_routes()
        .route_layer(middleware::from_fn_with_state(
            f.s.clone(),
            authorization::require_route_access,
        ))
        .route_layer(middleware::from_fn_with_state(identity, require_session))
        .with_state(f.s.clone());
    for route in ROUTES {
        for method in methods(route) {
            for actor in ACTORS {
                let personal = matches!(
                    actor,
                    Actor::PersonalOwner | Actor::PersonalAuditor | Actor::PersonalAdmin
                );
                let mut path = targets.concrete(route.path, &f);
                path = path.replace(&id(&owner_key).to_string(), &id(&holder_key).to_string());
                if personal {
                    path = path
                        .replace(&f.team.to_string(), &f.personal.to_string())
                        .replace(&id(&holder_key).to_string(), &id(&personal_key).to_string());
                }
                let cookie = match actor {
                    Actor::Anonymous | Actor::Inference => None,
                    Actor::Unentitled => Some(&no_role_session),
                    Actor::User => Some(&user_session),
                    Actor::Auditor | Actor::PersonalAuditor => Some(&auditor),
                    Actor::Admin | Actor::PersonalAdmin => Some(&admin),
                    Actor::NonMember => Some(&outsider),
                    Actor::NoCsrf => Some(match route.access {
                        Access::PlatformRead
                        | Access::PlatformWrite
                        | Access::WorkspacePlatformWrite => &admin,
                        Access::WorkspaceAdmin | Access::WorkspaceAdminOrReader => &administrator,
                        _ => &holder,
                    }),
                    Actor::KeyHolder => Some(&holder),
                    Actor::OtherMember => Some(&other),
                    Actor::WorkspaceAdmin => Some(&administrator),
                    Actor::Owner | Actor::PersonalOwner => Some(&owner),
                };
                let bearer =
                    matches!(actor, Actor::Inference).then(|| owner_key["token"].as_str().unwrap());
                let actual = request(
                    &app,
                    method,
                    &path,
                    cookie,
                    bearer,
                    !matches!(actor, Actor::NoCsrf),
                    ORIGIN,
                )
                .await;
                assert_eq!(
                    actual,
                    expected(route.access, actor, method),
                    "{method} {} {:?} {actor:?}",
                    route.path,
                    route.access
                );
            }
        }
    }
    // Foreign human rotations: a non-admin member never learns the key exists
    // (404 in every state); workspace admins and owners, who see every key in
    // the list, get 403.
    for state in ["active", "revoked", "expired", "disabled"] {
        let generated = crate::auth::NewApiKey::generate().unwrap();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash,governance_key_id,revoked_at,expires_at,disabled_at) VALUES($1,$2,$3,'State probe',$4,$1,CASE WHEN $5 THEN now() END,CASE WHEN $6 THEN now()-interval '1 minute' END,CASE WHEN $7 THEN now() END)")
            .bind(generated.id).bind(f.team).bind(f.member.user_id).bind(generated.digest.as_slice())
            .bind(state=="revoked").bind(state=="expired").bind(state=="disabled").execute(&pool).await.unwrap();
        for (cookie, expected) in [
            (&other, StatusCode::NOT_FOUND),
            (&administrator, StatusCode::FORBIDDEN),
            (&owner, StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                request(
                    &app,
                    "POST",
                    &format!("/api/v1/workspaces/{}/keys/{}/rotate", f.team, generated.id),
                    Some(cookie),
                    None,
                    true,
                    ORIGIN
                )
                .await,
                expected,
                "foreign human key state {state}"
            );
        }
    }
    // Metadata authority never satisfies key membership. 403 is independent of existence.
    for key in [id(&owner_key), Uuid::new_v4()] {
        for method in ["GET", "PUT"] {
            assert_eq!(
                request(
                    &app,
                    method,
                    &format!("/api/v1/workspaces/{}/keys/{key}/policy", f.team),
                    Some(&auditor),
                    None,
                    true,
                    ORIGIN
                )
                .await,
                StatusCode::FORBIDDEN
            );
        }
    }
    // Explicit missing/disabled contexts, independent of the live decision.
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(f.project)
        .execute(&pool)
        .await
        .unwrap();
    for (method, template, cookie, missing_status, disabled_status) in [
        (
            "GET",
            "/api/v1/workspaces/{ws}/keys",
            &holder,
            StatusCode::NOT_FOUND,
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            "/api/v1/workspaces/{ws}/access",
            &holder,
            StatusCode::NOT_FOUND,
            StatusCode::NOT_FOUND,
        ),
        (
            "GET",
            "/api/v1/workspaces/{ws}/access",
            &auditor,
            StatusCode::NOT_FOUND,
            StatusCode::NO_CONTENT,
        ),
        (
            "GET",
            "/api/v1/workspaces/{ws}/access",
            &admin,
            StatusCode::NOT_FOUND,
            StatusCode::NO_CONTENT,
        ),
        (
            "GET",
            "/api/v1/workspaces/{ws}/members",
            &admin,
            StatusCode::NOT_FOUND,
            StatusCode::NO_CONTENT,
        ),
        (
            "GET",
            "/api/v1/workspaces/{ws}/service-accounts",
            &administrator,
            StatusCode::NOT_FOUND,
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
            &holder,
            StatusCode::FORBIDDEN,
            StatusCode::FORBIDDEN,
        ),
        (
            "POST",
            "/api/v1/workspaces/{ws}/costs/{execution}/reconcile",
            &admin,
            StatusCode::NOT_FOUND,
            StatusCode::NOT_FOUND,
        ),
    ] {
        for (workspace, status) in [(Uuid::nil(), missing_status), (f.project, disabled_status)] {
            let path = targets
                .concrete(template, &f)
                .replace(&f.team.to_string(), &workspace.to_string());
            assert_eq!(
                request(&app, method, &path, Some(cookie), None, true, ORIGIN).await,
                status,
                "workspace context {method} {template}"
            );
        }
    }
    // Malformed route identifiers are missing objects, not authority failures.
    for route in ROUTES.iter().filter(|r| {
        r.path.contains("{ws}") && !matches!(r.access, Access::PlatformRead | Access::PlatformWrite)
    }) {
        for method in methods(route) {
            let path = targets
                .concrete(route.path, &f)
                .replace(&f.team.to_string(), "not-a-uuid");
            assert_eq!(
                request(&app, method, &path, Some(&admin), None, true, ORIGIN).await,
                StatusCode::NOT_FOUND,
                "malformed workspace {method} {}",
                route.path
            );
        }
    }
    for route in ROUTES.iter().filter(|r| r.path.contains("{key}")) {
        for method in methods(route) {
            let path = targets
                .concrete(route.path, &f)
                .replace(&id(&owner_key).to_string(), "not-a-uuid");
            assert_eq!(
                request(&app, method, &path, Some(&holder), None, true, ORIGIN).await,
                StatusCode::NOT_FOUND,
                "malformed key {method} {}",
                route.path
            );
        }
    }
}
