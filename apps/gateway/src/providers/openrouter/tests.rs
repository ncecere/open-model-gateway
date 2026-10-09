use super::super::secrets::Secret;
use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

const USER_ID: &str = "user_2xSecretAccountIdentifier";

#[derive(Default)]
struct Resolver {
    calls: AtomicUsize,
}
impl SecretResolver for Resolver {
    fn resolve(&self, reference: &str) -> Result<Secret> {
        assert_eq!(reference, "env:OPENROUTER_TEST_KEY");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Secret::new("local-mock-openrouter-key".into())
    }
}

struct Captured {
    headers: HeaderMap,
    uri: Uri,
    body: Value,
}
type Reply = (u16, &'static str, Vec<Vec<u8>>, Option<String>);
struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn reply(f: impl Fn(&Value) -> Reply + Clone + Send + Sync + 'static) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let capture = requests.clone();
        let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri, body: Bytes| {
            let capture = capture.clone();
            let f = f.clone();
            async move {
                let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let (status, content_type, chunks, location) = f(&body);
                capture
                    .lock()
                    .unwrap()
                    .push(Captured { headers, uri, body });
                let mut response = Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, content_type);
                if let Some(location) = location {
                    response = response.header(header::LOCATION, location);
                }
                let stream = futures_util::stream::iter(
                    chunks
                        .into_iter()
                        .map(|v| Ok::<_, Infallible>(Bytes::from(v))),
                );
                response.body(Body::from_stream(stream)).unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            requests,
            task,
        }
    }
    async fn json(value: Value) -> Self {
        let bytes = serde_json::to_vec(&value).unwrap();
        Self::reply(move |_| (200, "application/json", vec![bytes.clone()], None)).await
    }
    async fn raw(status: u16, body: &'static str) -> Self {
        Self::reply(move |_| {
            (
                status,
                "application/json",
                vec![body.as_bytes().to_vec()],
                None,
            )
        })
        .await
    }
    async fn sse(text: String, chunk: usize) -> Self {
        let chunks: Vec<Vec<u8>> = text.as_bytes().chunks(chunk).map(Vec::from).collect();
        Self::reply(move |_| (200, "text/event-stream", chunks.clone(), None)).await
    }
    fn adapter(&self) -> OpenRouterAdapter {
        self.adapter_with(OpenRouterConfig::default())
    }
    fn adapter_with(&self, config: OpenRouterConfig) -> OpenRouterAdapter {
        OpenRouterAdapter::for_test(Arc::new(Resolver::default()), config, self.base.clone())
    }
    fn last(&self) -> (HeaderMap, String, Value) {
        let requests = self.requests.lock().unwrap();
        let c = requests.last().unwrap();
        (c.headers.clone(), c.uri.path().to_owned(), c.body.clone())
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

fn target(model: &str) -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openrouter".into(),
        upstream_model: model.into(),
        credential_ref: "env:OPENROUTER_TEST_KEY".into(),
        endpoint: Some(BASE.into()),
        region: None,
        supported_protocols: vec![],
    }
}
fn chat(stream: bool) -> ChatRequest {
    ChatRequest {
        model: "company/glm".into(),
        messages: vec![Message {
            role: Role::User,
            content: Some("Do not log this prompt".into()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: Some(16),
        stream,
    }
}
fn glm_usage() -> Value {
    json!({"prompt_tokens":14,"completion_tokens":16,"total_tokens":30,"cost":1.01e-05,"is_byok":false,
        "prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0,"audio_tokens":0,"video_tokens":0},
        "cost_details":{"upstream_inference_cost":1.01e-05,"upstream_inference_prompt_cost":2.1e-06,"upstream_inference_completions_cost":8e-06},
        "completion_tokens_details":{"reasoning_tokens":16,"image_tokens":0,"audio_tokens":0}})
}
fn glm_complete() -> Value {
    json!({"id":"gen-1-abc","object":"chat.completion","created":0,"model":"z-ai/glm-5.3-flash-20260301","provider":"Parasail",
        "system_fingerprint":null,"service_tier":null,
        "choices":[{"index":0,"logprobs":null,"finish_reason":"length","native_finish_reason":"length",
            "message":{"role":"assistant","content":null,"refusal":null,"reasoning":"thinking",
                "reasoning_details":[{"type":"reasoning.text","text":"thinking","format":"unknown","index":0}]}}],
        "usage": glm_usage()})
}
fn chunk(delta: Value, finish: Option<&str>, usage: Option<Value>) -> String {
    let mut v = json!({"id":"gen-1","object":"chat.completion.chunk","created":0,"model":"z-ai/glm-5.3-flash-20260301","provider":"Parasail",
        "choices":[{"index":0,"delta":delta,"finish_reason":finish,"native_finish_reason":finish}]});
    if let Some(u) = usage {
        v["usage"] = u;
    }
    format!("data: {v}\n\n")
}
fn glm_stream() -> String {
    [
        ": OPENROUTER PROCESSING\n\n".to_owned(),
        chunk(
            json!({"content":"","role":"assistant","reasoning":"plan","reasoning_details":[{"type":"reasoning.text","text":"plan","format":"unknown","index":0}]}),
            None,
            None,
        ),
        chunk(json!({"content":"Hé 世界","role":"assistant"}), None, None),
        chunk(
            json!({"content":"","role":"assistant","reasoning":null}),
            Some("length"),
            None,
        ),
        // OpenRouter's accounting frame repeats the finish choice.
        chunk(
            json!({"content":"","role":"assistant"}),
            Some("length"),
            Some(glm_usage()),
        ),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat()
}
async fn collect(adapter: &OpenRouterAdapter, request: ChatRequest) -> Vec<Result<ChatEvent>> {
    let ProviderOutput::Stream(mut stream) = adapter
        .execute(&target("z-ai/glm-5.3-flash"), request)
        .await
        .unwrap()
    else {
        panic!("expected stream")
    };
    let mut events = vec![];
    while let Some(e) = stream.next().await {
        let err = e.is_err();
        events.push(e);
        if err {
            break;
        }
    }
    events
}

#[tokio::test]
async fn chat_complete_normalizes_reasoning_usage_and_exact_cost() {
    let mock = Mock::json(glm_complete()).await;
    let config = OpenRouterConfig::new(
        Some("https://gateway.example.test"),
        Some("Open Model Gateway"),
        DataCollection::Deny,
    )
    .unwrap();
    let ProviderOutput::Complete(response) = mock
        .adapter_with(config)
        .execute(&target("z-ai/glm-5.3-flash"), chat(false))
        .await
        .unwrap()
    else {
        panic!("expected complete")
    };
    assert_eq!(response.content, None);
    assert_eq!(response.finish_reason, FinishReason::Length);
    let usage = response.usage;
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(14), Some(16))
    );
    let billing = usage.billing.unwrap();
    assert_eq!(
        (
            billing.total_input_tokens,
            billing.uncached_input_tokens,
            billing.cache_read_input_tokens,
            billing.cache_write_default_input_tokens
        ),
        (Some(14), Some(14), Some(0), Some(0))
    );
    // 1.01e-05 USD = 10.1 µUSD, rounded up; evidence only.
    assert_eq!(usage.provider_cost_microusd, Some(11));
    let (headers, path, body) = mock.last();
    assert_eq!(path, "/api/v1/chat/completions");
    assert_eq!(headers["authorization"], "Bearer local-mock-openrouter-key");
    assert_eq!(headers["http-referer"], "https://gateway.example.test");
    assert_eq!(headers["x-title"], "Open Model Gateway");
    assert_eq!(body["model"], "z-ai/glm-5.3-flash");
    assert_eq!(body["provider"], json!({"data_collection":"deny"}));
    assert_eq!(body["usage"], json!({"include":true}));
    assert_eq!(body["max_completion_tokens"], 16);
    assert!(body.get("max_tokens").is_none());
}

#[tokio::test]
async fn chat_cached_tokens_and_cache_writes_follow_research_normalization() {
    let mut value = glm_complete();
    value["choices"][0]["message"]["content"] = json!("ok");
    value["choices"][0]["finish_reason"] = json!("stop");
    value["usage"] = json!({"prompt_tokens":13208,"completion_tokens":4,"total_tokens":13212,"cost":0.00013507,
        "prompt_tokens_details":{"cached_tokens":13000,"cache_write_tokens":197}});
    let mock = Mock::json(value).await;
    let ProviderOutput::Complete(r) = mock
        .adapter()
        .execute(&target("anthropic/claude-haiku-5.5"), chat(false))
        .await
        .unwrap()
    else {
        panic!()
    };
    let b = r.usage.billing.unwrap();
    assert_eq!(b.total_input_tokens, Some(13208));
    assert_eq!(b.cache_read_input_tokens, Some(13000));
    // OpenRouter reports no TTL split; writes are the default category.
    assert_eq!(b.cache_write_default_input_tokens, Some(197));
    assert_eq!(b.uncached_input_tokens, Some(11));
    assert_eq!(r.usage.provider_cost_microusd, Some(136));
}

#[tokio::test]
async fn shared_text_chat_contract_and_fragmented_stream() {
    let complete = serde_json::to_vec(&glm_complete()).unwrap();
    let stream = glm_stream().into_bytes();
    let mock = Mock::reply(move |body| {
        if body["stream"] == true {
            (
                200,
                "text/event-stream",
                stream.chunks(3).map(Vec::from).collect(),
                None,
            )
        } else {
            (200, "application/json", vec![complete.clone()], None)
        }
    })
    .await;
    // OpenRouter reports the served (versioned) slug, distinct from the
    // configured route id, and `completion_tokens_details.reasoning_tokens`.
    super::super::contract::assert_text_chat_contract(
        &mock.adapter(),
        &target("z-ai/glm-5.3-flash"),
        chat(false),
        super::super::contract::Telemetry {
            reported_model: Some("z-ai/glm-5.3-flash-20260301"),
            reasoning_tokens: Some(16),
        },
    )
    .await;
    assert_eq!(
        mock.last().2["stream_options"],
        json!({"include_usage":true})
    );
    for size in [1, 2, 7, 64] {
        let mock = Mock::sse(glm_stream(), size).await;
        let events = collect(&mock.adapter(), chat(true)).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Ok(ChatEvent::Delta { text, .. }) => text.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hé 世界", "chunk {size}");
        let usage: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ok(ChatEvent::Usage(u)) => Some(*u),
                _ => None,
            })
            .collect();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].input_tokens, Some(14));
        assert_eq!(usage[0].provider_cost_microusd, Some(11));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Ok(ChatEvent::Finish(FinishReason::Length))))
                .count(),
            1
        );
        assert!(matches!(events.last(), Some(Ok(ChatEvent::Done))));
    }
}

