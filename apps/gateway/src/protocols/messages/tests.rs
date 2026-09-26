use super::*;
#[test]
fn version_is_exact_and_beta_is_rejected() {
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
    assert!(!valid_version(&h));
}
fn parse(v: Value) -> Result<ChatRequest> {
    serde_json::from_value::<Request>(v)
        .map_err(|_| InferenceError::InvalidRequest)?
        .normalize()
}
#[test]
fn strict_text_tool_subset() {
    let v = json!({"model":"m","max_tokens":32,"system":[{"type":"text","text":"be brief"}],"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"weather","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":[{"type":"text","text":"sun"}]}]}],"tools":[{"name":"weather","input_schema":{"type":"object"}}],"tool_choice":{"type":"tool","name":"weather"}});
    let r = parse(v).unwrap();
    assert_eq!(r.messages.len(), 4);
    assert!(r.messages[3].role == Role::Tool);
    assert_eq!(r.messages[2].tool_calls[0].arguments, "{}");
    for extra in [
        json!({"thinking":{}}),
        json!({"metadata":{}}),
        json!({"temperature":1.1}),
        json!({"max_tokens":0}),
        json!({"system":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}),
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
        json!({"type":"tool_result","tool_use_id":"c","content":"bad","is_error":true}),
    ] {
        assert!(
            parse(
                json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":[block]}]})
            )
            .is_err()
        );
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
    assert!(!b.contains("output_tokens"));
    assert!(b.contains("input_tokens"));
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
