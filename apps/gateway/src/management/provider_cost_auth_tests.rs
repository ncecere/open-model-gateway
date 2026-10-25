use super::*;
use tracing::instrument::WithSubscriber;

#[derive(Clone, Default)]
struct RejectionLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for RejectionLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RejectionLogs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_json_rejections_preserve_status_without_echoing_or_logging_uploads(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = crate::management::router(identity.clone())
        .merge(crate::management::upload_router(identity))
        .with_state(f.s.clone());
    let admin = session(&pool, &f.admin).await;
    let logs = RejectionLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish();
    let dispatch = tracing::Dispatch::new(subscriber);
    const FIELD: &str = "canary_field_DO_NOT_ECHO";
    const VALUE: &str = "canary_value_DO_NOT_ECHO";
    for (path, valid, uuid_field) in [
        (
            "/api/v1/platform/provider-cost-imports",
            super::super::provider_cost_tests::upload(),
            "supersedes",
        ),
        (
            "/api/v1/platform/provider-cost-mappings",
            super::super::provider_cost_tests::mapping(Uuid::new_v4(), None),
            "supersedes",
        ),
        (
            "/api/v1/platform/provider-reconciliations",
            json!({"import_id":Uuid::new_v4(),"mapping_id":Uuid::new_v4()}),
            "import_id",
        ),
    ] {
        let mut unknown = valid.clone();
        unknown[FIELD] = json!(VALUE);
        let mut invalid_value = valid;
        invalid_value[uuid_field] = json!(VALUE);
        for (body, content_type, expected) in [
            (
                unknown.to_string(),
                "application/json",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                invalid_value.to_string(),
                "application/json",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                format!("{FIELD}={VALUE}"),
                "application/json",
                StatusCode::BAD_REQUEST,
            ),
            (
                format!("{FIELD}={VALUE}"),
                "text/plain",
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                format!("{FIELD}{VALUE}{}", " ".repeat(5 * 1024 * 1024 + 1)),
                "application/json",
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", content_type)
                        .header("origin", ORIGIN)
                        .header("cookie", format!("omg_session={}", admin.cookie))
                        .header("x-csrf-token", &admin.csrf)
                        .body(Body::from(body))
                        .unwrap(),
                )
                .with_subscriber(dispatch.clone())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let response = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(!response.contains(FIELD), "{response}");
            assert!(!response.contains(VALUE), "{response}");
        }
    }
    let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(!logged.contains(FIELD), "{logged}");
    assert!(!logged.contains(VALUE), "{logged}");
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_upload_real_http_router_allows_more_than_two_mib_but_refuses_more_than_five(
    pool: PgPool,
) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let admin = session(&pool, &f.admin).await;
    let mut body = super::super::provider_cost_tests::upload();
    let row = body["pages"][0]["data"][0]["results"][0].clone();
    body["pages"][0]["data"][0]["results"] = json!(
        (0..600)
            .map(|n| {
                let mut row = row.clone();
                row["description"] = json!(format!("{n}-{}", "x".repeat(3900)));
                row
            })
            .collect::<Vec<_>>()
    );
    let raw = body.to_string();
    assert!((2 * 1024 * 1024..5 * 1024 * 1024).contains(&raw.len()));
    for (raw, expected) in [
        (raw, StatusCode::OK),
        (
            " ".repeat(5 * 1024 * 1024 + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/platform/provider-cost-imports")
                    .header("content-type", "application/json")
                    .header("origin", ORIGIN)
                    .header("cookie", format!("omg_session={}", admin.cookie))
                    .header("x-csrf-token", &admin.csrf)
                    .body(Body::from(raw))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn provider_cost_browser_only_origin_csrf_and_live_roles(pool: PgPool) {
    let f = fixture(&pool).await;
    let mock = MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let app = crate::management::router(identity.clone())
        .merge(crate::management::upload_router(identity))
        .with_state(f.s.clone());
    let admin = session(&pool, &f.admin).await;
    let auditor = session(&pool, &f.auditor).await;
    let member = session(&pool, &f.owner).await;
    let (_, token) = call(&f.s, &f.admin, "POST", "/api/v1/platform/admin-tokens",
        json!({"name":"Cost refusal","scopes":["platform:read","governance:write"],"expires_in_days":1})).await;
    let token = token["token"].as_str().unwrap();
    for path in [
        "/api/v1/platform/provider-cost-imports",
        "/api/v1/platform/provider-cost-mappings",
        "/api/v1/platform/provider-reconciliations",
    ] {
        assert_eq!(
            request(&app, "GET", path, Some(&admin), None, false, ORIGIN).await,
            StatusCode::OK
        );
        assert_eq!(
            request(&app, "GET", path, Some(&auditor), None, false, ORIGIN).await,
            StatusCode::OK
        );
        assert_eq!(
            request(&app, "GET", path, Some(&member), None, false, ORIGIN).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(&app, "POST", path, Some(&auditor), None, true, ORIGIN).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(&app, "POST", path, Some(&admin), None, false, ORIGIN).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                &app,
                "POST",
                path,
                Some(&admin),
                None,
                true,
                "https://attacker.invalid"
            )
            .await,
            StatusCode::FORBIDDEN
        );
        for method in ["GET", "POST"] {
            assert_eq!(
                request(&app, method, path, None, Some(token), false, ORIGIN).await,
                StatusCode::FORBIDDEN
            );
        }
    }
    // Live demotion: keep an entitled User grant and drop only Admin (revoking
    // every role would make the session unentitled, which is 401 everywhere).
    sqlx::query("INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'user','manual')")
        .bind(f.admin.user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE platform_role_grants SET revoked_at=now() WHERE user_id=$1 AND role='admin'",
    )
    .bind(f.admin.user_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        request(
            &app,
            "POST",
            "/api/v1/platform/provider-cost-imports",
            Some(&admin),
            None,
            true,
            ORIGIN
        )
        .await,
        StatusCode::FORBIDDEN
    );
}
