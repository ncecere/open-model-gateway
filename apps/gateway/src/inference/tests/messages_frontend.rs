//! HTTP frontend -> real Engine -> real OpenAI adapter -> mock upstream.
use super::*;
use axum::{
    Extension, Json, Router,
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Resolver;
impl crate::providers::secrets::SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<crate::providers::secrets::Secret, InferenceError> {
        crate::providers::secrets::Secret::new("server-held-provider-key".into())
    }
}
struct Mock {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
type Captures = Arc<Mutex<Vec<(HeaderMap, Value)>>>;
async fn upstream(status: u16, response: Value, captures: Captures) -> Mock {
    let app = Router::new().fallback(move |headers: HeaderMap, bytes: Bytes| {
        let captures = captures.clone();
        let response = response.clone();
        async move {
            captures
                .lock()
                .unwrap()
                .push((headers, serde_json::from_slice(&bytes).unwrap()));
            (StatusCode::from_u16(status).unwrap(), Json(response))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Mock { url, task }
}
fn frontend(url: String) -> (Router, Arc<MemoryRepository>) {
    frontend_adapter(Arc::new(crate::providers::openai::OpenAiAdapter::for_test(
        Arc::new(Resolver),
        url,
    )))
}
fn frontend_adapter(adapter: Arc<dyn ProviderAdapter>) -> (Router, Arc<MemoryRepository>) {
    let repository = Arc::new(MemoryRepository::default());
    repository.deployments.lock().unwrap().push(Deployment {
        id: Uuid::new_v4(),
        provider: adapter.id().into(),
        upstream_model: "private".into(),
        credential_ref: "env:MOCK_KEY".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec!["messages".into()],
    });
    let mut registry = ProviderRegistry::default();
    registry.register(adapter).unwrap();
    let engine = Engine::new(repository.clone(), registry, EngineLimits::default()).unwrap();
    (
        Router::new()
            .route("/v1/messages", post(crate::protocols::messages::handle))
            .layer(Extension(engine))
            .layer(Extension(principal()))
            .layer(Extension(crate::http::RequestId(Uuid::new_v4()))),
        repository,
    )
}
fn wire(value: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages?beta=true")
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "interleaved-thinking-2025-05-14")
        .header("x-api-key", "inference-key-must-not-leave-gateway")
        .body(Body::from(value.to_string()))
        .unwrap()
}
#[tokio::test]
async fn claude_code_beta_query_reaches_real_engine_without_forwarding_inference_credentials() {
    let captures = Captures::default();
    let mock = upstream(200, json!({"id":"private","object":"chat.completion","model":"private","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}), captures.clone()).await;
    let (app, repository) = frontend(mock.url.clone());
    let response = app.oneshot(wire(json!({"model":"configured-name","max_tokens":8192,
        "system":[{"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}}],
        "thinking":{"type":"adaptive"}, "output_config":{"effort":"high"}, "metadata":{"user_id":"client-label"},
        "messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"private reasoning","signature":"opaque"},{"type":"redacted_thinking","data":"opaque"},{"type":"text","text":"answer"}]},{"role":"user","content":"continue"}]
    }))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(repository.starts.load(Ordering::SeqCst), 1);
    let captures = captures.lock().unwrap();
    assert_eq!(captures.len(), 1);
    assert_eq!(
        captures[0].0["authorization"],
        "Bearer server-held-provider-key"
    );
    for header in ["x-api-key", "anthropic-version", "anthropic-beta"] {
        assert!(!captures[0].0.contains_key(header));
    }
    let body = &captures[0].1;
    assert_eq!(body["model"], "private");
    assert_eq!(body["max_completion_tokens"], 8192);
    let serialized = body.to_string();
    for secret in [
        "inference-key-must-not-leave-gateway",
        "opaque",
        "private reasoning",
        "cache_control",
        "thinking",
        "output_config",
    ] {
        assert!(!serialized.contains(secret));
    }
    assert!(serialized.contains("instructions"));
    assert!(serialized.contains("continue"));
}
#[tokio::test]
async fn upstream_errors_have_anthropic_status_and_never_expose_upstream_bodies() {
    for (upstream_status, upstream_body, status, kind, message) in [
        (
            400,
            json!({"error":{"code":"context_length_exceeded","message":"SECRET_PROMPT SECRET_KEY"}}),
            400,
            "invalid_request_error",
            "prompt is too long",
        ),
        (
            529,
            json!({"error":{"message":"SECRET_PROMPT SECRET_KEY"}}),
            529,
            "overloaded_error",
            "overloaded",
        ),
        (
            429,
            json!({"error":{"message":"SECRET_PROMPT SECRET_KEY"}}),
            429,
            "rate_limit_error",
            "rate limit",
        ),
    ] {
        let mock = upstream(upstream_status, upstream_body, Captures::default()).await;
        let (app, _) = frontend(mock.url.clone());
        let response = app
            .oneshot(wire(
                json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":"hi"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        let bytes = axum::body::to_bytes(response.into_body(), 2048)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["type"], kind);
        let text = value["error"]["message"].as_str().unwrap();
        assert!(text.contains(message));
        assert!(text.len() <= 1024);
        assert!(!text.contains("SECRET"));
    }
}
#[tokio::test]
async fn messages_nonstream_provider_bound_allows_over_four_mib_and_refuses_over_sixteen() {
    for (size, success) in [(4 * 1024 * 1024 + 1, true), (16 * 1024 * 1024 + 1, false)] {
        let mock = upstream(200, json!({"id":"private","choices":[{"index":0,"message":{"role":"assistant","content":"x".repeat(size)},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}), Captures::default()).await;
        let (app, _) = frontend(mock.url.clone());
        let response = app
            .oneshot(wire(
                json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":"hi"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status().is_success(), success);
        let bytes = axum::body::to_bytes(response.into_body(), 20 * 1024 * 1024)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        if success {
            assert_eq!(value["content"][0]["text"].as_str().unwrap().len(), size);
        } else {
            assert_eq!(value["type"], "error");
        }
    }
}

#[tokio::test]
async fn native_anthropic_fidelity_refusals_do_not_admit_or_dispatch() {
    let captures = Captures::default();
    let mock = upstream(200, json!({}), captures.clone()).await;
    for (extra, field) in [
        (
            json!({"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral","ttl":"1h"}}]}),
            "cache_control",
        ),
        (json!({"thinking":{"type":"adaptive"}}), "thinking"),
        (
            json!({"output_config":{"effort":"high"}}),
            "output_config.effort",
        ),
        (
            json!({"output_config":{"task_budget":{"type":"tokens","total":64000}}}),
            "output_config.task_budget",
        ),
        (
            json!({"context_management":{"edits":[]}}),
            "context_management",
        ),
        (
            json!({"tools":[{"name":"lookup","input_schema":{"type":"object"},"defer_loading":false}]}),
            "defer_loading",
        ),
    ] {
        let adapter = crate::providers::anthropic::AnthropicAdapter::for_test(
            Arc::new(Resolver),
            mock.url.clone(),
        );
        let (app, repository) = frontend_adapter(Arc::new(adapter));
        let mut body =
            json!({"model":"m","max_tokens":8192,"messages":[{"role":"user","content":"hi"}]});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let mut request = wire(body);
        request.headers_mut().remove("anthropic-beta");
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), 2048)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value["error"]["message"].as_str().unwrap().contains(field));
        assert_eq!(repository.starts.load(Ordering::SeqCst), 0);
    }
    assert!(captures.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_field_is_named_and_refused_before_admission() {
    let captures = Captures::default();
    let mock = upstream(200, json!({}), captures.clone()).await;
    let (app, repository) = frontend(mock.url.clone());
    let response = app.oneshot(wire(json!({"model":"m","max_tokens":32,"future_field":null,"messages":[{"role":"user","content":"hi"}]}))).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), 2048)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("future_field")
    );
    assert_eq!(repository.starts.load(Ordering::SeqCst), 0);
    assert!(captures.lock().unwrap().is_empty());
}
