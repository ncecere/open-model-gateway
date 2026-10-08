use super::super::secrets::Secret;
use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use std::{
    convert::Infallible,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct Resolver {
    calls: AtomicUsize,
}
impl SecretResolver for Resolver {
    fn resolve(&self, reference: &str) -> Result<Secret> {
        assert_eq!(reference, "test-key-reference");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Secret::new("local-mock-key".into())
    }
}

struct Captured {
    headers: HeaderMap,
    uri: Uri,
    body: Value,
}
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
    async fn serve(
        status: u16,
        content_type: &'static str,
        chunks: Vec<Vec<u8>>,
        redirect: Option<String>,
    ) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let capture = requests.clone();
        let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri, body: Bytes| {
            let capture = capture.clone();
            let chunks = chunks.clone();
            let redirect = redirect.clone();
            async move {
                capture.lock().unwrap().push(Captured {
                    headers,
                    uri,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                });
                let mut response = Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, content_type);
                if let Some(location) = redirect {
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
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
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
        Self::serve(
            200,
            "application/json",
            vec![serde_json::to_vec(&value).unwrap()],
            None,
        )
        .await
    }

    async fn sse(bytes: Vec<u8>, chunk_size: usize) -> Self {
        Self::serve(
            200,
            "text/event-stream; charset=utf-8",
            bytes.chunks(chunk_size).map(Vec::from).collect(),
            None,
        )
        .await
    }

    fn adapter(&self) -> OpenAiAdapter {
        OpenAiAdapter::for_test(Arc::new(Resolver::default()), self.base.clone())
    }
}

fn target() -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openai".into(),
        upstream_model: "private-upstream-model".into(),
        credential_ref: "test-key-reference".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec![
            "chat_completions".into(),
            "responses".into(),
            "embeddings".into(),
        ],
    }
}

fn request(stream: bool) -> ChatRequest {
    ChatRequest {
        model: "public-model".into(),
        messages: vec![Message {
            role: Role::User,
            content: Some("Hello".into()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: None,
        stream,
    }
}

fn complete() -> Value {
    json!({"id":"private-id", "model":"private-model", "choices":[{"index":0, "message":{"role":"assistant", "content":"Hello 世界", "refusal": null}, "finish_reason":"stop"}]})
}

fn chunk(delta: Value, reason: Value) -> Value {
    json!({"id":"private-id", "model":"private-model", "choices":[{"index":0,"delta":delta,"finish_reason":reason}],"usage":null})
}

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn finished() -> String {
    event(chunk(json!({}), json!("stop")))
}

async fn stream_results(mock: &Mock) -> Vec<Result<ChatEvent>> {
    let output = mock
        .adapter()
        .execute(&target(), request(true))
        .await
        .unwrap();
    let ProviderOutput::Stream(stream) = output else {
        panic!("expected stream")
    };
    stream.collect().await
}

fn assert_error<T>(result: Result<T>, expected: InferenceError) {
    match result {
        Err(error) => assert_eq!(error, expected),
        Ok(_) => panic!("expected sanitized error"),
    }
}

#[tokio::test]
async fn satisfies_shared_adapter_contract_against_local_http() {
    let app = Router::new().fallback(|axum::Json(body): axum::Json<Value>| async move {
        if body["stream"] == true {
            assert_eq!(body["stream_options"]["include_usage"], true);
            let stream = format!(
                "{}{}{}data: [DONE]\n\n",
                event(chunk(json!({"content":"hello"}), Value::Null)),
                finished(),
                event(json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2}}))
            );
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from(stream))
                .unwrap()
        } else {
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(complete().to_string()))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock = Mock {
        base: format!("http://{}/v1", listener.local_addr().unwrap()),
        requests: Arc::new(Mutex::new(Vec::new())),
        task: tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    };
    crate::providers::contract::assert_text_chat_contract(
        &mock.adapter(),
        &target(),
        request(false),
    )
    .await;
}

#[tokio::test]
async fn translates_payload_and_owns_authentication() {
    let mock = Mock::json(complete()).await;
    let adapter = mock.adapter();
    assert_eq!(adapter.id(), "openai");
    let caps = adapter.capabilities();
    assert!(caps.text_chat && caps.streaming && caps.tools);
    let mut req = request(false);
    req.messages = vec![
        Message {
            role: Role::System,
            content: Some("system".into()),
            tool_calls: vec![],
            tool_call_id: None,
        },
        Message {
            role: Role::Developer,
            content: Some("developer".into()),
            tool_calls: vec![],
            tool_call_id: None,
        },
        req.messages[0].clone(),
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "weather".into(),
                arguments: "{ \"city\": \"NYC\" }".into(),
            }],
            tool_call_id: None,
        },
        Message {
            role: Role::Tool,
            content: Some("sunny".into()),
            tool_calls: vec![],
            tool_call_id: Some("call-1".into()),
        },
    ];
    req.tools = vec![FunctionTool {
        name: "weather".into(),
        description: Some("Get weather".into()),
        parameters: json!({"type":"object"}),
        strict: Some(true),
    }];
    req.tool_choice = Some(ToolChoice::Function("weather".into()));
    req.temperature = Some(0.25);
    req.max_output_tokens = Some(123);
    adapter.execute(&target(), req).await.unwrap();
    let captured = mock.requests.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let got = &captured[0];
    assert_eq!(got.uri.path(), "/v1/chat/completions");
    assert_eq!(got.headers[header::AUTHORIZATION], "Bearer local-mock-key");
    assert_eq!(
        got.body,
        json!({
            "model":"private-upstream-model", "stream":false, "temperature":0.25, "max_completion_tokens":123,
            "messages":[
                {"role":"system","content":"system"},
                {"role":"developer","content":"developer"},
                {"role":"user","content":"Hello"},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"weather","arguments":"{ \"city\": \"NYC\" }"}}]},
                {"role":"tool","content":"sunny","tool_call_id":"call-1"}
            ],
            "tools":[{"type":"function","function":{"name":"weather","description":"Get weather","parameters":{"type":"object"},"strict":true}}],
            "tool_choice":{"type":"function","function":{"name":"weather"}}
        })
    );
}

