use super::*;
#[path = "auth_tests.rs"]
mod auth_tests;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use futures_util::StreamExt;
use serde_json::json;
use std::{
    convert::Infallible,
    sync::{Arc, Mutex as StdMutex},
};

struct Captured {
    headers: HeaderMap,
    uri: Uri,
    body: Value,
}
struct Mock {
    base: String,
    requests: Arc<StdMutex<Vec<Captured>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn serve(complete: Value, frames: Vec<u8>, status: u16) -> Self {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let capture = requests.clone();
        let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri, bytes: Bytes| {
            let capture = capture.clone();
            let complete = complete.clone();
            let frames = frames.clone();
            async move {
                let stream = uri.path().ends_with("converse-stream");
                capture.lock().unwrap().push(Captured {
                    headers,
                    uri,
                    body: serde_json::from_slice(&bytes).unwrap(),
                });
                let data = if stream {
                    frames
                } else {
                    serde_json::to_vec(&complete).unwrap()
                };
                // Arbitrary fragmentation exercises AWS binary decoder across frame/header/CRC boundaries.
                let chunk_size = if data.len() > 4096 { 1024 } else { 7 };
                let chunks: Vec<_> = data
                    .chunks(chunk_size)
                    .map(|b| Ok::<_, Infallible>(Bytes::copy_from_slice(b)))
                    .collect();
                Response::builder()
                    .status(status)
                    .header(
                        "content-type",
                        if stream {
                            "application/vnd.amazon.eventstream"
                        } else {
                            "application/json"
                        },
                    )
                    .body(Body::from_stream(futures_util::stream::iter(chunks)))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base,
            requests,
            task,
        }
    }
    fn adapter(&self) -> BedrockAdapter {
        // Only test code can inject an endpoint or credentials. The normal constructor has
        // no endpoint argument and never inherits shared endpoint configuration.
        let mut adapter = BedrockAdapter::new().unwrap();
        let config = aws_sdk_bedrockruntime::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(aws_sdk_bedrockruntime::config::Credentials::new(
                "AKIDLOCALTEST",
                "local-not-a-secret",
                Some("local-session-token".into()),
                None,
                "local-test",
            ))
            .endpoint_url(&self.base)
            .http_client(adapter.http.clone())
            .retry_config(RetryConfig::standard().with_max_attempts(1))
            .build();
        adapter.test_client = Some(Client::from_conf(config));
        adapter
    }
}
fn target() -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "bedrock".into(),
        upstream_model: "test-model".into(),
        credential_ref: "aws:default".into(),
        endpoint: None,
        region: Some("us-east-1".into()),
        supported_protocols: vec!["chat_completions".into(), "messages".into()],
    }
}
fn message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: Some(text.into()),
        tool_calls: Vec::new(),
        tool_call_id: None,
    }
}
fn request() -> ChatRequest {
    ChatRequest {
        model: "public-model".into(),
        messages: vec![
            message(Role::System, "instructions"),
            message(Role::User, "hello"),
        ],
        tools: Vec::new(),
        tool_choice: None,
        temperature: Some(0.5),
        max_output_tokens: Some(100),
        stream: false,
    }
}
fn tool() -> FunctionTool {
    FunctionTool {
        name: "weather".into(),
        description: Some("Get weather".into()),
        strict: Some(false),
        parameters: json!({"type":"object", "properties":{"city":{"type":"string"}}, "required":["city"]}),
    }
}
fn complete(tool: bool) -> Value {
    json!({"output":{"message":{"role":"assistant", "content": if tool { json!([
        {"text":"Looking up weather"}, {"toolUse":{"toolUseId":"call_1", "name":"weather", "input":{"city":"Paris"}}}
    ]) } else { json!([{"text":"Hello"}]) }}}, "stopReason":if tool {"tool_use"} else {"end_turn"},
        "usage":{"inputTokens":3, "outputTokens":5,"totalTokens":8}, "metrics":{"latencyMs":2}})
}
fn events(tool: bool) -> Vec<(&'static str, Value)> {
    let mut events = vec![
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0, "delta":{"text":"Hello"}}),
        ),
        ("contentBlockStop", json!({"contentBlockIndex":0})),
    ];
    if tool {
        events.extend([
            ("contentBlockStart", json!({"contentBlockIndex":4, "start":{"toolUse":{"toolUseId":"call_1", "name":"weather"}}})),
            ("contentBlockDelta", json!({"contentBlockIndex":4, "delta":{"toolUse":{"input":"{\"city\":"}}})),
            ("contentBlockDelta", json!({"contentBlockIndex":4, "delta":{"toolUse":{"input":"\"Paris\"}"}}})),
            ("contentBlockStop", json!({"contentBlockIndex":4}))]);
    }
    events.extend([("messageStop", json!({"stopReason":if tool {"tool_use"} else {"end_turn"}})),
        ("metadata", json!({"usage":{"inputTokens":3,"outputTokens":5,"totalTokens":8}, "metrics":{"latencyMs":2}}))]);
    events
}
// AWS event-stream framing: big endian prelude, typed headers, payload, two CRC32s.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn frame(kind: &str, message_type: &str, payload: Value) -> Vec<u8> {
    let mut headers = Vec::new();
    let type_header = if message_type == "exception" {
        ":exception-type"
    } else {
        ":event-type"
    };
    for (name, value) in [
        (":message-type", message_type),
        (type_header, kind),
        (":content-type", "application/json"),
    ] {
        headers.push(name.len() as u8);
        headers.extend(name.as_bytes());
        headers.push(7);
        headers.extend((value.len() as u16).to_be_bytes());
        headers.extend(value.as_bytes());
    }
    let payload = serde_json::to_vec(&payload).unwrap();
    let mut result = Vec::new();
    result.extend(((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    result.extend((headers.len() as u32).to_be_bytes());
    result.extend(crc32(&result).to_be_bytes());
    result.extend(headers);
    result.extend(payload);
    result.extend(crc32(&result).to_be_bytes());
    result
}
fn wire(events: Vec<(&str, Value)>) -> Vec<u8> {
    events
        .into_iter()
        .flat_map(|(kind, value)| frame(kind, "event", value))
        .collect()
}

#[tokio::test]
async fn shared_text_contract_and_actual_sdk_signed_request() {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let adapter = mock.adapter();
    // Converse reports neither the served model (outside prompt routers) nor
    // reasoning tokens: both stay unknown and Logs fall back to the configured id.
    super::super::contract::assert_text_chat_contract(
        &adapter,
        &target(),
        request(),
        super::super::contract::Telemetry::UNKNOWN,
    )
    .await;
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for (i, req) in requests.iter().enumerate() {
        assert_eq!(
            req.uri.path(),
            if i == 0 {
                "/model/test-model/converse"
            } else {
                "/model/test-model/converse-stream"
            }
        );
        let auth = req.headers["authorization"].to_str().unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDLOCALTEST/"));
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"));
        assert!(auth.contains("Signature="));
        assert_eq!(req.headers["x-amz-security-token"], "local-session-token");
        assert!(req.headers.contains_key("x-amz-date"));
        assert_eq!(req.body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(req.body["system"][0]["text"], "instructions");
        assert_eq!(
            req.body["inferenceConfig"],
            json!({"maxTokens":100,"temperature":0.5})
        );
        assert!(!req.body.to_string().contains("local-session-token"));
    }
}

fn routed(mut events: Vec<(&'static str, Value)>, invoked: &str) -> Vec<(&'static str, Value)> {
    for (kind, value) in &mut events {
        if *kind == "metadata" {
            value["trace"] = json!({"promptRouter":{"invokedModelId":invoked}});
        }
    }
    events
}

#[tokio::test]
async fn prompt_router_trace_reports_the_invoked_model() {
    const INVOKED: &str =
        "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-3-haiku-20240307-v1:0";
    let mut body = complete(false);
    body["trace"] = json!({"promptRouter":{"invokedModelId":INVOKED}});
    let mock = Mock::serve(body, wire(routed(events(false), INVOKED)), 200).await;
    super::super::contract::assert_text_chat_contract(
        &mock.adapter(),
        &target(),
        request(),
        super::super::contract::Telemetry {
            reported_model: Some(INVOKED),
            reasoning_tokens: None,
        },
    )
    .await;
    // An invalid reported id is unknown and never fails the request.
    let mut body = complete(false);
    body["trace"] = json!({"promptRouter":{"invokedModelId":"not a model id"}});
    let mock = Mock::serve(body, wire(routed(events(false), "")), 200).await;
    super::super::contract::assert_text_chat_contract(
        &mock.adapter(),
        &target(),
        request(),
        super::super::contract::Telemetry::UNKNOWN,
    )
    .await;
}

#[tokio::test]
async fn tools_json_and_stream_tool_indices_usage_after_finish() {
    let mock = Mock::serve(complete(true), wire(events(true)), 200).await;
    let adapter = mock.adapter();
    let mut req = request();
    req.tools.push(tool());
    req.tool_choice = Some(ToolChoice::Required);
    let ProviderOutput::Complete(result) = adapter.execute(&target(), req.clone()).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(result.tool_calls[0].arguments, "{\"city\":\"Paris\"}");
    assert_eq!(result.finish_reason, FinishReason::ToolCalls);
    assert_eq!(result.usage.input_tokens, Some(3));
    req.stream = true;
    let ProviderOutput::Stream(mut stream) = adapter.execute(&target(), req).await.unwrap() else {
        panic!()
    };
    let mut args = String::new();
    let mut order = Vec::new();
    let mut starts = 0;
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            ChatEvent::Delta { tool_calls, .. } => {
                for delta in tool_calls {
                    assert_eq!(
                        delta.index, 0,
                        "canonical tool index must not be AWS content block index"
                    );
                    if let Some(id) = delta.id {
                        assert_eq!(id, "call_1");
                        starts += 1;
                    }
                    if let Some(value) = delta.arguments {
                        args.push_str(&value);
                    }
                }
            }
            ChatEvent::Finish(reason) => {
                assert_eq!(reason, FinishReason::ToolCalls);
                order.push("finish");
            }
            ChatEvent::Usage(usage) => {
                assert_eq!(usage.output_tokens, Some(5));
                order.push("usage");
            }
            ChatEvent::Done => order.push("done"),
        }
    }
    assert_eq!(starts, 1);
    assert_eq!(args, "{\"city\":\"Paris\"}");
    assert_eq!(order, ["finish", "usage", "done"]);
    let requests = mock.requests.lock().unwrap();
    assert_eq!(
        requests[0].body["toolConfig"]["toolChoice"],
        json!({"any":{}})
    );
    assert_eq!(
        requests[0].body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"],
        tool().parameters
    );
}

#[tokio::test]
async fn tool_history_preserves_json_input_and_result_text() {
    let mock = Mock::serve(complete(false), wire(events(false)), 200).await;
    let mut req = request();
    req.tools.push(tool());
    req.messages.push(Message {
        role: Role::Assistant,
        content: None,
        tool_call_id: None,
        tool_calls: vec![
            ToolCall {
                id: "call_1".into(),
                name: "weather".into(),
                arguments: "{\"city\":\"Paris\"}".into(),
            },
            ToolCall {
                id: "call_2".into(),
                name: "weather".into(),
                arguments: "{}".into(),
            },
        ],
    });
    for id in ["call_1", "call_2"] {
        req.messages.push(Message {
            role: Role::Tool,
            content: Some("{ \"temp\": 20 }".into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
        });
    }
    mock.adapter()
        .execute_protocol(&target(), req, ApiProtocol::Messages)
        .await
        .unwrap();
    let captured = mock.requests.lock().unwrap();
    let messages = &captured[0].body["messages"];
    assert_eq!(messages.as_array().unwrap().len(), 3);
    assert_eq!(
        messages[1]["content"][0]["toolUse"]["input"],
        json!({"city":"Paris"})
    );
    assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
    assert_eq!(
        messages[2]["content"][0]["toolResult"]["content"][0],
        json!({"text":"{ \"temp\": 20 }"})
    );
}

async fn assert_bad_stream(frames: Vec<u8>) {
    let mock = Mock::serve(complete(false), frames, 200).await;
    let mut req = request();
    req.stream = true;
    let mut output = match mock.adapter().execute(&target(), req).await {
        Ok(ProviderOutput::Stream(output)) => output,
        // SDK versions differ on whether an empty/corrupt first frame fails
        // during response establishment or the first stream read. Both fail closed.
        Err(InferenceError::InvalidUpstream | InferenceError::UpstreamUnavailable) => return,
        _ => panic!("invalid stream was not rejected"),
    };
    let mut failed = false;
    while let Some(event) = output.next().await {
        match event {
            Err(_) => failed = true,
            Ok(ChatEvent::Done) => panic!("invalid stream emitted Done"),
            _ => {}
        }
    }
    assert!(failed);
}

#[tokio::test]
async fn truncated_corrupt_exception_and_post_stop_streams_fail_closed() {
    let valid = events(false);
    for length in 0..valid.len() {
        assert_bad_stream(wire(valid[..length].to_vec())).await;
    }
    let mut bytes = wire(valid.clone());
    bytes.pop();
    assert_bad_stream(bytes).await;
    let mut bytes = wire(valid.clone());
    bytes[15] ^= 1;
    assert_bad_stream(bytes).await;
    let mut bytes = wire(valid.clone());
    bytes.extend(frame(
        "internalServerException",
        "exception",
        json!({"message":"secret provider details"}),
    ));
    assert_bad_stream(bytes).await;
    let mut bad = valid.clone();
    bad.push((
        "contentBlockDelta",
        json!({"contentBlockIndex":0,"delta":{"text":"late"}}),
    ));
    assert_bad_stream(wire(bad)).await;
    let mut duplicate = valid.clone();
    duplicate.push(valid.last().unwrap().clone());
    assert_bad_stream(wire(duplicate)).await;
}

#[tokio::test]
async fn incomplete_tool_arguments_unknown_content_and_stop_reason_fail() {
    let mut bad = events(true);
    bad[5].1["delta"]["toolUse"]["input"] = json!("broken");
    assert_bad_stream(wire(bad)).await;
    let mut bad = events(false);
    bad[1].1["delta"] = json!({"reasoningContent":{"text":"hidden reasoning"}});
    assert_bad_stream(wire(bad)).await;
    let mut bad = events(false);
    bad[3].1["stopReason"] = json!("new_future_reason");
    assert_bad_stream(wire(bad)).await;
    let mut bad = events(false);
    bad[1].1["contentBlockIndex"] = json!(-1);
    assert_bad_stream(wire(bad)).await;
}

#[tokio::test]
async fn safe_error_mapping_and_no_sdk_retries() {
    for (code, status, expected) in [
        ("ThrottlingException", 429, InferenceError::Busy),
        ("AccessDeniedException", 403, InferenceError::Configuration),
        ("ValidationException", 400, InferenceError::UpstreamRejected),
        (
            "InternalServerException",
            500,
            InferenceError::UpstreamUnavailable,
        ),
    ] {
        let mock = Mock::serve(
            json!({"__type":code,"message":"upstream secret prompt"}),
            Vec::new(),
            status,
        )
        .await;
        let result = mock.adapter().execute(&target(), request()).await;
        assert!(matches!(result, Err(error) if error == expected));
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        assert!(!expected.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn invalid_config_and_protocol_never_reach_credentials_or_network() {
    let adapter = BedrockAdapter::new().unwrap();
    for change in 0..7 {
        let mut target = target();
        match change {
            0 => target.credential_ref = "env:AWS_SECRET_ACCESS_KEY".into(),
            1 => target.credential_ref = "plaintext-secret".into(),
            2 => target.endpoint = Some("http://127.0.0.1".into()),
            3 => target.region = None,
            4 => target.region = Some("".into()),
            5 => target.provider = "openai".into(),
            _ => target.region = Some("us-east-1/evil".into()),
        }
        assert!(matches!(
            adapter.execute(&target, request()).await,
            Err(InferenceError::Configuration)
        ));
    }
    assert!(matches!(
        adapter
            .execute_protocol(&target(), request(), ApiProtocol::Responses)
            .await,
        Err(InferenceError::Unsupported)
    ));
}

#[test]
fn validates_semantics_without_silent_loss() {
    let check = |request: ChatRequest| assert!(encode(&request).is_err());
    for temp in [f64::NAN, f64::INFINITY, -0.1, 1.01] {
        let mut req = request();
        req.temperature = Some(temp);
        check(req);
    }
    for max in [0, u32::MAX] {
        let mut req = request();
        req.max_output_tokens = Some(max);
        check(req);
    }
    let mut req = request();
    req.messages.push(message(Role::System, "late"));
    check(req);
    let mut req = request();
    req.messages[0].role = Role::Developer;
    check(req);
    let mut req = request();
    req.tools.push(tool());
    req.tools[0].strict = Some(true);
    check(req);
    let mut req = request();
    req.tools.push(tool());
    req.tools[0].parameters = json!({"type":"array"});
    check(req);
    let mut req = request();
    req.tools.push(tool());
    req.tool_choice = Some(ToolChoice::None);
    check(req);
    let mut req = request();
    req.tool_choice = Some(ToolChoice::Required);
    check(req);
    let mut req = request();
    req.tools.push(tool());
    req.tool_choice = Some(ToolChoice::Function("missing".into()));
    check(req);
    let mut req = request();
    req.messages.push(message(Role::Assistant, "prefill"));
    check(req);
    let mut req = request();
    req.messages[1].tool_call_id = Some("orphan".into());
    check(req);
    let mut req = request();
    req.messages[1].content = Some("x".repeat(OUTPUT_LIMIT + 1));
    check(req);
    let mut req = request();
    req.tools.push(tool());
    req.tool_choice = Some(ToolChoice::Function("weather".into()));
    assert!(encode(&req).is_ok());
    assert_eq!(
        finish(&aws::StopReason::MaxTokens).unwrap(),
        FinishReason::Length
    );
    assert_eq!(
        finish(&aws::StopReason::GuardrailIntervened).unwrap(),
        FinishReason::ContentFilter
    );
    assert_eq!(
        finish(&aws::StopReason::StopSequence).unwrap(),
        FinishReason::Stop
    );
}

#[tokio::test]
async fn bounded_complete_and_stream_output() {
    let mut oversized = complete(false);
    oversized["output"]["message"]["content"][0]["text"] = json!("x".repeat(OUTPUT_LIMIT + 1));
    let mock = Mock::serve(oversized, Vec::new(), 200).await;
    assert!(matches!(
        mock.adapter().execute(&target(), request()).await,
        Err(InferenceError::InvalidUpstream)
    ));
    let mut events = events(false);
    events[1].1["delta"]["text"] = json!("x".repeat(OUTPUT_LIMIT + 1));
    assert_bad_stream(wire(events)).await;

    // Pre-deserialization wire bound, with no Content-Length, also limits error bodies.
    let mock = Mock::serve(
        json!({"message":"x".repeat(transport::WIRE_LIMIT + 1)}),
        Vec::new(),
        500,
    )
    .await;
    assert!(mock.adapter().execute(&target(), request()).await.is_err());
    assert_eq!(mock.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn dropping_stream_cancels_upstream_body() {
    struct Dropped(Arc<tokio::sync::Notify>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    let dropped = Arc::new(tokio::sync::Notify::new());
    let signal = dropped.clone();
    let initial = wire(events(false)[..2].to_vec());
    let app = Router::new().fallback(move || {
        let signal = signal.clone();
        let initial = initial.clone();
        async move {
            let stream = async_stream::stream! {
                let _guard = Dropped(signal);
                yield Ok::<_, Infallible>(Bytes::from(initial));
                std::future::pending::<()>().await;
            };
            Response::builder()
                .header("content-type", "application/vnd.amazon.eventstream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock = Mock {
        base: format!("http://{}", listener.local_addr().unwrap()),
        requests: Default::default(),
        task: tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    };
    let mut req = request();
    req.stream = true;
    let ProviderOutput::Stream(mut stream) = mock.adapter().execute(&target(), req).await.unwrap()
    else {
        panic!()
    };
    assert!(matches!(
        stream.next().await,
        Some(Ok(ChatEvent::Delta { .. }))
    ));
    drop(stream);
    tokio::time::timeout(Duration::from_secs(3), dropped.notified())
        .await
        .expect("dropping consumer must cancel upstream body");
}

#[test]
fn document_numeric_roundtrip_and_invalid_usage() {
    let value = json!({"huge":u64::MAX,"negative":i64::MIN,"fraction":0.5,"list":[true,null,"hi"]});
    assert_eq!(json_document(&document(&value).unwrap()).unwrap(), value);
    assert!(
        usage(
            &aws::TokenUsage::builder()
                .input_tokens(-1)
                .output_tokens(2)
                .total_tokens(1)
                .build()
                .unwrap()
        )
        .is_err()
    );
}

#[tokio::test]
async fn wire_missing_usage_never_becomes_sdk_zero_and_ttl_is_preserved() {
    for missing in [
        json!(null),
        json!({}),
        json!({"inputTokens":null,"outputTokens":0,"totalTokens":0}),
    ] {
        let mut body = complete(false);
        body["usage"] = missing.clone();
        let mock = Mock::serve(body, vec![], 200).await;
        assert!(mock.adapter().execute(&target(), request()).await.is_err());
        let mut e = events(false);
        e.last_mut().unwrap().1["usage"] = missing;
        let mock = Mock::serve(complete(false), wire(e), 200).await;
        let mut r = request();
        r.stream = true;
        let ProviderOutput::Stream(stream) = mock.adapter().execute(&target(), r).await.unwrap()
        else {
            panic!()
        };
        let events: Vec<_> = stream.collect().await;
        assert!(events.last().unwrap().is_err());
        assert!(!events.iter().any(|e| matches!(e, Ok(ChatEvent::Done))));
    }
    let mut body = complete(false);
    body["usage"] = json!({"inputTokens":3,"outputTokens":5,"totalTokens":38,"cacheReadInputTokens":10,"cacheWriteInputTokens":20,"cacheDetails":[{"ttl":"1h","inputTokens":12},{"ttl":"5m","inputTokens":8}]});
    let mock = Mock::serve(body, vec![], 200).await;
    let ProviderOutput::Complete(response) =
        mock.adapter().execute(&target(), request()).await.unwrap()
    else {
        panic!()
    };
    let b = response.usage.billing.unwrap();
    assert_eq!(response.usage.input_tokens, Some(3));
    assert_eq!(b.total_input_tokens, Some(33));
    assert_eq!(b.cache_write_1h_input_tokens, Some(12));
    assert_eq!(b.cache_write_5m_input_tokens, Some(8));
    assert_eq!(b.cache_write_default_input_tokens, Some(0));
}
