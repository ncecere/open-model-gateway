use super::*;
use crate::{bootstrap, config::Environment};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use sqlx::PgPool;
use tower::ServiceExt;

pub(super) async fn fixture(
    pool: &PgPool,
) -> (
    Store,
    crate::bootstrap::DevelopmentKeys,
    BrowserPrincipal,
    BrowserPrincipal,
) {
    let s = Store::new(pool.clone());
    let keys = bootstrap::seed(&s, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT issued_to_user_id FROM api_keys WHERE id=$1")
        .bind(keys.personal_key.id)
        .fetch_one(pool)
        .await
        .unwrap();
    let owner = BrowserPrincipal {
        user_id: id,
        email: "developer@local.invalid".into(),
        platform_admin: false,
    };
    let other = BrowserPrincipal {
        user_id: Uuid::new_v4(),
        email: "other@example.invalid".into(),
        platform_admin: false,
    };
    sqlx::query("INSERT INTO users(id,email) VALUES($1,$2)")
        .bind(other.user_id)
        .bind(&other.email)
        .execute(pool)
        .await
        .unwrap();
    (s, keys, owner, other)
}
pub(super) async fn call(
    s: &Store,
    u: &BrowserPrincipal,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    // Tests exercise authorization; identity middleware has its own OIDC/session/CSRF suite.
    let app = routes().layer(Extension(u.clone())).with_state(s.clone());
    let response = app
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
pub(super) async fn add_member(pool: &PgPool, org: Uuid, ws: Uuid, user: Uuid) {
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'member')",
    )
    .bind(org)
    .bind(user)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,'member')").bind(org).bind(ws).bind(user).execute(pool).await.unwrap();
}
#[sqlx::test]
async fn cross_tenant_access_and_personal_spaces_are_private(pool: PgPool) {
    let (s, k, owner, mut other) = fixture(&pool).await;
    assert_eq!(
        call(
            &s,
            &other,
            "GET",
            &format!("/api/v1/orgs/{}/members", k.organization_id),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    other.platform_admin = true;
    for suffix in ["keys", "grants", "executions", "usage", "service-accounts"] {
        assert_eq!(
            call(
                &s,
                &other,
                "GET",
                &format!("/api/v1/workspaces/{}/{suffix}", k.personal_workspace_id),
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(
            &s,
            &owner,
            "GET",
            &format!("/api/v1/workspaces/{}/keys", k.personal_workspace_id),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let me = call(&s, &other, "GET", "/api/v1/me", json!({})).await.1;
    assert!(
        !me["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == k.personal_workspace_id.to_string())
    );
}
#[sqlx::test]
async fn key_lifecycle_hashes_tokens_and_rotates_atomically(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    let base = format!("/api/v1/workspaces/{}/keys", k.team_workspace_id);
    let (status, value) = call(
        &s,
        &other,
        "POST",
        &base,
        json!({"name":"App","expires_in_days":30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = value["token"].as_str().unwrap();
    assert!(s.authenticate(token).await.unwrap().is_some());
    let listed = call(&s, &other, "GET", &base, json!({}))
        .await
        .1
        .to_string();
    assert!(!listed.contains(token));
    assert!(!listed.contains("secret_hash"));
    let id = value["id"].as_str().unwrap();
    // Workspace admins may revoke other users' keys, but cannot rotate/reveal them.
    assert_eq!(
        call(
            &s,
            &owner,
            "POST",
            &format!("{base}/{id}/rotate"),
            json!({"expires_in_days":20})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, rotated) = call(
        &s,
        &other,
        "POST",
        &format!("{base}/{id}/rotate"),
        json!({"expires_in_days":20}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(s.authenticate(token).await.unwrap().is_none());
    let new_token = rotated["token"].as_str().unwrap();
    assert!(s.authenticate(new_token).await.unwrap().is_some());
    assert_eq!(
        call(
            &s,
            &other,
            "DELETE",
            &format!("{base}/{}", rotated["id"].as_str().unwrap()),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(s.authenticate(new_token).await.unwrap().is_none());
    assert_eq!(
        call(
            &s,
            &other,
            "POST",
            &base,
            json!({"name":"bad","expires_in_days":0})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action LIKE 'key.%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 3);
}
#[sqlx::test]
async fn service_account_is_independent_of_creator_but_can_be_disabled(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    let accounts = format!(
        "/api/v1/workspaces/{}/service-accounts",
        k.team_workspace_id
    );
    assert_eq!(
        call(&s, &other, "POST", &accounts, json!({"name":"denied"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, account) = call(
        &s,
        &owner,
        "POST",
        &accounts,
        json!({"name":"Build service"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = account["id"].as_str().unwrap();
    let key = call(
        &s,
        &owner,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", k.team_workspace_id),
        json!({"name":"CI","expires_in_days":30,"service_account_id":id}),
    )
    .await
    .1;
    let token = key["token"].as_str().unwrap();
    let principal = s.authenticate(token).await.unwrap().unwrap();
    assert!(principal.user_id.is_none());
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(s.authenticate(token).await.unwrap().is_some());
    sqlx::query("UPDATE users SET disabled_at=NULL WHERE id=$1")
        .bind(owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &s,
            &owner,
            "PATCH",
            &format!("{accounts}/{id}"),
            json!({"disabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(s.authenticate(token).await.unwrap().is_none());
    call(
        &s,
        &owner,
        "PATCH",
        &format!("{accounts}/{id}"),
        json!({"disabled":false}),
    )
    .await;
    assert!(s.authenticate(token).await.unwrap().is_none());
}
#[sqlx::test]
async fn owner_protection_and_membership_revocation_are_enforced(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    for path in [
        format!(
            "/api/v1/orgs/{}/members/{}",
            k.organization_id, owner.user_id
        ),
        format!(
            "/api/v1/workspaces/{}/members/{}",
            k.team_workspace_id, owner.user_id
        ),
    ] {
        assert_eq!(
            call(
                &s,
                &owner,
                "PATCH",
                &path,
                json!({"role":"member","disabled":false})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }
    let key = call(
        &s,
        &other,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", k.team_workspace_id),
        json!({"name":"Member key","expires_in_days":30}),
    )
    .await
    .1;
    let token = key["token"].as_str().unwrap();
    assert!(s.authenticate(token).await.unwrap().is_some());
    let path = format!(
        "/api/v1/workspaces/{}/members/{}",
        k.team_workspace_id, other.user_id
    );
    assert_eq!(
        call(
            &s,
            &owner,
            "PATCH",
            &path,
            json!({"role":"member","disabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(s.authenticate(token).await.unwrap().is_none());
    call(
        &s,
        &owner,
        "PATCH",
        &path,
        json!({"role":"member","disabled":false}),
    )
    .await;
    assert!(s.authenticate(token).await.unwrap().is_none());
}
#[sqlx::test]
async fn invitations_are_email_bound_single_use_and_create_personal_space(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    let path = format!("/api/v1/orgs/{}/invitations", k.organization_id);
    let(status,invite)=call(&s,&owner,"POST",&path,json!({"email":other.email,"organization_role":"member","workspace_id":k.team_workspace_id,"workspace_role":"member"})).await;
    assert_eq!(status, StatusCode::OK);
    let token = invite["token"].as_str().unwrap();
    assert_eq!(
        call(
            &s,
            &owner,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":token})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &s,
            &other,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":token})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &s,
            &other,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":token})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let me = call(&s, &other, "GET", "/api/v1/me", json!({})).await.1;
    assert_eq!(me["workspaces"].as_array().unwrap().len(), 2);
    let list = call(&s, &owner, "GET", &path, json!({}))
        .await
        .1
        .to_string();
    assert!(!list.contains(token));
    assert!(!list.contains("token_hash"));
}
#[sqlx::test]
async fn revoked_or_expired_invites_do_not_admit_users(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    let path = format!("/api/v1/orgs/{}/invitations", k.organization_id);
    for expired in [true, false] {
        let invite = call(
            &s,
            &owner,
            "POST",
            &path,
            json!({"email":other.email,"organization_role":"member","workspace_role":"member"}),
        )
        .await
        .1;
        let id = Uuid::parse_str(invite["id"].as_str().unwrap()).unwrap();
        if expired {
            sqlx::query("UPDATE invitations SET expires_at=now()-interval '1 second' WHERE id=$1")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        } else {
            assert_eq!(
                call(&s, &owner, "DELETE", &format!("{path}/{id}"), json!({}))
                    .await
                    .0,
                StatusCode::OK
            );
        }
        assert_eq!(
            call(
                &s,
                &other,
                "POST",
                "/api/v1/invitations/accept",
                json!({"token":invite["token"]})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
}
#[sqlx::test]
async fn ownership_changes_and_invitation_redemption_serialize(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    sqlx::query(
        "UPDATE organization_memberships SET role='owner' WHERE organization_id=$1 AND user_id=$2",
    )
    .bind(k.organization_id)
    .bind(other.user_id)
    .execute(&pool)
    .await
    .unwrap();
    let first = format!(
        "/api/v1/orgs/{}/members/{}",
        k.organization_id, owner.user_id
    );
    let second = format!(
        "/api/v1/orgs/{}/members/{}",
        k.organization_id, other.user_id
    );
    let (a, b) = tokio::join!(
        call(
            &s,
            &owner,
            "PATCH",
            &first,
            json!({"role":"member","disabled":false})
        ),
        call(
            &s,
            &other,
            "PATCH",
            &second,
            json!({"role":"member","disabled":false})
        )
    );
    assert!(matches!(
        (a.0, b.0),
        (StatusCode::OK, StatusCode::CONFLICT) | (StatusCode::CONFLICT, StatusCode::OK)
    ));
    let actor = if a.0 == StatusCode::OK {
        &other
    } else {
        &owner
    };
    let invitation = call(
        &s,
        actor,
        "POST",
        &format!("/api/v1/orgs/{}/invitations", k.organization_id),
        json!({"email":owner.email,"organization_role":"member","workspace_role":"member"}),
    )
    .await
    .1;
    let token = invitation["token"].clone();
    let (a, b) = tokio::join!(
        call(
            &s,
            &owner,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":token})
        ),
        call(
            &s,
            &owner,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":token})
        )
    );
    assert!(matches!(
        (a.0, b.0),
        (StatusCode::OK, StatusCode::NOT_FOUND) | (StatusCode::NOT_FOUND, StatusCode::OK)
    ));
}
#[sqlx::test]
async fn removal_racing_key_creation_does_not_resurrect_a_key(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    let member = format!(
        "/api/v1/workspaces/{}/members/{}",
        k.team_workspace_id, other.user_id
    );
    let keys = format!("/api/v1/workspaces/{}/keys", k.team_workspace_id);
    let (created, removed) = tokio::join!(
        call(
            &s,
            &other,
            "POST",
            &keys,
            json!({"name":"Racing key","expires_in_days":1})
        ),
        call(
            &s,
            &owner,
            "PATCH",
            &member,
            json!({"role":"member","disabled":true})
        )
    );
    assert_eq!(removed.0, StatusCode::OK);
    assert!(matches!(created.0, StatusCode::OK | StatusCode::FORBIDDEN));
    assert_eq!(
        call(
            &s,
            &owner,
            "PATCH",
            &member,
            json!({"role":"member","disabled":false})
        )
        .await
        .0,
        StatusCode::OK
    );
    if let Some(token) = created.1["token"].as_str() {
        assert!(s.authenticate(token).await.unwrap().is_none());
    }
}
#[sqlx::test]
async fn organization_admin_cannot_promote_itself_to_owner(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    let path = format!(
        "/api/v1/orgs/{}/members/{}",
        k.organization_id, other.user_id
    );
    assert_eq!(
        call(
            &s,
            &owner,
            "PATCH",
            &path,
            json!({"role":"admin","disabled":false})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &s,
            &other,
            "PATCH",
            &path,
            json!({"role":"owner","disabled":false})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[sqlx::test]
async fn personal_catalog_policy_governs_existing_and_future_spaces_without_disclosure(
    pool: PgPool,
) {
    let (s, k, owner, other) = fixture(&pool).await;
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'member')",
    )
    .bind(k.organization_id)
    .bind(other.user_id)
    .execute(&pool)
    .await
    .unwrap();
    // Infrastructure provisioning is operator-only; this test's owner only
    // administers the organization's already-assigned personal entitlement.
    let model = Uuid::new_v4();
    sqlx::query("INSERT INTO models(id,organization_id,public_name,display_name,enabled) VALUES($1,$2,'personal/model','Personal model',true)")
        .bind(model).bind(k.organization_id).execute(&pool).await.unwrap();
    let model = model.to_string();
    let path = format!(
        "/api/v1/orgs/{}/models/{model}/personal-access",
        k.organization_id
    );
    assert_eq!(
        call(&s, &other, "PUT", &path, json!({"enabled":true}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&s, &owner, "PUT", &path, json!({"enabled":true}))
            .await
            .0,
        StatusCode::OK
    );
    let personal = call(
        &s,
        &other,
        "POST",
        &format!("/api/v1/orgs/{}/personal-workspace", k.organization_id),
        json!({}),
    )
    .await
    .1["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let grants = format!("/api/v1/workspaces/{personal}/grants");
    assert_eq!(
        call(&s, &owner, "GET", &grants, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&s, &other, "GET", &grants, json!({})).await.1["data"][0]["model_id"],
        model
    );
    let existing = call(
        &s,
        &owner,
        "GET",
        &format!("/api/v1/workspaces/{}/grants", k.personal_workspace_id),
        json!({}),
    )
    .await
    .1;
    assert!(
        existing["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["model_id"] == model)
    );
    assert_eq!(
        call(&s, &owner, "PUT", &path, json!({"enabled":false}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&s, &other, "GET", &grants, json!({})).await.1["data"],
        json!([])
    );
}
#[sqlx::test]
async fn owner_insertion_rechecks_authority_after_parent_lock(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'member')",
    )
    .bind(k.organization_id)
    .bind(other.user_id)
    .execute(&pool)
    .await
    .unwrap();
    // Remove the independent workspace role so the actor's authority is org-derived.
    sqlx::query("DELETE FROM workspace_memberships WHERE user_id=$1")
        .bind(owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(k.organization_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE organization_memberships SET role='member' WHERE organization_id=$1 AND user_id=$2",
    )
    .bind(k.organization_id)
    .bind(owner.user_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    let path = format!("/api/v1/workspaces/{}/members", k.team_workspace_id);
    let operation = call(
        &s,
        &owner,
        "POST",
        &path,
        json!({"user_id":other.user_id,"role":"owner"}),
    );
    tokio::pin!(operation);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut operation)
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    assert_eq!(operation.await.0, StatusCode::FORBIDDEN);
}
#[sqlx::test]
async fn organization_creation_rechecks_operator_authority(pool: PgPool) {
    let (s, _, mut actor, _) = fixture(&pool).await;
    actor.platform_admin = true;
    assert_eq!(
        call(
            &s,
            &actor,
            "POST",
            "/api/v1/orgs",
            json!({"name":"Unauthorized","slug":"unauthorized"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[sqlx::test]
async fn team_creation_rechecks_role_after_waiting_for_organization_lock(pool: PgPool) {
    let (s, k, actor, _) = fixture(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
        .bind(k.organization_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE organization_memberships SET role='member' WHERE organization_id=$1 AND user_id=$2",
    )
    .bind(k.organization_id)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    let path = format!("/api/v1/orgs/{}/workspaces", k.organization_id);
    let operation = call(
        &s,
        &actor,
        "POST",
        &path,
        json!({"name":"Unauthorized new team"}),
    );
    tokio::pin!(operation);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut operation)
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    assert_eq!(operation.await.0, StatusCode::FORBIDDEN);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workspaces WHERE name='Unauthorized new team'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}
#[sqlx::test]
async fn operator_provider_configuration_and_cross_org_constraints(pool: PgPool) {
    let (s, k, mut owner, other) = fixture(&pool).await;
    let path = "/api/v1/platform/providers".to_owned();
    for suffix in ["providers", "deployments", "models"] {
        assert_eq!(
            call(
                &s,
                &owner,
                "POST",
                &format!("/api/v1/orgs/{}/{suffix}", k.organization_id),
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let body = json!({"name":"AWS","provider":"bedrock","credential_ref":"aws:default","region":"us-east-1","enabled":false});
    assert_eq!(
        call(&s, &owner, "POST", &path, body.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    owner.platform_admin = true;
    sqlx::query("UPDATE users SET platform_admin=true WHERE id=$1")
        .bind(owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&s, &owner, "POST", &path, body).await.0,
        StatusCode::OK
    );
    assert!(
        !call(&s, &owner, "GET", &path, json!({}))
            .await
            .1
            .to_string()
            .contains("credential_ref")
    );
    assert_eq!(
        call(&s, &other, "GET", &path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    let bad = json!({"name":"Bad","provider":"bedrock","credential_ref":"aws:default","region":"us-east-1","endpoint":"http://127.0.0.1","enabled":true});
    assert_eq!(
        call(&s, &owner, "POST", &path, bad).await.0,
        StatusCode::BAD_REQUEST
    );
    let missing_model = json!({"model_id":Uuid::new_v4(),"provider_connection_id":Uuid::new_v4(),"upstream_model":"model","enabled":true});
    assert_eq!(
        call(
            &s,
            &owner,
            "POST",
            "/api/v1/platform/deployments",
            missing_model
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
}