#[tokio::test]
async fn stream_errors_eof_and_unknown_content_fail_without_success() {
    let error_frame = format!(
        "{}data: {}\n\n",
        chunk(json!({"content":"partial"}), None, None),
        json!({"id":"gen-1","error":{"code":502,"message":"provider died","metadata":{"error_type":"provider_unavailable"}},"user_id":USER_ID,
            "choices":[{"index":0,"delta":{"content":""},"finish_reason":"error"}]})
    );
    let truncated = chunk(json!({"content":"hi"}), Some("stop"), Some(glm_usage()));
    let images = chunk(
        json!({"content":"hi","images":[{"type":"image_url"}]}),
        None,
        None,
    );
    let finish_error = chunk(json!({"content":""}), Some("error"), None);
    for (body, expected) in [
        (error_frame, InferenceError::UpstreamUnavailable),
        (truncated, InferenceError::InvalidUpstream),
        (images, InferenceError::InvalidUpstream),
        (finish_error, InferenceError::InvalidUpstream),
    ] {
        let mock = Mock::sse(body, 5).await;
        let events = collect(&mock.adapter(), chat(true)).await;
        assert!(!events.iter().any(|e| matches!(e, Ok(ChatEvent::Done))));
        let error = events.last().unwrap().as_ref().err().copied().unwrap();
        assert_eq!(error, expected);
        assert!(!format!("{error} {}", error.code()).contains(USER_ID));
    }
}

