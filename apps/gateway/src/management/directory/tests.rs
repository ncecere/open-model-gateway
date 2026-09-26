use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use sqlx::PgPool;
use tower::ServiceExt;

struct Fixture {
    store: Store,
    org: Uuid,
    foreign_org: Uuid,
    team: Uuid,
    other_team: Uuid,
    personal: Uuid,
    owner: BrowserPrincipal,
    admin: BrowserPrincipal,
    team_admin: BrowserPrincipal,
    member: BrowserPrincipal,
    outsider: BrowserPrincipal,
    operator: BrowserPrincipal,
}

async fn principal(pool: &PgPool, name: &str, operator: bool) -> BrowserPrincipal {
    let user = BrowserPrincipal {
        user_id: Uuid::new_v4(),
        email: format!("{name}@example.invalid"),
        platform_admin: operator,
    };
    sqlx::query("INSERT INTO users(id,email,platform_admin) VALUES($1,$2,$3)")
        .bind(user.user_id)
        .bind(&user.email)
        .bind(operator)
        .execute(pool)
        .await
        .unwrap();
    user
}

async fn fixture(pool: &PgPool) -> Fixture {
    let owner = principal(pool, "owner", false).await;
    let admin = principal(pool, "admin", false).await;
    let team_admin = principal(pool, "team-admin", false).await;
    let member = principal(pool, "member", false).await;
    let outsider = principal(pool, "outsider", false).await;
    let operator = principal(pool, "operator", true).await;
    let org = Uuid::new_v4();
    let foreign_org = Uuid::new_v4();
    for (id, name) in [(org, "Alpha"), (foreign_org, "Beta")] {
        sqlx::query("INSERT INTO organizations(id,name,slug) VALUES($1,$2,$2)")
            .bind(id)
            .bind(name)
            .execute(pool)
            .await
            .unwrap();
    }
    for (user, role) in [
        (&owner, "owner"),
        (&admin, "admin"),
        (&team_admin, "member"),
        (&member, "member"),
    ] {
        sqlx::query(
            "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,$3)",
        )
        .bind(org)
        .bind(user.user_id)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'owner')",
    )
    .bind(foreign_org)
    .bind(outsider.user_id)
    .execute(pool)
    .await
    .unwrap();
    let team = Uuid::new_v4();
    let other_team = Uuid::new_v4();
    for (id, name) in [(team, "Alpha team"), (other_team, "Beta team")] {
        sqlx::query("INSERT INTO workspaces(id,organization_id,name,kind) VALUES($1,$2,$3,'team')")
            .bind(id)
            .bind(org)
            .bind(name)
            .execute(pool)
            .await
            .unwrap();
    }
    for (user, role) in [(&team_admin, "admin"), (&member, "member")] {
        sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,$4)")
            .bind(org).bind(team).bind(user.user_id).bind(role).execute(pool).await.unwrap();
    }
    let personal = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,organization_id,name,kind,owner_user_id) VALUES($1,$2,'PRIVATE PERSONAL NAME','personal',$3)")
        .bind(personal).bind(org).bind(owner.user_id).execute(pool).await.unwrap();
    Fixture {
        store: Store::new(pool.clone()),
        org,
        foreign_org,
        team,
        other_team,
        personal,
        owner,
        admin,
        team_admin,
        member,
        outsider,
        operator,
    }
}

