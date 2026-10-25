//! Delivery 2: exact native wire, not general request/header passthrough.
use super::{tests::*, *};

fn native(body: Value) -> ChatRequest {
    crate::protocols::messages::batch_request(body).unwrap()
}
fn fixture() -> Value {
    json!({"model":"public","max_tokens":8192,
    "system":[{"type":"text","text":"one","cache_control":{"type":"ephemeral","ttl":"1h"}},{"type":"text","text":"two"}],
    "metadata":{"user_id":"http://attacker.invalid/label"},
    "thinking":{"type":"enabled","budget_tokens":1024},
    "output_config":{"effort":"high","task_budget":{"type":"tokens","total":64000,"remaining":32000}},
    "context_management":{"edits":[{"type":"clear_tool_uses_20250919","exclude_tools":["http://attacker.invalid/tool"]}]},
    "tools":[{"name":"lookup","input_schema":{"type":"object"},"defer_loading":true,"cache_control":{"type":"ephemeral","ttl":"1h"}}],
    "messages":[
        {"role":"assistant","content":[{"type":"thinking","thinking":"http://attacker.invalid/reason","signature":"opaque+/=="},{"type":"redacted_thinking","data":"opaque-data"},{"type":"tool_use","id":"c","name":"lookup","input":{"url":"http://attacker.invalid/input"},"cache_control":{"type":"ephemeral"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"c","is_error":true,"content":[{"type":"text","text":"failed","cache_control":{"type":"ephemeral"}},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}],"cache_control":{"type":"ephemeral","ttl":"1h"}},{"type":"image","source":{"type":"base64","media_type":"image/gif","data":"R0lGODlh"}},{"type":"text","text":"continue","cache_control":{"type":"ephemeral"}}]}
    ]})
}
#[tokio::test]
async fn native_fidelity_exact_upstream_wire_and_headers() {
    for thinking in [
        json!({"type":"enabled","budget_tokens":1024}),
        json!({"type":"adaptive"}),
    ] {
        let mock = Mock::new(200, None).await;
        let mut wire = fixture();
        wire["thinking"] = thinking;
        let mut request = native(wire.clone());
        request.messages_options.as_mut().unwrap().beta_headers = vec![
            "interleaved-thinking-2025-05-14, context-management-2025-06-27".into(),
            "fine-grained-tool-streaming-2025-05-14".into(),
        ];
        mock.adapter.execute(&target(), request).await.unwrap();
        let capture = mock.capture.lock().unwrap();
        let (headers, uri, body) = &capture[0];
        assert_eq!(uri.path(), "/v1/messages");
        assert!(uri.query().is_none());
        assert_eq!(headers["x-api-key"], "mock-key");
        assert!(!headers.contains_key("authorization"));
        assert_eq!(
            headers
                .get_all("anthropic-beta")
                .iter()
                .map(|v| v.to_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "interleaved-thinking-2025-05-14, context-management-2025-06-27",
                "fine-grained-tool-streaming-2025-05-14"
            ]
        );
        wire.as_object_mut().unwrap().remove("metadata");
        wire["model"] = json!("private-model");
        wire["stream"] = json!(false);
        assert_eq!(body, &wire);
    }
}
#[tokio::test]
async fn thinking_signatures_and_redacted_blocks_round_trip() {
    let blocks = json!([{"type":"thinking","thinking":"reason","signature":"signed+/=="},{"type":"redacted_thinking","data":"encrypted+/=="},{"type":"text","text":"answer"}]);
    let mut response = complete();
    response["content"] = blocks.clone();
    let mock = Mock::new(200, Some(response.to_string().into_bytes())).await;
    let ProviderOutput::Complete(response) = mock
        .adapter
        .execute(&target(), native(fixture()))
        .await
        .unwrap()
    else {
        panic!()
    };
    let body = crate::protocols::messages::batch_response("id", "public", &response).unwrap();
    assert_eq!(body["content"], blocks);
    assert_eq!(body["model"], "public");
}
#[tokio::test]
async fn thinking_signature_deltas_stream_without_reconstruction() {
    let native_events = vec![
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed+/=="}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"opaque+/=="}}),
        json!({"type":"content_block_stop","index":1}),
    ];
    let mut frames = vec![stream()[0].clone()];
    frames.extend(native_events.clone());
    frames.extend(stream()[4..].iter().cloned());
    for terminal in [true, false] {
        let frames = if terminal {
            &frames[..]
        } else {
            &frames[..frames.len() - 1]
        };
        let bytes = frames
            .iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect::<String>()
            .into_bytes();
        let mock = Mock::new(200, Some(bytes)).await;
        let mut r = native(fixture());
        r.stream = true;
        let ProviderOutput::Stream(s) = mock.adapter.execute(&target(), r).await.unwrap() else {
            panic!()
        };
        let events: Vec<_> = s.collect().await;
        let observed: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Ok(ChatEvent::MessagesBlock(v)) => Some(v.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(observed, native_events);
        assert_eq!(
            events.iter().any(|e| matches!(e, Ok(ChatEvent::Done))),
            terminal
        );
        assert_eq!(events.last().unwrap().is_err(), !terminal);
    }
}
#[test]
fn native_images_reject_remote_urls_bad_base64_and_type_mismatches() {
    for source in [
        json!({"type":"url","url":"http://127.0.0.1:1/private"}),
        json!({"type":"url","url":"https://attacker.invalid/image"}),
        json!({"type":"base64","media_type":"image/png","data":"not base64"}),
        json!({"type":"base64","media_type":"image/jpeg","data":"iVBORw0KGgo="}),
        json!({"type":"base64","media_type":"image/svg+xml","data":"PHN2Zz4="}),
        json!({"type":"base64","media_type":"image/png","data":"A".repeat(8*1024*1024)}),
    ] {
        let body = json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":[{"type":"image","source":source}]}]});
        assert!(crate::protocols::messages::batch_request(body).is_err());
    }
}
#[test]
fn image_types_count_and_data_limits_are_enforced_before_dispatch() {
    use base64::Engine as _;
    for (media, bytes) in [
        ("image/png", &b"\x89PNG\r\n\x1a\n"[..]),
        ("image/jpeg", &[0xff, 0xd8, 0xff][..]),
        ("image/gif", &b"GIF89a"[..]),
        ("image/webp", &b"RIFF0000WEBP"[..]),
    ] {
        let block = json!({"type":"image","source":{"type":"base64","media_type":media,"data":base64::engine::general_purpose::STANDARD.encode(bytes)}});
        let body = |blocks| json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":blocks}]});
        let r = native(body(vec![block.clone()]));
        assert_eq!(
            encode("private", &r).unwrap()["messages"][0]["content"][0],
            block
        );
        assert!(crate::protocols::messages::batch_request(body(vec![block; 21])).is_err());
    }
}

#[test]
fn complete_thinking_is_native_messages_only() {
    let mut body = complete();
    body["content"] = json!([{"type":"thinking","thinking":"reason","signature":"opaque"},{"type":"redacted_thinking","data":"opaque"}]);
    assert!(decode_for(&body, true).is_ok());
    assert!(matches!(
        decode_for(&body, false),
        Err(InferenceError::InvalidUpstream)
    ));
}

#[test]
fn cached_native_batch_lines_use_request_aware_gateway_admission() {
    let request = crate::jobs::types::BatchRequest::Chat(native(fixture()));
    assert!(matches!(
        batches::encode_native_line(
            &target(),
            crate::jobs::types::BatchEndpoint::Messages,
            "l0",
            &request
        ),
        Err(InferenceError::InvalidField("cache_control"))
    ));
}

#[test]
fn malformed_native_thinking_streams_fail_without_done() {
    for delta in [
        json!({"type":"signature_delta","signature":7}),
        json!({"type":"thinking_delta","thinking":"reason","headers":{"Host":"attacker.invalid"}}),
        json!({"type":"text_delta","text":"wrong block"}),
    ] {
        let mut state = State {
            messages: true,
            ..Default::default()
        };
        state.push(stream()[0].clone()).unwrap();
        state.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}})).unwrap();
        assert!(
            state
                .push(json!({"type":"content_block_delta","index":0,"delta":delta}))
                .is_err()
        );
    }
}

#[test]
fn translated_error_results_remain_unsupported() {
    let r = native(fixture());
    for class in [
        super::super::messages::ProviderClass::OpenAi,
        super::super::messages::ProviderClass::Bedrock,
        super::super::messages::ProviderClass::Local,
    ] {
        // Image refusal is independent of native error-result support.
        let mut r = r.clone();
        r.messages_options.as_mut().unwrap().images.clear();
        assert!(matches!(
            super::super::messages::validate(&r, class),
            Err(InferenceError::Unsupported)
        ));
    }
}
