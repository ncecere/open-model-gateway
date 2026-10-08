use super::*;
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn role_source_readonly_last_admin_and_owner_protection(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, users) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/users?limit=2",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(users["data"].as_array().unwrap().len(), 2);
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "POST",
            "/api/v1/platform/users",
            json!({"email":"new@example.test","platform_role":"user"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/users/{}/roles/admin", f.admin.user_id),
            json!({})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/users/{}", f.admin.user_id),
            json!({"disabled":true})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/users/{}", f.owner.user_id),
            json!({"disabled":true})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, new) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/users",
        json!({"email":" Provisioned@Example.test ","platform_role":"auditor"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{new}");
    let uid = id(&new);
    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id=$1")
        .bind(uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(email, "provisioned@example.test");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM workspaces WHERE owner_user_id=$1")
            .bind(uid)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let map = Uuid::new_v4();
    sqlx::query("INSERT INTO oidc_group_mappings(id,issuer,group_value,target_kind,platform_role) VALUES($1,'https://id.test','audit','platform','auditor')").bind(map).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source,mapping_id) VALUES($1,'auditor','group',$2)").bind(uid).bind(map).execute(&pool).await.unwrap();
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/users/{uid}/roles/auditor"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let effective: String =
        sqlx::query_scalar("SELECT role FROM effective_platform_roles WHERE user_id=$1")
            .bind(uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(effective, "auditor");
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM platform_role_grants WHERE user_id=$1 AND source='group' AND revoked_at IS NULL").bind(uid).fetch_one(&pool).await.unwrap(),1);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn mutable_email_cannot_become_verified_invitation_evidence_in_an_existing_session(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let token = vec![3u8; 32];
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) VALUES($1,$2,$1,now()+interval '1 day',$3)").bind(&token).bind(f.member.user_id).bind(&f.member.email).execute(&pool).await.unwrap();
    let (status, invitation) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/invitations", f.project),
        json!({"email":"new-email@example.test","role":"member"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/users/{}", f.member.user_id),
            json!({"email":"new-email@example.test"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT revoked_at IS NOT NULL FROM browser_sessions WHERE token_hash=$1"
        )
        .bind(token)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "POST",
            "/api/v1/invitations/accept",
            json!({"token":invitation["token"]})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // A new session can be minted only by the signature/email_verified checked OIDC callback.
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn manual_suspension_of_personal_owner_revokes_human_credentials_not_service_accounts(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let personal = workspace(&pool, "Member personal", "personal", f.member.user_id).await;
    let (status, k) = key(&f, &f.member, personal, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let (status, a) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/service-accounts", f.team),
        json!({"name":"Survivor"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let account = id(&a);
    let (status, sa) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({"name":"Service","expires_in_days":1,"service_account_id":account}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/users/{}", f.member.user_id),
            json!({"disabled":true})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        f.s.authenticate(k["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        f.s.authenticate(sa["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/users/{}", f.member.user_id),
            json!({"disabled":false})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        f.s.authenticate(k["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_none()
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn group_mapping_control_plane_disable_delete_revokes_only_its_source_immediately(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let mut mappings = Vec::new();
    for group in ["first", "second"] {
        let(status,v)=call(&f.s,&f.admin,"POST","/api/v1/platform/oidc/group-mappings",json!({"issuer":"https://id.test","group_value":group,"target_kind":"workspace","workspace_id":f.team,"workspace_role":"admin","enabled":true})).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let map = id(&v);
        mappings.push(map);
        sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source,mapping_id) VALUES($1,$2,'admin','group',$3)").bind(f.team).bind(f.member.user_id).bind(map).execute(&pool).await.unwrap();
    }
    let (status, key) = key(&f, &f.member, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/oidc/group-mappings/{}", mappings[0]),
            json!({"enabled":false})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM workspace_membership_grants WHERE user_id=$1 AND source='group' AND revoked_at IS NULL").bind(f.member.user_id).fetch_one(&pool).await.unwrap(),1);
    assert!(
        f.s.authenticate(key["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/oidc/group-mappings/{}", mappings[1]),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let role: String = sqlx::query_scalar(
        "SELECT role FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(f.team)
    .bind(f.member.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(role, "member");
    assert!(
        f.s.authenticate(key["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_some()
    );
    let(status,v)=call(&f.s,&f.admin,"POST","/api/v1/platform/oidc/group-mappings",json!({"issuer":"https://id.test","group_value":"entitlement","target_kind":"platform","platform_role":"user","enabled":true})).await;
    assert_eq!(status, StatusCode::OK);
    let map = id(&v);
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source,mapping_id) VALUES($1,'user','group',$2)").bind(f.member.user_id).bind(map).execute(&pool).await.unwrap();
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/oidc/group-mappings/{map}"),
            json!({"enabled":false})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        f.s.authenticate(key["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    let (reason, due): (String, bool) = sqlx::query_as(
        "SELECT disable_reason,cleanup_due_at>now()+interval '29 days' FROM users WHERE id=$1",
    )
    .bind(f.member.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reason, "entitlement_loss");
    assert!(due);
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn shared_directory_excludes_personal_and_allocation_changes_only_future_workspace_pointer(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let (status, v) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/workspaces?kind=project",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["id"], f.project.to_string());
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "GET",
            "/api/v1/platform/workspaces?kind=personal",
            json!({})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            "/api/v1/platform/cost-centers",
            json!({"name":"Denied","code":"D"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/cost-centers",
        json!({"name":"Research","code":"R"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cc = id(&v);
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "PATCH",
            &format!("/api/v1/platform/workspaces/{}", f.team),
            json!({"cost_center_id":cc})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "PATCH",
            &format!("/api/v1/workspaces/{}", f.team),
            json!({"cost_center_id":cc})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for ws in [f.team, f.personal] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "PATCH",
                &format!("/api/v1/platform/workspaces/{ws}"),
                json!({"cost_center_id":cc})
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
            "PATCH",
            &format!("/api/v1/platform/cost-centers/{cc}"),
            json!({"name":"Renamed","code":"NEW"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/platform/cost-centers/{cc}"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM workspaces WHERE cost_center_id IS NOT NULL"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    // Historical execution snapshot assertions live in financial admission tests, not rewritten here.
}
async fn signed_in(pool: &PgPool, user: Uuid, days_ago: i32) {
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,created_at,expires_at) VALUES($1,$2,$3,now()-make_interval(days=>$4),now()+interval '1 day')")
        .bind(Uuid::new_v4().as_bytes().repeat(2))
        .bind(user)
        .bind(Uuid::new_v4().as_bytes().repeat(2))
        .bind(days_ago)
        .execute(pool)
        .await
        .unwrap();
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn directory_display_fields_and_filters_stay_shared_only(pool: PgPool) {
    let f = fixture(&pool).await;
    signed_in(&pool, f.owner.user_id, 3).await;
    signed_in(&pool, f.owner.user_id, 1).await;
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, v) = call(&f.s, &f.auditor, "GET", "/api/v1/platform/users", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let owner = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == f.owner.user_id.to_string())
        .unwrap()
        .clone();
    // Team + Project; the owner's personal workspace grant is never counted.
    assert_eq!(owner["shared_workspace_count"], 2);
    assert!(owner["last_sign_in_at"].is_string());
    let never = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == f.admin.user_id.to_string())
        .unwrap();
    assert!(never["last_sign_in_at"].is_null());
    let ids = |v: &Value| -> Vec<String> {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["id"].as_str().unwrap().to_owned())
            .collect()
    };
    // Search also matches the optional display name (presentation only).
    sqlx::query("UPDATE users SET display_name='Quinn Zephyr' WHERE id=$1")
        .bind(f.auditor.user_id)
        .execute(&pool)
        .await
        .unwrap();
    for (query, expected) in [
        ("q=zephyr", vec![f.auditor.user_id]),
        ("q=OWN", vec![f.owner.user_id]),
        ("role=auditor", vec![f.auditor.user_id]),
        ("role=none", vec![f.outsider.user_id]),
        ("status=suspended", vec![f.outsider.user_id]),
        ("status=active&role=user&q=member", vec![f.member.user_id]),
    ] {
        let (status, v) = call(
            &f.s,
            &f.admin,
            "GET",
            &format!("/api/v1/platform/users?{query}"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{query}: {v}");
        assert_eq!(
            ids(&v),
            expected.iter().map(Uuid::to_string).collect::<Vec<_>>(),
            "{query}"
        );
    }
    for query in ["role=owner", "status=disabled", "unknown=1"] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "GET",
                &format!("/api/v1/platform/users?{query}"),
                json!({})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST,
            "{query}"
        );
    }
    assert_eq!(
        call(&f.s, &f.owner, "GET", "/api/v1/platform/users", json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, detail) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/users/{}", f.owner.user_id),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(detail["first_sign_in_at"].is_string());
    assert_ne!(detail["first_sign_in_at"], detail["last_sign_in_at"]);
    let memberships = detail["shared_memberships"].as_array().unwrap();
    assert_eq!(memberships.len(), 2);
    assert!(memberships.iter().all(|m| m["role"] == "owner"
        && m["sources"] == json!(["manual"])
        && m["kind"] != "personal"));
    assert!(!detail.to_string().contains(&f.personal.to_string()));
    let (_, none) = call(
        &f.s,
        &f.admin,
        "GET",
        &format!("/api/v1/platform/users/{}", f.admin.user_id),
        json!({}),
    )
    .await;
    assert!(none["first_sign_in_at"].is_null());
    assert_eq!(none["shared_memberships"], json!([]));
    let (_, cc) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/cost-centers",
        json!({"name":"Research","code":"R-1"}),
    )
    .await;
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/workspaces/{}", f.team),
            json!({"cost_center_id":id(&cc)})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, v) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/workspaces?kind=team&status=active",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["data"][0]["member_count"], 2);
    assert_eq!(v["data"][0]["cost_center"]["name"], "Research");
    assert_eq!(v["data"][0]["cost_center"]["code"], "R-1");
    // Workspace Settings › General names the cost center for members (not just "assigned").
    let (status, v) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}", f.team),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["cost_center"]["name"], "Research");
    assert_eq!(v["cost_center"]["code"], "R-1");
    let (_, v) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}", f.personal),
        json!({}),
    )
    .await;
    assert!(v["cost_center"].is_null(), "{v}");
    let (_, v) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/workspaces/{}", f.project),
        json!({}),
    )
    .await;
    assert_eq!(v["member_count"], 1);
    assert!(v["cost_center"].is_null());
    let (_, v) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/workspaces?status=disabled",
        json!({}),
    )
    .await;
    assert_eq!(v["data"], json!([]));
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "GET",
            "/api/v1/platform/workspaces?status=archived",
            json!({})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