#[tokio::test]
async fn error_statuses_are_sanitized_and_bodies_never_read() {
    let body = r#"{"error":{"message":"nope/not-a-model is not a valid model ID","code":400},"user_id":"user_2xSecretAccountIdentifier"}"#;
    for (status, expected) in [
        (400, InferenceError::UpstreamRejected),
        (401, InferenceError::Configuration),
        (402, InferenceError::Configuration),
        (403, InferenceError::UpstreamRejected),
        (404, InferenceError::UpstreamRejected),
        (408, InferenceError::Timeout),
        (413, InferenceError::UpstreamRejected),
        (429, InferenceError::Busy),
        (500, InferenceError::UpstreamUnavailable),
        (502, InferenceError::UpstreamUnavailable),
        (503, InferenceError::UpstreamUnavailable),
        (524, InferenceError::Timeout),
        (529, InferenceError::UpstreamUnavailable),
    ] {
        let mock = Mock::raw(status, body).await;
        let adapter = mock.adapter();
        let t = target("m");
        let results = [
            adapter.execute(&t, chat(false)).await.err(),
            adapter.execute_rerank(&t, rerank_request(None)).await.err(),
            adapter
                .execute_systemone(&t, systemone_request())
                .await
                .err(),
        ];
        for error in results {
            let error = error.unwrap();
            assert_eq!(error, expected, "{status}");
            let rendered = format!("{error:?} {error} {}", error.code());
            assert!(!rendered.contains(USER_ID) && !rendered.contains("valid model ID"));
        }
    }
    // A 200 carrying only an error object fails by its code, never passes it through.
    let mock = Mock::raw(
        200,
        r#"{"error":{"code":402,"message":"Insufficient credits","metadata":{"limit_source":"openrouter_credits"}},"user_id":"user_2xSecretAccountIdentifier"}"#,
    )
    .await;
    assert_eq!(
        mock.adapter()
            .execute(&target("m"), chat(false))
            .await
            .err(),
        Some(InferenceError::Configuration)
    );
    // Non-JSON 200 (e.g. HTML from a wrong path) is invalid, not success.
    let mock = Mock::reply(|_| {
        (
            200,
            "text/html",
            vec![b"<html>models</html>".to_vec()],
            None,
        )
    })
    .await;
    assert_eq!(
        mock.adapter()
            .execute(&target("m"), chat(false))
            .await
            .err(),
        Some(InferenceError::InvalidUpstream)
    );
}

