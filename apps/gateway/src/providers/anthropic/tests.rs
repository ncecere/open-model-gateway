use super::super::secrets::Secret;
use super::*;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use std::{convert::Infallible, sync::Mutex};
struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret> {
        Secret::new("mock-key".into())
    }
}
fn target() -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "anthropic".into(),
        upstream_model: "private-model".into(),
        credential_ref: "ref".into(),
        endpoint: None,
        region: None,
        supported_protocols: vec!["chat_completions".into(), "messages".into()],
    }
}
fn request(stream: bool) -> ChatRequest {
    ChatRequest {
        model: "public".into(),
        messages: vec![Message {
            role: Role::User,
            content: Some("hello".into()),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        tools: vec![],
        tool_choice: None,
        temperature: None,
        max_output_tokens: Some(50),
        stream,
    }
}
fn complete() -> Value {
    json!({"id":"upstream","type":"message","role":"assistant","model":"private","content":[{"type":"text","text":"世界"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":4,"output_tokens":2}})
}
fn stream() -> Vec<Value> {
    vec![
        json!({"type":"message_start","message":{"id":"x","type":"message","role":"assistant","model":"private","content":[],"stop_reason":null,"usage":{"input_tokens":4,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"世界"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ]
}
struct Mock {
    adapter: AnthropicAdapter,
    capture: Arc<Mutex<Vec<(HeaderMap, Uri, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Mock {
    async fn new(status: u16, content: Option<Vec<u8>>) -> Self {
        let capture = Arc::new(Mutex::new(vec![]));
        let c = capture.clone();
        let app = Router::new().fallback(move |headers: HeaderMap, uri: Uri, bytes: Bytes| {
            let c = c.clone();
            let content = content.clone();
            async move {
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                let streaming = value["stream"] == true;
                c.lock().unwrap().push((headers, uri, value));
                let bytes = content.unwrap_or_else(|| {
                    if streaming {
                        stream()
                            .iter()
                            .map(|v| {
                                format!(
                                    "event: {}\r\ndata: {v}\r\n\r\n",
                                    v["type"].as_str().unwrap()
                                )
                            })
                            .collect::<String>()
                            .into_bytes()
                    } else {
                        complete().to_string().into_bytes()
                    }
                });
                let chunks = bytes
                    .into_iter()
                    .map(|b| Ok::<_, Infallible>(Bytes::from(vec![b])))
                    .collect::<Vec<_>>();
                Response::builder()
                    .status(status)
                    .header(
                        "content-type",
                        if streaming {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                    )
                    .body(Body::from_stream(futures_util::stream::iter(chunks)))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            adapter: AnthropicAdapter::for_test(Arc::new(Resolver), base),
            capture,
            task,
        }
    }
}
#[tokio::test]
async fn local_contract_and_native_headers() {
    let mock = Mock::new(200, None).await;
    super::super::contract::assert_text_chat_contract(&mock.adapter, &target(), request(false))
        .await;
    let capture = mock.capture.lock().unwrap();
    assert_eq!(capture.len(), 2);
    for (h, uri, v) in capture.iter() {
        assert_eq!(uri.path(), "/v1/messages");
        assert_eq!(h["x-api-key"], "mock-key");
        assert_eq!(h["anthropic-version"], "2023-06-01");
        assert!(!h.contains_key("authorization"));
        assert_eq!(v["model"], "private-model");
        assert_eq!(v["max_tokens"], 50);
    }
}
#[tokio::test]
async fn eof_is_not_done_and_http_errors_are_sanitized() {
    let bytes = stream()
        .iter()
        .take(5)
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>()
        .into_bytes();
    let mock = Mock::new(200, Some(bytes)).await;
    let ProviderOutput::Stream(s) = mock
        .adapter
        .execute(&target(), request(true))
        .await
        .unwrap()
    else {
        panic!()
    };
    let events: Vec<_> = s.collect().await;
    assert!(events.last().unwrap().is_err());
    assert!(!events.iter().any(|v| matches!(v, Ok(ChatEvent::Done))));
    for (code, expected) in [
        (302, InferenceError::Configuration),
        (401, InferenceError::Configuration),
        (429, InferenceError::Busy),
        (503, InferenceError::UpstreamUnavailable),
    ] {
        let mock = Mock::new(code, Some(b"secret upstream body".to_vec())).await;
        assert!(
            matches!(mock.adapter.execute(&target(), request(false)).await, Err(e) if e == expected)
        );
    }
}
#[test]
fn strict_subset_and_unknown_usage() {
    let mut r = request(false);
    r.temperature = Some(1.1);
    assert!(encode("x", &r).is_err());
    let mut v = complete();
    v["content"][0] = json!({"type":"thinking","thinking":"private"});
    assert!(decode(&v).is_err());
    let mut v = complete();
    v["content"][0]["citations"] = json!([{"url":"private"}]);
    assert!(decode(&v).is_err());
    let mut v = complete();
    v.as_object_mut().unwrap().remove("usage");
    assert_eq!(decode(&v).unwrap().usage, Usage::default());
    assert!(
        AnthropicAdapter::new(Arc::new(Resolver))
            .unwrap()
            .supports_protocol(ApiProtocol::Messages)
    );
    assert!(
        !AnthropicAdapter::new(Arc::new(Resolver))
            .unwrap()
            .supports_protocol(ApiProtocol::Responses)
    );
}
#[test]
fn tool_deltas_and_ordering() {
    let mut state = State::default();
    state.push(stream()[0].clone()).unwrap();
    let events = state.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call","name":"weather","input":{}}})).unwrap();
    assert!(matches!(&events[0], ChatEvent::Delta { tool_calls, .. } if tool_calls[0].index == 0));
    for part in ["{\"city\":", "\"東京\"}"] {
        state.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":part}})).unwrap();
    }
    assert!(state.push(json!({"type":"message_stop"})).is_err());
    state
        .push(json!({"type":"content_block_stop","index":0}))
        .unwrap();
    state
        .push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}))
        .unwrap();
    let events = state.push(json!({"type":"message_stop"})).unwrap();
    assert!(matches!(
        events[0],
        ChatEvent::Usage(Usage {
            input_tokens: Some(4),
            output_tokens: None,
            ..
        })
    ));
    assert!(matches!(events[1], ChatEvent::Done));
}
#[tokio::test]
async fn rejects_custom_endpoint_before_network() {
    let adapter = AnthropicAdapter::new(Arc::new(Resolver)).unwrap();
    let mut t = target();
    t.endpoint = Some("http://127.0.0.1:1".into());
    assert!(matches!(
        adapter.execute(&t, request(false)).await,
        Err(InferenceError::Configuration)
    ));
}

#[test]
fn cumulative_cache_usage_has_one_finish_and_one_final_usage() {
    let mut state = State::default();
    state.push(json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":4,"output_tokens":0,"cache_read_input_tokens":10,"cache_creation_input_tokens":20,"cache_creation":{"ephemeral_5m_input_tokens":8,"ephemeral_1h_input_tokens":12}}}})).unwrap();
    assert!(state.push(json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":2}})).unwrap().is_empty());
    assert!(state.push(json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":5,"input_tokens":6}})).unwrap().is_empty());
    let finished=state.push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}})).unwrap();
    assert_eq!(finished.len(), 1);
    assert!(matches!(finished[0], ChatEvent::Finish(_)));
    assert!(
        state
            .push(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}))
            .is_err()
    );
    let terminal = state.push(json!({"type":"message_stop"})).unwrap();
    let ChatEvent::Usage(u) = terminal[0] else {
        panic!()
    };
    assert_eq!(u.input_tokens, Some(6));
    assert_eq!(u.output_tokens, Some(7));
    assert_eq!(u.billing.unwrap().total_input_tokens, Some(36));
    assert_eq!(u.billing.unwrap().cache_write_1h_input_tokens, Some(12));
    assert!(matches!(terminal[1], ChatEvent::Done));
    assert!(state.push(json!({"type":"message_stop"})).is_err());
}
#[tokio::test]
async fn invalid_messages_body_preserves_validated_usage_as_evidence() {
    use crate::inference::evidence::capture;
    let bad = json!({"id":"m","type":"message","role":"assistant","model":"private",
        "content":[{"type":"thinking","thinking":"x"}],"stop_reason":"end_turn","stop_sequence":null,
        "usage":{"input_tokens":13,"output_tokens":14,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}});
    let (result, observed) = capture(async { decode(&bad) }).await;
    assert_eq!(result.err(), Some(InferenceError::InvalidUpstream));
    let observed = observed.unwrap();
    assert_eq!(
        (observed.input_tokens, observed.output_tokens),
        (Some(13), Some(14))
    );
}
