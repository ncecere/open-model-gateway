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
async fn native_frontend_preserves_beta_values_and_blocks_through_the_engine() {
    let captures = Captures::default();
    let native = json!({"id":"private","type":"message","role":"assistant","model":"private","content":[{"type":"thinking","thinking":"reason","signature":"opaque+/=="},{"type":"text","text":"answer"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":4}});
    let mock = upstream(200, native.clone(), captures.clone()).await;
    let adapter = crate::providers::anthropic::AnthropicAdapter::for_test(
        Arc::new(Resolver),
        mock.url.clone(),
    );
    let (app, repository) = frontend_adapter(Arc::new(adapter));
    let system = json!([{"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}}]);
    let mut input = wire(
        json!({"model":"configured-name","max_tokens":8192,"system":system,"thinking":{"type":"adaptive"},"metadata":{"user_id":"do-not-forward"},"messages":[{"role":"user","content":"hello"}]}),
    );
    input.headers_mut().append(
        "anthropic-beta",
        "claude-code-20250219, extended-cache-ttl-2025-04-11, tool-search-2025-10-19"
            .parse()
            .unwrap(),
    );
    let response = app.oneshot(input).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let output: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(output["content"], native["content"]);
    assert_eq!(output["model"], "configured-name");
    assert_eq!(repository.starts.load(Ordering::SeqCst), 1);
    let captures = captures.lock().unwrap();
    let (headers, body) = &captures[0];
    assert_eq!(headers["x-api-key"], "server-held-provider-key");
    assert!(!headers.contains_key("authorization"));
    assert_eq!(
        headers
            .get_all("anthropic-beta")
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "interleaved-thinking-2025-05-14",
            "claude-code-20250219, extended-cache-ttl-2025-04-11, tool-search-2025-10-19"
        ]
    );
    assert_eq!(body["system"], system);
    assert_eq!(body["max_tokens"], 8192);
    assert!(body.get("metadata").is_none());
    assert!(
        !body
            .to_string()
            .contains("inference-key-must-not-leave-gateway")
    );
}

#[tokio::test]
async fn denied_betas_use_anthropic_retry_wording_before_admission() {
    let captures = Captures::default();
    let mock = upstream(200, json!({"id":"private","type":"message","role":"assistant","model":"private","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":1}}), captures.clone()).await;
    let adapter = crate::providers::anthropic::AnthropicAdapter::for_test(
        Arc::new(Resolver),
        mock.url.clone(),
    );
    let (app, repository) = frontend_adapter(Arc::new(adapter));
    for (values, denied) in [
        (vec!["context-1m-2025-08-07"], "context-1m-2025-08-07"),
        (vec!["CONTEXT-1M-2025-08-07"], "CONTEXT-1M-2025-08-07"),
        (
            vec!["claude-code-20250219, Web-Search-2025-03-05"],
            "Web-Search-2025-03-05",
        ),
        (
            vec![
                "claude-code-20250219",
                "effort-2025-11-24, context-1m-2025-08-07, files-api-2025-04-14",
            ],
            "context-1m-2025-08-07, files-api-2025-04-14",
        ),
    ] {
        let mut request =
            wire(json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":"hi"}]}));
        request.headers_mut().remove("anthropic-beta");
        for value in values {
            request
                .headers_mut()
                .append("anthropic-beta", value.parse().unwrap());
        }
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), 32768)
            .await
            .unwrap();
        let actual: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            actual,
            json!({"type":"error","error":{"type":"invalid_request_error","message":format!("Unexpected value(s) `{denied}` for the `anthropic-beta` header. Please consult our documentation at docs.anthropic.com or try again without the header.")}})
        );
        assert_eq!(repository.starts.load(Ordering::SeqCst), 0);
        assert!(captures.lock().unwrap().is_empty());
    }
    // Claude Code's capability recovery is a new request without rejected betas.
    let mut request =
        wire(json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":"hi"}]}));
    request.headers_mut().insert(
        "anthropic-beta",
        "claude-code-20250219, effort-2025-11-24".parse().unwrap(),
    );
    assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    assert_eq!(repository.starts.load(Ordering::SeqCst), 1);
    assert_eq!(captures.lock().unwrap().len(), 1);
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
async fn native_anthropic_unsupported_fields_do_not_admit_or_dispatch() {
    let captures = Captures::default();
    let mock = upstream(200, json!({}), captures.clone()).await;
    for (extra, field) in [
        (
            json!({"output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}}),
            "output_config.format",
        ),
        (
            json!({"tools":[{"name":"lookup","input_schema":{"type":"object"},"strict":true}]}),
            "strict",
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
