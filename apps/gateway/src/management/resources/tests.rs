use super::*;
#[path = "tests/audit_filters.rs"]
mod audit_filters;
#[path = "tests/bedrock_connections.rs"]
mod bedrock_connections;
#[path = "tests/catalog_read.rs"]
mod catalog_read;

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn deployment_metadata_labels_do_not_expose_credential_references(pool: PgPool) {
    let f = fixture(&pool).await;
    let model = Uuid::new_v4();
    let provider = Uuid::new_v4();
    let deployment = Uuid::new_v4();
    sqlx::query("INSERT INTO models(id,public_name) VALUES($1,'company/embed')")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,'Datacenter inference','openai','env:PRIVATE_REFERENCE')").bind(provider).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model) VALUES($1,$2,$3,'private-upstream-name')").bind(deployment).bind(model).bind(provider).execute(&pool).await.unwrap();
    for actor in [&f.admin, &f.auditor] {
        for path in [
            "/api/v1/platform/deployments".to_owned(),
            format!("/api/v1/platform/deployments/{deployment}"),
        ] {
            let (status, body) = call(&f.s, actor, "GET", &path, json!({})).await;
            assert_eq!(status, StatusCode::OK);
            let row = if path.ends_with("deployments") {
                &body["data"][0]
            } else {
                &body
            };
            assert_eq!(row["model_public_name"], "company/embed");
            assert_eq!(row["provider_name"], "Datacenter inference");
            assert!(row.get("credential_ref").is_none());
            assert!(!body.to_string().contains("PRIVATE_REFERENCE"));
        }
    }
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "GET",
            "/api/v1/platform/deployments",
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn platform_catalog_reads_are_auditor_readonly_live_and_literal_bounded(pool: PgPool) {
    let f = fixture(&pool).await;
    let names = ["alpha%literal", "alpha_other", "zeta"];
    for name in names {
        let mid = Uuid::new_v4();
        sqlx::query("INSERT INTO models(id,public_name,enabled) VALUES($1,$2,$3)")
            .bind(mid)
            .bind(name)
            .bind(name != "zeta")
            .execute(&pool)
            .await
            .unwrap();
    }
    let (status, v) = call(
        &f.s,
        &f.auditor,
        "GET",
        "/api/v1/platform/models?q=%25&enabled=true&limit=1&offset=0",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["public_name"], "alpha%literal");
    let mid = id(&v["data"][0]);
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "GET",
            &format!("/api/v1/platform/models/{mid}"),
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "PATCH",
            &format!("/api/v1/platform/models/{mid}"),
            json!({"enabled":false})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &f.s,
            &f.member,
            "GET",
            &format!("/api/v1/platform/models/{mid}"),
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for query in ["limit=0", "limit=201", "offset=-1", "offset=100001"] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "GET",
                &format!("/api/v1/platform/models?{query}"),
                json!({})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    for query in [
        "limit=1&limit=2",
        "organization_id=00000000-0000-0000-0000-000000000000",
    ] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "GET",
                &format!("/api/v1/platform/models?{query}"),
                json!({})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (_, page) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/models?enabled=true&limit=1&offset=1",
        json!({}),
    )
    .await;
    assert_eq!(page["data"][0]["public_name"], "alpha_other");
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1")
        .bind(f.auditor.user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &f.s,
            &f.auditor,
            "GET",
            "/api/v1/platform/models",
            json!({})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn model_declared_protocols_default_and_validated_distinct_subset(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, v) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/models",
        json!({"public_name":"global/model","display_name":"Global model","enabled":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let mid = id(&v);
    let (_, v) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/models/{mid}"),
        json!({}),
    )
    .await;
    assert_eq!(v["supported_protocols"], json!(["chat_completions"]));
    for protocols in [
        json!([]),
        json!(["embeddings", "embeddings"]),
        json!(["audio"]),
        json!(["chat_completions", "embeddings"]),
        json!(["images", "rerank"]),
        Value::Null,
    ] {
        let body = json!({"public_name":"bad","display_name":"Bad","enabled":true,"supported_protocols":protocols});
        assert!(matches!(
            call(&f.s, &f.admin, "POST", "/api/v1/platform/models", body)
                .await
                .0,
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
        ));
    }
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/models/{mid}"),
            json!({"supported_protocols":["embeddings","messages"]})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/models/{mid}"),
            json!({"supported_protocols":["images"]})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, v) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/models/{mid}"),
        json!({}),
    )
    .await;
    assert_eq!(v["supported_protocols"], json!(["images"]));
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &format!("/api/v1/platform/models/{mid}"),
            json!({"supported_protocols":[]})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_direct_reads_never_expose_credentials_and_deployments_filter_both_foreign_keys(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let m1 = model(&pool, "one").await;
    let m2 = model(&pool, "two").await;
    let p1 = Uuid::new_v4();
    let p2 = Uuid::new_v4();
    for (p, n) in [(p1, "First"), (p2, "Second")] {
        sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref) VALUES($1,$2,'openai','env:PRIVATE_KEY')").bind(p).bind(n).execute(&pool).await.unwrap();
    }
    for (m, p, n) in [
        (m1, p1, "one-first"),
        (m1, p2, "one-second"),
        (m2, p1, "two-first"),
    ] {
        sqlx::query("INSERT INTO deployments(id,model_id,provider_connection_id,upstream_model,enabled) VALUES($1,$2,$3,$4,true)").bind(Uuid::new_v4()).bind(m).bind(p).bind(n).execute(&pool).await.unwrap();
    }
    let(status,v)=call(&f.s,&f.auditor,"GET",&format!("/api/v1/platform/deployments?model_id={m1}&provider_connection_id={p1}&q=first&enabled=true"),json!({})).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["upstream_model"], "one-first");
    let (_, provider) = call(
        &f.s,
        &f.auditor,
        "GET",
        &format!("/api/v1/platform/providers/{p1}"),
        json!({}),
    )
    .await;
    assert!(provider.get("credential_ref").is_none());
    assert_eq!(provider["auth_mode"], "credential");
    assert!(!provider.to_string().contains("PRIVATE_KEY"));
    for provider in ["vllm", "sglang", "ollama", "openai_compatible"] {
        assert_eq!(call(&f.s,&f.admin,"POST","/api/v1/platform/providers",json!({"name":"Unapproved","provider":provider,"credential_ref":"none","endpoint":"http://unapproved.invalid/v1","enabled":false})).await.0,StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/providers",
            json!({"name":"Cloud none","provider":"openai","credential_ref":"none","enabled":false})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
