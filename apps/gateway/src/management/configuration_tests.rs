//! Configuration is typed data, not a process/host control plane (#197).
use super::*;

async fn rejects_extra(pool: &PgPool, path: &str, base: Value) -> Fixture {
    let f = fixture(pool).await;
    assert!(
        call(&f.s, &f.admin, "PUT", path, base.clone())
            .await
            .0
            .is_success(),
        "{path}: base body must succeed"
    );
    for patch in [
        json!({"environment_variables":{"UI_USERNAME":"attacker"}}),
        json!({"UI_LOGO_PATH":"/etc/passwd"}),
        json!({"plugin":"s3://attacker/module.py"}),
    ] {
        let mut body = base.clone();
        body.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        assert_eq!(
            call(&f.s, &f.admin, "PUT", path, body).await.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}"
        );
    }
    f
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn mapping_patch_rejects_unknown_fields_after_valid_control(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, created) = call(&f.s, &f.admin, "POST", "/api/v1/platform/oidc/group-mappings", json!({"issuer":"https://idp.example.test","group_value":"control","target_kind":"platform","platform_role":"user","enabled":false})).await;
    assert!(status.is_success());
    let path = format!("/api/v1/platform/oidc/group-mappings/{}", id(&created));
    assert_eq!(
        call(&f.s, &f.admin, "PATCH", &path, json!({"enabled":false}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &path,
            json!({"enabled":false,"environment_variables":{}})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn general_rejects_configuration_primitives(pool: PgPool) {
    rejects_extra(
        &pool,
        "/api/v1/platform/settings/general",
        json!({"display_name":"Acme","human_key_max_lifetime_days":30}),
    )
    .await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn privacy_rejects_configuration_primitives(pool: PgPool) {
    rejects_extra(
        &pool,
        "/api/v1/platform/settings/privacy",
        json!({"openrouter_data_collection":"deny"}),
    )
    .await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn email_rejects_configuration_primitives(pool: PgPool) {
    rejects_extra(
        &pool,
        "/api/v1/platform/settings/email",
        json!({"host":null}),
    )
    .await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn storage_rejects_configuration_primitives(pool: PgPool) {
    let f = rejects_extra(&pool, "/api/v1/platform/settings/storage", json!({"batch":{"enabled":false,"retention_days":7},"video":{"enabled":false,"retention_days":7},"user_files":{"enabled":false,"retention_days":30},"export":{"retention_days":1}})).await;
    let (status, _) = call(&f.s, &f.admin, "PUT", "/api/v1/platform/settings/storage", json!({"batch":{"enabled":false,"retention_days":7,"path":"/etc/passwd"},"video":{"enabled":false,"retention_days":7},"user_files":{"enabled":false,"retention_days":30},"export":{"retention_days":1}})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn limits_and_alerts_reject_configuration_primitives(pool: PgPool) {
    let f = fixture(&pool).await;
    let (status, _) = call(&f.s, &f.admin, "PUT", "/api/v1/platform/workspace-types/team/policy", json!({"requests_per_minute":null,"tokens_per_minute":null,"concurrent_requests":null,"budgets":[],"environment_variables":{"UI_PASSWORD":"attacker"}})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let mut rule = json!({"name":"Watch","kind":"batch_failed","notify_platform_admins":true,"template":"{{env.DATABASE_URL}}"});
    let path = "/api/v1/platform/alerts/rules";
    assert_eq!(
        call(&f.s, &f.admin, "POST", path, rule.clone()).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    rule.as_object_mut().unwrap().remove("template");
    let (status, created) = call(&f.s, &f.admin, "POST", path, rule.clone()).await;
    assert_eq!(status, StatusCode::OK);
    rule["expression"] = json!("load('s3://attacker/module.py')");
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PUT",
            &format!("{path}/{}", id(&created)),
            rule
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn test_actions_reject_configuration_bodies(pool: PgPool) {
    let mut f = fixture(&pool).await;
    let (port, _mail) = crate::email::mock::serve(crate::email::mock::Behaviour::default()).await;
    assert_eq!(call(&f.s, &f.admin, "PUT", "/api/v1/platform/settings/email", json!({"host":"127.0.0.1","port":port,"tls":"none","from_address":"gateway@example.test"})).await.0, StatusCode::OK);
    let (url, _seen) = super::audit_export_tests::receiver().await;
    let exporter = crate::audit_export::Exporter::new(&url, vec![7; 32], true)
        .unwrap()
        .unwrap();
    f.s = f.s.with_audit_export(std::sync::Arc::new(exporter));
    let router = routes()
        .layer(Extension(ManagementPrincipal::Browser(f.admin.clone())))
        .layer(Extension(f.admin.clone()))
        .layer(Extension(crate::filestore::FileStoreRuntime::memory()))
        .with_state(f.s.clone());
    for path in ["email/test", "storage/test", "audit-export/test"] {
        let path = format!("/api/v1/platform/settings/{path}");
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{path}: empty-body control"
        );
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"environment_variables":{"UI_LOGO_PATH":"/etc/passwd"}}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
    }
    // These are summaries, never writable settings.
    for path in ["sign-in", "audit-export"] {
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "GET",
                &format!("/api/v1/platform/settings/{path}"),
                json!({})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "PUT",
                &format!("/api/v1/platform/settings/{path}"),
                json!({"environment_variables":{}})
            )
            .await
            .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn connections_reject_unknown_fields_and_gateway_secrets(pool: PgPool) {
    use crate::providers::secrets::{EnvSecrets, SECRET_POINTERS};
    let mut f = fixture(&pool).await;
    let mut names = vec![
        "DATABASE_URL".to_owned(),
        "DATABASE_URL_FILE".to_owned(),
        "GATEWAY_OIDC_CLIENT_SECRET".to_owned(),
        "AWS_SECRET_ACCESS_KEY".to_owned(),
        "PGPASSWORD".to_owned(),
        "PGSSLKEY".to_owned(),
        "PGPASSFILE".to_owned(),
        "PGFUTURE_OPTION".to_owned(),
        "HTTP_PROXY".to_owned(),
        "HTTPS_PROXY".to_owned(),
        "ALL_PROXY".to_owned(),
        "NO_PROXY".to_owned(),
        "http_proxy".to_owned(),
        "OTEL_EXPORTER_OTLP_HEADERS".to_owned(),
    ];
    names.extend((0..SECRET_POINTERS.len()).map(|i| format!("CFG197_SECRET_{i}")));
    let lookup = |setting: &str| {
        assert!(
            SECRET_POINTERS.contains(&setting),
            "only pointer names may be read"
        );
        SECRET_POINTERS
            .iter()
            .position(|p| *p == setting)
            .map(|i| format!("CFG197_SECRET_{i}"))
    };
    f.s = f.s.with_secret_allowlists(
        EnvSecrets::from_lookup(
            names.iter().cloned().chain(["OPENAI_API_KEY".into()]),
            lookup,
        ),
        EnvSecrets::from_lookup(
            names.iter().cloned().chain(["SMTP_PASSWORD".into()]),
            lookup,
        ),
    );
    let path = "/api/v1/platform/providers";
    let base = json!({"name":"Cloud control","provider":"openai","credential_ref":"env:OPENAI_API_KEY","enabled":false});
    assert_eq!(
        call(&f.s, &f.admin, "POST", path, base.clone()).await.0,
        StatusCode::OK,
        "injected provider allowlist must govern the real handler"
    );
    assert_eq!(call(&f.s, &f.admin, "PUT", "/api/v1/platform/settings/email", json!({"host":"smtp.example.test","port":587,"tls":"starttls","from_address":"gateway@example.test","username":"smtp","password_ref":"env:SMTP_PASSWORD"})).await.0, StatusCode::OK, "injected SMTP allowlist must govern the real handler");
    let mut extra = base.clone();
    extra["environment_variables"] = json!({"UI_LOGO_PATH":"/etc/passwd"});
    assert_eq!(
        call(&f.s, &f.admin, "POST", path, extra).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Existing row for PATCH validation; no endpoint approvals/network required.
    let connection = Uuid::new_v4();
    sqlx::query("INSERT INTO provider_connections(id,name,provider,credential_ref,enabled) VALUES($1,'Connection','openai','env:OPENAI_API_KEY',false)")
        .bind(connection).execute(&pool).await.unwrap();
    let update = format!("{path}/{connection}");
    assert_eq!(
        call(&f.s, &f.admin, "PATCH", &update, json!({"enabled":false}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &f.s,
            &f.admin,
            "PATCH",
            &update,
            json!({"enabled":false,"UI_LOGO_PATH":"/etc/passwd"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for name in names {
        let reference = format!("env:{name}");
        let create = json!({"name":"Forbidden","provider":"openai","credential_ref":reference,"enabled":false});
        assert_eq!(
            call(&f.s, &f.admin, "POST", path, create).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                &f.s,
                &f.admin,
                "PATCH",
                &update,
                json!({"enabled":false,"credential_ref":reference})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(call(&f.s, &f.admin, "PUT", "/api/v1/platform/settings/email", json!({"host":"smtp.example.test","port":587,"tls":"starttls","from_address":"gateway@example.test","username":"smtp","password_ref":reference})).await.0, StatusCode::BAD_REQUEST);
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn logo_headers_cannot_select_a_filesystem_path(pool: PgPool) {
    use crate::filestore::{FileStoreConfig, FileStoreRuntime, ObjectKey};
    use base64::Engine;
    let f = fixture(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("victim");
    std::fs::write(&sentinel, b"must stay unchanged").unwrap();
    let store_dir = dir.path().join("store");
    let key = crate::entropy::bytes::<32>().unwrap();
    let config = FileStoreConfig::from_lookup(&|name| match name {
        "GATEWAY_FILE_STORE" => Some("local".into()),
        "GATEWAY_FILE_STORE_DIR" => Some(store_dir.to_str().unwrap().into()),
        "GATEWAY_FILE_ENCRYPTION_KEYS_ENV" => Some("TEST_KEYS".into()),
        "TEST_KEYS" => Some(format!(
            "test:{}",
            base64::prelude::BASE64_STANDARD.encode(key)
        )),
        _ => None,
    })
    .unwrap();
    let rt = FileStoreRuntime::build(config).await.unwrap();
    let router = routes()
        .layer(Extension(ManagementPrincipal::Browser(f.admin.clone())))
        .layer(Extension(f.admin.clone()))
        .layer(Extension(rt.clone()))
        .with_state(f.s.clone());
    let png = branding::fixtures::png(64, 64);
    for content_type in [
        "image/../victim",
        "image/png; filename=../victim",
        "image/png",
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/platform/settings/general/logo?path=../victim")
                    .header("content-type", content_type)
                    .header("content-disposition", "attachment; filename=\"../victim\"")
                    .body(Body::from(png.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if content_type == "image/../victim" {
                StatusCode::UNSUPPORTED_MEDIA_TYPE
            } else {
                StatusCode::OK
            }
        );
    }
    assert_eq!(std::fs::read(sentinel).unwrap(), b"must stay unchanged");
    let (key,filename,kind):(String,String,String) = sqlx::query_as("SELECT object_key,filename,content_type FROM stored_files WHERE id=(SELECT branding_logo_file_id FROM installation_settings WHERE singleton)").fetch_one(&pool).await.unwrap();
    assert_eq!(filename, "logo.png");
    assert_eq!(kind, "image/png");
    assert_eq!(
        ObjectKey::parse(&key).unwrap().purpose(),
        Some(crate::filestore::Purpose::Branding)
    );
    assert!(store_dir.join(&key).is_file());
    let response = crate::http::router(f.s.clone())
        .layer(Extension(rt))
        .oneshot(
            Request::builder()
                .uri("/api/v1/branding/logo?path=../victim")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .as_ref(),
        png.as_slice()
    );
}
