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
    for table in ["provider_connections", "deployments"] {
        sqlx::query(&format!("UPDATE {table} SET enabled = true"))
            .execute(pool)
            .await
            .unwrap();
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bootstrap_is_idempotent_and_stores_only_hashes(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    assert!(store.is_ready().await);
    let installation: Uuid = sqlx::query_scalar("SELECT id FROM installation WHERE singleton")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(keys.installation_id, installation);
    assert!(
        bootstrap::seed(&store, Environment::Development)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM api_keys")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    for key in [&keys.personal_key, &keys.team_key] {
        let stored: Vec<u8> = sqlx::query_scalar("SELECT secret_hash FROM api_keys WHERE id = $1")
            .bind(key.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, key.digest);
        assert!(store.authenticate(&key.token).await.unwrap().is_some());
        // Check every serialized row, not just the named hash column.
        for table in ["api_keys", "audit_events"] {
            let rows: String = sqlx::query_scalar(&format!(
                "SELECT coalesce(jsonb_agg(to_jsonb(t)), '[]'::jsonb)::text FROM {table} t"
            ))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(!rows.contains(&key.token));
        }
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn bootstrap_refuses_production(pool: PgPool) {
    let store = Store::new(pool.clone());
    assert!(
        bootstrap::seed(&store, Environment::Production)
            .await
            .is_err()
    );
    for table in ["users", "api_keys", "workspaces", "provider_connections"] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "{table}"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM installation")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn key_revocation_expiration_and_wrong_secret_deny_access(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    let token = &keys.personal_key.token;
    let mut wrong = token.clone();
    let last = wrong.pop().unwrap();
    wrong.push(if last == '0' { '1' } else { '0' });
    assert!(store.authenticate(&wrong).await.unwrap().is_none());
    assert!(
        store
            .authenticate(&NewApiKey::generate().unwrap().token)
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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn membership_and_account_changes_take_effect_without_cache(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at = now() WHERE workspace_id=$1")
        .bind(keys.team_workspace_id)
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
    sqlx::query("UPDATE workspace_membership_grants SET revoked_at = NULL WHERE workspace_id=$1")
        .bind(keys.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    for key in [&keys.personal_key, &keys.team_key] {
        assert!(store.authenticate(&key.token).await.unwrap().is_none());
    }
    sqlx::query("UPDATE platform_role_grants SET revoked_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    for table in ["users", "workspaces"] {
        sqlx::query(&format!("UPDATE {table} SET disabled_at = now()"))
            .execute(&pool)
            .await
            .unwrap();
        for key in [&keys.personal_key, &keys.team_key] {
            assert!(
                store.authenticate(&key.token).await.unwrap().is_none(),
                "{table}"
            );
        }
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
    // Authentication without entitlement is not a platform user.
    sqlx::query("UPDATE users SET cleaned_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        store
            .authenticate(&keys.personal_key.token)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn personal_workspace_cannot_be_shared_by_issuing_another_users_key(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email) VALUES($1,'other@local.invalid')")
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO platform_role_grants(user_id,role,source) VALUES($1,'admin','manual')",
    )
    .bind(other)
    .execute(&pool)
    .await
    .unwrap();
    // Even an accidental foreign key and membership grant cannot confer owner-private access.
    sqlx::query("UPDATE api_keys SET issued_to_user_id=$1 WHERE id=$2")
        .bind(other)
        .bind(keys.personal_key.id)
        .execute(&pool)
        .await
        .unwrap();
    let sharing = sqlx::query("INSERT INTO workspace_membership_grants(workspace_id,user_id,role,source) VALUES($1,$2,'member','manual')")
        .bind(keys.personal_workspace_id).bind(other).execute(&pool).await;
    // A schema guard may reject it; otherwise the effective access view must exclude it.
    if let Err(error) = sharing {
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
    } else {
        let effective: i64 = sqlx::query_scalar("SELECT count(*) FROM effective_workspace_memberships WHERE workspace_id=$1 AND user_id=$2")
            .bind(keys.personal_workspace_id).bind(other).fetch_one(&pool).await.unwrap();
        assert_eq!(effective, 0);
    }
    assert!(
        store
            .authenticate(&keys.personal_key.token)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn database_enforces_workspace_lineage_and_service_account_foreign_keys(pool: PgPool) {
    let (_store, keys) = fixture(&pool).await;
    let account = Uuid::new_v4();
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'automation')")
        .bind(account)
        .bind(keys.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    let foreign_key = NewApiKey::generate().unwrap();
    let result = sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash) VALUES($1,$2,$3,'foreign service',$4)")
        .bind(foreign_key.id).bind(keys.personal_workspace_id).bind(account).bind(foreign_key.digest.as_slice()).execute(&pool).await;
    assert_eq!(
        result
            .unwrap_err()
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23503")
    );
    let lineage = sqlx::query("UPDATE api_keys SET governance_key_id=$1 WHERE id=$2")
        .bind(keys.team_key.id)
        .bind(keys.personal_key.id)
        .execute(&pool)
        .await;
    assert_eq!(
        lineage
            .unwrap_err()
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23503")
    );
    let restriction = sqlx::query(
        "INSERT INTO key_model_restrictions(workspace_id,governance_key_id) VALUES($1,$2)",
    )
    .bind(keys.personal_workspace_id)
    .bind(keys.team_key.id)
    .execute(&pool)
    .await;
    assert_eq!(
        restriction
            .unwrap_err()
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23503")
    );
    let personal_service = sqlx::query(
        "INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'private automation')",
    )
    .bind(Uuid::new_v4())
    .bind(keys.personal_workspace_id)
    .execute(&pool)
    .await;
    assert_eq!(
        personal_service
            .unwrap_err()
            .as_database_error()
            .unwrap()
            .code()
            .as_deref(),
        Some("23514")
    );
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM information_schema.columns WHERE table_schema='public' AND column_name='organization_id'")
        .fetch_one(&pool).await.unwrap(), 0);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn service_keys_survive_human_entitlement_loss_but_not_account_or_workspace_disable(
    pool: PgPool,
) {
    let (store, keys) = fixture(&pool).await;
    let account = Uuid::new_v4();
    let key = NewApiKey::generate().unwrap();
    sqlx::query("INSERT INTO service_accounts(id,workspace_id,name) VALUES($1,$2,'CI')")
        .bind(account)
        .bind(keys.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO api_keys(id,workspace_id,service_account_id,name,secret_hash) VALUES($1,$2,$3,'CI',$4)")
        .bind(key.id).bind(keys.team_workspace_id).bind(account).bind(key.digest.as_slice()).execute(&pool).await.unwrap();
    sqlx::query("UPDATE platform_role_grants SET revoked_at=now()")
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
    let principal = store.authenticate(&key.token).await.unwrap().unwrap();
    assert_eq!(principal.user_id, None);
    for (table, id) in [
        ("service_accounts", account),
        ("workspaces", keys.team_workspace_id),
    ] {
        sqlx::query(&format!("UPDATE {table} SET disabled_at=now() WHERE id=$1"))
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(store.authenticate(&key.token).await.unwrap().is_none());
        sqlx::query(&format!("UPDATE {table} SET disabled_at=NULL WHERE id=$1"))
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(store.authenticate(&key.token).await.unwrap().is_some());
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn http_enforces_workspace_scope_and_unregistered_protocol_errors(pool: PgPool) {
    let (store, keys) = fixture(&pool).await;
    enable_example(&pool).await;
    // Declare these protocols so failure tests the unregistered adapter, not model metadata.
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','responses','messages']",
    )
    .execute(&pool)
    .await
    .unwrap();
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

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn readiness_rejects_schema_drift(pool: PgPool) {
    let store = Store::new(pool.clone());
    assert!(store.is_ready().await);
    sqlx::query("UPDATE _sqlx_migrations SET checksum = decode('00', 'hex')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!store.is_ready().await);
}
