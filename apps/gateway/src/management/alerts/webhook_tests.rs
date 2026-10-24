use super::*;
const WEBHOOKS: &str = "/api/v1/platform/alerts/webhooks";

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_running_gateway_sessions_scopes_rotation_and_revocation(pool: PgPool) {
    use sha2::{Digest, Sha256};
    let mut f = fixture(&pool).await;
    // No HOOK_MISSING variable exists: save/enable must not resolve it.
    f.s = f.s.with_alert_webhooks(
        crate::config::AlertWebhookPolicy::parse(
            &["https://example.test/private-path".into()],
            vec!["HOOK_MISSING".into(), "HOOK_OLD".into()],
            crate::config::Environment::Production,
        )
        .unwrap(),
    );
    let cookie = "a".repeat(64);
    let csrf = "b".repeat(64);
    sqlx::query("INSERT INTO browser_sessions(token_hash,user_id,csrf_hash,expires_at,verified_email) VALUES($1,$2,$3,now()+interval '1 hour',$4)")
        .bind(Sha256::digest(cookie.as_bytes()).as_slice()).bind(f.admin.user_id).bind(Sha256::digest(csrf.as_bytes()).as_slice()).bind(&f.admin.email).execute(&pool).await.unwrap();
    async fn mint(pool: &PgPool, owner: Uuid, scope: &str) -> (Uuid, String) {
        let mut tx = crate::db::begin(pool).await.unwrap();
        crate::governance::locks::exclusive(
            &mut tx,
            [crate::governance::locks::Scope::User(owner)],
        )
        .await
        .unwrap();
        let token = crate::admin_tokens::create_admin_token(
            &mut tx,
            owner,
            "webhook test",
            &scope.split(',').map(str::to_owned).collect::<Vec<_>>(),
            30,
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        (token.id, token.token)
    }
    let (write_id, write) = mint(&pool, f.admin.user_id, "settings:write").await;
    let (_, read) = mint(&pool, f.admin.user_id, "platform:read").await;
    let (_, wrong) = mint(&pool, f.admin.user_id, "governance:write").await;
    let (_, read_write) = mint(&pool, f.admin.user_id, "platform:read,settings:write").await;
    let mock = crate::identity::tests::MockProvider::start(None).await;
    let identity = mock.state(pool.clone()).await;
    let engine = crate::inference::Engine::new(
        std::sync::Arc::new(f.s.clone()),
        crate::providers::ProviderRegistry::default(),
        crate::inference::EngineLimits::default(),
    )
    .unwrap();
    let app = crate::http::router_with_identity(f.s.clone(), None, engine, identity);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let body = json!({"label":"receiver","url":"https://EXAMPLE.test:443/private-path","current_secret_ref":"env:HOOK_MISSING"});
    let browser = |method: reqwest::Method, path: &str, token: bool, origin: &str| {
        let req = client
            .request(method, format!("{base}{path}"))
            .header("cookie", format!("omg_session={cookie}"))
            .header("origin", origin);
        if token {
            req.header("x-csrf-token", &csrf)
        } else {
            req
        }
    };
    assert_eq!(
        browser(
            reqwest::Method::POST,
            WEBHOOKS,
            false,
            "http://127.0.0.1:3000"
        )
        .json(&body)
        .send()
        .await
        .unwrap()
        .status(),
        403
    );
    assert_eq!(
        browser(reqwest::Method::POST, WEBHOOKS, true, "https://evil.test")
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    for token in [&read, &wrong] {
        assert_eq!(
            client
                .post(format!("{base}{WEBHOOKS}"))
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    let response = browser(
        reqwest::Method::POST,
        WEBHOOKS,
        true,
        "http://127.0.0.1:3000",
    )
    .json(&body)
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    let created: Value = response.json().await.unwrap();
    let id = id(&created);
    let path = format!("{WEBHOOKS}/{id}");
    assert_eq!(created["enabled"], false);
    assert_eq!(created["url"], "https://example.test/private-path");
    for route in [WEBHOOKS, path.as_str()] {
        let redacted: Value = client
            .get(format!("{base}{route}"))
            .bearer_auth(&read)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            !redacted.to_string().contains("private-path"),
            "read scope leaked URL"
        );
        assert!(
            !redacted.to_string().contains("HOOK_MISSING"),
            "read scope leaked reference"
        );
        let record = if route == WEBHOOKS {
            &redacted["data"][0]
        } else {
            &redacted
        };
        for field in ["url", "current_secret_ref", "previous_secret_ref"] {
            assert!(
                record.get(field).is_none(),
                "read-only status returned {field}"
            );
        }
    }
    let detailed: Value = client
        .get(format!("{base}{path}"))
        .bearer_auth(&read_write)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detailed["url"], "https://example.test/private-path");
    assert_eq!(detailed["current_secret_ref"], "env:HOOK_MISSING");
    assert_eq!(
        client
            .get(format!("{base}{WEBHOOKS}"))
            .bearer_auth(&read)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(format!("{base}{WEBHOOKS}"))
            .bearer_auth(&write)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    for bad in [
        json!({"url":"https://other.test/"}),
        json!({"enabled":null}),
        json!({"previous_secret_ref":"env:HOOK_OLD"}),
        json!({"previous_secret_ref":"file:/secret"}),
        json!({"current_secret_ref":"env:OPENAI_API_KEY"}),
        json!({"previous_secret_ref":"env:HOOK_OLD","previous_secret_until":(chrono::Utc::now()+chrono::Duration::hours(25)).to_rfc3339()}),
    ] {
        assert_eq!(
            client
                .patch(format!("{base}{path}"))
                .bearer_auth(&write)
                .json(&bad)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    assert_eq!(client.patch(format!("{base}{path}")).bearer_auth(&write).json(&json!({"enabled":true,"previous_secret_ref":"env:HOOK_OLD","previous_secret_until":(chrono::Utc::now()+chrono::Duration::hours(1)).to_rfc3339()})).send().await.unwrap().status(),200);
    assert_eq!(
        client
            .patch(format!("{base}{path}"))
            .bearer_auth(&write)
            .json(&json!({"previous_secret_ref":null,"previous_secret_until":null}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .patch(format!("{base}{path}"))
            .bearer_auth(&write)
            .json(&json!({"enabled":false}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    // Token attribution and field-name-only auditing on the real path.
    let metadata: Vec<Value>=sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action LIKE 'alert_webhook.%' AND admin_token_id=$1").bind(write_id).fetch_all(&pool).await.unwrap();
    assert_eq!(metadata.len(), 3);
    for metadata in metadata {
        assert!(metadata["changed_fields"].is_array());
        assert!(!metadata.to_string().contains("HOOK_"));
        assert!(!metadata.to_string().contains("private-path"));
    }
    let mut tx = crate::db::begin(&pool).await.unwrap();
    crate::admin_tokens::revoke_admin_token(&mut tx, write_id)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        client
            .patch(format!("{base}{path}"))
            .bearer_auth(&write)
            .json(&json!({"enabled":true}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // Bypass middleware to model a preflight racing revocation: write_tx must
    // still reject the stale principal inside its own transaction.
    let stale = ManagementPrincipal::AdminToken(crate::identity::AdminTokenContext {
        token_id: write_id,
        owner_user_id: f.admin.user_id,
        scopes: vec!["settings:write".into()],
        ip_allowlist: None,
        client_address: None,
    });
    let response = routes()
        .layer(Extension(stale))
        .with_state(f.s.clone())
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(&path)
                .header("content-type", "application/json")
                .body(Body::from("{\"enabled\":true}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT enabled FROM alert_webhook_destinations WHERE id=$1"
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    server.abort();
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn webhook_authority_redaction_and_strict_management(pool: PgPool) {
    let f = fixture(&pool).await;
    assert_eq!(
        call(&f.s, &f.admin, "GET", WEBHOOKS, json!({})).await.0,
        StatusCode::OK
    );
    for user in [&f.owner, &f.member, &f.outsider] {
        assert_eq!(
            call(&f.s, user, "GET", WEBHOOKS, json!({})).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&f.s, user, "POST", WEBHOOKS, json!({})).await.0,
            StatusCode::FORBIDDEN
        );
    }
    let id: Uuid = sqlx::query_scalar("INSERT INTO alert_webhook_destinations(label,url,current_secret_ref) VALUES('receiver','https://example.test/private-path','env:HOOK_PRIVATE') RETURNING id").fetch_one(&pool).await.unwrap();
    let path = format!("{WEBHOOKS}/{id}");
    let (_, redacted) = call(&f.s, &f.auditor, "GET", &path, json!({})).await;
    assert!(!redacted.to_string().contains("private-path"));
    assert!(!redacted.to_string().contains("HOOK_PRIVATE"));
    assert_eq!(redacted["signing_configured"], true);
    for method in ["POST", "PATCH", "DELETE"] {
        assert_eq!(
            call(
                &f.s,
                &f.auditor,
                method,
                if method == "POST" { WEBHOOKS } else { &path },
                json!({})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    for body in [
        json!({"url":"https://other.test/"}),
        json!({"enabled":null}),
        json!({"extra":true}),
        json!({"previous_secret_ref":"file:/secret"}),
    ] {
        assert_eq!(
            call(&f.s, &f.admin, "PATCH", &path, body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(&f.s, &f.admin, "PATCH", &path, json!({"label":"renamed"}))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f.s, &f.admin, "DELETE", &path, json!({})).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&f.s, &f.admin, "GET", &path, json!({})).await.0,
        StatusCode::NOT_FOUND
    );
    let audit: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT metadata FROM audit_events WHERE action LIKE 'alert_webhook.%'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(audit.len(), 2);
    for details in audit {
        assert!(
            details["changed_fields"]
                .as_array()
                .is_some_and(|fields| !fields.is_empty())
        );
        assert!(!details.to_string().contains("private-path"));
        assert!(!details.to_string().contains("HOOK_PRIVATE"));
    }
}
