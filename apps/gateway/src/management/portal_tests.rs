use super::*;
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn user_direct_read_is_live_platform_metadata_only(pool: PgPool) {
    let f = fixture(&pool).await;
    let path = format!("/api/v1/platform/users/{}", f.member.user_id);
    for actor in [&f.admin, &f.auditor] {
        let (status, body) = call(&f.s, actor, "GET", &path, json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], f.member.user_id.to_string());
        assert_eq!(body["email"], f.member.email);
        assert!(body.get("keys").is_none());
        assert!(body.get("executions").is_none());
        assert!(body.get("personal_workspace").is_none());
    }
    assert_eq!(
        call(&f.s, &f.member, "GET", &path, json!({})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "GET",
            &format!("/api/v1/platform/users/{}", Uuid::new_v4()),
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_audit_is_readonly_authority_and_excludes_all_personal_events(pool: PgPool) {
    let f = fixture(&pool).await;
    for ws in [None, Some(f.team), Some(f.personal)] {
        sqlx::query("INSERT INTO audit_events(id,actor_user_id,workspace_id,action,resource_type,metadata) VALUES($1,$2,$3,'test.scope','test','{}')")
            .bind(Uuid::new_v4()).bind(f.owner.user_id).bind(ws).execute(&pool).await.unwrap();
    }
    for actor in [&f.admin, &f.auditor] {
        let (status, body) = call(&f.s, actor, "GET", "/api/v1/platform/audit", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"].as_array().unwrap().len(), 2);
        assert!(
            !body["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["workspace_id"] == f.personal.to_string())
        );
    }
    assert_eq!(
        call(&f.s, &f.member, "GET", "/api/v1/platform/audit", json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "POST",
            "/api/v1/platform/audit",
            json!({})
        )
        .await
        .0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "GET",
            "/api/v1/platform/audit?limit=1&offset=2",
            json!({})
        )
        .await
        .1["data"],
        json!([])
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn auditor_workspace_contexts_require_actual_shared_membership(pool: PgPool) {
    let f = fixture(&pool).await;
    let personal = workspace(&pool, "Auditor personal", "personal", f.auditor.user_id).await;
    let (status, me) = call(&f.s, &f.auditor, "GET", "/api/v1/me", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["capabilities"]["platform_read"], true);
    assert_eq!(me["capabilities"]["platform_write"], false);
    let contexts = me["workspaces"].as_array().unwrap();
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0]["id"], personal.to_string());
    member(&pool, f.project, f.auditor.user_id, "member").await;
    let (_, me) = call(&f.s, &f.auditor, "GET", "/api/v1/me", json!({})).await;
    let project = me["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == f.project.to_string())
        .unwrap();
    assert_eq!(project["role"], "member");
    assert_eq!(project["capabilities"]["issue_own_key"], true);
    assert_eq!(project["capabilities"]["view_all_activity"], false);
    assert!(
        !me["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == f.team.to_string())
    );
    // Platform Admins, like Auditors, get no platform-staff workspace contexts.
    let (_, admin) = call(&f.s, &f.admin, "GET", "/api/v1/me", json!({})).await;
    assert!(
        !admin["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == f.team.to_string())
    );
    assert!(
        !admin["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == personal.to_string())
    );
}
/// Live acceptance F6: route and price audit targets carry a display name derived
/// from platform metadata only.
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_audit_names_route_and_price_targets(pool: PgPool) {
    let f = fixture(&pool).await;
    let (provider, deployment) = (Uuid::new_v4(), Uuid::new_v4());
    let m = model(&pool, "company/luna").await;
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'OpenAI','openai','env:TEST')").bind(provider).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'gpt-6-luna')").bind(deployment).bind(m).bind(provider).execute(&pool).await.unwrap();
    let mut prices = vec![];
    for minute in 0..2 {
        let price = Uuid::new_v4();
        sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version,created_at) VALUES($1,$2,1,1,10,10,1,now()+make_interval(mins=>$3))").bind(price).bind(deployment).bind(minute).execute(&pool).await.unwrap();
        prices.push(price);
    }
    for (kind, target) in [
        ("deployment", deployment),
        ("price", prices[1]),
        ("model", m),
    ] {
        sqlx::query("INSERT INTO audit_events(id,actor_user_id,action,resource_type,resource_id) VALUES($1,$2,$3,$4,$5)")
            .bind(Uuid::new_v4()).bind(f.admin.user_id).bind(format!("{kind}.created")).bind(kind).bind(target).execute(&pool).await.unwrap();
    }
    let (status, body) = call(&f.s, &f.auditor, "GET", "/api/v1/platform/audit", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let name = |kind: &str| {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["resource_type"] == kind)
            .unwrap()["target_name"]
            .clone()
    };
    assert_eq!(name("deployment"), "gpt-6-luna on OpenAI");
    assert_eq!(name("price"), "Price v2 for gpt-6-luna on OpenAI");
    // Named targets are named by the server too (R06, #67): the dashboard no longer loads every model.
    assert_eq!(name("model"), "company/luna");
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn suspension_and_reactivation_are_audited_as_what_they_are(pool: PgPool) {
    // #158: an off-host audit export must show who was suspended and
    // reactivated, distinct from a profile edit, without personal data.
    let f = fixture(&pool).await;
    let path = format!("/api/v1/platform/users/{}", f.outsider.user_id);
    let events = || async {
        sqlx::query_as::<_, (String, Value)>(
            "SELECT action,metadata FROM audit_events WHERE resource_type='user' AND resource_id=$1 ORDER BY created_at,id",
        )
        .bind(f.outsider.user_id)
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    let patch = |body: Value| call(&f.s, &f.admin, "PATCH", &path, body);
    assert_eq!(patch(json!({"disabled":true})).await.0, StatusCode::OK);
    // A repeat revokes credentials again and moves the cleanup date: recorded too.
    assert_eq!(patch(json!({"disabled":true})).await.0, StatusCode::OK);
    assert_eq!(patch(json!({"disabled":false})).await.0, StatusCode::OK);
    assert_eq!(
        patch(json!({"email":"renamed@example.invalid"})).await.0,
        StatusCode::OK
    );
    let link: bool = sqlx::query_scalar("SELECT oidc_link_allowed FROM users WHERE id=$1")
        .bind(f.outsider.user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    // Changing it records once; setting the value it already has records nothing.
    for _ in 0..2 {
        assert_eq!(
            patch(json!({"oidc_link_allowed":!link})).await.0,
            StatusCode::OK
        );
    }
    // Disabled for entitlement loss, as the role-sync path does; no grants left.
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND revoked_at IS NULL; ")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET disabled_at=now(),disable_reason='entitlement_loss',cleanup_due_at=now()+interval '30 days' WHERE id=$1")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    // Reactivation is refused without a live grant, and leaves no record.
    assert_eq!(
        patch(json!({"disabled":false})).await.0,
        StatusCode::FORBIDDEN
    );
    // An admin suspension of that user, with an email change: two records.
    assert_eq!(
        patch(json!({"disabled":true,"email":"again@example.invalid"}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        events().await,
        vec![
            (
                "user.suspended".into(),
                json!({"reason":"admin_suspension"})
            ),
            (
                "user.suspended".into(),
                json!({"reason":"admin_suspension"})
            ),
            ("user.reactivated".into(), json!({})),
            ("user.updated".into(), json!({"email_changed":true})),
            ("user.updated".into(), json!({"oidc_link_changed":true})),
            (
                "user.suspended".into(),
                json!({"reason":"admin_suspension"})
            ),
            ("user.updated".into(), json!({"email_changed":true})),
        ],
        "no email or other personal value in the metadata"
    );
}
