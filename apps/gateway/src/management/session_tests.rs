use super::{
    tests::{add_member, call, fixture},
    *,
};
use sqlx::PgPool;

async fn session(s: &Store, u: &BrowserPrincipal) -> Value {
    let (status, body) = call(s, u, "GET", "/api/v1/me", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn entry<'a>(body: &'a Value, collection: &str, id: Uuid) -> &'a Value {
    body[collection]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == id.to_string())
        .unwrap_or_else(|| panic!("Missing {collection} {id}: {body}"))
}

fn org_capabilities(is_admin: bool, is_owner: bool, is_member: bool) -> Value {
    json!({
        "create_workspace": is_admin && is_member,
        "create_personal_workspace": is_member,
        "manage_members": is_admin,
        "manage_owners": is_owner,
        "delegate_models": is_admin,
        "manage_policy": is_admin,
    })
}

fn shared_capabilities(issue: bool, is_admin: bool, is_owner: bool, org_admin: bool) -> Value {
    json!({
        "issue_own_key": issue,
        "manage_members": is_admin,
        "manage_service_accounts": is_admin,
        "manage_owners": is_owner,
        "delegate_models": org_admin,
        "manage_policy": is_admin,
        "view_all_activity": is_admin,
    })
}

async fn issue_key(s: &Store, u: &BrowserPrincipal, ws: Uuid) -> (StatusCode, Value) {
    call(
        s,
        u,
        "POST",
        &format!("/api/v1/workspaces/{ws}/keys"),
        json!({"name":"Session capability probe", "expires_in_days":1}),
    )
    .await
}

async fn put_policy(s: &Store, u: &BrowserPrincipal, ws: Uuid) -> StatusCode {
    call(
        s,
        u,
        "PUT",
        &format!("/api/v1/workspaces/{ws}/policy"),
        json!({"requests_per_minute":10,"tokens_per_minute":null,
            "concurrent_requests":null,"monthly_budget_microusd":null}),
    )
    .await
    .0
}

