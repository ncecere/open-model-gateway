#![cfg(feature = "integration-tests")]
//! `POST /v1/images/generations` through real HTTP, authentication, catalog,
//! routing, v3 admission and settlement against a disposable database.

use async_trait::async_trait;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use open_model_gateway::{
    billing::{BillingUsage, MeterUsage, MeterVariant},
    bootstrap::{self, DevelopmentKeys},
    config::Environment,
    http,
    inference::{Engine, EngineLimits, error::InferenceError, types::*},
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;
use uuid::Uuid;

const PNG_1X1: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

struct Fixture {
    calls: AtomicUsize,
}
#[async_trait]
impl ProviderAdapter for Fixture {
    fn id(&self) -> &'static str {
        "test_vendor"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        matches!(protocol, ApiProtocol::ChatCompletions | ApiProtocol::Images)
    }
    fn supports_image_request(&self, _: &Deployment, request: &ImageRequest) -> bool {
        request.seed.is_none()
    }
    async fn execute(
        &self,
        _: &Deployment,
        _: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        Err(InferenceError::Unsupported)
    }
    async fn execute_images(
        &self,
        target: &Deployment,
        request: ImageRequest,
    ) -> Result<ImageResponse, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(target.upstream_model, "private-upstream-id");
        assert_eq!(request.max_response_bytes, 20 * 1024 * 1024);
        Ok(ImageResponse {
            created: 1_791_432_289,
            images: (0..request.n)
                .map(|_| GeneratedImage {
                    b64_json: PNG_1X1.into(),
                    media_type: ImageMediaType::Png,
                    revised_prompt: None,
                })
                .collect(),
            usage: Usage {
                input_tokens: Some(9),
                output_tokens: Some(272),
                billing: Some(BillingUsage {
                    total_input_tokens: Some(9),
                    uncached_input_tokens: Some(9),
                    cache_read_input_tokens: Some(0),
                    cache_write_input_tokens: Some(0),
                    cache_write_default_input_tokens: Some(0),
                    cache_write_5m_input_tokens: Some(0),
                    cache_write_1h_input_tokens: Some(0),
                }),
                meters: Some(MeterUsage {
                    output_images: Some(u64::from(request.n)),
                    input_characters: Some(0),
                    input_audio_seconds_ms: Some(0),
                    output_audio_seconds_ms: Some(0),
                    search_units: Some(0),
                    requests: Some(1),
                }),
                output_image_variant: MeterVariant::new("1024x1024"),
                provider_cost_microusd: None,
                reasoning_tokens: None,
            },
        })
    }
}

