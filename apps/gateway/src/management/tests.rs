//! Disposable enterprise tests replacing organization-scoped management scenarios.
use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use sqlx::PgPool;
use tower::ServiceExt;
#[path = "alerts/tests.rs"]
mod alert_tests;
#[path = "batch_scheduling_tests.rs"]
mod batch_scheduling_tests;
#[path = "settings/branding_tests.rs"]
mod branding_tests;
#[path = "catalog_ux_tests.rs"]
mod catalog_ux_tests;
#[path = "directory/tests.rs"]
mod directory_tests;
#[path = "files_tests.rs"]
mod files_tests;
#[path = "key_safety_tests.rs"]
mod key_safety_tests;
#[path = "keys/tests.rs"]
mod key_tests;
#[path = "members/tests.rs"]
mod member_tests;
#[path = "portal_tests.rs"]
mod portal_tests;
#[path = "project_tests.rs"]
mod project_tests;
#[path = "resources/tests.rs"]
mod resource_tests;
#[path = "session_tests.rs"]
mod session_tests;
#[path = "settings/tests.rs"]
mod settings_tests;
#[path = "setup/tests.rs"]
mod setup_tests;
#[path = "snapshot_tests.rs"]
mod snapshot_tests;
#[path = "settings/storage_tests.rs"]
mod storage_tests;
#[path = "ux_tests.rs"]
mod ux_tests;