#[tokio::test]
async fn redirects_are_refused_and_never_followed() {
    let landing = Mock::json(glm_complete()).await;
    let location = format!("{}/chat/completions", landing.base);
    for status in [301, 302, 307, 308] {
        let location = location.clone();
        let mock =
            Mock::reply(move |_| (status, "application/json", vec![], Some(location.clone())))
                .await;
        assert_eq!(
            mock.adapter()
                .execute(&target("m"), chat(false))
                .await
                .err(),
            Some(InferenceError::Configuration)
        );
    }
    assert_eq!(landing.count(), 0);
}

#[tokio::test]
async fn oversize_bodies_are_rejected() {
    let big = vec![b' '; BODY_LIMIT + 1];
    let mock = Mock::reply(move |_| (200, "application/json", vec![big.clone()], None)).await;
    assert_eq!(
        mock.adapter()
            .execute(&target("m"), chat(false))
            .await
            .err(),
        Some(InferenceError::InvalidUpstream)
    );
    let frame = format!("data: {}\n\n", "x".repeat(1024 * 1024 + 1));
    let mock = Mock::sse(frame, 65536).await;
    let events = collect(&mock.adapter(), chat(true)).await;
    assert_eq!(
        events.last().unwrap().as_ref().err(),
        Some(&InferenceError::InvalidUpstream)
    );
}

