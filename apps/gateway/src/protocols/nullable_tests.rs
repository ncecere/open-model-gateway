//! Priced loopback gateway → real HTTP adapter → recording fixture contracts.
use crate::{
    bootstrap,
    config::Environment,
    http,
    inference::{Engine, EngineLimits, error::InferenceError},
    providers::{
        ProviderRegistry,
        local::{LocalAdapter, Profile, endpoints::ApprovedEndpoints},
        openai::OpenAiAdapter,
        secrets::{Secret, SecretResolver},
    },
    store::Store,
};
use axum::{Router, body::Bytes, http::Uri, response::IntoResponse};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
        Secret::new("fixture-only".into())
    }
}
struct Server {
    base: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(app: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base, task }
    }
}
fn response() -> Value {
    json!({"id":"upstream","object":"response","model":"private","status":"completed","output":[{"id":"m","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"世界","annotations":[]}]}],"usage":{"input_tokens":3,"output_tokens":2}})
}
fn native_events() -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"status":"in_progress"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"m","type":"message","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"m","content_index":0,"delta":"世界"}),
        json!({"type":"response.output_text.done","output_index":0,"item_id":"m","content_index":0,"text":"世界"}),
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"世界","annotations":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":response()["output"][0]}),
        json!({"type":"response.completed","response":response()}),
    ]
}

async fn prove_nullable_defaults(pool: sqlx::PgPool, native: bool) {
    let capture = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let truncated = Arc::new(AtomicBool::new(false));
    let c = capture.clone();
    let t = truncated.clone();
    let upstream = Server::start(Router::new().fallback(move |uri: Uri, bytes: Bytes| {
        let capture = c.clone();
        let truncated = t.clone();
        async move {
            let request: Value = serde_json::from_slice(&bytes).unwrap();
            let stream = request["stream"] == true;
            capture.lock().unwrap().push((uri.path().into(), request));
            let body = if native {
                if stream {
                    let mut events = native_events();
                    if truncated.load(Ordering::SeqCst) {
                        events.pop();
                    }
                    events.into_iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect::<String>()
                } else {
                    response().to_string()
                }
            } else if stream {
                format!("data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"世界"},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                    json!({"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}))
            } else {
                json!({"choices":[{"index":0,"message":{"role":"assistant","content":"世界"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}}).to_string()
            };
            ([("content-type", if stream { "text/event-stream" } else { "application/json" })], body).into_response()
        }
    })).await;
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    let provider = if native {
        "openai"
    } else {
        "openai_compatible"
    };
    let endpoint = format!("{}/v1", upstream.base);
    sqlx::query(
        "UPDATE provider_connections SET provider=$1,enabled=true,credential_ref=$2,endpoint=$3",
    )
    .bind(provider)
    .bind(if native { "env:FIXTURE_ONLY" } else { "none" })
    .bind(if native { None } else { Some(&endpoint) })
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE deployments SET enabled=true,upstream_model='private'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE models SET supported_protocols=ARRAY['chat_completions','responses','messages']",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT gen_random_uuid(),id,1000000,1000000,1000,100,1 FROM deployments").execute(&pool).await.unwrap();
    let mut registry = ProviderRegistry::default();
    if native {
        registry
            .register(Arc::new(OpenAiAdapter::for_test(
                Arc::new(Resolver),
                endpoint,
            )))
            .unwrap();
    } else {
        registry
            .register(Arc::new(LocalAdapter::new(
                Profile::OpenAiCompatible,
                Arc::new(Resolver),
                Arc::new(ApprovedEndpoints::for_test(&endpoint)),
            )))
            .unwrap();
    }
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    let gateway = Server::start(http::router_with_engine(store, None, engine)).await;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("node")
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../tests/nullable-sdk-contracts.mjs"),
            )
            .env("SDK_TEST_BASE_URL", &gateway.base)
            .env("SDK_TEST_API_KEY", &keys.personal_key.token)
            .env(
                "SDK_TEST_PROTOCOL",
                if native { "responses" } else { "chat" },
            )
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("SDK deadline")
    .expect("node/npm ci required");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let expected: i64 = 19;
    assert_eq!(report["positives"], expected);
    assert_eq!(report["refusals"], if native { 44 } else { 49 });
    let records = capture.lock().unwrap().clone();
    assert_eq!(records.len(), expected as usize, "refusals cannot dispatch");
    for (path, sent) in &records {
        assert_eq!(
            path,
            if native {
                "/v1/responses"
            } else {
                "/v1/chat/completions"
            }
        );
        assert_eq!(sent["model"], "private");
        assert!(sent["stream"].is_boolean());
        assert_eq!(
            sent[if native {
                "max_output_tokens"
            } else {
                "max_tokens"
            }],
            100
        );
        assert!(sent.get("tools").is_none());
        if native {
            assert_eq!(sent["store"], false, "never enable provider storage");
            assert_eq!(sent["input"][0]["content"], "hello");
            assert!(sent.get("messages").is_none());
        } else {
            assert_eq!(sent["messages"][0]["content"], "hello");
            assert!(sent["messages"][0].get("refusal").is_none());
        }
    }
    assert_eq!(
        records.iter().filter(|(_, v)| v["stream"] == true).count(),
        4
    );
    let attempts: (i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER(WHERE state='succeeded') FROM inference_executions",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attempts, (expected, expected), "refusals cannot admit");
    let holds: Vec<(i64, i64, i64)> = sqlx::query_as("SELECT r.reserved_tokens,l.amount_microusd,p.output_token_limit FROM governance_reservations r JOIN deployment_prices p ON p.id=r.price_id JOIN monetary_ledger l ON l.execution_id=r.execution_id WHERE l.kind='hold'").fetch_all(&pool).await.unwrap();
    assert_eq!(holds, vec![(1100, 1100, 100); expected as usize]);
    let settlements: Vec<(i64, i64, i64)> = sqlx::query_as("SELECT actual_microusd,input_tokens,output_tokens FROM governance_reservations WHERE state='settled'").fetch_all(&pool).await.unwrap();
    assert_eq!(settlements, vec![(5, 3, 2); expected as usize]);
    let ledger: (i64, i64) = sqlx::query_as(
        "SELECT count(*),sum(amount_microusd)::bigint FROM monetary_ledger WHERE kind='settlement'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ledger, (expected, expected * 5));
    if native {
        // Null storage on a real, truncated native stream must not fabricate a
        // successful terminal event or release unknown financial evidence.
        truncated.store(true, Ordering::SeqCst);
        let reply = reqwest::Client::new()
            .post(format!("{}/v1/responses", gateway.base))
            .bearer_auth(&keys.personal_key.token)
            .json(&json!({"model":"company/smart","input":"hello","stream":true,"store":null}))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), 200);
        let body = reply.text().await.unwrap();
        assert!(!body.contains("event: response.completed"));
        assert!(body.contains("event: error"));
        let unknown: Vec<(i64, Option<i64>)> = sqlx::query_as(
            "SELECT held_microusd,actual_microusd FROM governance_reservations WHERE state='unknown'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            unknown,
            vec![(1100, None)],
            "unknown cost retains its full hold"
        );
        let settlements: i64 =
            sqlx::query_scalar("SELECT count(*) FROM monetary_ledger WHERE kind='settlement'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(settlements, expected);
    }
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn priced_native_responses_nullable_defaults(pool: sqlx::PgPool) {
    prove_nullable_defaults(pool, true).await;
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn priced_local_chat_nullable_defaults(pool: sqlx::PgPool) {
    prove_nullable_defaults(pool, false).await;
}
