use super::*;
fn parse(v: Value) -> Result<ChatRequest> {
    serde_json::from_value::<Request>(v)
        .map_err(|_| InferenceError::InvalidRequest)?
        .normalize()
}
#[test]
fn strict_stateless_subset() {
    for extra in [
        json!({"store":true}),
        json!({"previous_response_id":"x"}),
        json!({"background":false}),
        json!({"reasoning":{}}),
        // metadata/user are Logs session labels only: string pairs, else rejected.
        json!({"metadata":{"k":1}}),
        json!({"user":7}),
        json!({"tools":[{"type":"web_search"}]}),
        json!({"text":{"format":{"type":"json_object"}}}),
    ] {
        let mut v = json!({"model":"m","input":"hello"});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(parse(v).is_err());
    }
    assert!(parse(json!({"model":"m","input":[{"role":"user","content":[{"type":"input_image","image_url":"private"}]}]})).is_err());
    assert!(
        parse(json!({"model":"m","input":"hi","store":false,"text":{"format":{"type":"text"}}}))
            .is_ok()
    );
    assert!(
        parse(json!({"model":"m","input":"hi","user":"u","metadata":{"session_id":"s"}})).is_ok()
    );
}
#[test]
fn native_input_and_tools_normalize() {
    let r=parse(json!({"model":"m","instructions":"be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},{"type":"function_call","call_id":"c","name":"weather","arguments":"{}"},{"type":"function_call_output","call_id":"c","output":"sun"}],"tools":[{"type":"function","name":"weather","parameters":{"type":"object"}}],"tool_choice":{"type":"function","name":"weather"},"max_output_tokens":20})).unwrap();
    assert_eq!(r.messages.len(), 4);
    assert!(r.messages[0].role == Role::System);
    assert!(r.messages[3].role == Role::Tool);
    assert_eq!(r.tools.len(), 1);
}
#[test]
fn own_snapshots_are_stateless_followup_input() {
    for reason in [
        FinishReason::ToolCalls,
        FinishReason::Length,
        FinishReason::ContentFilter,
    ] {
        let response = ChatResponse {
            content: Some("First 世界, then tools".into()),
            tool_calls: vec![ToolCall {
                id: "semantic-call".into(),
                name: "weather".into(),
                arguments: "{\"city\": \"東京\"}".into(),
            }],
            finish_reason: reason,
            usage: Usage::default(),
        };
        let output = snapshot("resp_readonly", "m", 1, &response)["output"].clone();
        assert_ne!(output[1]["id"], output[1]["call_id"]);
        let mut input = vec![json!({"role":"user","content":"start"})];
        input.extend(output.as_array().unwrap().clone());
        input.push(json!({"type":"function_call_output","call_id":"semantic-call","output":"sun"}));
        input.push(json!({"role":"user","content":"continue"}));
        let normalized = parse(json!({"model":"m","input":input})).unwrap();
        assert_eq!(normalized.messages.len(), 5);
        assert_eq!(normalized.messages[1].content, response.content);
        assert_eq!(normalized.messages[2].tool_calls.len(), 1);
        let call = &normalized.messages[2].tool_calls[0];
        assert_eq!(call.id, response.tool_calls[0].id);
        assert_eq!(call.name, response.tool_calls[0].name);
        assert_eq!(call.arguments, response.tool_calls[0].arguments);
        assert_eq!(
            normalized.messages[3].tool_call_id.as_deref(),
            Some("semantic-call")
        );
        assert_eq!(normalized.messages[4].content.as_deref(), Some("continue"));
        assert_eq!(normalized.max_output_tokens, None);
    }
}
#[test]
fn supported_item_metadata_and_empty_annotations() {
    for status in [
        None,
        Some("in_progress"),
        Some("completed"),
        Some("incomplete"),
    ] {
        for id in [None, Some(""), Some("arbitrary client id")] {
            for mut item in [
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"a","annotations":[]},{"type":"output_text","text":"世界"}]}),
                json!({"type":"function_call","call_id":"c","name":"weather","arguments":"{ \"x\": 1 }"}),
                json!({"type":"function_call_output","call_id":"c","output":"sun"}),
            ] {
                if let Some(id) = id {
                    item["id"] = json!(id);
                }
                if let Some(status) = status {
                    item["status"] = json!(status);
                }
                let r = parse(json!({"model":"m","input":[item.clone()]})).unwrap();
                match item["type"].as_str().unwrap() {
                    "message" => assert_eq!(r.messages[0].content.as_deref(), Some("a世界")),
                    "function_call" => {
                        assert_eq!(r.messages[0].tool_calls[0].id, "c");
                        assert_eq!(r.messages[0].tool_calls[0].arguments, "{ \"x\": 1 }");
                    }
                    _ => assert_eq!(r.messages[0].tool_call_id.as_deref(), Some("c")),
                }
            }
        }
    }
    // Unlike message/call metadata, the SDK explicitly allows null on outputs.
    for metadata in [
        json!({"id":null}),
        json!({"status":null}),
        json!({"id":null,"status":null}),
    ] {
        let mut item = json!({"type":"function_call_output","call_id":"c","output":"sun"});
        item.as_object_mut()
            .unwrap()
            .extend(metadata.as_object().unwrap().clone());
        assert!(parse(json!({"model":"m","input":[item]})).is_ok());
    }
}
#[test]
fn echoed_metadata_does_not_widen_the_supported_subset() {
    for base in [
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}),
        json!({"type":"function_call","call_id":"c","name":"weather","arguments":"{}"}),
        json!({"type":"function_call_output","call_id":"c","output":"sun"}),
    ] {
        for field in ["id", "status"] {
            for bad in [json!(1), json!(true), json!([]), json!({}), json!(null)] {
                if base["type"] == "function_call_output" && bad.is_null() {
                    continue;
                }
                let mut item = base.clone();
                item[field] = bad;
                assert!(parse(json!({"model":"m","input":[item]})).is_err());
            }
        }
        for status in ["", "failed", "cancelled", "COMPLETED"] {
            let mut item = base.clone();
            item["status"] = json!(status);
            assert!(parse(json!({"model":"m","input":[item]})).is_err());
        }
        let mut item = base;
        item["extra"] = json!(true);
        assert!(parse(json!({"model":"m","input":[item]})).is_err());
    }
    for annotations in [
        json!(null),
        json!({}),
        json!(""),
        json!([{"type":"url_citation","url":"https://example.invalid","title":"citation","start_index":0,"end_index":1}]),
        json!([null]),
    ] {
        assert!(parse(json!({"model":"m","input":[{"role":"assistant","content":[{"type":"output_text","text":"hi","annotations":annotations}]}]})).is_err());
    }
    for item in [
        json!({"type":"message","role":"user","content":[{"type":"output_text","text":"hi","annotations":[]}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"input_text","text":"hi","annotations":[]}]}),
        json!({"type":"message","role":"tool","content":"hi"}),
        json!({"type":"message","role":"assistant","content":[]}),
        json!({"type":"message","role":"assistant","content":null}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":null}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"no"}]}),
        json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi","logprobs":[]}]}),
        json!({"type":"function_call","id":"readonly","name":"weather","arguments":"{}"}),
        json!({"type":"function_call","id":"readonly","call_id":"","name":"weather","arguments":"{}"}),
        json!({"type":"function_call","call_id":"c","name":"weather","arguments":{}}),
        json!({"type":"function_call_output","id":"readonly","call_id":null,"output":"sun"}),
        json!({"type":"function_call_output","call_id":"c","output":[]}),
        json!({"type":"item_reference","id":"readonly"}),
        json!({"type":"reasoning","id":"readonly","summary":[]}),
        json!({"type":"web_search_call","id":"readonly","status":"completed"}),
    ] {
        assert!(parse(json!({"model":"m","input":[item]})).is_err());
    }
}
fn stream(events: Vec<Result<ChatEvent>>) -> ProviderOutput {
    ProviderOutput::Stream(Box::pin(futures_util::stream::iter(events)))
}
async fn body(response: Response) -> String {
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}
fn frames(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|s| s.strip_prefix("data: "))
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
#[tokio::test]
async fn native_lifecycle_sequence_and_final_snapshot() {
    let output = stream(vec![
        Ok(ChatEvent::Delta {
            text: Some("世界".into()),
            tool_calls: vec![ToolCallDelta {
                index: 0,
                id: Some("call".into()),
                name: Some("weather".into()),
                arguments: Some("{}".into()),
            }],
        }),
        Ok(ChatEvent::Finish(FinishReason::ToolCalls)),
        Ok(ChatEvent::Usage(Usage {
            input_tokens: Some(2),
            output_tokens: Some(3),
            ..Usage::default()
        })),
        Ok(ChatEvent::Done),
    ]);
    let b = body(render(output, "resp_test".into(), "public".into(), 1)).await;
    let f = frames(&b);
    for (i, v) in f.iter().enumerate() {
        assert_eq!(v["sequence_number"], i as u64);
    }
    assert_eq!(f.first().unwrap()["type"], "response.created");
    assert_eq!(f.last().unwrap()["type"], "response.completed");
    assert!(b.contains("event: response.content_part.added"));
    assert!(b.contains("event: response.function_call_arguments.done"));
    assert_eq!(
        f.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "世界"
    );
    assert_eq!(f.last().unwrap()["response"]["usage"]["total_tokens"], 5);
}
#[tokio::test]
async fn no_completion_without_done_or_for_incomplete() {
    for events in [
        vec![Ok(ChatEvent::Finish(FinishReason::Stop))],
        vec![Err(InferenceError::UpstreamUnavailable)],
        vec![Ok(ChatEvent::Done)],
        vec![Ok(ChatEvent::Delta {
            text: Some("x".repeat(OUTPUT_LIMIT + 1)),
            tool_calls: vec![],
        })],
    ] {
        let b = body(render(stream(events), "r".into(), "m".into(), 1)).await;
        assert!(!b.contains("event: response.completed"));
        assert!(b.contains("event: error"));
    }
    let b = body(render(
        stream(vec![
            Ok(ChatEvent::Finish(FinishReason::Length)),
            Ok(ChatEvent::Done),
        ]),
        "r".into(),
        "m".into(),
        1,
    ))
    .await;
    assert!(b.contains("event: response.incomplete"));
    assert!(!b.contains("event: response.completed"));
}
#[test]
fn unknown_usage_and_tool_argument_fragments() {
    let mut a = Accumulator::default();
    for (id, n, args) in [
        (Some("ca"), Some("wea"), Some("{")),
        (Some("ll"), Some("ther"), Some("}")),
    ] {
        a.push(ChatEvent::Delta {
            text: None,
            tool_calls: vec![ToolCallDelta {
                index: 0,
                id: id.map(String::from),
                name: n.map(String::from),
                arguments: args.map(String::from),
            }],
        })
        .unwrap();
    }
    a.push(ChatEvent::Finish(FinishReason::ToolCalls)).unwrap();
    let r = a.finish().unwrap();
    assert_eq!(r.tool_calls[0].id, "call");
    assert_eq!(r.tool_calls[0].name, "weather");
    assert_eq!(r.tool_calls[0].arguments, "{}");
    assert!(snapshot("r", "m", 1, &r)["usage"].is_null());
}
#[test]
fn responses_keep_max_output_tokens_and_reject_chat_maxima() {
    let base = json!({"model":"m","input":"hi","store":false});
    let mut v = base.clone();
    v["max_output_tokens"] = json!(7);
    assert_eq!(parse(v).unwrap().max_output_tokens, Some(7));
    // The Chat Completions names (including the legacy alias) are not Responses fields.
    for field in ["max_tokens", "max_completion_tokens"] {
        let mut v = base.clone();
        v[field] = json!(7);
        assert!(parse(v).is_err(), "{field}");
    }
}
fn text(t: &str) -> Result<ChatEvent> {
    Ok(ChatEvent::Delta {
        text: Some(t.into()),
        tool_calls: vec![],
    })
}
/// Frames available before the upstream stream ends (it stays open forever).
async fn frames_while_open(events: Vec<Result<ChatEvent>>, want: usize) -> Vec<Value> {
    use http_body_util::BodyExt;
    let output = ProviderOutput::Stream(Box::pin(
        futures_util::stream::iter(events).chain(futures_util::stream::pending()),
    ));
    let mut body = render(output, "r".into(), "m".into(), 1).into_body();
    let mut text = String::new();
    while frames(&text).len() < want {
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("frame arrives before upstream completion")
            .unwrap()
            .unwrap();
        if let Ok(data) = frame.into_data() {
            text.push_str(std::str::from_utf8(&data).unwrap());
        }
    }
    frames(&text)
}
#[tokio::test]
async fn text_deltas_reach_the_client_before_upstream_completion() {
    let f = frames_while_open(vec![text("Hel"), text("lo")], 6).await;
    let kinds: Vec<_> = f.iter().map(|v| v["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
        ]
    );
    assert_eq!(f[2]["item"]["id"], "msg_r");
    assert_eq!(f[2]["item"]["status"], "in_progress");
    assert_eq!(f[4]["delta"], "Hel");
    assert_eq!(f[5]["delta"], "lo");
    assert_eq!(f[5]["item_id"], "msg_r");
}
#[tokio::test]
async fn incremental_text_then_tools_keep_snapshot_order() {
    let output = stream(vec![
        text("a"),
        Ok(ChatEvent::Delta {
            text: Some("b".into()),
            tool_calls: vec![ToolCallDelta {
                index: 0,
                id: Some("call".into()),
                name: Some("weather".into()),
                arguments: Some("{\"c\":".into()),
            }],
        }),
        Ok(ChatEvent::Delta {
            text: None,
            tool_calls: vec![ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: Some("1}".into()),
            }],
        }),
        Ok(ChatEvent::Finish(FinishReason::ToolCalls)),
        Ok(ChatEvent::Done),
    ]);
    let f = frames(&body(render(output, "r".into(), "m".into(), 1)).await);
    let kinds: Vec<_> = f.iter().map(|v| v["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    for (i, v) in f.iter().enumerate() {
        assert_eq!(v["sequence_number"], i as u64);
    }
    let terminal = &f.last().unwrap()["response"];
    // Events agree with the terminal snapshot.
    assert_eq!(f[6]["text"], "ab");
    assert_eq!(f[8]["item"], terminal["output"][0]);
    assert_eq!(f[9]["output_index"], 1);
    assert_eq!(f[11]["arguments"], "{\"c\":1}");
    assert_eq!(f[12]["item"], terminal["output"][1]);
    assert_eq!(
        terminal["usage"],
        Value::Null,
        "unknown usage stays unknown"
    );
}
#[tokio::test]
async fn mid_stream_failure_after_deltas_never_completes() {
    let b = body(render(
        stream(vec![
            text("partial"),
            Err(InferenceError::UpstreamUnavailable),
        ]),
        "r".into(),
        "m".into(),
        1,
    ))
    .await;
    assert!(b.contains("event: response.output_text.delta"));
    assert!(b.contains("event: error"));
    assert!(!b.contains("event: response.completed"));
    assert!(!b.contains("event: response.output_text.done"));
}
