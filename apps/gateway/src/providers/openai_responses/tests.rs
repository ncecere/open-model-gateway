use super::super::{
    ProviderAdapter,
    secrets::{Secret, SecretResolver},
};
use super::*;
#[test]
fn native_function_lifecycle_uses_contiguous_tool_indices_after_text() {
    let mut state = State::default();
    for v in stream().into_iter().take(7) {
        state.push(v).unwrap();
    }
    let item = json!({"id":"fc","type":"function_call","status":"in_progress","call_id":"call","name":"weather","arguments":""});
    let events = state
        .push(json!({"type":"response.output_item.added","output_index":1,"item":item}))
        .unwrap();
    assert!(matches!(&events[0], ChatEvent::Delta { tool_calls, .. } if tool_calls[0].index == 0));
    for s in ["{", "}"] {
        let events = state.push(json!({"type":"response.function_call_arguments.delta","output_index":1,"item_id":"fc","delta":s})).unwrap();
        assert!(
            matches!(&events[0], ChatEvent::Delta { tool_calls, .. } if tool_calls[0].index == 0)
        );
    }
    let mut done = item;
    done["status"] = "completed".into();
    done["arguments"] = "{}".into();
    state.push(json!({"type":"response.function_call_arguments.done","output_index":1,"item_id":"fc","arguments":"{}"})).unwrap();
    state
        .push(json!({"type":"response.output_item.done","output_index":1,"item":done}))
        .unwrap();
    let mut response = complete();
    response["output"].as_array_mut().unwrap().push(done);
    let events = state
        .push(json!({"type":"response.completed","response":response}))
        .unwrap();
    assert!(matches!(
        events[0],
        ChatEvent::Finish(FinishReason::ToolCalls)
    ));
    assert!(matches!(events[2], ChatEvent::Done));
}
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Response, Uri},
};
use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};
struct Resolver;
impl SecretResolver for Resolver {
    fn resolve(&self, _: &str) -> Result<Secret> {
        Secret::new("mock-key".into())
    }
}
fn target() -> Deployment {
    Deployment {
        id: uuid::Uuid::new_v4(),
        provider: "openai".into(),
        upstream_model: "private".into(),
        credential_ref: "ref".into(),
        endpoint: None,
        region: None,
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
    json!({"id":"private","object":"response","status":"completed","output":[{"id":"m","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"世界","annotations":[]}]}],"usage":{"input_tokens":3,"output_tokens":2}})
}
fn stream() -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"status":"in_progress"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m","role":"assistant","content":[],"status":"in_progress"}}),
        json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"m","content_index":0,"delta":"世界"}),
        json!({"type":"response.output_text.done","output_index":0,"item_id":"m","content_index":0,"text":"世界"}),
        json!({"type":"response.content_part.done","output_index":0,"content_index":0,"part":{"type":"output_text","text":"世界","annotations":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":complete()["output"][0]}),
        json!({"type":"response.completed","response":complete()}),
    ]
}
struct Mock {
    adapter: OpenAiAdapter,
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
        let app = Router::new().fallback(move |h: HeaderMap, uri: Uri, bytes: Bytes| {
            let c = c.clone();
            let content = content.clone();
            async move {
                let v: Value = serde_json::from_slice(&bytes).unwrap();
                let streaming = v["stream"] == true;
                c.lock().unwrap().push((h, uri, v));
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
            adapter: OpenAiAdapter::for_test(Arc::new(Resolver), base),
            capture,
            task,
        }
    }
}
#[tokio::test]
async fn native_json_and_byte_fragmented_sse() {
    let mock = Mock::new(200, None).await;
    let ProviderOutput::Complete(r) = mock
        .adapter
        .execute_protocol(&target(), request(false), ApiProtocol::Responses)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(r.content.as_deref(), Some("世界"));
    let ProviderOutput::Stream(s) = mock
        .adapter
        .execute_protocol(&target(), request(true), ApiProtocol::Responses)
        .await
        .unwrap()
    else {
        panic!()
    };
    let events: Vec<_> = s.collect().await;
    assert!(events.iter().all(|v| v.is_ok()));
    assert!(matches!(events.last(), Some(Ok(ChatEvent::Done))));
    for (h, uri, v) in mock.capture.lock().unwrap().iter() {
        assert_eq!(uri.path(), "/v1/responses");
        assert_eq!(h["authorization"], "Bearer mock-key");
        assert_eq!(v["store"], false);
        assert!(v.get("messages").is_none());
        assert_eq!(v["input"][0]["content"], "hello");
        assert_eq!(v["max_output_tokens"], 50);
    }
}
#[tokio::test]
async fn eof_and_upstream_failure_never_succeed() {
    let bytes = stream()
        .iter()
        .take(7)
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>()
        .into_bytes();
    let mock = Mock::new(200, Some(bytes)).await;
    let ProviderOutput::Stream(s) = execute(&mock.adapter, &target(), request(true))
        .await
        .unwrap()
    else {
        panic!()
    };
    let events: Vec<_> = s.collect().await;
    assert!(events.last().unwrap().is_err());
    assert!(!events.iter().any(|v| matches!(v, Ok(ChatEvent::Done))));
    let mock = Mock::new(429, Some(b"private body".to_vec())).await;
    assert!(matches!(
        execute(&mock.adapter, &target(), request(false)).await,
        Err(InferenceError::Busy)
    ));
}
#[test]
fn rejects_reasoning_modalities_and_mismatched_terminal() {
    for item in [
        json!({"type":"reasoning","summary":[]}),
        json!({"type":"web_search_call"}),
        json!({"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"no"}]}),
    ] {
        let mut v = complete();
        v["output"] = json!([item]);
        assert!(decode(&v).is_err());
    }
    let mut state = State::default();
    for v in stream().into_iter().take(7) {
        state.push(v).unwrap();
    }
    let mut end = complete();
    end["output"][0]["content"][0]["text"] = "different".into();
    assert!(
        state
            .push(json!({"type":"response.completed","response":end}))
            .is_err()
    );
    assert!(
        State::default()
            .push(json!({"type":"response.completed","response":complete()}))
            .is_err()
    );
}
#[test]
fn function_tools_are_native_and_incomplete_is_preserved() {
    let mut r = request(false);
    r.tools.push(FunctionTool {
        name: "weather".into(),
        description: None,
        parameters: json!({"type":"object"}),
        strict: Some(true),
    });
    r.tool_choice = Some(ToolChoice::Function("weather".into()));
    r.messages.push(Message {
        role: Role::Tool,
        content: Some("ok".into()),
        tool_calls: vec![],
        tool_call_id: Some("call".into()),
    });
    let v = encode("private", &r);
    assert_eq!(v["tools"][0]["type"], "function");
    assert_eq!(v["tools"][0]["name"], "weather");
    assert_eq!(v["input"][1]["type"], "function_call_output");
    assert_eq!(v["tool_choice"]["name"], "weather");
    let mut v = complete();
    v["status"] = "incomplete".into();
    v["incomplete_details"] = json!({"reason":"max_output_tokens"});
    assert_eq!(decode(&v).unwrap().finish_reason, FinishReason::Length);
}