#[test]
fn tool_choice_variants_and_absent_options() {
    for (choice, expected) in [
        (ToolChoice::Auto, "auto"),
        (ToolChoice::None, "none"),
        (ToolChoice::Required, "required"),
    ] {
        let mut req = request(false);
        req.tool_choice = Some(choice);
        let value = encode("upstream", &req);
        assert_eq!(value["tool_choice"], expected);
        for omitted in [
            "stream_options",
            "temperature",
            "tools",
            "max_tokens",
            "max_completion_tokens",
        ] {
            assert!(value.get(omitted).is_none());
        }
    }
}

#[tokio::test]
async fn nonstream_tools_usage_and_unknown_usage() {
    let mut value = complete();
    value["choices"][0]["message"]["tool_calls"] = json!([{"id":"call-2","type":"function","function":{"name":"weather","arguments":"{  \"city\":\"東京\" }"}}]);
    value["choices"][0]["finish_reason"] = json!("tool_calls");
    value["usage"] = json!({"prompt_tokens": 12,"completion_tokens":7,"total_tokens":19,"prompt_tokens_details":{"cached_tokens":3}});
    let mock = Mock::json(value).await;
    let ProviderOutput::Complete(response) = mock
        .adapter()
        .execute(&target(), request(false))
        .await
        .unwrap()
    else {
        panic!("expected complete")
    };
    assert_eq!(response.content.as_deref(), Some("Hello 世界"));
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: Some(12),
            output_tokens: Some(7),
            billing: usage(&json!({"prompt_tokens":12,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":3}})).unwrap().billing, ..Default::default() }
    );
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "call-2");
    assert_eq!(response.tool_calls[0].name, "weather");
    assert_eq!(response.tool_calls[0].arguments, "{  \"city\":\"東京\" }");
    let mock = Mock::json(complete()).await;
    let ProviderOutput::Complete(response) = mock
        .adapter()
        .execute(&target(), request(false))
        .await
        .unwrap()
    else {
        panic!("expected complete")
    };
    assert_eq!(response.usage, Usage::default());
    assert_eq!(
        usage(&json!({"completion_tokens":0})).unwrap(),
        Usage {
            input_tokens: None,
            output_tokens: Some(0),
            billing: Some(crate::billing::BillingUsage {
                cache_write_5m_input_tokens: Some(0),
                cache_write_1h_input_tokens: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        }
    );
}

#[tokio::test]
async fn nonstream_rejects_lossy_or_malformed_responses() {
    let mut cases = Vec::new();
    for field in [
        "refusal",
        "audio",
        "reasoning_content",
        "function_call",
        "future_modality",
    ] {
        let mut value = complete();
        value["choices"][0]["message"][field] = json!("not safe to drop");
        cases.push(value);
    }
    for content in [json!([{"type":"text","text":"not canonical"}]), json!(42)] {
        let mut value = complete();
        value["choices"][0]["message"]["content"] = content;
        cases.push(value);
    }
    for reason in [json!(null), json!("future"), json!("function_call")] {
        let mut value = complete();
        value["choices"][0]["finish_reason"] = reason;
        cases.push(value);
    }
    let mut value = complete();
    value["choices"][0]["message"]["role"] = json!("user");
    cases.push(value);
    let mut value = complete();
    value["choices"][0]["index"] = json!(1);
    cases.push(value);
    let mut value = complete();
    let extra = value["choices"][0].clone();
    value["choices"].as_array_mut().unwrap().push(extra);
    cases.push(value);
    let mut value = complete();
    value["choices"] = json!([]);
    cases.push(value);
    let mut value = complete();
    value["usage"] = json!({"prompt_tokens":-1});
    cases.push(value);
    let mut value = complete();
    value["choices"][0]["message"]["tool_calls"] = Value::Array((0..=MAX_TOOL_CALLS).map(|i| json!({"id":format!("call-{i}"),"type":"function","function":{"name":"f","arguments":"{}"}})).collect());
    cases.push(value);
    for value in cases {
        let mock = Mock::json(value).await;
        assert_error(
            mock.adapter().execute(&target(), request(false)).await,
            InferenceError::InvalidUpstream,
        );
    }
}

#[tokio::test]
async fn chunked_sse_preserves_unicode_tools_usage_and_done() {
    let mut text = String::from("\u{feff}: comment\r\nid: ignored\r\n\r\n");
    text.push_str(
        &event(chunk(
            json!({"role":"assistant","content":"Hé世界"}),
            Value::Null,
        ))
        .replace('\n', "\r\n"),
    );
    text.push_str(&event(chunk(json!({"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"weather","arguments":"{ \"city\":"}}]}), Value::Null)));
    text.push_str(&event(chunk(
        json!({"tool_calls":[{"index":0,"function":{"arguments":"\"東京\" }"}}]}),
        Value::Null,
    )));
    text.push_str(&event(chunk(json!({}), json!("tool_calls"))));
    // A JSON event split across data fields is joined with a newline.
    text.push_str("data: {\"choices\": [],\r\ndata: \"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4}}\r\n\r\n");
    text.push_str(": keepalive\n\ndata: [DONE]\r\n\r\n");
    for size in [1, 2, 7, 31, 4096] {
        let mock = Mock::sse(text.as_bytes().to_vec(), size).await;
        let events: Vec<_> = stream_results(&mock)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(events.len(), 6);
        assert!(
            matches!(&events[0], ChatEvent::Delta { text: Some(text), tool_calls } if text == "Hé世界" && tool_calls.is_empty())
        );
        let ChatEvent::Delta {
            text: None,
            tool_calls,
        } = &events[1]
        else {
            panic!("expected tool delta")
        };
        assert_eq!(tool_calls[0].index, 0);
        assert_eq!(tool_calls[0].id.as_deref(), Some("call-1"));
        assert_eq!(tool_calls[0].name.as_deref(), Some("weather"));
        assert_eq!(tool_calls[0].arguments.as_deref(), Some("{ \"city\":"));
        let ChatEvent::Delta { tool_calls, .. } = &events[2] else {
            panic!("expected tool delta")
        };
        assert_eq!(tool_calls[0].arguments.as_deref(), Some("\"東京\" }"));
        assert!(tool_calls[0].id.is_none() && tool_calls[0].name.is_none());
        assert!(matches!(
            events[3],
            ChatEvent::Finish(FinishReason::ToolCalls)
        ));
        assert!(matches!(
            events[4],
            ChatEvent::Usage(Usage {
                input_tokens: Some(9),
                output_tokens: Some(4),
                ..
            })
        ));
        assert!(matches!(events[5], ChatEvent::Done));
        let captured = mock.requests.lock().unwrap();
        assert_eq!(
            captured[0].body["stream_options"],
            json!({"include_usage":true})
        );
    }
}

#[tokio::test]
async fn malformed_truncated_and_out_of_order_streams_fail_closed() {
    let cases = vec![
        Vec::new(),
        b"data: {bad json}\n\n".to_vec(),
        b"data: \xff\n\n".to_vec(),
        b"data: [DONE]\n\n".to_vec(),
        finished().into_bytes(),
        format!("{}data: [DONE]", finished()).into_bytes(),
        format!("{}data: [DONE]\n", finished()).into_bytes(),
        format!("{}{}data: [DONE]\n\n", finished(), finished()).into_bytes(),
        format!(
            "{}{}",
            finished(),
            event(chunk(json!({"content":"late"}), Value::Null))
        )
        .into_bytes(),
        event(chunk(json!({"audio":{"data":"unsupported"}}), Value::Null)).into_bytes(),
        event(chunk(json!({"refusal":"refused"}), Value::Null)).into_bytes(),
        event(chunk(
            json!({"reasoning_content":"must not drop"}),
            Value::Null,
        ))
        .into_bytes(),
        event(chunk(json!({"role":"user"}), Value::Null)).into_bytes(),
        event(json!({"choices":[]})).into_bytes(),
        event(json!({"choices":[{"index":1,"delta":{},"finish_reason":"stop"}]})).into_bytes(),
        event(json!({"choices":[{"index":0,"delta":{}},{"index":1,"delta":{}}]})).into_bytes(),
        event(chunk(json!({"tool_calls":[{"index":-1}]}), Value::Null)).into_bytes(),
        event(chunk(
            json!({"tool_calls":[{"index":u32::MAX}]}),
            Value::Null,
        ))
        .into_bytes(),
        event(chunk(
            json!({"tool_calls":[{"index":0,"function":{"arguments":{}}}]}),
            Value::Null,
        ))
        .into_bytes(),
        event(chunk(json!({}), json!("future_reason"))).into_bytes(),
    ];
    for bytes in cases {
        let mock = Mock::sse(bytes, 7).await;
        let results = stream_results(&mock).await;
        assert!(matches!(
            results.last(),
            Some(Err(InferenceError::InvalidUpstream))
        ));
        assert!(!results.iter().any(|v| matches!(v, Ok(ChatEvent::Done))));
    }
}

#[tokio::test]
async fn nonstream_and_sse_bodies_are_bounded() {
    // Valid JSON padded with legal whitespace proves the cap, not a JSON error.
    let mut boundary = serde_json::to_vec(&complete()).unwrap();
    boundary.resize(BODY_LIMIT, b' ');
    let mock = Mock::serve(200, "application/json", vec![boundary.clone()], None).await;
    assert!(
        mock.adapter()
            .execute(&target(), request(false))
            .await
            .is_ok()
    );
    let mock = Mock::serve(200, "application/json", vec![boundary, vec![b' ']], None).await;
    assert_error(
        mock.adapter().execute(&target(), request(false)).await,
        InferenceError::InvalidUpstream,
    );
    for payload in [
        format!("data: {}", "x".repeat(SSE_LIMIT)).into_bytes(),
        format!(
            "{}\n{}data: [DONE]\n\n",
            ": many bounded comment lines\n".repeat(SSE_LIMIT / 20),
            finished()
        )
        .into_bytes(),
        format!("{}\n", "data: x\n".repeat(SSE_LIMIT / 7)).into_bytes(),
    ] {
        let mock = Mock::sse(payload, 8192).await;
        let results = stream_results(&mock).await;
        assert!(matches!(
            results.last(),
            Some(Err(InferenceError::InvalidUpstream))
        ));
    }
    // The limit is per-event, not a limit on the full lifetime of a stream.
    let text = format!(
        "{}{}data: [DONE]\n\n",
        ": comment\n\n".repeat(SSE_LIMIT / 10 + 1),
        finished()
    );
    let mock = Mock::sse(text.into_bytes(), 8192).await;
    let results = stream_results(&mock).await;
    assert_eq!(results.len(), 2);
    assert!(matches!(results.last(), Some(Ok(ChatEvent::Done))));
}

#[tokio::test]
async fn statuses_are_sanitized_and_never_retried() {
    for (status, expected) in [
        (400, InferenceError::UpstreamRejected),
        (401, InferenceError::Configuration),
        (403, InferenceError::Configuration),
        (404, InferenceError::UpstreamRejected),
        (408, InferenceError::Timeout),
        (422, InferenceError::UpstreamRejected),
        (429, InferenceError::Busy),
        (500, InferenceError::UpstreamUnavailable),
        (502, InferenceError::UpstreamUnavailable),
        (503, InferenceError::UpstreamUnavailable),
        (504, InferenceError::UpstreamUnavailable),
    ] {
        let mock = Mock::serve(
            status,
            "application/json",
            vec![b"private upstream body, key, prompt, URL".to_vec()],
            None,
        )
        .await;
        assert_error(
            mock.adapter().execute(&target(), request(false)).await,
            expected,
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn redirects_are_not_followed_or_given_credentials() {
    let destination = Mock::json(complete()).await;
    for status in [301, 302, 303, 307, 308] {
        let mock = Mock::serve(
            status,
            "application/json",
            vec![],
            Some(format!("{}/chat/completions", destination.base)),
        )
        .await;
        assert_error(
            mock.adapter().execute(&target(), request(false)).await,
            InferenceError::Configuration,
        );
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        assert!(destination.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn endpoint_and_region_rejected_before_credentials_or_network() {
    let resolver = Arc::new(Resolver::default());
    let adapter = OpenAiAdapter::new(resolver.clone()).unwrap();
    for endpoint in [
        "http://127.0.0.1:1/v1",
        "http://api.openai.com/v1",
        "https://api.openai.com/v1//",
        "https://api.openai.com/v1?x=1",
        "https://api.openai.com/v1#fragment",
        "https://api.openai.com.evil/v1",
        "https://api.openai.com/v1/other",
        "",
    ] {
        let mut target = target();
        target.endpoint = Some(endpoint.into());
        assert_error(
            adapter.execute(&target, request(false)).await,
            InferenceError::Configuration,
        );
    }
    for region in ["us-east-1", " "] {
        let mut target = target();
        target.region = Some(region.into());
        assert_error(
            adapter.execute(&target, request(false)).await,
            InferenceError::Configuration,
        );
    }
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    let mock = Mock::json(complete()).await;
    for endpoint in [None, Some(BASE.into()), Some(format!("{BASE}/"))] {
        let mut target = target();
        target.endpoint = endpoint;
        target.region = Some(String::new());
        mock.adapter()
            .execute(&target, request(false))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn invalid_nonstream_json_and_stream_content_type_are_rejected() {
    for bytes in [b"not json".to_vec(), b"\xff".to_vec(), b"{}".to_vec()] {
        let mock = Mock::serve(200, "application/json", vec![bytes], None).await;
        assert_error(
            mock.adapter().execute(&target(), request(false)).await,
            InferenceError::InvalidUpstream,
        );
    }
    let mock = Mock::json(complete()).await;
    assert_error(
        mock.adapter().execute(&target(), request(true)).await,
        InferenceError::InvalidUpstream,
    );
}

#[tokio::test]
async fn dropping_stream_or_pending_completion_cancels_the_response_body() {
    struct DropNotice(Arc<tokio::sync::Notify>);
    impl Drop for DropNotice {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    for streaming in [true, false] {
        let dropped = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let notify_drop = dropped.clone();
        let notify_start = started.clone();
        let app = Router::new().fallback(move || {
            let guard = DropNotice(notify_drop.clone());
            let notify_start = notify_start.clone();
            async move {
                let stream = async_stream::stream! {
                    let _guard = guard;
                    notify_start.notify_one();
                    yield Ok::<Bytes, Infallible>(Bytes::from(event(chunk(json!({"content":"start"}), Value::Null))));
                    std::future::pending::<()>().await;
                };
                Response::builder().header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from_stream(stream)).unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = Mock {
            base,
            requests: Arc::new(Mutex::new(Vec::new())),
            task: tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }),
        };
        let adapter = server.adapter();
        let deployment = target();
        if streaming {
            let ProviderOutput::Stream(mut stream) =
                adapter.execute(&deployment, request(true)).await.unwrap()
            else {
                panic!("expected stream")
            };
            assert!(matches!(
                stream.next().await,
                Some(Ok(ChatEvent::Delta { .. }))
            ));
            drop(stream);
        } else {
            let mut future = Box::pin(adapter.execute(&deployment, request(false)));
            tokio::select! {
                _ = &mut future => panic!("body should remain pending"),
                _ = started.notified() => {},
                _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("request did not start"),
            }
            drop(future);
        }
        tokio::time::timeout(Duration::from_secs(3), dropped.notified())
            .await
            .expect("dropping the consumer must release the upstream response");
    }
}

#[test]
fn sse_line_and_event_accumulation_fail_at_the_limit() {
    let mut decoder = SseDecoder::new();
    for _ in 0..SSE_LIMIT {
        assert!(decoder.push(b'x').unwrap().is_none());
    }
    assert_error(decoder.push(b'x'), InferenceError::InvalidUpstream);
    let mut decoder = SseDecoder::new();
    // Repeated short data lines must not evade the per-event limit.
    for byte in b"data: x\n".iter().cycle().take(SSE_LIMIT) {
        assert!(decoder.push(*byte).unwrap().is_none());
    }
    assert_error(decoder.push(b'x'), InferenceError::InvalidUpstream);
}

#[test]
fn supported_finish_reasons_and_sse_cr_framing() {
    for (wire, expected) in [
        ("stop", FinishReason::Stop),
        ("length", FinishReason::Length),
        ("tool_calls", FinishReason::ToolCalls),
        ("content_filter", FinishReason::ContentFilter),
    ] {
        let mut value = complete();
        value["choices"][0]["finish_reason"] = json!(wire);
        assert_eq!(decode_complete(&value).unwrap().finish_reason, expected);
    }
    let mut decoder = SseDecoder::new();
    let mut output = Vec::new();
    for &byte in b":comment\rid: 123\rdata: first\rdata:second\r\rdata\r\r" {
        if let Some(data) = decoder.push(byte).unwrap() {
            output.push(data);
        }
    }
    assert_eq!(output, vec![b"first\nsecond".to_vec(), vec![]]);
}

#[tokio::test]
async fn cloud_embeddings_native_path_auth_payload_and_numeric_usage() {
    let mock = Mock::json(json!({"object":"list","model":"private","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],"usage":{"prompt_tokens":4,"total_tokens":4}})).await;
    let adapter = mock.adapter();
    let request = EmbeddingRequest {
        model: "public".into(),
        input: vec!["hello".into()],
        dimensions: Some(2),
    };
    let response = adapter
        .execute_embeddings(&target(), request.clone())
        .await
        .unwrap();
    assert_eq!(response.embeddings[0].len(), 2);
    assert_eq!(response.usage.input_tokens, Some(4));
    assert_eq!(response.usage.output_tokens, Some(0));
    assert!(response.usage.billing.unwrap().is_complete());
    {
        let captured = mock.requests.lock().unwrap();
        assert_eq!(captured[0].uri.path(), "/v1/embeddings");
        assert_eq!(
            captured[0].headers["authorization"],
            "Bearer local-mock-key"
        );
        assert_eq!(captured[0].body["encoding_format"], "float");
        assert_eq!(captured[0].body["dimensions"], 2);
        assert_eq!(captured[0].body["model"], "private-upstream-model");
    }
    let mut target = target();
    target.credential_ref = "none".into();
    assert!(matches!(
        adapter.execute_embeddings(&target, request).await,
        Err(InferenceError::Configuration)
    ));
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn invalid_chat_body_preserves_validated_usage_as_evidence() {
    use crate::inference::evidence::capture;
    let bad = serde_json::json!({"choices":[{"index":0,"message":{"role":"assistant","content":"hi","audio":{"id":"x"}},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":9,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":0}}});
    let (result, observed) = capture(async { decode_complete(&bad) }).await;
    assert_eq!(result.err(), Some(InferenceError::InvalidUpstream));
    let observed = observed.unwrap();
    assert_eq!(
        (observed.input_tokens, observed.output_tokens),
        (Some(9), Some(5))
    );
    let (result, observed) = capture(async { decode_complete_profile(&bad, Some("vllm")) }).await;
    assert!(result.is_err());
    assert_eq!(observed.unwrap().input_tokens, Some(9));
}
