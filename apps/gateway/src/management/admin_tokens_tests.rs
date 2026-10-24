use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

/// #37 PR2 item 2: the plaintext creation response carries an explicit
/// `Cache-Control: no-store`, set by the handler itself rather than only
/// relying on the global `request_context` middleware default (which the
/// `call()` test helper bypasses -- it invokes the router directly, not
/// through the full `axum::serve` stack middleware is layered on in
/// production -- so a regression here would not otherwise be caught by
/// this suite's other tests, which all use `call()`).
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn creation_response_sets_no_store_explicitly(pool: PgPool) {
    let f = fixture(&pool).await;
    let response = routes()
        .layer(Extension(ManagementPrincipal::Browser(f.admin.clone())))
        .layer(Extension(f.admin.clone()))
        .with_state(f.s.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/platform/admin-tokens")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"name":"CI bot","scopes":["platform:read"],"expires_in_days":30})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[axum::http::header::CACHE_CONTROL],
        "no-store",
        "the plaintext creation response must set no-store explicitly, not only through the global default"
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn admin_lists_creates_and_revokes_tokens(pool: PgPool) {
    let f = fixture(&pool).await;
    // List starts empty.
    let (status, list) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/admin-tokens",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"], json!([]));

    let (status, created) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/admin-tokens",
        json!({"name":"CI bot","scopes":["platform:read"],"expires_in_days":30}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let token = created["token"].as_str().unwrap();
    assert!(token.starts_with("omg_admin_"));
    let id = id(&created);

    // Listed, metadata only, never a hash or the plaintext.
    let (status, list) = call(
        &f.s,
        &f.admin,
        "GET",
        "/api/v1/platform/admin-tokens",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rows = list["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], id.to_string());
    assert_eq!(rows[0]["owner_user_id"], f.admin.user_id.to_string());
    assert_eq!(rows[0]["name"], "CI bot");
    assert!(rows[0].get("secret_hash").is_none());
    assert!(rows[0].get("token").is_none());

    // The live token authenticates.
    let principal = f.s.authenticate_admin_token(token).await.unwrap().unwrap();
    assert_eq!(principal.owner_user_id, f.admin.user_id);
    assert_eq!(principal.scopes, vec!["platform:read".to_string()]);

    // Any Admin (not only the owner) may revoke; idempotent.
    let other_admin = user(&pool, "other-admin", "admin").await;
    for _ in 0..2 {
        let (status, _) = call(
            &f.s,
            &other_admin,
            "DELETE",
            &format!("/api/v1/platform/admin-tokens/{id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert!(
        f.s.authenticate_admin_token(token).await.unwrap().is_none(),
        "a revoked token must not authenticate"
    );
    // Revoking an unknown id is still OK (idempotent across missing rows too).
    let (status, _) = call(
        &f.s,
        &f.admin,
        "DELETE",
        &format!("/api/v1/platform/admin-tokens/{}", Uuid::new_v4()),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let audited: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_events WHERE resource_id=$1 ORDER BY created_at",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        audited,
        vec![
            "admin_token.created",
            "admin_token.revoked",
            "admin_token.revoked"
        ]
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn auditor_cannot_manage_admin_tokens_and_creation_is_only_for_oneself(pool: PgPool) {
    let f = fixture(&pool).await;
    for method in ["GET", "POST"] {
        let (status, _) = call(
            &f.s,
            &f.auditor,
            method,
            "/api/v1/platform/admin-tokens",
            json!({"name":"x","scopes":["platform:read"],"expires_in_days":1}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
    }
    // The request body cannot name another user as owner: creation is
    // always for the authenticated caller.
    let (status, created) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/admin-tokens",
        json!({"name":"x","scopes":["platform:read"],"expires_in_days":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let owner: Uuid = sqlx::query_scalar("SELECT owner_user_id FROM admin_api_tokens WHERE id=$1")
        .bind(id(&created))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(owner, f.admin.user_id);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn invalid_creation_input_is_rejected(pool: PgPool) {
    let f = fixture(&pool).await;
    for body in [
        json!({"name":"","scopes":["platform:read"],"expires_in_days":30}),
        json!({"name":"x","scopes":[],"expires_in_days":30}),
        json!({"name":"x","scopes":["wizard:cast"],"expires_in_days":30}),
        json!({"name":"x","scopes":["platform:read"],"expires_in_days":0}),
        json!({"name":"x","scopes":["platform:read"],"expires_in_days":91}),
        json!({"name":"x","scopes":["platform:read"],"expires_in_days":30,"ip_allowlist":["0.0.0.0/0"]}),
    ] {
        let (status, body_out) = call(
            &f.s,
            &f.admin,
            "POST",
            "/api/v1/platform/admin-tokens",
            body.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {body_out}");
    }
    let (status, created) = call(
        &f.s,
        &f.admin,
        "POST",
        "/api/v1/platform/admin-tokens",
        json!({"name":"x","scopes":["platform:read"],"expires_in_days":30,"ip_allowlist":["10.0.0.0/24"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
}
