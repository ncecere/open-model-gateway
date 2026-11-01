//! Real gateway -> real OpenAI HTTP decoder -> loopback mock upstream.
//! Response names are provider data, not request-side function identifiers.
use super::responses as r;
use crate::{
    bootstrap,
    config::Environment,
    http,
    inference::{Engine, EngineLimits, error::InferenceError},
    providers::{
        ProviderRegistry,
        openai::OpenAiAdapter,
        secrets::{Secret, SecretResolver},
    },
    store::Store,
};
use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret, InferenceError> {
        Secret::new("fixture-only".into())
    }
}
struct Upstream {
    endpoint: String,
    reply: Arc<Mutex<Value>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Upstream {
    async fn start() -> Self {
        let reply = Arc::new(Mutex::new(Value::Null));
        let current = reply.clone();
        let app = Router::new().fallback(move || {
            let current = current.clone();
            async move { Json(current.lock().unwrap().clone()) }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            endpoint,
            reply,
            task,
        }
    }
    fn tool(&self, name: &str, id: &str) {
        *self.reply.lock().unwrap() = json!({"choices":[{"index":0,"message":{
            "role":"assistant","content":null,"tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":"{}"}}]
        },"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":2}});
    }
}
async fn gateway(pool: &PgPool, upstream: &Upstream) -> (Router, String) {
    let store = Store::new(pool.clone());
    let keys = bootstrap::seed(&store, Environment::Development)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE provider_connections SET provider='openai',enabled=true,credential_ref='env:FIXTURE_ONLY'")
        .execute(pool).await.unwrap();
    sqlx::query("UPDATE deployments SET enabled=true,upstream_model='private'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE models SET supported_protocols=ARRAY['chat_completions']")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO deployment_prices(id,deployment_id,input_microusd_per_million,output_microusd_per_million,input_token_limit,output_token_limit,pricing_version) SELECT gen_random_uuid(),id,1000000,1000000,1000,100,1 FROM deployments")
        .execute(pool).await.unwrap();
    let mut registry = ProviderRegistry::default();
    registry
        .register(Arc::new(OpenAiAdapter::for_test(
            Arc::new(Resolver),
            upstream.endpoint.clone(),
        )))
        .unwrap();
    let engine = Engine::new(Arc::new(store.clone()), registry, EngineLimits::default()).unwrap();
    (
        http::router_with_engine(store, None, engine),
        keys.personal_key.token,
    )
}
fn request(key: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
#[sqlx::test(migrations = "./enterprise_migrations")]
async fn inference_openapi_chat_response_names_follow_real_upstream_decode(pool: PgPool) {
    let upstream = Upstream::start().await;
    let (app, key) = gateway(&pool, &upstream).await;
    let body = json!({"model":"company/smart","messages":[{"role":"user","content":"hello"}],"max_completion_tokens":32,
        "tools":[{"type":"function","function":{"name":"ping"}}]});
    // Current handler behavior: undeclared/dotted/long names are copied, not
    // validated against the request grammar. Do not fix production here (#82).
    for (case, name, id) in [
        (
            "dotted",
            "functions.get_weather".to_owned(),
            "call.dotted".to_owned(),
        ),
        (
            "colon",
            "functions:get_weather".to_owned(),
            "call:colon".to_owned(),
        ),
        ("long", "n".repeat(65), "i".repeat(65)),
        ("unicode", "天气".to_owned(), "调用".to_owned()),
        ("whitespace", " ".to_owned(), " ".to_owned()),
        ("ordinary", "ping".to_owned(), "call_0".to_owned()),
    ] {
        upstream.tool(&name, &id);
        let response = app.clone().oneshot(request(&key, &body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{case} response status");
        let captured = r::capture("POST", "/v1/chat/completions", response).await;
        let value: Value = serde_json::from_slice(&captured.bytes)
            .unwrap_or_else(|_| panic!("response JSON kind"));
        let call = &value["choices"][0]["message"]["tool_calls"][0];
        assert!(
            call["function"]["name"].as_str() == Some(name.as_str()),
            "{case} response /choices/0/message/tool_calls/0/function/name"
        );
        assert!(
            call["id"].as_str() == Some(id.as_str()),
            "{case} response /choices/0/message/tool_calls/0/id"
        );
    }
    for (case, name, id) in [("empty name", "", "call_0"), ("empty id", "ping", "")] {
        upstream.tool(name, id);
        let response = app.clone().oneshot(request(&key, &body)).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_GATEWAY,
            "{case} decoder refusal status"
        );
        r::capture("POST", "/v1/chat/completions", response).await;
    }
    let mut invalid = body;
    invalid["tools"][0]["function"]["name"] = json!("functions.get_weather");
    assert!(
        super::generation::validate_request(r::artifact(), "/v1/chat/completions", &invalid)
            .is_err(),
        "request name grammar remains strict"
    );
    let response = app.oneshot(request(&key, &invalid)).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "request name refusal status"
    );
    r::capture("POST", "/v1/chat/completions", response).await;
}

#[sqlx::test(migrations = "./enterprise_migrations")]
async fn inference_openapi_generation_model_trim_matches_real_handlers(pool: PgPool) {
    let upstream = Upstream::start().await;
    let (app, key) = gateway(&pool, &upstream).await;
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        // Rust trim() does not strip U+FEFF. A nonexistent but syntactically
        // valid model must reach lookup (404), not be rejected by the contract.
        let body = match path {
            "/v1/chat/completions" => {
                json!({"model":"\u{feff}","messages":[{"role":"user","content":"hello"}]})
            }
            "/v1/responses" => json!({"model":"\u{feff}","input":"hello"}),
            _ => {
                json!({"model":"\u{feff}","max_tokens":32,"messages":[{"role":"user","content":"hello"}]})
            }
        };
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {key}"))
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "model trim lookup status"
        );
        r::capture("POST", path, response).await;
        super::generation::validate_request(r::artifact(), path, &body)
            .expect("contract must accept handler model trim boundary");
    }
}