struct Fixture {
    s: Store,
    admin: BrowserPrincipal,
    auditor: BrowserPrincipal,
    owner: BrowserPrincipal,
    member: BrowserPrincipal,
    outsider: BrowserPrincipal,
    team: Uuid,
    project: Uuid,
    personal: Uuid,
}
async fn user(pool: &PgPool, name: &str, role: &str) -> BrowserPrincipal {
    let id = Uuid::new_v4();
    let email = format!("{name}@example.test");
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(id)
        .bind(&email)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,$2,'manual')")
        .bind(id)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    BrowserPrincipal {
        user_id: id,
        email,
        platform_admin: role == "admin",
        platform_auditor: matches!(role, "admin" | "auditor"),
    }
}
async fn workspace(pool: &PgPool, name: &str, kind: &str, owner: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO workspaces(id,name,kind,owner_user_id) VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(name)
        .bind(kind)
        .bind(if kind == "personal" {
            Some(owner)
        } else {
            None
        })
        .execute(pool)
        .await
        .unwrap();
    member(pool, id, owner, "owner").await;
    id
}
async fn member(pool: &PgPool, ws: Uuid, user: Uuid, role: &str) {
    sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,$3,'manual')").bind(ws).bind(user).bind(role).execute(pool).await.unwrap();
}
async fn fixture(pool: &PgPool) -> Fixture {
    let admin = user(pool, "admin", "admin").await;
    let auditor = user(pool, "auditor", "auditor").await;
    let owner = user(pool, "owner", "user").await;
    let member_user = user(pool, "member", "user").await;
    let outsider = user(pool, "outsider", "user").await;
    let team = workspace(pool, "Team", "team", owner.user_id).await;
    let project = workspace(pool, "Project", "project", owner.user_id).await;
    let personal = workspace(pool, "Personal", "personal", owner.user_id).await;
    member(pool, team, member_user.user_id, "member").await;
    Fixture {
        s: Store::new(pool.clone()),
        admin,
        auditor,
        owner,
        member: member_user,
        outsider,
        team,
        project,
        personal,
    }
}
async fn call(
    s: &Store,
    u: &BrowserPrincipal,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = routes()
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
fn id(v: &Value) -> Uuid {
    Uuid::parse_str(v["id"].as_str().unwrap()).unwrap()
}
async fn model(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO models(id,public_name) VALUES($1,$2)")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn direct(pool: &PgPool, ws: Uuid, model: Uuid) {
    sqlx::query(
        "INSERT INTO workspace_model_grants(workspace_id,model_id,source) VALUES($1,$2,'direct')",
    )
    .bind(ws)
    .bind(model)
    .execute(pool)
    .await
    .unwrap();
}
async fn key(f: &Fixture, u: &BrowserPrincipal, ws: Uuid, models: Value) -> (StatusCode, Value) {
    call(
        &f.s,
        u,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys"),
        json!({"name":"Key","expires_in_days":1,"model_ids":models}),
    )
    .await
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn personal_details_are_owner_only_even_platform_admin_and_auditor(pool: PgPool) {
    let f = fixture(&pool).await;
    for user in [&f.admin, &f.auditor, &f.outsider] {
        for endpoint in ["", "/keys", "/executions", "/usage", "/audit", "/models"] {
            assert_eq!(
                call(
                    &f.s,
                    user,
                    "GET",
                    &format!("/api/v1/workspaces/{}{endpoint}", f.personal),
                    json!({})
                )
                .await
                .0,
                StatusCode::FORBIDDEN
            );
        }
    }
    for endpoint in ["", "/keys", "/executions", "/usage", "/audit", "/models"] {
        assert_eq!(
            call(
                &f.s,
                &f.owner,
                "GET",
                &format!("/api/v1/workspaces/{}{endpoint}", f.personal),
                json!({})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "GET",
            &format!("/api/v1/platform/workspaces/{}", f.personal),
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn legacy_organization_paths_are_not_registered(pool: PgPool) {
    let f = fixture(&pool).await;
    let org = Uuid::new_v4();
    for path in [
        "/api/v1/orgs".to_string(),
        format!("/api/v1/orgs/{org}/workspaces"),
        format!("/api/v1/orgs/{org}/models"),
        format!("/api/v1/orgs/{org}/policy"),
        format!("/api/v1/platform/orgs/{org}/policy"),
    ] {
        for method in ["GET", "POST"] {
            assert_eq!(
                call(&f.s, &f.admin, method, &path, json!({})).await.0,
                StatusCode::NOT_FOUND,
                "{method} {path}"
            );
        }
    }
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn shared_request_details_usage_and_audit_are_actor_filtered_before_pagination(pool: PgPool) {
    let f = fixture(&pool).await;
    let m = model(&pool, "request-model").await;
    let provider = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Mock','openai','env:TEST')").bind(provider).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'Mock')").bind(deployment).bind(m).bind(provider).execute(&pool).await.unwrap();
    for (actor, tokens) in [(&f.owner, 11_i64), (&f.member, 7_i64)] {
        let (status, k) = key(&f, actor, f.team, Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let kid = id(&k);
        let execution = Uuid::new_v4();
        sqlx::query("INSERT INTO inference_executions(id,workspace_id,api_key_id,deployment_id,public_model,provider,streamed,state,root_request_id,input_tokens,output_tokens) VALUES($1,$2,$3,$4,'request-model','openai',false,'succeeded',$1,$5,0)").bind(execution).bind(f.team).bind(kid).bind(deployment).bind(tokens).execute(&pool).await.unwrap();
    }
    let (status, rows) = call(
        &f.s,
        &f.member,
        "GET",
        &format!("/api/v1/workspaces/{}/executions?limit=1", f.team),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows["data"].as_array().unwrap().len(), 1);
    assert_eq!(rows["data"][0]["input_tokens"], "7");
    let (_, ownusage) = call(
        &f.s,
        &f.member,
        "GET",
        &format!("/api/v1/workspaces/{}/usage", f.team),
        json!({}),
    )
    .await;
    assert_eq!(ownusage["requests"], "1");
    assert_eq!(ownusage["input_tokens"], "7");
    // Platform Admins without membership get no workspace activity.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "GET",
            &format!("/api/v1/workspaces/{}/usage", f.team),
            json!({}),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, allusage) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/usage", f.team),
        json!({}),
    )
    .await;
    assert_eq!(allusage["requests"], "2");
    assert_eq!(allusage["input_tokens"], "18");
    let (_, audit) = call(
        &f.s,
        &f.member,
        "GET",
        &format!("/api/v1/workspaces/{}/audit", f.team),
        json!({}),
    )
    .await;
    assert!(
        audit["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["actor_user_id"] == f.member.user_id.to_string())
    );
    let (_, private_key) = key(&f, &f.owner, f.personal, Value::Null).await;
    let private_id = id(&private_key);
    let (_, platform_audit) =
        call(&f.s, &f.auditor, "GET", "/api/v1/platform/audit", json!({})).await;
    assert!(
        platform_audit["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["workspace_id"] != f.personal.to_string()
                && a["resource_id"] != private_id.to_string())
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn sanitized_audit_failure_rolls_back_resource_creation(pool: PgPool) {
    let f = fixture(&pool).await;
    sqlx::query("CREATE FUNCTION reject_management_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test audit outage'; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_management_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_management_audit()").execute(&pool).await.unwrap();
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/catalogs",
            json!({"name":"Must rollback"})
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM catalogs")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query("DROP TRIGGER reject_management_audit ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = resources::installation_tx(&f.s).await.unwrap();
    audit(
        &mut tx,
        &f.admin,
        None,
        "test",
        "catalog",
        None,
        json!({"token":"secret","email":"private","role":"admin","enabled":true,"name":"secret"}),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let metadata: Value =
        sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action='test'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(metadata, json!({"role":"admin","enabled":true}));
}
