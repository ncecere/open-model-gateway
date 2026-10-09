#![cfg(feature = "integration-tests")]
//! Operations surfaces against a real database: structured readiness and
//! Prometheus metrics fed by real admission/settlement (docs/operations.md).
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use open_model_gateway::{
    bootstrap,
    config::Environment,
    http,
    inference::{Engine, EngineLimits, error::InferenceError, types::*},
    metrics::METRICS,
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
    web::WebAssets,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

struct OpsMock;
#[async_trait]
impl ProviderAdapter for OpsMock {
    fn id(&self) -> &'static str {
        "ops_mock"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: false,
        }
    }
    async fn execute(
        &self,
        _target: &Deployment,
        _request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        Ok(ProviderOutput::Complete(ChatResponse {
            content: Some("ok".into()),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: Usage {
                input_tokens: Some(7),
                output_tokens: Some(2),
                ..Default::default()
            },
        }))
    }
}

async fn ready(app: axum::Router) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn readiness_checks_database_schema_and_web_build(pool: PgPool) {
    let store = Store::new(pool.clone());
    bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap();
    let web = tempfile::tempdir().unwrap();
    std::fs::write(web.path().join("index.html"), "<!doctype html>").unwrap();
    let assets = WebAssets::load(web.path()).unwrap();
    let app = http::router_with_web(store.clone(), Some(assets));
    assert_eq!(
        ready(app.clone()).await,
        (
            StatusCode::OK,
            json!({"status":"ready","checks":{"database":"ok","schema":"ok","web":"ok"}})
        )
    );
    assert_eq!(
        ready(http::router(store.clone())).await.1["checks"]["web"],
        "disabled"
    );
    // A removed/unmounted build fails readiness instead of serving 404s.
    std::fs::remove_file(web.path().join("index.html")).unwrap();
    assert_eq!(
        ready(app.clone()).await,
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"status":"not_ready","checks":{"database":"ok","schema":"ok","web":"fail"}})
        )
    );
    // Lineage that no longer matches this binary is not ready.
    sqlx::query(
        "DELETE FROM _sqlx_migrations WHERE version=(SELECT max(version) FROM _sqlx_migrations)",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        ready(http::router(store)).await,
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"status":"not_ready","checks":{"database":"ok","schema":"fail","web":"disabled"}})
        )
    );
}

fn sample(exposition: &str, prefix: &str) -> u64 {
    exposition
        .lines()
        .find_map(|line| line.strip_prefix(prefix)?.trim().parse().ok())
        .unwrap_or(0)
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn metrics_follow_real_admission_and_settlement(pool: PgPool) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::raw_sql(
        "UPDATE provider_connections SET provider='ops_mock', enabled=true;
         UPDATE deployments SET enabled=true;
         UPDATE models SET supported_protocols=ARRAY['chat_completions'], public_name='ops/metrics';
         INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version)
           SELECT gen_random_uuid(),id,1000000,2000000,100,10,1 FROM deployments;
         INSERT INTO installation_policy(singleton,requests_per_minute) VALUES(true,2);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut registry = ProviderRegistry::default();
    registry.register(Arc::new(OpsMock)).unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let app = http::router_with_engine(store.clone(), None, engine);
    // requests_per_minute=2 over three requests: keep them in one UTC minute.
    store.freeze_admission_clock().await.unwrap();
    let before = METRICS.render(None).await;
    let settled = r#"gateway_settlements_total{outcome="settled"} "#;
    let denied = r#"gateway_admission_denials_total{code="rate_limit_error",scope="policy"} "#;
    let route =
        r#"gateway_http_requests_total{method="POST",route="/v1/chat/completions",status="429"} "#;
    let mut statuses = vec![];
    for _ in 0..3 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(
                        "authorization",
                        format!("Bearer {}", keys.personal_key.token),
                    )
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"model":"ops/metrics","max_completion_tokens":10,
                            "messages":[{"role":"user","content":"never a metric label"}]})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        statuses.push(response.status());
    }
    assert_eq!(
        statuses,
        [
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );
    let after = METRICS.render(Some(&store)).await;
    assert_eq!(sample(&after, settled) - sample(&before, settled), 2);
    assert_eq!(sample(&after, denied) - sample(&before, denied), 1);
    assert_eq!(sample(&after, route) - sample(&before, route), 1);
    for line in [
        r#"gateway_inference_attempts_total{provider="ops_mock",model="ops/metrics",outcome="succeeded"} 2"#,
        r#"gateway_inference_tokens_total{provider="ops_mock",model="ops/metrics",direction="input"} 14"#,
        r#"gateway_inference_tokens_total{provider="ops_mock",model="ops/metrics",direction="output"} 4"#,
        r#"gateway_upstream_duration_seconds_count{provider="ops_mock",model="ops/metrics"} 2"#,
        r#"gateway_reservations_held{state="pending"} 0"#,
        r#"gateway_reservations_held{state="unknown"} 0"#,
        r#"gateway_db_pool_max_connections "#,
    ] {
        assert!(after.contains(line), "missing {line}");
    }
    // Low cardinality: no identifiers, keys or prompt data in any label.
    for secret in [
        keys.personal_key.token.as_str(),
        &keys.personal_workspace_id.to_string(),
        &keys.installation_id.to_string(),
        "never a metric label",
    ] {
        assert!(!after.contains(secret));
    }
}