#[tokio::test]
async fn connection_is_validated_before_credentials() {
    let mock = Mock::json(glm_complete()).await;
    let resolver = Arc::new(Resolver::default());
    let adapter = OpenRouterAdapter::for_test(
        resolver.clone(),
        OpenRouterConfig::default(),
        mock.base.clone(),
    );
    for mutate in [
        (|t: &mut Deployment| t.provider = "openai".into()) as fn(&mut Deployment),
        |t| t.credential_ref = "none".into(),
        |t| t.credential_ref = "aws:default".into(),
        |t| t.endpoint = Some("https://evil.example/api/v1".into()),
        |t| t.endpoint = Some("http://openrouter.ai/api/v1".into()),
        |t| t.region = Some("us-east-1".into()),
    ] {
        let mut t = target("m");
        mutate(&mut t);
        assert_eq!(
            adapter.execute(&t, chat(false)).await.err(),
            Some(InferenceError::Configuration)
        );
    }
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    assert_eq!(mock.count(), 0);
    let mut t = target("m");
    t.endpoint = None;
    assert!(adapter.execute(&t, chat(false)).await.is_ok());
}

#[test]
fn server_config_is_validated() {
    assert!(OpenRouterConfig::new(Some("ftp://x"), None, DataCollection::Deny).is_err());
    assert!(OpenRouterConfig::new(Some("https://a b"), None, DataCollection::Deny).is_err());
    assert!(OpenRouterConfig::new(None, Some(" padded"), DataCollection::Deny).is_err());
    assert!(OpenRouterConfig::new(None, Some("bad\nline"), DataCollection::Deny).is_err());
    assert!(OpenRouterConfig::new(None, Some(&"x".repeat(129)), DataCollection::Deny).is_err());
    assert!(
        OpenRouterConfig::new(
            Some("https://ok.test/app"),
            Some("OK"),
            DataCollection::Allow
        )
        .is_ok()
    );
}

#[tokio::test]
async fn data_collection_is_configurable_and_present_on_every_workload() {
    let mock = Mock::json(rerank_body()).await;
    let adapter =
        mock.adapter_with(OpenRouterConfig::new(None, None, DataCollection::Allow).unwrap());
    adapter
        .execute_rerank(
            &target("nvidia/llama-nemotron-rerank-vl-1b-v2:free"),
            rerank_request(Some(2)),
        )
        .await
        .unwrap();
    let (headers, _, body) = mock.last();
    assert_eq!(body["provider"], json!({"data_collection":"allow"}));
    assert!(headers.get("http-referer").is_none() && headers.get("x-title").is_none());
}