async fn call(
    s: &Store,
    u: &BrowserPrincipal,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    // Parent routing also checks that the GET /orgs merge preserves POST.
    // Session/CSRF middleware is tested separately in the identity suite.
    let response = super::super::routes()
        .layer(Extension(u.clone()))
        .with_state(s.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get(f: &Fixture, user: &BrowserPrincipal, path: &str) -> Value {
    let (status, value) = call(&f.store, user, "GET", path, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{path}: {value}");
    value
}

fn fields(value: &Value, expected: &[&str]) {
    let object = value.as_object().unwrap();
    assert_eq!(object.len(), expected.len(), "{value}");
    assert!(expected.iter().all(|key| object.contains_key(*key)));
}

#[sqlx::test]
async fn directories_apply_roles_tenant_filters_and_personal_privacy(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = format!("/api/v1/orgs/{}/teams", f.org);
    for (user, count, role) in [
        (&f.owner, 2, "owner"),
        (&f.admin, 2, "admin"),
        (&f.team_admin, 1, "admin"),
        (&f.member, 1, "member"),
        (&f.operator, 2, "owner"),
    ] {
        let value = get(&f, user, &path).await;
        let teams = value["data"].as_array().unwrap();
        assert_eq!(teams.len(), count);
        let me = get(&f, user, "/api/v1/me").await;
        for team in teams {
            fields(team, &["id", "organization_id", "name", "kind", "role"]);
            assert_eq!(team["kind"], "team");
            assert_eq!(team["role"], role);
            assert_eq!(team["organization_id"], f.org.to_string());
            let matching = me["workspaces"]
                .as_array()
                .unwrap()
                .iter()
                .find(|ws| ws["id"] == team["id"])
                .unwrap();
            assert_eq!(matching["role"], team["role"]);
        }
        assert!(!value.to_string().contains("PRIVATE PERSONAL NAME"));
        assert!(!value.to_string().contains(&f.personal.to_string()));
    }
    for (user, org) in [(&f.outsider, f.org), (&f.owner, f.foreign_org)] {
        assert_eq!(
            call(
                &f.store,
                user,
                "GET",
                &format!("/api/v1/orgs/{org}/teams"),
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let mine = get(&f, &f.member, "/api/v1/orgs").await;
    assert_eq!(mine["data"].as_array().unwrap().len(), 1);
    assert_eq!(mine["data"][0]["id"], f.org.to_string());
    assert_eq!(mine["data"][0]["membership_role"], "member");
    fields(
        &mine["data"][0],
        &[
            "id",
            "name",
            "slug",
            "role",
            "membership_role",
            "created_at",
        ],
    );
    let operator_orgs = get(&f, &f.operator, "/api/v1/orgs").await;
    assert_eq!(operator_orgs["data"].as_array().unwrap().len(), 2);
    for org in operator_orgs["data"].as_array().unwrap() {
        assert_eq!(org["role"], "operator");
        assert!(org["membership_role"].is_null());
    }
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'member')",
    )
    .bind(f.org)
    .bind(f.operator.user_id)
    .execute(&pool)
    .await
    .unwrap();
    let actual = get(&f, &f.operator, "/api/v1/orgs").await;
    assert_eq!(actual["data"][0]["role"], "operator");
    assert_eq!(actual["data"][0]["membership_role"], "member");
    // A workspace owner still wins over organization-admin, exactly as /me.
    sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,'owner')")
        .bind(f.org).bind(f.team).bind(f.admin.user_id).execute(&pool).await.unwrap();
    assert_eq!(get(&f, &f.admin, &path).await["data"][0]["role"], "owner");
}

#[sqlx::test]
async fn inactive_orgs_teams_and_memberships_are_not_listed(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = format!("/api/v1/orgs/{}/teams", f.org);
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(f.other_team)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get(&f, &f.operator, &path).await["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    sqlx::query("UPDATE workspace_memberships SET disabled_at=now() WHERE user_id=$1")
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        get(&f, &f.member, &path).await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    sqlx::query("UPDATE organization_memberships SET disabled_at=now() WHERE user_id=$1")
        .bind(f.team_admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        get(&f, &f.team_admin, "/api/v1/orgs").await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        call(&f.store, &f.team_admin, "GET", &path, json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE organizations SET disabled_at=now() WHERE id=$1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let orgs = get(&f, &f.operator, "/api/v1/orgs").await;
    assert_eq!(orgs["data"].as_array().unwrap().len(), 1);
    assert_eq!(orgs["data"][0]["id"], f.foreign_org.to_string());
    assert_eq!(
        call(&f.store, &f.operator, "GET", &path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn user_directory_is_operator_only_minimal_readonly_and_rechecks_database(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = "/api/v1/platform/users";
    for user in [&f.owner, &f.admin, &f.team_admin, &f.member, &f.outsider] {
        assert_eq!(
            call(&f.store, user, "GET", path, json!({})).await.0,
            StatusCode::FORBIDDEN
        );
    }
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let users = get(&f, &f.operator, path).await;
    assert_eq!(users["data"].as_array().unwrap().len(), 6);
    for user in users["data"].as_array().unwrap() {
        fields(
            user,
            &["id", "email", "platform_admin", "disabled_at", "created_at"],
        );
    }
    assert!(!users.to_string().contains(&f.personal.to_string()));
    assert!(!users.to_string().contains("PRIVATE PERSONAL NAME"));
    assert!(
        users["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|user| user["id"] == f.outsider.user_id.to_string()
                && !user["disabled_at"].is_null())
    );
    for method in ["POST", "PATCH", "DELETE"] {
        assert_eq!(
            call(
                &f.store,
                &f.operator,
                method,
                path,
                json!({"email":"new@example.invalid","platform_admin":true})
            )
            .await
            .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
    let forged = BrowserPrincipal {
        platform_admin: true,
        ..f.member.clone()
    };
    assert_eq!(
        call(&f.store, &forged, "GET", path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
        .bind(f.operator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.store, &f.operator, "GET", path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE users SET platform_admin=true,disabled_at=now() WHERE id=$1")
        .bind(f.operator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.store, &f.operator, "GET", path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audits, 0);
}

#[sqlx::test]
async fn platform_teams_list_all_orgs_filter_and_paginate_without_private_data(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = "/api/v1/platform/teams";
    let foreign_team = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workspaces(id,organization_id,name,kind) VALUES($1,$2,'Alpha team','team')",
    )
    .bind(foreign_team)
    .bind(f.foreign_org)
    .execute(&pool)
    .await
    .unwrap();
    let all = get(&f, &f.operator, path).await;
    assert_eq!(
        all,
        json!({"data":[
            {"id":f.team,"organization_id":f.org,"organization_name":"Alpha","name":"Alpha team","kind":"team","role":"operator"},
            {"id":f.other_team,"organization_id":f.org,"organization_name":"Alpha","name":"Beta team","kind":"team","role":"operator"},
            {"id":foreign_team,"organization_id":f.foreign_org,"organization_name":"Beta","name":"Alpha team","kind":"team","role":"operator"}
        ]})
    );
    for offset in 0..3 {
        let page = get(&f, &f.operator, &format!("{path}?limit=1&offset={offset}")).await;
        assert_eq!(page["data"], json!([all["data"][offset]]));
    }
    for (org, expected) in [
        (f.org, json!([all["data"][0], all["data"][1]])),
        (f.foreign_org, json!([all["data"][2]])),
        (Uuid::new_v4(), json!([])),
    ] {
        let filtered = get(&f, &f.operator, &format!("{path}?organization_id={org}")).await;
        assert_eq!(filtered["data"], expected);
    }
    let page = get(
        &f,
        &f.operator,
        &format!("{path}?organization_id={}&limit=1&offset=1", f.org),
    )
    .await;
    assert_eq!(page["data"], json!([all["data"][1]]));
    for query in ["organization_id=not-a-uuid", "organization_id="] {
        assert_eq!(
            call(
                &f.store,
                &f.operator,
                "GET",
                &format!("{path}?{query}"),
                json!({})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    // Identical names still have a stable UUID tie-breaker, including pages.
    sqlx::query("UPDATE workspaces SET name='Alpha team' WHERE id=$1")
        .bind(f.other_team)
        .execute(&pool)
        .await
        .unwrap();
    let mut ids = [f.team, f.other_team];
    ids.sort();
    for (offset, id) in ids.iter().enumerate() {
        let page = get(
            &f,
            &f.operator,
            &format!("{path}?organization_id={}&limit=1&offset={offset}", f.org),
        )
        .await;
        assert_eq!(page["data"][0]["id"], id.to_string());
    }
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(f.other_team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE organizations SET disabled_at=now() WHERE id=$1")
        .bind(f.foreign_org)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get(&f, &f.operator, path).await,
        json!({"data":[all["data"][0]]})
    );
    assert_eq!(
        get(
            &f,
            &f.operator,
            &format!("{path}?organization_id={}", f.foreign_org)
        )
        .await,
        json!({"data":[]})
    );
}

#[sqlx::test]
async fn platform_teams_require_current_operator_and_are_readonly(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = "/api/v1/platform/teams";
    for user in [&f.owner, &f.admin, &f.team_admin, &f.member, &f.outsider] {
        for query in [String::new(), format!("?organization_id={}", f.org)] {
            assert_eq!(
                call(&f.store, user, "GET", &format!("{path}{query}"), json!({}))
                    .await
                    .0,
                StatusCode::FORBIDDEN
            );
        }
    }
    for method in ["POST", "PATCH", "DELETE"] {
        assert_eq!(
            call(
                &f.store,
                &f.operator,
                method,
                path,
                json!({"name":"No mutation"})
            )
            .await
            .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
    let forged = BrowserPrincipal {
        platform_admin: true,
        ..f.member.clone()
    };
    assert_eq!(
        call(&f.store, &forged, "GET", path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    // Current database authority wins over a stale session in either direction.
    let stale_nonoperator = BrowserPrincipal {
        platform_admin: false,
        ..f.operator.clone()
    };
    assert_eq!(
        get(&f, &stale_nonoperator, path).await["data"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
        .bind(f.operator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.store, &f.operator, "GET", path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE users SET platform_admin=true,disabled_at=now() WHERE id=$1")
        .bind(f.operator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.store, &f.operator, "GET", path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audits, 0);
}

#[sqlx::test]
async fn all_directories_have_stable_bounded_pagination(pool: PgPool) {
    let f = fixture(&pool).await;
    for path in [
        "/api/v1/platform/users".to_string(),
        "/api/v1/platform/teams".to_string(),
        "/api/v1/orgs".to_string(),
        format!("/api/v1/orgs/{}/teams", f.org),
    ] {
        let all = get(&f, &f.operator, &path).await;
        for offset in 0..2 {
            let page = get(&f, &f.operator, &format!("{path}?limit=1&offset={offset}")).await;
            assert_eq!(page["data"], json!([all["data"][offset]]));
        }
        assert!(
            get(&f, &f.operator, &format!("{path}?offset=100000")).await["data"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        for query in [
            "limit=0",
            "limit=201",
            "offset=-1",
            "offset=100001",
            "unknown=1",
        ] {
            assert_eq!(
                call(
                    &f.store,
                    &f.operator,
                    "GET",
                    &format!("{path}?{query}"),
                    json!({})
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
    }
}

#[sqlx::test]
async fn renames_enforce_roles_and_audit_only_successes(pool: PgPool) {
    let f = fixture(&pool).await;
    let org_path = format!("/api/v1/orgs/{}", f.org);
    let team_path = format!("/api/v1/workspaces/{}", f.team);
    for (user, org_status, team_status) in [
        (&f.owner, StatusCode::OK, StatusCode::OK),
        (&f.admin, StatusCode::OK, StatusCode::OK),
        (&f.team_admin, StatusCode::FORBIDDEN, StatusCode::OK),
        (&f.member, StatusCode::FORBIDDEN, StatusCode::FORBIDDEN),
        (&f.outsider, StatusCode::FORBIDDEN, StatusCode::FORBIDDEN),
        (&f.operator, StatusCode::OK, StatusCode::OK),
    ] {
        for (path, status) in [(&org_path, org_status), (&team_path, team_status)] {
            assert_eq!(
                call(&f.store, user, "PATCH", path, json!({"name":"Renamed"}))
                    .await
                    .0,
                status
            );
        }
    }
    assert_eq!(
        call(
            &f.store,
            &f.owner,
            "PATCH",
            &format!("/api/v1/orgs/{}", f.foreign_org),
            json!({"name":"Wrong tenant"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.store,
            &f.team_admin,
            "PATCH",
            &format!("/api/v1/workspaces/{}", f.other_team),
            json!({"name":"Not my team"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for user in [&f.owner, &f.operator] {
        assert_eq!(
            call(
                &f.store,
                user,
                "PATCH",
                &format!("/api/v1/workspaces/{}", f.personal),
                json!({"name":"Not allowed"})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    for path in [&org_path, &team_path] {
        for name in [
            "".to_string(),
            "  ".into(),
            "bad\nname".into(),
            "x".repeat(121),
        ] {
            assert_eq!(
                call(&f.store, &f.owner, "PATCH", path, json!({"name":name}))
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        for extra in [
            json!({"name":"Valid","disabled":true}),
            json!({"name":"Valid","slug":"new"}),
        ] {
            assert_eq!(
                call(&f.store, &f.owner, "PATCH", path, extra).await.0,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
    }
    let org: (String, String) = sqlx::query_as("SELECT name,slug FROM organizations WHERE id=$1")
        .bind(f.org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(org, ("Renamed".into(), "Alpha".into()));
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM workspaces WHERE id=$1 OR id=$2 ORDER BY kind")
            .bind(f.team)
            .bind(f.personal)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(names, vec!["PRIVATE PERSONAL NAME", "Renamed"]);
    let events: Vec<(String, Uuid, Option<Uuid>, Uuid)> =
        sqlx::query_as("SELECT action,organization_id,workspace_id,target_id FROM audit_events")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(events.len(), 7);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.0 == "organization.updated")
            .count(),
        3
    );
    for (action, org, workspace, target) in events {
        assert_eq!(org, f.org);
        if action == "organization.updated" {
            assert_eq!(workspace, None);
            assert_eq!(target, f.org);
        } else {
            assert_eq!(action, "workspace.updated");
            assert_eq!(workspace, Some(f.team));
            assert_eq!(target, f.team);
        }
    }
}

#[sqlx::test]
async fn renames_recheck_current_user_membership_and_resource_state(pool: PgPool) {
    let f = fixture(&pool).await;
    let org = format!("/api/v1/orgs/{}", f.org);
    let team = format!("/api/v1/workspaces/{}", f.team);
    sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
        .bind(f.operator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for path in [&org, &team] {
        assert_eq!(
            call(
                &f.store,
                &f.operator,
                "PATCH",
                path,
                json!({"name":"Stale operator"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for path in [&org, &team] {
        assert_eq!(
            call(
                &f.store,
                &f.owner,
                "PATCH",
                path,
                json!({"name":"Disabled owner"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    sqlx::query("UPDATE organization_memberships SET role='member' WHERE user_id=$1")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for path in [&org, &team] {
        assert_eq!(
            call(
                &f.store,
                &f.admin,
                "PATCH",
                path,
                json!({"name":"Former admin"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    sqlx::query("UPDATE workspace_memberships SET disabled_at=now() WHERE user_id=$1")
        .bind(f.team_admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &f.store,
            &f.team_admin,
            "PATCH",
            &team,
            json!({"name":"Former team admin"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &f.store,
            &f.team_admin,
            "PATCH",
            &team,
            json!({"name":"Disabled team"})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE organizations SET disabled_at=now() WHERE id=$1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &f.store,
            &f.admin,
            "PATCH",
            &org,
            json!({"name":"Disabled org"})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn rename_waits_for_org_lock_then_rechecks_authority(pool: PgPool) {
    let f = fixture(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(f.org)
        .execute(&mut *tx)
        .await
        .unwrap();
    let store = f.store.clone();
    let user = f.team_admin.clone();
    let path = format!("/api/v1/workspaces/{}", f.team);
    let request = tokio::spawn(async move {
        call(
            &store,
            &user,
            "PATCH",
            &path,
            json!({"name":"Must not persist"}),
        )
        .await
    });
    // Observe the blocked request rather than relying on scheduling or sleeps.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%FROM organizations%FOR NO KEY UPDATE%')")
                .fetch_one(&pool).await.unwrap();
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    sqlx::query("UPDATE workspace_memberships SET role='member' WHERE user_id=$1")
        .bind(f.team_admin.user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(request.await.unwrap().0, StatusCode::FORBIDDEN);
    let name: String = sqlx::query_scalar("SELECT name FROM workspaces WHERE id=$1")
        .bind(f.team)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Alpha team");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn audit_failure_rolls_back_both_rename_types(pool: PgPool) {
    let f = fixture(&pool).await;
    sqlx::query("CREATE FUNCTION reject_directory_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test audit failure'; END $$")
        .execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_directory_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_directory_audit()")
        .execute(&pool).await.unwrap();
    for path in [
        format!("/api/v1/orgs/{}", f.org),
        format!("/api/v1/workspaces/{}", f.team),
    ] {
        assert_eq!(
            call(
                &f.store,
                &f.owner,
                "PATCH",
                &path,
                json!({"name":"Must roll back"})
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    let org_name: String = sqlx::query_scalar("SELECT name FROM organizations WHERE id=$1")
        .bind(f.org)
        .fetch_one(&pool)
        .await
        .unwrap();
    let team_name: String = sqlx::query_scalar("SELECT name FROM workspaces WHERE id=$1")
        .bind(f.team)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(org_name, "Alpha");
    assert_eq!(team_name, "Alpha team");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn project_directories_are_fixed_kind_bounded_and_private(pool: PgPool) {
    let f = fixture(&pool).await;
    sqlx::query("UPDATE workspaces SET kind='project' WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    let foreign_project = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,organization_id,name,kind) VALUES($1,$2,'Foreign project','project')")
        .bind(foreign_project).bind(f.foreign_org).execute(&pool).await.unwrap();
    let org_path = format!("/api/v1/orgs/{}/projects", f.org);
    let platform_path = "/api/v1/platform/projects";
    for (user, role) in [
        (&f.owner, "owner"),
        (&f.admin, "admin"),
        (&f.team_admin, "admin"),
        (&f.member, "member"),
        (&f.operator, "owner"),
    ] {
        let result = get(&f, user, &org_path).await;
        assert_eq!(result["data"].as_array().unwrap().len(), 1);
        assert_eq!(result["data"][0]["id"], f.team.to_string());
        assert_eq!(result["data"][0]["kind"], "project");
        assert_eq!(result["data"][0]["role"], role);
        let me = get(&f, user, "/api/v1/me").await;
        assert!(
            me["workspaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w["id"] == f.team.to_string()
                    && w["kind"] == "project"
                    && w["role"] == role)
        );
        if user.user_id != f.owner.user_id {
            assert!(!me.to_string().contains(&f.personal.to_string()));
        }
    }
    let all = get(&f, &f.operator, platform_path).await;
    assert_eq!(all["data"].as_array().unwrap().len(), 2);
    for project in all["data"].as_array().unwrap() {
        fields(
            project,
            &[
                "id",
                "organization_id",
                "organization_name",
                "name",
                "kind",
                "role",
            ],
        );
        assert_eq!(project["kind"], "project");
        assert_eq!(project["role"], "operator");
    }
    assert!(!all.to_string().contains(&f.personal.to_string()));
    assert!(!all.to_string().contains("PRIVATE PERSONAL NAME"));
    assert!(!all.to_string().contains(&f.other_team.to_string()));
    assert_eq!(
        get(
            &f,
            &f.operator,
            &format!("{platform_path}?organization_id={}", f.foreign_org)
        )
        .await["data"],
        json!([all["data"][1]])
    );
    for offset in 0..2 {
        assert_eq!(
            get(
                &f,
                &f.operator,
                &format!("{platform_path}?limit=1&offset={offset}")
            )
            .await["data"],
            json!([all["data"][offset]])
        );
    }
    for path in [platform_path, &org_path] {
        for query in [
            "limit=0",
            "limit=201",
            "offset=-1",
            "offset=100001",
            "kind=personal",
            "unknown=1",
        ] {
            assert_eq!(
                call(
                    &f.store,
                    &f.operator,
                    "GET",
                    &format!("{path}?{query}"),
                    json!({})
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
        for method in ["POST", "PATCH", "DELETE"] {
            assert_eq!(
                call(&f.store, &f.operator, method, path, json!({})).await.0,
                StatusCode::METHOD_NOT_ALLOWED
            );
        }
    }
    for user in [&f.owner, &f.admin, &f.team_admin, &f.member, &f.outsider] {
        assert_eq!(
            call(&f.store, user, "GET", platform_path, json!({}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(&f.store, &f.outsider, "GET", &org_path, json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    // Being a team member is not membership in a sibling project.
    sqlx::query("DELETE FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2")
        .bind(f.team)
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,'member')")
        .bind(f.org).bind(f.other_team).bind(f.member.user_id).execute(&pool).await.unwrap();
    assert_eq!(get(&f, &f.member, &org_path).await["data"], json!([]));
    let teams = get(&f, &f.operator, "/api/v1/platform/teams").await;
    assert_eq!(teams["data"].as_array().unwrap().len(), 1);
    assert_eq!(teams["data"][0]["id"], f.other_team.to_string());
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(f.team)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE organizations SET disabled_at=now() WHERE id=$1")
        .bind(f.foreign_org)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(get(&f, &f.operator, platform_path).await["data"], json!([]));
    sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
        .bind(f.operator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.store, &f.operator, "GET", platform_path, json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn directory_merge_preserves_existing_creation_routes(pool: PgPool) {
    let f = fixture(&pool).await;
    assert_eq!(
        call(
            &f.store,
            &f.operator,
            "POST",
            "/api/v1/orgs",
            json!({"name":"New org","slug":"new-org"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.store,
            &f.owner,
            "POST",
            &format!("/api/v1/orgs/{}/workspaces", f.org),
            json!({"name":"New team"})
        )
        .await
        .0,
        StatusCode::OK
    );
}
