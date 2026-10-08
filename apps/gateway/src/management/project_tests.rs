use super::*;
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn team_and_project_creation_requires_live_platform_admin_and_entitled_initial_owner(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    for actor in [&f.owner, &f.auditor, &f.member] {
        assert_eq!(
            call(
                &f.s,
                actor,
                "POST",
                "/api/v1/platform/workspaces",
                json!({"name":"No","kind":"team","owner_user_id":f.owner.user_id})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    for kind in ["team", "project"] {
        let (status, v) = call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/workspaces",
            json!({"name":kind,"kind":kind,"owner_user_id":f.member.user_id}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let ws = id(&v);
        let role:String=sqlx::query_scalar("SELECT role FROM workspace_membership_grants WHERE workspace_id=$1 AND user_id=$2 AND source='manual' AND revoked_at IS NULL").bind(ws).bind(f.member.user_id).fetch_one(&pool).await.unwrap();
        assert_eq!(role, "owner");
        let owner_column: Option<Uuid> =
            sqlx::query_scalar("SELECT owner_user_id FROM workspaces WHERE id=$1")
                .bind(ws)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(owner_column.is_none());
    }
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/workspaces",
            json!({"name":"Private","kind":"personal","owner_user_id":f.member.user_id})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    sqlx::query("UPDATE users SET disabled_at=now() WHERE id=$1")
        .bind(f.outsider.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/workspaces",
            json!({"name":"Inactive owner","kind":"project","owner_user_id":f.outsider.user_id})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn project_is_sibling_not_implicitly_authorized_by_team_membership(pool: PgPool) {
    let f = fixture(&pool).await;
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "GET",
            &format!("/api/v1/workspaces/{}", f.team),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "GET",
            &format!("/api/v1/workspaces/{}", f.project),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for ws in [f.team, f.project] {
        let (status, _) = call(
            &f.s,
            &f.admin,
            "POST",
            &format!("/api/v1/platform/workspaces/{ws}/members"),
            json!({"user_id":f.outsider.user_id,"role":"admin"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            call(
                &f.s,
                &f.outsider,
                "PATCH",
                &format!("/api/v1/workspaces/{ws}"),
                json!({"name":"Renamed"})
            )
            .await
            .0,
            StatusCode::OK
        );
        for u in [&f.member, &f.admin, &f.auditor] {
            assert_eq!(
                call(
                    &f.s,
                    u,
                    "PATCH",
                    &format!("/api/v1/workspaces/{ws}"),
                    json!({"name":"Denied"})
                )
                .await
                .0,
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "PATCH",
                &format!("/api/v1/platform/workspaces/{ws}"),
                json!({"kind":"personal"})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn personal_workspace_name_is_fixed_and_local_limits_are_platform_controlled(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, v) = call(
        &f.s,
        &f.owner,
        "PATCH",
        &format!("/api/v1/workspaces/{}", f.personal),
        json!({"name":"Mine"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["error"]["reason"], "personal_workspace_name_fixed");
    let name: String = sqlx::query_scalar("SELECT name FROM workspaces WHERE id=$1")
        .bind(f.personal)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Personal");
    let (_, w) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}", f.personal),
        json!({}),
    )
    .await;
    assert_eq!(w["capabilities"]["rename"], false);
    assert_eq!(w["capabilities"]["manage_policy"], false);
    let body = json!({"requests_per_minute":1,"tokens_per_minute":null,"concurrent_requests":null,"monthly_budget_microusd":null});
    let (status, v) = call(
        &f.s,
        &f.owner,
        "PUT",
        &format!("/api/v1/workspaces/{}/policy", f.personal),
        body.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(v["error"]["reason"], "personal_limits_platform_controlled");
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM workspace_local_policies WHERE workspace_id=$1)"
        )
        .bind(f.personal)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    // Read-only limits remain visible; per-key caps stay allowed.
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "GET",
            &format!("/api/v1/workspaces/{}/policy", f.personal),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, k) = key(&f, &f.owner, f.personal, Value::Null).await;
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "PUT",
            &format!("/api/v1/workspaces/{}/keys/{}/policy", f.personal, id(&k)),
            json!({"requests_per_minute":1,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[{"period":"day","amount_microusd":"5"}]})
        )
        .await
        .0,
        StatusCode::OK
    );
}
