use super::*;
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_restrictions_null_empty_subset_validation_rotation_lineage_and_visibility(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let m1 = model(&pool, "one").await;
    let m2 = model(&pool, "two").await;
    let outside = model(&pool, "outside").await;
    direct(&pool, f.personal, m1).await;
    direct(&pool, f.personal, m2).await;
    let provider = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Mock','openai','env:TEST',true)").bind(provider).execute(&pool).await.unwrap();
    for model in [m1, m2] {
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,'Mock',true)").bind(Uuid::new_v4()).bind(model).bind(provider).execute(&pool).await.unwrap();
    }
    for (selections, count) in [(Value::Null, 2), (json!([]), 0), (json!([m1, m1]), 1)] {
        let (status, v) = key(&f, &f.owner, f.personal, selections).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let kid = id(&v);
        let principal =
            f.s.authenticate(v["token"].as_str().unwrap())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(f.s.visible_models(&principal).await.unwrap().len(), count);
        sqlx::query("INSERT INTO key_policies(workspace_id,governance_key_id,requests_per_minute) VALUES($1,$2,7)").bind(f.personal).bind(kid).execute(&pool).await.unwrap();
        crate::governance::set_test_budget(
            &pool,
            "key",
            None,
            Some(f.personal),
            Some(kid),
            "month",
            Some(100),
        )
        .await;
        let (status, rotated) = call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/keys/{kid}/rotate", f.personal),
            json!({"expires_in_days":2}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rotated}");
        let new = id(&rotated);
        let lineage: Uuid =
            sqlx::query_scalar("SELECT governance_key_id FROM api_keys WHERE id=$1")
                .bind(new)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(lineage, kid);
        assert!(
            f.s.authenticate(v["token"].as_str().unwrap())
                .await
                .unwrap()
                .is_none()
        );
        let newprincipal =
            f.s.authenticate(rotated["token"].as_str().unwrap())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            f.s.visible_models(&newprincipal).await.unwrap().len(),
            count
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT requests_per_minute FROM key_policies WHERE governance_key_id=$1"
            )
            .bind(lineage)
            .fetch_one(&pool)
            .await
            .unwrap(),
            7
        );
    }
    assert_eq!(
        key(&f, &f.owner, f.personal, json!([outside])).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        key(&f, &f.owner, f.personal, json!(vec![m1; 201])).await.0,
        StatusCode::BAD_REQUEST
    );
    let (_, keys) = call(
        &f.s,
        &f.owner,
        "GET",
        &format!("/api/v1/workspaces/{}/keys", f.personal),
        json!({}),
    )
    .await;
    assert!(!keys.to_string().contains("token"));
    assert!(!keys.to_string().contains("secret_hash"));
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn shared_admin_revocation_is_not_human_impersonation_and_members_see_only_own_keys(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let (status, k) = key(&f, &f.member, f.team, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let kid = id(&k);
    assert_eq!(
        key(&f, &f.admin, f.team, Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            &format!("/api/v1/workspaces/{}/keys/{kid}/rotate", f.team),
            json!({"expires_in_days":1})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, owner_key) = key(&f, &f.owner, f.team, Value::Null).await;
    let (_, member_keys) = call(
        &f.s,
        &f.member,
        "GET",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({}),
    )
    .await;
    assert_eq!(member_keys["data"].as_array().unwrap().len(), 1);
    assert_eq!(member_keys["data"][0]["id"], kid.to_string());
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "DELETE",
            &format!("/api/v1/workspaces/{}/keys/{}", f.team, id(&owner_key)),
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "GET",
            &format!("/api/v1/workspaces/{}/keys", f.team),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    // A non-member Platform Admin has no workspace key authority; the owner revokes.
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "DELETE",
            &format!("/api/v1/workspaces/{}/keys/{kid}", f.team),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "DELETE",
            &format!("/api/v1/workspaces/{}/keys/{kid}", f.team),
            json!({})
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
    member(&pool, f.team, f.admin.user_id, "member").await;
    assert_eq!(
        key(&f, &f.admin, f.team, Value::Null).await.0,
        StatusCode::OK
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn service_account_disable_reenable_cannot_resurrect_credentials_and_is_shared_only(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/service-accounts", f.personal),
            json!({"name":"Private robot"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "POST",
            &format!("/api/v1/workspaces/{}/service-accounts", f.team),
            json!({"name":"Denied robot"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, v) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/service-accounts", f.team),
        json!({"name":"Robot"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let account = id(&v);
    let (status, k) = call(
        &f.s,
        &f.owner,
        "POST",
        &format!("/api/v1/workspaces/{}/keys", f.team),
        json!({"name":"Service","expires_in_days":1,"service_account_id":account}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{k}");
    assert!(
        f.s.authenticate(k["token"].as_str().unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/keys", f.project),
            json!({"name":"Cross workspace","expires_in_days":1,"service_account_id":account})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for disabled in [true, false] {
        assert_eq!(
            call(
                &f.s,
                &f.owner,
                "PATCH",
                &format!("/api/v1/workspaces/{}/service-accounts/{account}", f.team),
                json!({"disabled":disabled})
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
    assert_eq!(
        call(
            &f.s,
            &f.owner,
            "POST",
            &format!("/api/v1/workspaces/{}/keys/{}/rotate", f.team, id(&k)),
            json!({"expires_in_days":1})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}
