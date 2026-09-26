use super::{
    tests::{add_member, call, fixture},
    *,
};
use sqlx::PgPool;

async fn project(s: &Store, owner: &BrowserPrincipal, org: Uuid) -> Uuid {
    let (status, body) = call(
        s,
        owner,
        "POST",
        &format!("/api/v1/orgs/{org}/workspaces"),
        json!({"name":"Research", "kind":"project"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

#[sqlx::test]
async fn projects_are_siblings_and_personal_owner_constraints_remain_exact(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    let ws = project(&s, &owner, k.organization_id).await;
    let shape: (Uuid, String, Option<Uuid>) =
        sqlx::query_as("SELECT organization_id,kind,owner_user_id FROM workspaces WHERE id=$1")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(shape, (k.organization_id, "project".into(), None));
    for kind in ["personal", "arbitrary", "", "Project"] {
        assert_eq!(
            call(
                &s,
                &owner,
                "POST",
                &format!("/api/v1/orgs/{}/workspaces", k.organization_id),
                json!({"name":"Invalid", "kind":kind})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (_, default) = call(
        &s,
        &owner,
        "POST",
        &format!("/api/v1/orgs/{}/workspaces", k.organization_id),
        json!({"name":"Default team"}),
    )
    .await;
    let kind: String = sqlx::query_scalar("SELECT kind FROM workspaces WHERE id=$1")
        .bind(Uuid::parse_str(default["id"].as_str().unwrap()).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kind, "team");
    add_member(&pool, k.organization_id, ws, other.user_id).await;
    for (kind, owner_id) in [
        ("project", Some(owner.user_id)),
        ("personal", None),
        ("unknown", None),
    ] {
        let error = sqlx::query("INSERT INTO workspaces(id,organization_id,name,kind,owner_user_id) VALUES($1,$2,'invalid',$3,$4)").bind(Uuid::new_v4()).bind(k.organization_id).bind(kind).bind(owner_id).execute(&pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
    }
    let error = sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,'member')").bind(k.organization_id).bind(k.personal_workspace_id).bind(other.user_id).execute(&pool).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
    let error = sqlx::query("INSERT INTO service_accounts(id,organization_id,workspace_id,name) VALUES($1,$2,$3,'private service')").bind(Uuid::new_v4()).bind(k.organization_id).bind(k.personal_workspace_id).execute(&pool).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23514")
    );
}

#[sqlx::test]
async fn projects_require_their_own_membership_and_isolate_keys_and_grants(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    let ws = project(&s, &owner, k.organization_id).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    for suffix in [
        "keys",
        "grants",
        "usage",
        "executions",
        "members",
        "service-accounts",
    ] {
        assert_eq!(
            call(
                &s,
                &other,
                "GET",
                &format!("/api/v1/workspaces/{ws}/{suffix}"),
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
            "POST",
            &format!("/api/v1/workspaces/{ws}/members"),
            json!({"user_id":other.user_id,"role":"member"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &s,
            &owner,
            "PATCH",
            &format!("/api/v1/workspaces/{ws}/members/{}", owner.user_id),
            json!({"role":"member","disabled":false})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, key) = call(
        &s,
        &other,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys"),
        json!({"name":"Project human", "expires_in_days":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = key["token"].as_str().unwrap();
    let principal = s.authenticate(token).await.unwrap().unwrap();
    assert_eq!(principal.workspace_id, ws);
    assert!(s.visible_models(&principal).await.unwrap().is_empty());
    let model: Uuid = sqlx::query_scalar(
        "SELECT model_id FROM organization_model_grants WHERE organization_id=$1",
    )
    .bind(k.organization_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        call(
            &s,
            &owner,
            "POST",
            &format!("/api/v1/workspaces/{ws}/grants"),
            json!({"model_id":model})
        )
        .await
        .0,
        StatusCode::OK
    );
    // The same-org team's grant never admitted this project; its explicit grant does.
    sqlx::query("UPDATE provider_connections SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(s.visible_models(&principal).await.unwrap().len(), 1);
    assert_eq!(
        call(
            &s,
            &other,
            "DELETE",
            &format!(
                "/api/v1/workspaces/{}/keys/{}",
                k.team_workspace_id,
                key["id"].as_str().unwrap()
            ),
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, rotated) = call(
        &s,
        &other,
        "POST",
        &format!(
            "/api/v1/workspaces/{ws}/keys/{}/rotate",
            key["id"].as_str().unwrap()
        ),
        json!({"expires_in_days":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(s.authenticate(token).await.unwrap().is_none());
    let member = format!("/api/v1/workspaces/{ws}/members/{}", other.user_id);
    assert_eq!(
        call(
            &s,
            &owner,
            "PATCH",
            &member,
            json!({"role":"member","disabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        s.authenticate(rotated["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_none()
    );
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
    assert!(
        s.authenticate(rotated["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(s.authenticate(&k.team_key.token).await.unwrap().is_some());
}

#[sqlx::test]
async fn project_invitations_and_service_accounts_follow_shared_lifecycle(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    let ws = project(&s, &owner, k.organization_id).await;
    let invitations = format!("/api/v1/orgs/{}/invitations", k.organization_id);
    let (status, invite) = call(&s, &owner, "POST", &invitations, json!({"email":other.email,"organization_role":"member","workspace_id":ws,"workspace_role":"admin"})).await;
    assert_eq!(status, StatusCode::OK);
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
        StatusCode::OK
    );
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
    let me = call(&s, &other, "GET", "/api/v1/me", json!({})).await.1;
    assert_eq!(me["workspaces"].as_array().unwrap().len(), 2);
    assert!(
        me["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == ws.to_string() && w["role"] == "admin")
    );
    assert!(
        !me.to_string()
            .contains(&k.personal_workspace_id.to_string())
    );
    assert_eq!(
        call(
            &s,
            &other,
            "PATCH",
            &format!("/api/v1/workspaces/{ws}"),
            json!({"name":"Renamed project"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let accounts = format!("/api/v1/workspaces/{ws}/service-accounts");
    let (status, account) = call(&s, &other, "POST", &accounts, json!({"name":"Project CI"})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, key) = call(
        &s,
        &other,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys"),
        json!({"name":"CI", "expires_in_days":1,"service_account_id":account["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = key["token"].as_str().unwrap();
    assert!(
        s.authenticate(token)
            .await
            .unwrap()
            .unwrap()
            .user_id
            .is_none()
    );
    let (status, rotated) = call(
        &s,
        &other,
        "POST",
        &format!(
            "/api/v1/workspaces/{ws}/keys/{}/rotate",
            key["id"].as_str().unwrap()
        ),
        json!({"expires_in_days":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(s.authenticate(token).await.unwrap().is_none());
    let token = rotated["token"].as_str().unwrap();
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(other.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(s.authenticate(token).await.unwrap().is_some());
    for disabled in [true, false] {
        assert_eq!(
            call(
                &s,
                &owner,
                "PATCH",
                &format!("{accounts}/{}", account["id"].as_str().unwrap()),
                json!({"disabled":disabled})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert!(s.authenticate(token).await.unwrap().is_none());
    }
    let audit = call(
        &s,
        &owner,
        "GET",
        &format!("/api/v1/orgs/{}/audit", k.organization_id),
        json!({}),
    )
    .await
    .1;
    assert!(
        audit["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["workspace_id"] == ws.to_string() && a["action"] == "workspace.updated")
    );
}

#[sqlx::test]
async fn project_key_and_service_mutations_recheck_authority_after_org_lock(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    let ws = project(&s, &owner, k.organization_id).await;
    add_member(&pool, k.organization_id, ws, other.user_id).await;
    sqlx::query(
        "UPDATE workspace_memberships SET role='admin' WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(ws)
    .bind(other.user_id)
    .execute(&pool)
    .await
    .unwrap();
    let account = call(
        &s,
        &other,
        "POST",
        &format!("/api/v1/workspaces/{ws}/service-accounts"),
        json!({"name":"CI"}),
    )
    .await
    .1;
    let key = call(
        &s,
        &other,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys"),
        json!({"name":"CI", "expires_in_days":1,"service_account_id":account["id"]}),
    )
    .await
    .1;
    for (method, path, body) in [
        (
            "POST",
            format!("/api/v1/workspaces/{ws}/keys"),
            json!({"name":"Forbidden", "expires_in_days":1,"service_account_id":account["id"]}),
        ),
        (
            "POST",
            format!(
                "/api/v1/workspaces/{ws}/keys/{}/rotate",
                key["id"].as_str().unwrap()
            ),
            json!({"expires_in_days":1}),
        ),
        (
            "DELETE",
            format!(
                "/api/v1/workspaces/{ws}/keys/{}",
                key["id"].as_str().unwrap()
            ),
            json!({}),
        ),
        (
            "POST",
            format!("/api/v1/workspaces/{ws}/service-accounts"),
            json!({"name":"Forbidden"}),
        ),
        (
            "PATCH",
            format!(
                "/api/v1/workspaces/{ws}/service-accounts/{}",
                account["id"].as_str().unwrap()
            ),
            json!({"disabled":true}),
        ),
    ] {
        sqlx::query("UPDATE workspace_memberships SET role='admin',disabled_at=NULL WHERE workspace_id=$1 AND user_id=$2").bind(ws).bind(other.user_id).execute(&pool).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR NO KEY UPDATE")
            .bind(k.organization_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE workspace_memberships SET disabled_at=now() WHERE workspace_id=$1 AND user_id=$2").bind(ws).bind(other.user_id).execute(&mut *tx).await.unwrap();
        let operation = call(&s, &other, method, &path, body);
        tokio::pin!(operation);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut operation)
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        assert_eq!(operation.await.0, StatusCode::FORBIDDEN, "{method} {path}");
    }
    assert!(
        s.authenticate(key["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test]
async fn project_cross_tenant_relationships_and_personal_sharing_are_rejected(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    let ws = project(&s, &owner, k.organization_id).await;
    let foreign = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations(id,name,slug) VALUES($1,'Foreign','foreign')")
        .bind(foreign)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'owner')",
    )
    .bind(foreign)
    .bind(other.user_id)
    .execute(&pool)
    .await
    .unwrap();
    let foreign_ws = project(&s, &other, foreign).await;
    assert_eq!(
        call(
            &s,
            &owner,
            "POST",
            &format!("/api/v1/workspaces/{ws}/members"),
            json!({"user_id":other.user_id,"role":"member"})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for (actor, target) in [(&owner, foreign_ws), (&other, ws)] {
        for suffix in [
            "keys",
            "members",
            "service-accounts",
            "grants",
            "usage",
            "executions",
        ] {
            assert_eq!(
                call(
                    &s,
                    actor,
                    "GET",
                    &format!("/api/v1/workspaces/{target}/{suffix}"),
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
                actor,
                "PATCH",
                &format!("/api/v1/workspaces/{target}"),
                json!({"name":"Foreign rename"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(call(&s, &owner, "POST", &format!("/api/v1/orgs/{}/invitations", k.organization_id), json!({"email":other.email,"organization_role":"member","workspace_id":foreign_ws,"workspace_role":"member"})).await.0, StatusCode::FORBIDDEN);
    let foreign_account = call(
        &s,
        &other,
        "POST",
        &format!("/api/v1/workspaces/{foreign_ws}/service-accounts"),
        json!({"name":"Foreign CI"}),
    )
    .await
    .1;
    assert_eq!(call(&s, &owner, "POST", &format!("/api/v1/workspaces/{ws}/keys"), json!({"name":"Foreign CI key","expires_in_days":1,"service_account_id":foreign_account["id"]})).await.0, StatusCode::NOT_FOUND);
    let error = sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,'member')")
        .bind(k.organization_id).bind(foreign_ws).bind(owner.user_id).execute(&pool).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23503")
    );
    for (suffix, body) in [
        ("members", json!({"user_id":other.user_id,"role":"member"})),
        ("service-accounts", json!({"name":"Private CI"})),
    ] {
        assert_eq!(
            call(
                &s,
                &owner,
                "POST",
                &format!("/api/v1/workspaces/{}/{suffix}", k.personal_workspace_id),
                body
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(call(&s, &owner, "POST", &format!("/api/v1/orgs/{}/invitations", k.organization_id), json!({"email":other.email,"organization_role":"member","workspace_id":k.personal_workspace_id,"workspace_role":"member"})).await.0, StatusCode::FORBIDDEN);
    // An org admin can govern a shared project, but cannot issue a human key
    // there without its own current project membership.
    sqlx::query(
        "INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'admin')",
    )
    .bind(k.organization_id)
    .bind(other.user_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        call(
            &s,
            &other,
            "POST",
            &format!("/api/v1/workspaces/{ws}/keys"),
            json!({"name":"Not a project member","expires_in_days":1})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn project_admin_can_tighten_project_and_key_policies_not_parent_or_siblings(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    let ws = project(&s, &owner, k.organization_id).await;
    add_member(&pool, k.organization_id, ws, other.user_id).await;
    sqlx::query(
        "UPDATE workspace_memberships SET role='admin' WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(ws)
    .bind(other.user_id)
    .execute(&pool)
    .await
    .unwrap();
    let policy = |rpm| json!({"requests_per_minute":rpm,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null});
    let org_policy = format!("/api/v1/orgs/{}/policy", k.organization_id);
    assert_eq!(
        call(&s, &owner, "PUT", &org_policy, policy(20)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&s, &other, "PUT", &org_policy, policy(10)).await.0,
        StatusCode::FORBIDDEN
    );
    let project_policy = format!("/api/v1/workspaces/{ws}/policy");
    assert_eq!(
        call(&s, &other, "PUT", &project_policy, policy(10)).await.0,
        StatusCode::OK
    );
    let response = call(&s, &other, "GET", &project_policy, json!({})).await.1;
    assert_eq!(response["policy"]["requests_per_minute"], 10);
    assert_eq!(response["ceiling"]["requests_per_minute"], 20);
    assert_eq!(
        call(&s, &other, "PUT", &project_policy, policy(21)).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &s,
            &other,
            "PUT",
            &format!("/api/v1/workspaces/{}/policy", k.team_workspace_id),
            policy(10)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let key = call(
        &s,
        &other,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys"),
        json!({"name":"Governed", "expires_in_days":1}),
    )
    .await
    .1;
    let key_policy = format!(
        "/api/v1/workspaces/{ws}/keys/{}/policy",
        key["id"].as_str().unwrap()
    );
    assert_eq!(
        call(&s, &other, "PUT", &key_policy, policy(5)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&s, &other, "GET", &key_policy, json!({})).await.1["ceiling"]["requests_per_minute"],
        10
    );
}

#[sqlx::test]
async fn personal_creation_uses_org_entitlement_not_legacy_global_flag(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    let model: Uuid = sqlx::query_scalar(
        "SELECT model_id FROM organization_model_grants WHERE organization_id=$1",
    )
    .bind(k.organization_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE models SET personal_enabled=false WHERE id=$1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE organization_model_grants SET personal_enabled=true WHERE organization_id=$1 AND model_id=$2").bind(k.organization_id).bind(model).execute(&pool).await.unwrap();
    let (status, personal) = call(
        &s,
        &other,
        "POST",
        &format!("/api/v1/orgs/{}/personal-workspace", k.organization_id),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let path = format!(
        "/api/v1/workspaces/{}/grants",
        personal["id"].as_str().unwrap()
    );
    let grants = call(&s, &other, "GET", &path, json!({})).await.1;
    assert!(
        grants["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|grant| grant["model_id"] == model.to_string())
    );
    assert_eq!(
        call(&s, &owner, "GET", &path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
}
