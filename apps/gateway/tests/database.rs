#![cfg(feature = "integration-tests")]

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use open_model_gateway::{
    auth::NewApiKey,
    bootstrap::{self, DevelopmentKeys},
    config::Environment,
    http,
    store::Store,
};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn fixture(pool: &PgPool) -> (Store, DevelopmentKeys) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    (store, keys)
}

async fn enable_example(pool: &PgPool) {
    sqlx::query("UPDATE provider_connections SET enabled = true")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET enabled = true")
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test]
async fn bootstrap_is_idempotent_and_stores_only_hashes(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    assert!(store.is_ready().await);
    assert!(
        bootstrap::seed(&store, Environment::Development)
            .await
            .unwrap()
            .is_none()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
    let stored: Vec<u8> = sqlx::query_scalar("SELECT secret_hash FROM api_keys WHERE id = $1")
        .bind(keys.personal_key.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, keys.personal_key.digest);
    assert!(
        store
            .authenticate(&keys.personal_key.token)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .authenticate(&keys.team_key.token)
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test]
async fn bootstrap_refuses_production(pool: PgPool) {
    let store = Store::new(pool.clone());
    assert!(
        bootstrap::seed(&store, Environment::Production)
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM organizations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn catalog_requires_grant_model_deployment_and_connection(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    let personal = store
        .authenticate(&keys.personal_key.token)
        .await
        .unwrap()
        .unwrap();
    let team = store
        .authenticate(&keys.team_key.token)
        .await
        .unwrap()
        .unwrap();
    assert!(store.visible_models(&personal).await.unwrap().is_empty());
    enable_example(&pool).await;
    let models = store.visible_models(&personal).await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "company/smart");
    assert_eq!(models[0].object, "model");
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id = $1")
        .bind(keys.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.visible_models(&team).await.unwrap().is_empty());
    assert_eq!(store.visible_models(&personal).await.unwrap().len(), 1);
    for table in ["models", "deployments", "provider_connections"] {
        sqlx::query(&format!("UPDATE {table} SET enabled = false"))
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            store.visible_models(&personal).await.unwrap().is_empty(),
            "{table}"
        );
        sqlx::query(&format!("UPDATE {table} SET enabled = true"))
            .execute(&pool)
            .await
            .unwrap();
    }
}

#[sqlx::test]
async fn key_revocation_expiration_and_wrong_secret_deny_access(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    let token = &keys.personal_key.token;
    let mut wrong = token.clone();
    let last = wrong.pop().unwrap();
    wrong.push(if last == '0' { '1' } else { '0' });
    assert!(store.authenticate(&wrong).await.unwrap().is_none());
    assert!(
        store
            .authenticate(&NewApiKey::generate().token)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE api_keys SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(keys.personal_key.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.authenticate(token).await.unwrap().is_none());
    sqlx::query("UPDATE api_keys SET expires_at = now() + interval '1 day' WHERE id = $1")
        .bind(keys.personal_key.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.authenticate(token).await.unwrap().is_some());
    sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = $1")
        .bind(keys.personal_key.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(store.authenticate(token).await.unwrap().is_none());
}

#[sqlx::test]
async fn membership_and_account_changes_take_effect_without_cache(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    sqlx::query("UPDATE workspace_memberships SET disabled_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        store
            .authenticate(&keys.team_key.token)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .authenticate(&keys.personal_key.token)
            .await
            .unwrap()
            .is_some()
    );
    sqlx::query("UPDATE workspace_memberships SET disabled_at = NULL")
        .execute(&pool)
        .await
        .unwrap();
    for table in [
        "organization_memberships",
        "users",
        "workspaces",
        "organizations",
    ] {
        sqlx::query(&format!("UPDATE {table} SET disabled_at = now()"))
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            store
                .authenticate(&keys.team_key.token)
                .await
                .unwrap()
                .is_none(),
            "{table}"
        );
        assert!(
            store
                .authenticate(&keys.personal_key.token)
                .await
                .unwrap()
                .is_none(),
            "{table}"
        );
        sqlx::query(&format!("UPDATE {table} SET disabled_at = NULL"))
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            store
                .authenticate(&keys.team_key.token)
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[sqlx::test]
async fn personal_workspace_cannot_be_shared_by_issuing_another_users_key(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    let other_user = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, email) VALUES ($1, 'other@local.invalid')")
        .bind(other_user)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO organization_memberships (organization_id, user_id, role) VALUES ($1, $2, 'member')")
        .bind(keys.organization_id).bind(other_user).execute(&pool).await.unwrap();
    // An accidentally provisioned key cannot share a personal space; the DB also rejects sharing memberships.
    sqlx::query("UPDATE api_keys SET issued_to_user_id = $1 WHERE id = $2")
        .bind(other_user)
        .bind(keys.personal_key.id)
        .execute(&pool)
        .await
        .unwrap();
    let sharing = sqlx::query("INSERT INTO workspace_memberships (organization_id, workspace_id, user_id, role) VALUES ($1, $2, $3, 'member')")
        .bind(keys.organization_id).bind(keys.personal_workspace_id).bind(other_user).execute(&pool).await;
    assert_eq!(
        sharing
            .unwrap_err()
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23514")
    );
    assert!(
        store
            .authenticate(&keys.personal_key.token)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test]
async fn database_enforces_consumer_tenancy_with_shared_catalog(pool: PgPool) {
    let (_store, keys) = fixture(&pool).await;
    let other_org = Uuid::new_v4();
    let other_model = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, 'other', 'Other tenant')")
        .bind(other_org)
        .execute(&pool)
        .await
        .unwrap();
    // Canonical models are global; organization aliases remain independent.
    sqlx::query("INSERT INTO models (id, organization_id, public_name, display_name, enabled) VALUES ($1, $2, 'company/other-smart', 'Other model', true)")
        .bind(other_model).bind(other_org).execute(&pool).await.unwrap();
    sqlx::query("UPDATE organization_model_grants SET public_name='company/smart' WHERE organization_id=$1 AND model_id=$2")
        .bind(other_org).bind(other_model).execute(&pool).await.unwrap();
    let grant = sqlx::query("INSERT INTO workspace_model_grants (organization_id, workspace_id, model_id) VALUES ($1, $2, $3)")
        .bind(keys.organization_id).bind(keys.personal_workspace_id).bind(other_model).execute(&pool).await;
    assert_eq!(
        grant
            .unwrap_err()
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23503")
    );
    let provider: Uuid = sqlx::query_scalar("SELECT id FROM provider_connections LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let deployment = sqlx::query("INSERT INTO deployments (id, organization_id, model_id, provider_connection_id, upstream_model) VALUES ($1, $2, $3, $4, 'other')")
        .bind(Uuid::new_v4()).bind(other_org).bind(other_model).bind(provider).execute(&pool).await;
    // Infrastructure is platform-owned: its historical provenance cannot fence
    // which provider a deployment uses. Entitlements fence consumer access instead.
    assert!(deployment.is_ok());
    let key_move = sqlx::query("UPDATE api_keys SET organization_id = $1 WHERE id = $2")
        .bind(other_org)
        .bind(keys.personal_key.id)
        .execute(&pool)
        .await;
    assert_eq!(
        key_move
            .unwrap_err()
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23503")
    );
    let workspace = sqlx::query("UPDATE workspaces SET organization_id = $1 WHERE id = $2")
        .bind(other_org)
        .bind(keys.personal_workspace_id)
        .execute(&pool)
        .await;
    assert_eq!(
        workspace
            .unwrap_err()
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23503")
    );
}

#[sqlx::test]
async fn http_enforces_workspace_scope_and_unregistered_protocol_errors(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    enable_example(&pool).await;
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id = $1")
        .bind(keys.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    let app = http::router(store);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/models?workspace_id={}",
                    keys.personal_workspace_id
                ))
                .header("authorization", format!("Bearer {}", keys.team_key.token))
                .header("x-workspace-id", keys.personal_workspace_id.to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body["data"], serde_json::json!([]));
    for path in ["/v1/responses", "/v1/messages"] {
        let builder = Request::builder()
            .uri(path)
            .method("POST")
            .header("content-type", "application/json");
        let builder = if path == "/v1/messages" {
            builder
                .header("x-api-key", &keys.personal_key.token)
                .header("anthropic-version", "2023-06-01")
        } else {
            builder.header(
                "authorization",
                format!("Bearer {}", keys.personal_key.token),
            )
        };
        let payload = if path == "/v1/messages" {
            serde_json::json!({"model":"company/smart","messages":[{"role":"user","content":"hello"}],"max_tokens":16})
        } else {
            serde_json::json!({"model":"company/smart","input":"hello","store":false})
        };
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(payload.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED, "{path}");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert!(body["error"]["message"].is_string());
        if path == "/v1/messages" {
            assert_eq!(body["type"], "error");
        }
    }
}

#[sqlx::test]
async fn readiness_rejects_schema_drift(pool: PgPool) {
    let store = Store::new(pool.clone());
    assert!(store.is_ready().await);
    sqlx::query("UPDATE _sqlx_migrations SET checksum = decode('00', 'hex')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!store.is_ready().await);
}
