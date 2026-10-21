#![cfg(feature = "integration-tests")]

use async_trait::async_trait;
use axum::{http::StatusCode, middleware, routing::post};
use open_model_gateway::{
    bootstrap,
    config::Environment,
    http,
    inference::{Engine, EngineLimits, error::InferenceError, types::*},
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};
use sqlx::PgPool;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct RetryFixture {
    pool: PgPool,
    upstream: String,
    dispatches: AtomicUsize,
}
#[async_trait]
impl ProviderAdapter for RetryFixture {
    fn id(&self) -> &'static str {
        "retry_fixture"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: false,
            tools: false,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        matches!(
            protocol,
            ApiProtocol::ChatCompletions | ApiProtocol::Responses | ApiProtocol::Messages
        )
    }
    async fn execute(
        &self,
        _: &Deployment,
        request: ChatRequest,
    ) -> Result<ProviderOutput, InferenceError> {
        assert!(!request.stream);
        assert_eq!(request.max_output_tokens, Some(7));
        let attempt = self.dispatches.fetch_add(1, Ordering::SeqCst) + 1;
        // A distinct, committed admission must exist BEFORE every upstream call.
        let admitted: i64 = sqlx::query_scalar("SELECT count(*) FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id JOIN monetary_ledger l ON l.execution_id=e.id AND l.kind='hold'")
            .fetch_one(&self.pool).await.unwrap();
        assert_eq!(admitted, attempt as i64);
        let response = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .unwrap()
            .post(&self.upstream)
            .send()
            .await
            .unwrap();
        if !response.status().is_success() {
            return Err(InferenceError::UpstreamUnavailable);
        }
        Ok(ProviderOutput::Complete(ChatResponse {
            content: Some("recovered".into()),
            tool_calls: vec![],
            finish_reason: FinishReason::Stop,
            usage: Usage {
                input_tokens: Some(3),
                output_tokens: Some(4),
                ..Default::default()
            },
        }))
    }
}

async fn run(pool: PgPool, permanent: bool) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE provider_connections SET provider='retry_fixture',enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','responses','messages']",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT gen_random_uuid(),id,1000000,1000000,1000,100,1 FROM deployments")
        .execute(&pool).await.unwrap();
    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let calls = upstream_calls.clone();
    let upstream = axum::Router::new().route(
        "/",
        post(move || {
            let calls = calls.clone();
            async move {
                // Explicit transient failure, then recovery, once per SDK call.
                if calls.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}/", listener.local_addr().unwrap());
    let upstream_server =
        tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let adapter = Arc::new(RetryFixture {
        pool: pool.clone(),
        upstream: upstream_url,
        dispatches: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::default();
    registry.register(adapter.clone()).unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    // Capture each completed gateway request, including before the SDK retries.
    let observations = Arc::new(Mutex::new(Vec::new()));
    let records = observations.clone();
    let db = pool.clone();
    let calls = upstream_calls.clone();
    let app = http::router_with_engine(store, None, engine).layer(middleware::from_fn(
        move |request: axum::extract::Request, next: middleware::Next| {
            let records = records.clone();
            let db = db.clone();
            let calls = calls.clone();
            async move {
                let path = request.uri().path().to_owned();
                let response = next.run(request).await;
                let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM inference_executions),(SELECT count(*) FROM governance_reservations),(SELECT count(*) FROM monetary_ledger WHERE kind='hold')")
                    .fetch_one(&db).await.unwrap();
                records.lock().unwrap().push((path, response.status(), counts, calls.load(Ordering::SeqCst)));
                response
            }
        }
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/sdk-retry-contracts.mjs");
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new("node")
            .arg(script)
            .arg(if permanent { "permanent" } else { "transient" })
            .env("SDK_TEST_BASE_URL", format!("http://{address}"))
            .env("SDK_TEST_API_KEY", keys.personal_key.token)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    server.abort();
    upstream_server.abort();
    let output = result
        .expect("retry SDK deadline")
        .expect("node and npm ci are required");
    assert!(
        output.status.success(),
        "SDK retry contracts failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let records = observations.lock().unwrap().clone();
    let paths = ["/v1/chat/completions", "/v1/responses", "/v1/messages"];
    if permanent {
        assert_eq!(records.len(), 3);
        for (record, path) in records.iter().zip(paths) {
            assert_eq!(
                record,
                &(path.into(), StatusCode::NOT_IMPLEMENTED, (0, 0, 0), 0)
            );
        }
        assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 0);
        let writes: i64 = sqlx::query_scalar("SELECT (SELECT count(*) FROM monetary_ledger)+(SELECT count(*) FROM rate_minute_counters)+(SELECT count(*) FROM inflight_counters)+(SELECT count(*) FROM budget_totals)")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(writes, 0);
    } else {
        assert_eq!(records.len(), 6);
        for (i, record) in records.iter().enumerate() {
            let attempts = (i + 1) as i64;
            assert_eq!(
                record,
                &(
                    paths[i / 2].into(),
                    if i.is_multiple_of(2) {
                        StatusCode::BAD_GATEWAY
                    } else {
                        StatusCode::OK
                    },
                    (attempts, attempts, attempts),
                    i + 1
                )
            );
        }
        assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 6);
        // SDK retries are NEW HTTP requests, not gateway failover attempts.
        let roots: (i64, i64) = sqlx::query_as("SELECT count(DISTINCT root_request_id),count(*) FILTER(WHERE attempt_number=1 AND id=root_request_id) FROM inference_executions")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(roots, (6, 6));
        let settled: Vec<(String, Option<i64>, Option<i64>)> = sqlx::query_as("SELECT r.state,r.held_microusd,r.actual_microusd FROM governance_reservations r JOIN inference_executions e ON e.id=r.execution_id ORDER BY r.admitted_at")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(settled.len(), 6);
        for (i, row) in settled.iter().enumerate() {
            assert_eq!(
                row,
                &if i.is_multiple_of(2) {
                    ("unknown".into(), Some(1007), None)
                } else {
                    ("settled".into(), Some(1007), Some(7))
                }
            );
        }
        let retained: Vec<(String, i64, i64)> = sqlx::query_as("SELECT l.kind,count(*),sum(l.amount_microusd)::bigint FROM monetary_ledger l JOIN governance_reservations r ON r.execution_id=l.execution_id WHERE r.state='unknown' GROUP BY l.kind ORDER BY l.kind")
            .fetch_all(&pool).await.unwrap();
        assert_eq!(
            retained,
            vec![("hold".into(), 3, 3021), ("unknown".into(), 3, 0)],
            "unknown failures keep holds and zero-valued unknown markers, never releases or settlements"
        );
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn default_sdks_do_not_retry_permanent_capability_refusals(pool: PgPool) {
    run(pool, true).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn default_sdks_retry_transient_errors_with_separate_durable_attempts(pool: PgPool) {
    run(pool, false).await;
}
