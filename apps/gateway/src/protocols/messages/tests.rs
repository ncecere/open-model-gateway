use super::*;
#[test]
fn version_is_exact_and_beta_is_accepted() {
    let mut h = HeaderMap::new();
    assert!(!valid_version(&h));
    h.insert("anthropic-version", "2023-06-01".parse().unwrap());
    assert!(valid_version(&h));
    h.append("anthropic-version", "2023-06-01".parse().unwrap());
    assert!(!valid_version(&h));
    h.insert("anthropic-version", "2024-01-01".parse().unwrap());
    assert!(!valid_version(&h));
    h.insert("anthropic-version", "2023-06-01".parse().unwrap());
    h.insert("anthropic-beta", "future".parse().unwrap());
    assert!(valid_version(&h));
}
fn parse(v: Value) -> Result<ChatRequest> {
    serde_json::from_value::<Request>(v)
        .map_err(|_| InferenceError::InvalidRequest)?
        .normalize()
}
#[test]
fn strict_text_tool_subset() {
    let v = json!({"model":"m","max_tokens":32,"system":[{"type":"text","text":"be brief"}],"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"weather","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":[{"type":"text","text":"sun"}]}]}],"tools":[{"name":"weather","input_schema":{"type":"object"}}],"tool_choice":{"type":"tool","name":"weather"}});
    let mut labelled = v.clone();
    labelled["metadata"] = json!({"user_id":"session-1"});
    assert!(parse(labelled).is_ok());
    let r = parse(v).unwrap();
    assert_eq!(r.messages.len(), 4);
    assert!(r.messages[3].role == Role::Tool);
    assert_eq!(r.messages[2].tool_calls[0].arguments, "{}");
    for extra in [
        json!({"thinking":{}}),
        // metadata.user_id is a Logs session label; other metadata fields are rejected.
        json!({"metadata":{"other":"x"}}),
        json!({"temperature":1.1}),
        json!({"max_tokens":0}),
    ] {
        let mut v =
            json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":"hello"}]});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(parse(v).is_err());
    }
    for block in [
        json!({"type":"image","source":{}}),
        json!({"type":"text","text":"x","cache_control":null}),
        json!({"type":"tool_result","tool_use_id":"c","content":"bad","is_error":null}),
    ] {
        assert!(
            parse(
                json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":[block]}]})
            )
            .is_err()
        );
    }
}
#[test]
fn empty_successful_tool_result_defaults_are_absent_only() {
    let request = |block| {
        json!({"model":"m","max_tokens":32,"messages":[
        {"role":"assistant","content":[{"type":"tool_use","id":"call-ping","name":"ping","input":{}}]},
        {"role":"user","content":[block]}]})
    };
    for content in [
        None,
        Some(json!("")),
        Some(json!([])),
        Some(json!([{"type":"text","text":""}])),
    ] {
        for flag in [None, Some(false)] {
            let mut block = json!({"type":"tool_result","tool_use_id":"call-ping"});
            if let Some(content) = &content {
                block["content"] = content.clone();
            }
            if let Some(flag) = flag {
                block["is_error"] = json!(flag);
            }
            let r = parse(request(block)).unwrap();
            assert_eq!(r.messages[0].tool_calls[0].id, "call-ping");
            assert_eq!(r.messages[1].tool_call_id.as_deref(), Some("call-ping"));
            assert_eq!(r.messages[1].content.as_deref(), Some(""));
            assert!(r.messages[1].role == Role::Tool);
        }
    }
    let error_result = parse(request(
        json!({"type":"tool_result","tool_use_id":"call-ping","is_error":true}),
    ))
    .unwrap();
    assert!(
        crate::providers::messages::validate(
            &error_result,
            crate::providers::messages::ProviderClass::Anthropic
        )
        .is_ok()
    );
    assert!(matches!(
        crate::providers::messages::validate(
            &error_result,
            crate::providers::messages::ProviderClass::OpenAi
        ),
        Err(InferenceError::Unsupported)
    ));
    for block in [
        json!({"type":"tool_result","tool_use_id":"call-ping","content":null}),
        json!({"type":"tool_result","tool_use_id":"call-ping","content":{}}),
        json!({"type":"tool_result","tool_use_id":"call-ping","content":[{"type":"text","text":"x","extra":true}]}),
        json!({"type":"tool_result","tool_use_id":"call-ping","extra":true}),
        json!({"type":"tool_result"}),
        json!({"type":"tool_result","tool_use_id":null}),
        json!({"type":"tool_result","tool_use_id":""}),
    ] {
        assert!(parse(request(block.clone())).is_err(), "{block}");
    }
    for flag in [Value::Null, json!("false"), json!(0), json!([]), json!({})] {
        assert!(
            parse(request(
                json!({"type":"tool_result","tool_use_id":"call-ping","is_error":flag})
            ))
            .is_err()
        );
    }
    // The default must not remove the existing block bound.
    assert!(parse(json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":vec![json!({"type":"tool_result","tool_use_id":"call-ping"});257]}]})).is_err());
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
#[tokio::test]
async fn native_message_blocks_and_usage() {
    let output = stream(vec![
        Ok(ChatEvent::Delta {
            text: Some("世界".into()),
            tool_calls: vec![ToolCallDelta {
                index: 0,
                id: Some("c".into()),
                name: Some("weather".into()),
                arguments: Some("{\"city\":\"東京\"}".into()),
            }],
        }),
        Ok(ChatEvent::Finish(FinishReason::ToolCalls)),
        Ok(ChatEvent::Usage(Usage {
            input_tokens: Some(5),
            output_tokens: None,
            ..Usage::default()
        })),
        Ok(ChatEvent::Done),
    ]);
    let b = body(render(output, "msg_test".into(), "public".into())).await;
    let names = b
        .lines()
        .filter_map(|l| l.strip_prefix("event: "))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    assert!(b.contains("世界"));
    assert!(b.contains("東京"));
    // Unknown output is never reported, not even as message_start's 0; observed input
    // arrives with message_delta.
    assert!(!b.contains("output_tokens"));
    let start = sse_data(&b, "message_start");
    assert_eq!(start["message"]["usage"], json!({"input_tokens":0}));
    let delta = sse_data(&b, "message_delta");
    assert_eq!(delta["usage"], json!({"input_tokens":5}));
}
fn sse_data(body: &str, name: &str) -> Value {
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        if line == format!("event: {name}") {
            let data = lines.next().unwrap().strip_prefix("data: ").unwrap();
            return serde_json::from_str(data).unwrap();
        }
    }
    panic!("{name} missing")
}
/// D3 (streaming): message_start is sent before anything is observed, so its usage is only the
/// `input_tokens: 0` placeholder strict clients require; message_delta carries observed usage.
#[tokio::test]
async fn message_start_usage_has_vendor_shape() {
    let output = stream(vec![
        Ok(ChatEvent::Delta {
            text: Some("hi".into()),
            tool_calls: vec![],
        }),
        Ok(ChatEvent::Finish(FinishReason::Stop)),
        Ok(ChatEvent::Usage(Usage {
            input_tokens: Some(13),
            output_tokens: Some(14),
            ..Usage::default()
        })),
        Ok(ChatEvent::Done),
    ]);
    let b = body(render(output, "msg_test".into(), "public".into())).await;
    let start = sse_data(&b, "message_start");
    assert_eq!(start["message"]["usage"], json!({"input_tokens":0}));
    let delta = sse_data(&b, "message_delta");
    assert_eq!(
        delta["usage"],
        json!({"input_tokens":13,"output_tokens":14})
    );
    assert!(
        b.find("event: message_start").unwrap() < b.find("event: content_block_start").unwrap()
    );
}
#[tokio::test]
async fn malformed_tool_json_and_eof_never_stop() {
    for events in [
        vec![Ok(ChatEvent::Finish(FinishReason::Stop))],
        vec![Err(InferenceError::Timeout)],
        vec![
            Ok(ChatEvent::Delta {
                text: None,
                tool_calls: vec![ToolCallDelta {
                    index: 0,
                    id: Some("c".into()),
                    name: Some("weather".into()),
                    arguments: Some("[1]".into()),
                }],
            }),
            Ok(ChatEvent::Finish(FinishReason::ToolCalls)),
            Ok(ChatEvent::Done),
        ],
    ] {
        let b = body(render(stream(events), "id".into(), "m".into())).await;
        assert!(b.contains("event: error"));
        assert!(!b.contains("event: message_stop"));
        assert!(!b.contains("event: content_block_start"));
    }
}
#[test]
fn missing_usage_not_zero() {
    assert_eq!(usage_json(Usage::default()), json!({}));
}
#[test]
fn exclusive_and_ttl_counts_remain_protocol_native_numbers() {
    let usage = Usage {
        input_tokens: Some(4),
        output_tokens: Some(3),
        billing: Some(crate::billing::BillingUsage {
            total_input_tokens: Some(34),
            uncached_input_tokens: Some(4),
            cache_read_input_tokens: Some(10),
            cache_write_input_tokens: Some(20),
            cache_write_default_input_tokens: Some(0),
            cache_write_5m_input_tokens: Some(8),
            cache_write_1h_input_tokens: Some(12),
        }),
        ..Default::default()
    };
    let value = usage_json(usage);
    assert_eq!(value["input_tokens"], 4);
    assert_eq!(value["cache_creation_input_tokens"], 20);
    assert_eq!(value["cache_creation"]["ephemeral_1h_input_tokens"], 12);
    assert!(value.get("total_input_tokens").is_none());
}
fn text(t: &str) -> Result<ChatEvent> {
    Ok(ChatEvent::Delta {
        text: Some(t.into()),
        tool_calls: vec![],
    })
}
/// (event name, data) pairs available before the upstream stream ends (it stays open).
async fn events_while_open(events: Vec<Result<ChatEvent>>, want: usize) -> Vec<(String, Value)> {
    use http_body_util::BodyExt;
    let output = ProviderOutput::Stream(Box::pin(
        futures_util::stream::iter(events).chain(futures_util::stream::pending()),
    ));
    let mut body = render(output, "msg_r".into(), "m".into()).into_body();
    let mut raw = String::new();
    loop {
        let parsed = parse_events(&raw);
        if parsed.len() >= want {
            return parsed;
        }
        let frame = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("event arrives before upstream completion")
            .unwrap()
            .unwrap();
        if let Ok(data) = frame.into_data() {
            raw.push_str(std::str::from_utf8(&data).unwrap());
        }
    }
}
fn parse_events(body: &str) -> Vec<(String, Value)> {
    let mut out = vec![];
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        if let Some(name) = line.strip_prefix("event: ")
            && let Some(data) = lines.next().and_then(|l| l.strip_prefix("data: "))
        {
            out.push((name.to_string(), serde_json::from_str(data).unwrap()));
        }
    }
    out
}
#[tokio::test]
async fn text_deltas_reach_the_client_before_upstream_completion() {
    let e = events_while_open(vec![text("Hel"), text("lo")], 4).await;
    let names: Vec<_> = e.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta"
        ]
    );
    // Nothing is observed yet: only the input placeholder, never an output count.
    assert_eq!(e[0].1["message"]["usage"], json!({"input_tokens":0}));
    assert_eq!(
        e[1].1,
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
    );
    assert_eq!(e[2].1["delta"], json!({"type":"text_delta","text":"Hel"}));
    assert_eq!(e[3].1["delta"], json!({"type":"text_delta","text":"lo"}));
}
#[tokio::test]
async fn incremental_text_then_whole_tool_blocks_and_final_usage() {
    let output = stream(vec![
        text("a"),
        Ok(ChatEvent::Delta {
            text: Some("b".into()),
            tool_calls: vec![ToolCallDelta {
                index: 0,
                id: Some("c".into()),
                name: Some("weather".into()),
                arguments: Some("{\"city\":".into()),
            }],
        }),
        Ok(ChatEvent::Delta {
            text: None,
            tool_calls: vec![ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments: Some("\"x\"}".into()),
            }],
        }),
        Ok(ChatEvent::Finish(FinishReason::ToolCalls)),
        Ok(ChatEvent::Usage(Usage {
            input_tokens: Some(5),
            output_tokens: Some(7),
            ..Usage::default()
        })),
        Ok(ChatEvent::Done),
    ]);
    let e = parse_events(&body(render(output, "msg_r".into(), "m".into())).await);
    let names: Vec<_> = e.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    assert_eq!(e[4].1["index"], 0);
    assert_eq!(
        e[5].1["content_block"],
        json!({"type":"tool_use","id":"c","name":"weather","input":{}})
    );
    assert_eq!(e[5].1["index"], 1);
    assert_eq!(
        e[6].1["delta"],
        json!({"type":"input_json_delta","partial_json":"{\"city\":\"x\"}"})
    );
    assert_eq!(e[8].1["delta"]["stop_reason"], "tool_use");
    // Observed usage arrives with message_delta (the SDK overwrites input when present).
    assert_eq!(e[8].1["usage"], json!({"input_tokens":5,"output_tokens":7}));
}
#[tokio::test]
async fn mid_stream_failure_after_text_never_stops() {
    let b = body(render(
        stream(vec![
            text("partial"),
            Err(InferenceError::UpstreamUnavailable),
        ]),
        "id".into(),
        "m".into(),
    ))
    .await;
    assert!(b.contains("event: content_block_delta"));
    assert!(b.contains("event: error"));
    assert!(!b.contains("event: message_delta"));
    assert!(!b.contains("event: message_stop"));
}
