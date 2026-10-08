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
        supported_protocols: vec![
            "chat_completions".into(),
            "responses".into(),
            "embeddings".into(),
        ],
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
        json!({"type":"reasoning","summary":[{"type":"summary_text","text":"visible"}]}),
        json!({"type":"reasoning","content":[{"type":"reasoning_text","text":"visible"}]}),
        json!({"type":"reasoning","summary":[],"extra":true}),
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
/// Exact live `gpt-6-luna` shape (D1): message `phase`, cache/reasoning usage
/// details and a top-level `reasoning` object.
fn luna(phases: &[&str]) -> Value {
    let output: Vec<Value> = phases
        .iter()
        .enumerate()
        .map(|(i, phase)| {
            json!({"id":format!("msg_{i}"),"type":"message","status":"completed",
                "content":[{"type":"output_text","annotations":[],"logprobs":[],"text":format!("{phase}.")}],
                "phase":phase,"role":"assistant"})
        })
        .collect();
    json!({"id":"resp_private","object":"response","status":"completed",
        "reasoning":{"effort":"medium","summary":null},"output":output,
        "usage":{"input_tokens":9,"input_tokens_details":{"cache_write_tokens":0,"cached_tokens":0},
            "output_tokens":16,"output_tokens_details":{"reasoning_tokens":4},"total_tokens":25}})
}
fn luna_stream(phases: &[&str]) -> Vec<Value> {
    let done = luna(phases);
    let mut events = vec![json!({"type":"response.created","response":{"status":"in_progress"}})];
    for (i, item) in done["output"].as_array().unwrap().iter().enumerate() {
        let text = item["content"][0]["text"].clone();
        let mut added = item.clone();
        added["status"] = "in_progress".into();
        added["content"] = json!([]);
        events.extend([
            json!({"type":"response.output_item.added","output_index":i,"item":added}),
            json!({"type":"response.content_part.added","output_index":i,"content_index":0,"item_id":item["id"],"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}),
            json!({"type":"response.output_text.delta","output_index":i,"item_id":item["id"],"content_index":0,"delta":text,"logprobs":[]}),
            json!({"type":"response.output_text.done","output_index":i,"item_id":item["id"],"content_index":0,"text":text,"logprobs":[]}),
            json!({"type":"response.content_part.done","output_index":i,"content_index":0,"item_id":item["id"],"part":item["content"][0]}),
            json!({"type":"response.output_item.done","output_index":i,"item":item}),
        ]);
    }
    events.push(json!({"type":"response.completed","response":done}));
    events
}
#[test]
fn luna_phase_messages_and_usage_details_decode() {
    let r = decode(&luna(&["final_answer"])).unwrap();
    assert_eq!(r.content.as_deref(), Some("final_answer."));
    assert_eq!(r.usage.input_tokens, Some(9));
    assert_eq!(r.usage.output_tokens, Some(16));
    let billing = r.usage.billing.unwrap();
    assert_eq!(billing.cache_read_input_tokens, Some(0));
    assert_eq!(billing.cache_write_input_tokens, Some(0));
    assert_eq!(billing.uncached_input_tokens, Some(9));
    // Commentary is assistant text too; both phases are kept in output order.
    let r = decode(&luna(&["commentary", "final_answer"])).unwrap();
    assert_eq!(r.content.as_deref(), Some("commentary.final_answer."));
    for phase in [json!("analysis"), json!(""), json!(1), json!({})] {
        let mut v = luna(&["final_answer"]);
        v["output"][0]["phase"] = phase;
        assert_eq!(decode(&v).err(), Some(InferenceError::InvalidUpstream));
    }
}
#[test]
fn luna_phase_stream_lifecycle_matches_terminal() {
    for phases in [&["final_answer"][..], &["commentary", "final_answer"][..]] {
        let mut state = State::default();
        let mut text = String::new();
        let mut usage = None;
        for event in luna_stream(phases) {
            for e in state.push(event).unwrap() {
                match e {
                    ChatEvent::Delta { text: Some(t), .. } => text.push_str(&t),
                    ChatEvent::Usage(u) => usage = Some(u),
                    _ => {}
                }
            }
        }
        assert_eq!(
            text,
            phases.iter().map(|p| format!("{p}.")).collect::<String>()
        );
        assert_eq!(usage.unwrap().output_tokens, Some(16));
    }
    // Unknown phase at item start, or a phase change at done, fails.
    let mut events = luna_stream(&["final_answer"]);
    events[1]["item"]["phase"] = "analysis".into();
    let mut state = State::default();
    state.push(events[0].clone()).unwrap();
    assert!(state.push(events[1].clone()).is_err());
    let mut events = luna_stream(&["final_answer"]);
    events[6]["item"]["phase"] = "commentary".into();
    let mut state = State::default();
    assert!(
        events
            .into_iter()
            .try_for_each(|e| state.push(e).map(drop))
            .is_err()
    );
}
#[tokio::test]
async fn invalid_body_preserves_validated_usage_as_evidence() {
    use crate::inference::evidence::capture;
    let mut bad = luna(&["final_answer"]);
    bad["output"][0]["phase"] = "analysis".into();
    let mock = Mock::new(200, Some(bad.to_string().into_bytes())).await;
    let (result, observed) = capture(execute(&mock.adapter, &target(), request(false))).await;
    assert!(matches!(result, Err(InferenceError::InvalidUpstream)));
    let observed = observed.unwrap();
    assert_eq!(
        (observed.input_tokens, observed.output_tokens),
        (Some(9), Some(16))
    );
    // Invalid usage is never reported as evidence.
    bad["usage"]["input_tokens"] = json!(-1);
    let (result, observed) = capture(async { decode(&bad) }).await;
    assert!(result.is_err());
    assert_eq!(observed, None);
    // Stream terminal snapshot that contradicts streamed content keeps usage.
    let mut events = luna_stream(&["final_answer"]);
    let last = events.len() - 1;
    events[last]["response"]["output"][0]["content"][0]["text"] = "different".into();
    let (result, observed) = capture(async {
        let mut state = State::default();
        events.into_iter().try_for_each(|e| state.push(e).map(drop))
    })
    .await;
    assert!(result.is_err());
    assert_eq!(observed.unwrap().output_tokens, Some(16));
}

/// Live 2026-10-08 `gpt-6-luna` stream: an opaque reasoning item precedes the
/// final_answer message (probe openai_responses_gpt6luna_full_stream.json).
#[test]
fn luna_opaque_reasoning_items_are_accepted_and_not_text() {
    let reasoning = json!({"id":"rs_1","type":"reasoning","content":[],"encrypted_content":"opaque","summary":[]});
    let mut done = luna(&["final_answer"]);
    let message = done["output"][0].clone();
    done["output"] = json!([reasoning.clone(), message.clone()]);
    let r = decode(&done).unwrap();
    assert_eq!(r.content.as_deref(), Some("final_answer."));
    let mut events = vec![
        json!({"type":"response.created","response":{"status":"in_progress"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":reasoning}),
        json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
    ];
    for mut e in luna_stream(&["final_answer"]).into_iter().skip(1) {
        if e["output_index"].is_u64() {
            e["output_index"] = json!(1);
        }
        if e["type"] == "response.completed" {
            e["response"] = done.clone();
        }
        events.push(e);
    }
    let mut state = State::default();
    let mut text = String::new();
    let mut finished = false;
    for e in events {
        for ev in state.push(e).unwrap() {
            match ev {
                ChatEvent::Delta { text: Some(t), .. } => text.push_str(&t),
                ChatEvent::Done => finished = true,
                _ => {}
            }
        }
    }
    assert_eq!(text, "final_answer.");
    assert!(finished);
}