#[sqlx::test]
async fn shared_role_matrix_keeps_membership_authority_and_endpoint_powers_distinct(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    let (status, project) = call(
        &s,
        &owner,
        "POST",
        &format!("/api/v1/orgs/{}/workspaces", k.organization_id),
        json!({"name":"Session project", "kind":"project"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let project = Uuid::parse_str(project["id"].as_str().unwrap()).unwrap();
    // Explicit expectations catch shared ownership outranking inherited admin,
    // and inherited authority winning ties without pretending to be membership.
    let cases = [
        ("member", None, None, "direct"),
        ("member", Some("member"), Some("member"), "direct"),
        ("member", Some("admin"), Some("admin"), "direct"),
        ("member", Some("owner"), Some("owner"), "direct"),
        ("admin", None, Some("admin"), "organization"),
        ("admin", Some("member"), Some("admin"), "organization"),
        ("admin", Some("admin"), Some("admin"), "organization"),
        ("admin", Some("owner"), Some("owner"), "direct"),
        ("owner", None, Some("owner"), "organization"),
        ("owner", Some("member"), Some("owner"), "organization"),
        ("owner", Some("admin"), Some("owner"), "organization"),
        ("owner", Some("owner"), Some("owner"), "organization"),
    ];
    for ws in [k.team_workspace_id, project] {
        for (org_role, membership, effective, source) in cases {
            sqlx::query("UPDATE organization_memberships SET role=$3 WHERE organization_id=$1 AND user_id=$2")
                .bind(k.organization_id).bind(other.user_id).bind(org_role)
                .execute(&pool).await.unwrap();
            sqlx::query("DELETE FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2")
                .bind(ws)
                .bind(other.user_id)
                .execute(&pool)
                .await
                .unwrap();
            if let Some(role) = membership {
                sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,$4)")
                    .bind(k.organization_id).bind(ws).bind(other.user_id).bind(role)
                    .execute(&pool).await.unwrap();
            }
            let body = session(&s, &other).await;
            let org = entry(&body, "organizations", k.organization_id);
            assert_eq!(org["role"], org_role);
            assert_eq!(org["membership_role"], org_role);
            assert_eq!(org["authority_source"], "direct");
            assert_eq!(
                org["capabilities"],
                org_capabilities(org_role != "member", org_role == "owner", true)
            );
            assert!(
                !body
                    .to_string()
                    .contains(&k.personal_workspace_id.to_string())
            );
            let (status, key) = issue_key(&s, &other, ws).await;
            assert_eq!(
                status,
                if membership.is_some() {
                    StatusCode::OK
                } else {
                    StatusCode::FORBIDDEN
                }
            );
            let Some(role) = effective else {
                assert!(
                    !body["workspaces"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|w| w["id"] == ws.to_string())
                );
                continue;
            };
            let workspace = entry(&body, "workspaces", ws);
            assert_eq!(workspace["organization_id"], k.organization_id.to_string());
            assert_eq!(workspace["role"], role);
            assert_eq!(workspace["membership_role"], json!(membership));
            assert_eq!(workspace["authority_source"], source);
            assert_eq!(
                workspace["capabilities"],
                shared_capabilities(
                    membership.is_some(),
                    role != "member",
                    role == "owner",
                    org_role != "member"
                )
            );
            assert_eq!(
                workspace["own_key_denial_reason"],
                if membership.is_some() {
                    Value::Null
                } else {
                    json!("workspace_membership_required")
                }
            );
            assert_eq!(
                put_policy(&s, &other, ws).await,
                if role != "member" {
                    StatusCode::OK
                } else {
                    StatusCode::FORBIDDEN
                }
            );
            if membership.is_some() {
                let id = Uuid::parse_str(key["id"].as_str().unwrap()).unwrap();
                let lineage: (Uuid, Uuid, Uuid, Uuid) = sqlx::query_as("SELECT organization_id,workspace_id,issued_to_user_id,governance_key_id FROM api_keys WHERE id=$1")
                    .bind(id).fetch_one(&pool).await.unwrap();
                assert_eq!(lineage, (k.organization_id, ws, other.user_id, id));
            }
        }
    }
}

#[sqlx::test]
async fn operator_authority_never_substitutes_for_active_key_membership(pool: PgPool) {
    let (s, k, owner, mut operator) = fixture(&pool).await;
    operator.platform_admin = true;
    sqlx::query("UPDATE users SET platform_admin=true WHERE id=$1")
        .bind(operator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for state in [
        "absent",
        "active_org",
        "active_workspace",
        "disabled_workspace",
        "disabled_org",
    ] {
        match state {
            "active_org" => {
                sqlx::query("INSERT INTO organization_memberships(organization_id,user_id,role) VALUES($1,$2,'member')")
                    .bind(k.organization_id).bind(operator.user_id).execute(&pool).await.unwrap();
            }
            "active_workspace" => {
                sqlx::query("INSERT INTO workspace_memberships(organization_id,workspace_id,user_id,role) VALUES($1,$2,$3,'owner')")
                    .bind(k.organization_id).bind(k.team_workspace_id).bind(operator.user_id).execute(&pool).await.unwrap();
            }
            "disabled_workspace" => {
                let (status, _) = call(
                    &s,
                    &owner,
                    "PATCH",
                    &format!(
                        "/api/v1/workspaces/{}/members/{}",
                        k.team_workspace_id, operator.user_id
                    ),
                    json!({"role":"owner","disabled":true}),
                )
                .await;
                assert_eq!(status, StatusCode::OK);
            }
            "disabled_org" => {
                // Leave a direct active row to prove the organization gate wins.
                sqlx::query("UPDATE workspace_memberships SET disabled_at=NULL WHERE user_id=$1")
                    .bind(operator.user_id)
                    .execute(&pool)
                    .await
                    .unwrap();
                let (status, _) = call(
                    &s,
                    &owner,
                    "PATCH",
                    &format!(
                        "/api/v1/orgs/{}/members/{}",
                        k.organization_id, operator.user_id
                    ),
                    json!({"role":"member","disabled":true}),
                )
                .await;
                assert_eq!(status, StatusCode::OK);
            }
            _ => {}
        }
        let active_org = !matches!(state, "absent" | "disabled_org");
        let active_workspace = matches!(state, "active_workspace" | "disabled_org");
        let body = session(&s, &operator).await;
        let org = entry(&body, "organizations", k.organization_id);
        assert_eq!(org["role"], "operator");
        assert_eq!(
            org["membership_role"],
            if active_org {
                json!("member")
            } else {
                Value::Null
            }
        );
        assert_eq!(org["authority_source"], "platform");
        assert_eq!(
            org["capabilities"],
            org_capabilities(true, true, active_org)
        );
        let ws = entry(&body, "workspaces", k.team_workspace_id);
        assert_eq!(ws["role"], "owner");
        assert_eq!(
            ws["membership_role"],
            if active_workspace {
                json!("owner")
            } else {
                Value::Null
            }
        );
        assert_eq!(ws["authority_source"], "platform");
        assert_eq!(
            ws["capabilities"],
            shared_capabilities(active_org && active_workspace, true, true, true)
        );
        assert_eq!(
            ws["own_key_denial_reason"],
            if !active_org {
                json!("organization_membership_required")
            } else if !active_workspace {
                json!("workspace_membership_required")
            } else {
                Value::Null
            }
        );
        assert_eq!(
            issue_key(&s, &operator, k.team_workspace_id).await.0,
            if active_org && active_workspace {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            }
        );
        assert_eq!(
            put_policy(&s, &operator, k.team_workspace_id).await,
            StatusCode::OK
        );
        assert!(
            !body
                .to_string()
                .contains(&k.personal_workspace_id.to_string())
        );
        if !active_org {
            for (suffix, payload) in [
                ("workspaces", json!({"name":"Cannot own"})),
                ("personal-workspace", json!({})),
            ] {
                assert_eq!(
                    call(
                        &s,
                        &operator,
                        "POST",
                        &format!("/api/v1/orgs/{}/{suffix}", k.organization_id),
                        payload
                    )
                    .await
                    .0,
                    StatusCode::FORBIDDEN
                );
            }
        }
    }
    // /me reads current privilege, not the stale BrowserPrincipal snapshot.
    sqlx::query("UPDATE users SET platform_admin=false WHERE id=$1")
        .bind(operator.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let body = session(&s, &operator).await;
    assert_eq!(body["user"]["platform_admin"], false);
    assert_eq!(body["organizations"], json!([]));
    assert_eq!(body["workspaces"], json!([]));
}

#[sqlx::test]
async fn personal_ownership_does_not_imply_policy_or_model_delegation(pool: PgPool) {
    let (s, k, owner, mut other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    let (status, personal) = call(
        &s,
        &other,
        "POST",
        &format!("/api/v1/orgs/{}/personal-workspace", k.organization_id),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let personal = Uuid::parse_str(personal["id"].as_str().unwrap()).unwrap();
    // Personal ownership does not rely on a workspace_memberships row.
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workspace_memberships WHERE workspace_id=$1")
            .bind(personal)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    for role in ["member", "admin", "owner", "operator"] {
        other.platform_admin = role == "operator";
        sqlx::query("UPDATE users SET platform_admin=$2 WHERE id=$1")
            .bind(other.user_id)
            .bind(other.platform_admin)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE organization_memberships SET role=$3 WHERE organization_id=$1 AND user_id=$2",
        )
        .bind(k.organization_id)
        .bind(other.user_id)
        .bind(if role == "operator" { "member" } else { role })
        .execute(&pool)
        .await
        .unwrap();
        let body = session(&s, &other).await;
        let ws = entry(&body, "workspaces", personal);
        assert_eq!(ws["role"], "owner");
        assert_eq!(ws["membership_role"], "owner");
        assert_eq!(ws["authority_source"], "personal");
        assert_eq!(ws["own_key_denial_reason"], Value::Null);
        assert_eq!(
            ws["capabilities"],
            json!({
                "issue_own_key":true,"manage_members":false,"manage_service_accounts":false,
                "manage_owners":false,"delegate_models":role != "member",
                "manage_policy":role != "member","view_all_activity":true,
            })
        );
        assert_eq!(issue_key(&s, &other, personal).await.0, StatusCode::OK);
        assert_eq!(
            put_policy(&s, &other, personal).await,
            if role == "member" {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::OK
            }
        );
        assert!(
            !body
                .to_string()
                .contains(&k.personal_workspace_id.to_string())
        );
        assert_eq!(
            call(
                &s,
                &other,
                "GET",
                &format!("/api/v1/workspaces/{}/keys", k.personal_workspace_id),
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    // Operators retain visibility of their own personal space after org removal,
    // but never gain the right to issue a human key without active membership.
    assert_eq!(
        call(
            &s,
            &owner,
            "PATCH",
            &format!(
                "/api/v1/orgs/{}/members/{}",
                k.organization_id, other.user_id
            ),
            json!({"role":"member","disabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    let body = session(&s, &other).await;
    let ws = entry(&body, "workspaces", personal);
    assert_eq!(ws["authority_source"], "personal");
    assert_eq!(ws["membership_role"], "owner");
    assert_eq!(ws["capabilities"]["issue_own_key"], false);
    assert_eq!(
        ws["own_key_denial_reason"],
        "organization_membership_required"
    );
    assert_eq!(
        issue_key(&s, &other, personal).await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn membership_revocation_changes_metadata_and_revokes_key_lineage(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    sqlx::query("UPDATE organization_memberships SET role='admin' WHERE user_id=$1")
        .bind(other.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, key) = issue_key(&s, &other, k.team_workspace_id).await;
    assert_eq!(status, StatusCode::OK);
    let id = key["id"].as_str().unwrap();
    let token = key["token"].as_str().unwrap();
    let (status, rotated) = call(
        &s,
        &other,
        "POST",
        &format!(
            "/api/v1/workspaces/{}/keys/{id}/rotate",
            k.team_workspace_id
        ),
        json!({"expires_in_days":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rotated_id = Uuid::parse_str(rotated["id"].as_str().unwrap()).unwrap();
    let lineage: Uuid = sqlx::query_scalar("SELECT governance_key_id FROM api_keys WHERE id=$1")
        .bind(rotated_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lineage.to_string(), id);
    assert!(s.authenticate(token).await.unwrap().is_none());
    let token = rotated["token"].as_str().unwrap();
    assert!(s.authenticate(token).await.unwrap().is_some());
    let path = format!(
        "/api/v1/workspaces/{}/members/{}",
        k.team_workspace_id, other.user_id
    );
    for disabled in [true, false] {
        assert_eq!(
            call(
                &s,
                &owner,
                "PATCH",
                &path,
                json!({"role":"member","disabled":disabled})
            )
            .await
            .0,
            StatusCode::OK
        );
        let body = session(&s, &other).await;
        let ws = entry(&body, "workspaces", k.team_workspace_id);
        assert_eq!(ws["role"], "admin");
        assert_eq!(ws["authority_source"], "organization");
        assert_eq!(
            ws["membership_role"],
            if disabled {
                Value::Null
            } else {
                json!("member")
            }
        );
        assert_eq!(ws["capabilities"]["issue_own_key"], !disabled);
        assert_eq!(
            issue_key(&s, &other, k.team_workspace_id).await.0,
            if disabled {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::OK
            }
        );
        assert!(s.authenticate(token).await.unwrap().is_none());
    }
    // Organization membership still gates an otherwise active direct membership.
    assert_eq!(
        call(
            &s,
            &owner,
            "PATCH",
            &format!(
                "/api/v1/orgs/{}/members/{}",
                k.organization_id, other.user_id
            ),
            json!({"role":"admin","disabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    let body = session(&s, &other).await;
    assert_eq!(body["organizations"], json!([]));
    assert_eq!(body["workspaces"], json!([]));
    assert_eq!(
        issue_key(&s, &other, k.team_workspace_id).await.0,
        StatusCode::FORBIDDEN
    );
}

// Opt in separately: normal integration tests only require CREATEDB. This probe
// additionally needs CREATEROLE, and rolls back the temporary role and all ACLs
// inside the disposable SQLx database (never touching existing runtime roles).
#[sqlx::test]
#[ignore = "requires CREATEROLE for the rollback-only runtime ACL probe"]
async fn session_runtime_acl_rollback_probe(pool: PgPool) {
    let mut tx = pool.begin().await.unwrap();
    let role = format!("session_acl_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE ROLE {role} NOLOGIN"))
        .execute(&mut *tx)
        .await
        .unwrap();
    let grants = include_str!("../../../../deploy/staging/runtime-grants.sql")
        .replace("gateway_runtime", &role)
        .replace("\nBEGIN;\n", "\n")
        .replace("\nCOMMIT;", "\n");
    sqlx::raw_sql(&grants).execute(&mut *tx).await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    // Run the actual rollback smoke statements, not installation checks that
    // intentionally depend on staging's database/cluster role configuration.
    let (_, smoke) = include_str!("../../../../deploy/staging/verify-privileges.sql")
        .split_once("\nBEGIN;\nSET LOCAL ROLE gateway_runtime;\n")
        .unwrap();
    let smoke = smoke.strip_suffix("ROLLBACK;\n").unwrap();
    sqlx::raw_sql(smoke).execute(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
}

#[sqlx::test]
async fn disabled_resources_and_users_remain_excluded(pool: PgPool) {
    let (s, k, owner, other) = fixture(&pool).await;
    add_member(&pool, k.organization_id, k.team_workspace_id, other.user_id).await;
    sqlx::query("UPDATE workspace_memberships SET disabled_at=now() WHERE user_id=$1")
        .bind(other.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(session(&s, &other).await["workspaces"], json!([]));
    assert_eq!(
        issue_key(&s, &other, k.team_workspace_id).await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE workspaces SET disabled_at=now() WHERE id=$1")
        .bind(k.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    let body = session(&s, &owner).await;
    assert!(!body.to_string().contains(&k.team_workspace_id.to_string()));
    assert_eq!(
        entry(&body, "workspaces", k.personal_workspace_id)["role"],
        "owner"
    );
    sqlx::query("UPDATE organizations SET disabled_at=now() WHERE id=$1")
        .bind(k.organization_id)
        .execute(&pool)
        .await
        .unwrap();
    let body = session(&s, &owner).await;
    assert_eq!(body["organizations"], json!([]));
    assert_eq!(body["workspaces"], json!([]));
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(owner.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&s, &owner, "GET", "/api/v1/me", json!({})).await.0,
        StatusCode::FORBIDDEN
    );
}
