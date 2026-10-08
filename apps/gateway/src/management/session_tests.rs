use super::*;
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn organization_free_me_is_live_and_capabilities_match_authority_not_session_hints(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let (_, me) = call(&f.s, &f.admin, "GET", "/api/v1/me", json!({})).await;
    assert!(me.get("organizations").is_none());
    assert!(me["installation"]["id"].is_string());
    assert_eq!(me["user"]["platform_role"], "admin");
    assert_eq!(
        me["capabilities"],
        json!({"platform_read":true,"platform_write":true,"create_workspace":true})
    );
    assert!(
        me["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["kind"] != "personal")
    );
    // Platform Admin workspace context lists only real memberships (none here).
    assert_eq!(me["workspaces"], json!([]));
    // With a real membership, the role and capabilities follow it, not the platform role.
    member(&pool, f.team, f.admin.user_id, "member").await;
    let (_, me) = call(&f.s, &f.admin, "GET", "/api/v1/me", json!({})).await;
    assert_eq!(me["workspaces"].as_array().unwrap().len(), 1);
    let team = &me["workspaces"][0];
    assert_eq!(team["id"], f.team.to_string());
    assert_eq!(team["role"], "member");
    assert_eq!(
        team["capabilities"],
        json!({"issue_own_key":true,"manage_members":false,"manage_service_accounts":false,"manage_policy":false,"delegate_models":false,"view_all_activity":false,"rename":false,"manage_keys":false})
    );
    let (_, owner_me) = call(&f.s, &f.owner, "GET", "/api/v1/me", json!({})).await;
    let personal = &owner_me["workspaces"][0];
    assert_eq!(personal["kind"], "personal");
    assert_eq!(
        personal["capabilities"],
        json!({"issue_own_key":true,"manage_members":false,"manage_service_accounts":false,"manage_policy":false,"delegate_models":true,"view_all_activity":true,"rename":false,"manage_keys":true})
    );
    let shared_owner = owner_me["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == f.team.to_string())
        .unwrap();
    assert_eq!(shared_owner["capabilities"]["rename"], true);
    assert_eq!(shared_owner["capabilities"]["manage_policy"], true);
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        key(&f, &f.admin, f.team, Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    let (_, audit_me) = call(&f.s, &f.auditor, "GET", "/api/v1/me", json!({})).await;
    assert_eq!(audit_me["capabilities"]["platform_read"], true);
    assert_eq!(audit_me["capabilities"]["platform_write"], false);
    // Global financial/configuration visibility is not an operating workspace
    // membership. Foreign shared contexts would offer forbidden detail pages.
    assert_eq!(audit_me["workspaces"], json!([]));
    let (_, member_me) = call(&f.s, &f.member, "GET", "/api/v1/me", json!({})).await;
    assert_eq!(member_me["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(member_me["workspaces"][0]["role"], "member");
    assert_eq!(member_me["workspaces"][0]["membership_source"], "manual");
    assert_eq!(
        member_me["workspaces"][0]["capabilities"]["issue_own_key"],
        true
    );
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    // stale BrowserPrincipal.platform_admin=true cannot preserve administrative authority.
    let (_, demoted) = call(&f.s, &f.admin, "GET", "/api/v1/me", json!({})).await;
    assert_eq!(demoted["user"]["platform_role"], "user");
    assert_eq!(demoted["capabilities"]["platform_read"], false);
    assert_eq!(demoted["workspaces"], json!([]));
    assert_eq!(
        call(&f.s, &f.admin, "GET", "/api/v1/platform/models", json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.member.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f.s, &f.member, "GET", "/api/v1/me", json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn management_router_requires_browser_session_not_inference_key(pool: PgPool) {
    let f = fixture(&pool).await;
    let identity = IdentityState::new(f.s.clone(), None).await.unwrap();
    let app = router(identity).with_state(f.s.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/me")
                .header("authorization", "Bearer omg_fake")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