#[tokio::test]
async fn dropping_the_request_cancels_upstream() {
    use std::time::Duration;
    struct Guard(Arc<tokio::sync::Notify>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    for streaming in [false, true] {
        let dropped = Arc::new(tokio::sync::Notify::new());
        let began = Arc::new(tokio::sync::Notify::new());
        let (d, b) = (dropped.clone(), began.clone());
        let app = Router::new().fallback(move || {
            let guard = Guard(d.clone());
            let began = b.clone();
            async move {
                let stream = async_stream::stream! {
                    let _guard = guard;
                    began.notify_one();
                    yield Ok::<_, Infallible>(Bytes::from_static(b": OPENROUTER PROCESSING\n\n"));
                    std::future::pending::<()>().await;
                };
                Response::builder()
                    .header(
                        "content-type",
                        if streaming {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    )
                    .body(Body::from_stream(stream))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let adapter = OpenRouterAdapter::for_test(
            Arc::new(Resolver::default()),
            OpenRouterConfig::default(),
            base,
        );
        if streaming {
            let ProviderOutput::Stream(mut stream) =
                adapter.execute(&target("m"), chat(true)).await.unwrap()
            else {
                panic!()
            };
            tokio::select! {
                _ = stream.next() => panic!("stream unexpectedly yielded"),
                _ = began.notified() => {}
            }
            drop(stream);
        } else {
            let t = target("m");
            let mut future = Box::pin(adapter.execute_systemone(&t, systemone_request()));
            tokio::select! {
                _ = &mut future => panic!("request unexpectedly completed"),
                _ = began.notified() => {}
            }
            drop(future);
        }
        let cancelled = tokio::time::timeout(Duration::from_secs(3), dropped.notified()).await;
        task.abort();
        assert!(
            cancelled.is_ok(),
            "drop must cancel upstream (streaming={streaming})"
        );
    }
}

fn embed_request(dimensions: Option<u32>) -> EmbeddingRequest {
    EmbeddingRequest {
        model: "company/embed".into(),
        input: vec!["hello".into(), "world".into()],
        dimensions,
    }
}
fn embed_body(width: usize) -> Value {
    json!({"object":"list","data":[
            {"object":"embedding","embedding":vec![0.01; width],"index":0},
            {"object":"embedding","embedding":vec![0.02; width],"index":1}],
        "model":"private/openrouter/nvidia/nemotron-3-embed-1b",
        "usage":{"prompt_tokens":6,"total_tokens":6,"cost":0,"is_byok":false},
        "provider":"Nvidia","id":"gen-emb-1"})
}

#[tokio::test]
async fn nemotron_embeddings_have_fixed_2048_dimensions() {
    let mock = Mock::json(embed_body(2048)).await;
    let adapter = mock.adapter();
    let nemotron = target("nvidia/nemotron-3-embed-1b:free");
    assert!(adapter.supports_embedding_target(&nemotron, &embed_request(None)));
    assert!(adapter.supports_embedding_target(&nemotron, &embed_request(Some(2048))));
    assert!(!adapter.supports_embedding_target(&nemotron, &embed_request(Some(256))));
    assert!(!adapter.supports_embedding_target(
        &target("openai/text-embedding-3-small"),
        &embed_request(Some(256))
    ));
    assert_eq!(
        adapter
            .execute_embeddings(&nemotron, embed_request(Some(256)))
            .await
            .err(),
        Some(InferenceError::Unsupported)
    );
    assert_eq!(mock.count(), 0);
    let response = adapter
        .execute_embeddings(&nemotron, embed_request(None))
        .await
        .unwrap();
    // The reported `private/...` model differs from the slug; never compared.
    assert_eq!(response.embeddings.len(), 2);
    assert_eq!(response.embeddings[0].len(), 2048);
    assert_eq!(response.usage.input_tokens, Some(6));
    assert_eq!(response.usage.output_tokens, Some(0));
    assert_eq!(response.usage.provider_cost_microusd, Some(0));
    assert_eq!(response.usage.meters.unwrap().requests, Some(1));
    let (_, path, body) = mock.last();
    assert_eq!(path, "/api/v1/embeddings");
    assert_eq!(body["encoding_format"], "float");
    assert_eq!(body["provider"], json!({"data_collection":"deny"}));
    assert!(body.get("dimensions").is_none());
    let mock = Mock::json(embed_body(256)).await;
    assert_eq!(
        mock.adapter()
            .execute_embeddings(&nemotron, embed_request(None))
            .await
            .err(),
        Some(InferenceError::InvalidUpstream)
    );
}

fn rerank_request(top_n: Option<u32>) -> RerankRequest {
    RerankRequest {
        model: "company/rerank".into(),
        query: "cat".into(),
        documents: vec!["kitten".into(), "airplane".into(), "dog".into()],
        top_n,
    }
}
fn rerank_body() -> Value {
    json!({"model":"nvidia/llama-nemotron-rerank-vl-1b-v2",
        "results":[{"index":0,"relevance_score":0.038683497086407705,"document":{"text":"kitten"}},
                   {"index":2,"relevance_score":0.016785489890063697,"document":{"text":"dog"}}],
        "usage":{"total_tokens":25,"cost":0},"provider":"Nvidia","id":"gen-rerank-1"})
}

#[tokio::test]
async fn shared_rerank_and_systemone_contracts() {
    let mock = Mock::json(rerank_body()).await;
    crate::providers::contract::assert_rerank_contract(
        &mock.adapter(),
        &target("m"),
        rerank_request(Some(2)),
        Some(25),
        None,
    )
    .await;
    let mock = Mock::json(json!({"answers":{"is_q":{"type":"noul","noul":0.5}},
        "usage":{"input_tokens":145,"output_tokens":0}}))
    .await;
    crate::providers::contract::assert_systemone_contract(
        &mock.adapter(),
        &target("m"),
        systemone_request(),
        145,
        0,
    )
    .await;
}

#[tokio::test]
async fn rerank_wire_results_and_meters() {
    let mock = Mock::json(rerank_body()).await;
    let response = mock
        .adapter()
        .execute_rerank(
            &target("nvidia/llama-nemotron-rerank-vl-1b-v2:free"),
            rerank_request(Some(5)),
        )
        .await
        .unwrap();
    assert_eq!(
        response.results,
        vec![
            RerankResult {
                index: 0,
                relevance_score: 0.038683497086407705
            },
            RerankResult {
                index: 2,
                relevance_score: 0.016785489890063697
            }
        ]
    );
    let u = response.usage;
    assert_eq!((u.input_tokens, u.output_tokens), (Some(25), Some(0)));
    let m = u.meters.unwrap();
    // Search units are not reported by this provider: unknown, not zero.
    assert_eq!(
        (m.requests, m.search_units, m.output_images),
        (Some(1), None, Some(0))
    );
    assert_eq!(u.provider_cost_microusd, Some(0));
    let (_, path, body) = mock.last();
    assert_eq!(path, "/api/v1/rerank");
    assert_eq!(body["top_n"], 3, "top_n is clamped to the document count");
    assert_eq!(body["documents"], json!(["kitten", "airplane", "dog"]));
    // Cohere-style search units are observed when present.
    let mut cohere = rerank_body();
    cohere["usage"] = json!({"search_units":1,"cost":0.002});
    let mock = Mock::json(cohere).await;
    let u = mock
        .adapter()
        .execute_rerank(&target("cohere/rerank-4-fast"), rerank_request(None))
        .await
        .unwrap()
        .usage;
    assert_eq!(u.meters.unwrap().search_units, Some(1));
    assert_eq!(u.input_tokens, None);
    assert_eq!(u.provider_cost_microusd, Some(2000));
    // Invalid results fail but keep valid usage evidence.
    let mut bad = rerank_body();
    bad["results"][1]["index"] = json!(0);
    let mock = Mock::json(bad).await;
    let adapter = mock.adapter();
    let t = target("m");
    let (result, evidence) =
        crate::inference::evidence::capture(adapter.execute_rerank(&t, rerank_request(None))).await;
    assert_eq!(result.err(), Some(InferenceError::InvalidUpstream));
    assert_eq!(evidence.unwrap().input_tokens, Some(25));
    for mutate in [
        (|v: &mut Value| v["results"][0]["relevance_score"] = json!("high")) as fn(&mut Value),
        |v| v["results"][0]["index"] = json!(3),
        |v| v["results"][0]["extra"] = json!(1),
        |v| v["results"] = json!([]),
        |v| v["usage"]["total_tokens"] = json!(-1),
        |v| v["usage"]["cost"] = json!("0.1"),
    ] {
        let mut body = rerank_body();
        mutate(&mut body);
        let mock = Mock::json(body).await;
        assert_eq!(
            mock.adapter()
                .execute_rerank(&target("m"), rerank_request(None))
                .await
                .err(),
            Some(InferenceError::InvalidUpstream)
        );
    }
}

fn systemone_request() -> SystemoneRequest {
    SystemoneRequest {
        model: "company/clef".into(),
        state: json!("hello there"),
        questions: BTreeMap::from([(
            "is_q".to_owned(),
            Question {
                kind: QuestionKind::Noul,
                instructions: json!("Is the text a greeting?"),
                criteria: None,
            },
        )]),
    }
}

#[tokio::test]
async fn systemone_wire_answers_usage_and_cost() {
    let mock = Mock::json(json!({"model":"cloudflare/clef-flash","answers":{"is_q":{"type":"noul","noul":0.9566}},
        "usage":{"input_tokens":145,"output_tokens":0,"cost":3.045e-06},"id":"gen-dec-1","provider":"PrimeIntellect"}))
    .await;
    let response = mock
        .adapter()
        .execute_systemone(&target("cloudflare/clef-flash"), systemone_request())
        .await
        .unwrap();
    assert!(response.answers["is_q"] == Answer::Noul { noul: 0.9566 });
    let u = response.usage;
    assert_eq!((u.input_tokens, u.output_tokens), (Some(145), Some(0)));
    assert_eq!(u.billing.unwrap().uncached_input_tokens, Some(145));
    assert_eq!(u.meters.unwrap().requests, Some(1));
    assert_eq!(u.provider_cost_microusd, Some(4));
    let (_, path, body) = mock.last();
    assert_eq!(path, "/api/v1/systemone");
    assert_eq!(
        body,
        json!({"model":"cloudflare/clef-flash","state":"hello there",
            "questions":{"is_q":{"type":"noul","instructions":"Is the text a greeting?"}},
            "provider":{"data_collection":"deny"}})
    );
    // Jev reports free output tokens; they are observed, not dropped.
    let mock = Mock::json(
        json!({"model":"typesafe/jev-1.13-20260917","answers":{"is_q":{"type":"noul","noul":0.99}},
        "usage":{"input_tokens":274,"output_tokens":21,"cost":1.1508e-05}}),
    )
    .await;
    let u = mock
        .adapter()
        .execute_systemone(&target("jev-latest"), systemone_request())
        .await
        .unwrap()
        .usage;
    assert_eq!(
        (u.output_tokens, u.provider_cost_microusd),
        (Some(21), Some(12))
    );
    // Missing usage counters or invalid answers fail.
    for body in [
        json!({"answers":{"is_q":{"type":"noul","noul":0.5}},"usage":{"input_tokens":1}}),
        json!({"answers":{"is_q":{"type":"noul","noul":0.5}}}),
        json!({"answers":{"is_q":{"type":"choice","choice":"a"}},"usage":{"input_tokens":1,"output_tokens":0}}),
        json!({"answers":{},"usage":{"input_tokens":1,"output_tokens":0}}),
    ] {
        let mock = Mock::json(body).await;
        assert_eq!(
            mock.adapter()
                .execute_systemone(&target("m"), systemone_request())
                .await
                .err(),
            Some(InferenceError::InvalidUpstream)
        );
    }
}

#[tokio::test]
async fn provider_cost_is_parsed_from_source_text_not_f64() {
    // f64 would round 1.0000000000000001e-6 USD down to exactly 1 µUSD.
    let body = r#"{"model":"m","answers":{"is_q":{"type":"noul","noul":0.5}},"usage":{"input_tokens":1,"output_tokens":0,"cost":0.0000010000000000000001}}"#;
    let mock = Mock::raw(200, body).await;
    let u = mock
        .adapter()
        .execute_systemone(&target("m"), systemone_request())
        .await
        .unwrap()
        .usage;
    assert_eq!(u.provider_cost_microusd, Some(2));
    let body = r#"{"model":"m","answers":{"is_q":{"type":"noul","noul":0.5}},"usage":{"input_tokens":1,"output_tokens":0,"cost":null}}"#;
    let mock = Mock::raw(200, body).await;
    let u = mock
        .adapter()
        .execute_systemone(&target("m"), systemone_request())
        .await
        .unwrap()
        .usage;
    assert_eq!(u.provider_cost_microusd, None);
    let body = r#"{"model":"m","answers":{"is_q":{"type":"noul","noul":0.5}},"usage":{"input_tokens":1,"output_tokens":0,"cost":-0.5}}"#;
    let mock = Mock::raw(200, body).await;
    assert_eq!(
        mock.adapter()
            .execute_systemone(&target("m"), systemone_request())
            .await
            .err(),
        Some(InferenceError::InvalidUpstream)
    );
}

#[test]
fn registry_protocols() {
    let adapter =
        OpenRouterAdapter::new(Arc::new(Resolver::default()), OpenRouterConfig::default()).unwrap();
    assert_eq!(adapter.id(), "openrouter");
    for p in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Embeddings,
        ApiProtocol::Rerank,
        ApiProtocol::Systemone,
        ApiProtocol::Images,
        ApiProtocol::AudioSpeech,
        ApiProtocol::AudioTranscriptions,
    ] {
        assert!(adapter.supports_protocol(p));
    }
    for p in [ApiProtocol::Responses, ApiProtocol::Messages] {
        assert!(!adapter.supports_protocol(p));
    }
    let mut registry = crate::providers::ProviderRegistry::default();
    registry.register(Arc::new(adapter)).unwrap();
    assert!(registry.get("openrouter").is_some());
}

#[test]
fn management_profile_and_adapter_share_the_fixed_origin() {
    assert_eq!(crate::management::resources::OPENROUTER_BASE, BASE);
}