async fn fixture(
    pool: &PgPool,
    limits: EngineLimits,
) -> (axum::Router, DevelopmentKeys, Arc<Fixture>) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    for sql in [
        "UPDATE provider_connections SET provider='test_vendor', enabled=true",
        "UPDATE deployments SET enabled=true, upstream_model='private-upstream-id'",
        "UPDATE models SET supported_protocols=ARRAY['images']",
    ] {
        sqlx::query(sql).execute(pool).await.unwrap();
    }
    let adapter = Arc::new(Fixture {
        calls: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::default();
    registry.register(adapter.clone()).unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, limits).unwrap();
    (http::router_with_engine(store, None, engine), keys, adapter)
}
fn post(key: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/images/generations")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn body() -> Value {
    json!({"model":"company/smart","prompt":"Do not log this prompt","n":2,"size":"1024x1024","quality":"low","response_format":"b64_json"})
}
async fn json_body(response: axum::response::Response) -> (StatusCode, Value, String) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text,
    )
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn images_reach_http_with_v3_per_image_and_token_settlement(pool: PgPool) {
    let (app, keys, adapter) = fixture(&pool, EngineLimits::default()).await;
    let deployment: Uuid = sqlx::query_scalar("SELECT id FROM deployments")
        .fetch_one(&pool)
        .await
        .unwrap();
    let na = |m: &str| json!({"meter":m,"not_applicable":true});
    let tokens = |m: &str, a: &str| json!({"meter":m,"microusd_per_batch":a,"batch":1000000,"unit_label":"/M tokens","sku_label":"Tokens"});
    let image = |a: &str, v: &str| json!({"meter":"output_images","microusd_per_batch":a,"batch":1,"unit_label":"/image","sku_label":"Image","variant":v});
    let lines = json!([
        tokens("input_tokens", "2000000"), tokens("output_tokens", "8000000"),
        na("cache_read_tokens"), na("cache_write_tokens"), na("cache_write_5m_tokens"), na("cache_write_1h_tokens"),
        image("5000", "1024x1024"), image("11000", "1536x1024"),
        {"meter":"requests","microusd_per_batch":"0","batch":1,"unit_label":"/request","sku_label":"Request"}
    ]);
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_token_limit,output_token_limit,pricing_version,price_lines,max_units) VALUES($1,$2,1000,4000,3,$3,'{}')")
        .bind(Uuid::new_v4()).bind(deployment).bind(lines).execute(&pool).await.unwrap();
    // Hold: 1000 × $2/M + 4000 × $8/M + 2 × $0.011 (highest variant) = 56,000.
    sqlx::query(
        "INSERT INTO policy_budgets(layer,period,amount_microusd) VALUES('installation','month',56000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let response = app
        .clone()
        .oneshot(post(&keys.personal_key.token, body()))
        .await
        .unwrap();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let (status, value, text) = json_body(response).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        value,
        json!({"created":1791432289,"data":[{"b64_json":PNG_1X1},{"b64_json":PNG_1X1}],
            "usage":{"input_tokens":9,"output_tokens":272,"total_tokens":281}})
    );
    for leaked in ["private-upstream-id", "Do not log", "url"] {
        assert!(!text.contains(leaked), "{leaked}");
    }
    type Row = (String, String, i64, i64, Value, Option<String>);
    let row: Row = sqlx::query_as("SELECT e.workload_kind,r.state,r.held_microusd,r.actual_microusd,r.meter_usage,r.output_image_variant FROM inference_executions e JOIN governance_reservations r ON r.execution_id=e.id WHERE e.id=$1::uuid")
        .bind(&id).fetch_one(&pool).await.unwrap();
    // ceil(9 × 2) + ceil(272 × 8) + 2 × 5000 = 18 + 2176 + 10000.
    assert_eq!(
        (row.0.as_str(), row.1.as_str(), row.2, row.3),
        ("images", "settled", 56_000, 12_194)
    );
    assert_eq!(row.4["output_images"], "2");
    assert_eq!(row.5.as_deref(), Some("1024x1024"));
    // 43,806 remain: the next 56,000 hold is a non-retryable budget denial.
    let response = app
        .oneshot(post(&keys.personal_key.token, body()))
        .await
        .unwrap();
    assert_eq!(response.headers()["x-should-retry"], "false");
    let (status, value, _) = json_body(response).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(value["error"]["code"], "budget_exceeded");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn images_route_enforces_auth_catalog_protocol_and_contract(pool: PgPool) {
    let (app, keys, adapter) = fixture(&pool, EngineLimits::default()).await;
    let unauthenticated = Request::builder()
        .method("POST")
        .uri("/v1/images/generations")
        .header("content-type", "application/json")
        .body(Body::from(body().to_string()))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(unauthenticated).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(post("omg_live_not_a_real_key", body()))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let call = |b: Value| {
        let app = app.clone();
        let token = keys.personal_key.token.clone();
        async move { json_body(app.oneshot(post(&token, b)).await.unwrap()).await }
    };
    // URL responses are an explicit capability gap; bad shapes are 400.
    let mut url = body();
    url["response_format"] = json!("url");
    let (status, value, _) = call(url).await;
    assert_eq!(
        (status, value["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("unsupported_capability"))
    );
    for bad in [
        json!({"model":"company/smart","prompt":"p","n":5}),
        json!({"model":"company/smart","prompt":""}),
        json!({"model":"company/smart","prompt":"p","output_format":"webp"}),
        json!({"model":"company/smart","prompt":"p","size":"big"}),
    ] {
        let (status, value, _) = call(bad).await;
        assert_eq!(
            (status, value["error"]["code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_request_error"))
        );
    }
    // Adapter request gating (seed unsupported) happens before admission.
    let mut seeded = body();
    seeded["seed"] = json!(7);
    let (status, value, _) = call(seeded).await;
    assert_eq!(
        (status, value["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("unsupported_capability"))
    );
    // Chat models are never selected for images.
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions']")
        .execute(&pool)
        .await
        .unwrap();
    let (status, value, _) = call(body()).await;
    assert_eq!(
        (status, value["error"]["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("unsupported_capability"))
    );
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['images']")
        .execute(&pool)
        .await
        .unwrap();
    // Workspace catalog denial.
    sqlx::query("DELETE FROM workspace_model_grants WHERE workspace_id=$1")
        .bind(keys.team_workspace_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, value, _) = json_body(
        app.clone()
            .oneshot(post(&keys.team_key.token, body()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        (status, value["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("model_not_found"))
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inference_executions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn images_body_cap_is_configured_per_route(pool: PgPool) {
    let mut limits = EngineLimits::default();
    limits.workloads.images_body_bytes = 4096;
    let (app, keys, adapter) = fixture(&pool, limits).await;
    let mut big = body();
    big["prompt"] = json!("x".repeat(5000));
    assert_eq!(
        app.clone()
            .oneshot(post(&keys.personal_key.token, big))
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        app.oneshot(post(&keys.personal_key.token, body()))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}
