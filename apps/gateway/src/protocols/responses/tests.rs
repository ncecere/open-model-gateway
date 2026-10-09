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
