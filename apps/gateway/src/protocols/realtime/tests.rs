use super::*;
use axum::http::HeaderValue;

const KEY: &str = "omg_0123456789abcdef0123456789abcdef.0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.append(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    h
}

#[test]
fn exactly_one_inference_key_from_header_or_subprotocol() {
    let bearer = format!("Bearer {KEY}");
    let protocol = format!("realtime, openai-insecure-api-key.{KEY}");
    assert_eq!(
        credential(&headers(&[("authorization", &bearer)])).as_deref(),
        Some(KEY)
    );
    assert_eq!(
        credential(&headers(&[("sec-websocket-protocol", &protocol)])).as_deref(),
        Some(KEY)
    );
    // Split across header lines as browsers may send them.
    assert_eq!(
        credential(&headers(&[
            ("sec-websocket-protocol", "realtime"),
            (
                "sec-websocket-protocol",
                &format!("openai-insecure-api-key.{KEY}")
            )
        ]))
        .as_deref(),
        Some(KEY)
    );
    for ambiguous in [
        headers(&[]),
        headers(&[("sec-websocket-protocol", "realtime")]),
        headers(&[
            ("authorization", &bearer),
            ("sec-websocket-protocol", &protocol),
        ]),
        headers(&[("authorization", &bearer), ("authorization", &bearer)]),
        headers(&[("authorization", &bearer), ("x-api-key", KEY)]),
        headers(&[("x-api-key", KEY)]),
        headers(&[("authorization", &format!("Basic {KEY}"))]),
        headers(&[(
            "sec-websocket-protocol",
            &format!("openai-insecure-api-key.{KEY}, openai-insecure-api-key.other"),
        )]),
        headers(&[("sec-websocket-protocol", "openai-insecure-api-key.")]),
        headers(&[("sec-websocket-protocol", "realtime,,x")]),
    ] {
        assert_eq!(credential(&ambiguous), None, "{ambiguous:?}");
    }
}

#[test]
fn only_one_model_query_parameter() {
    let uri = |q: &str| format!("/v1/realtime{q}").parse::<Uri>().unwrap();
    assert_eq!(
        model_param(&uri("?model=company%2Fvoice")).unwrap(),
        "company/voice"
    );
    assert_eq!(model_param(&uri("?model=a+b")).unwrap(), "a b");
    for bad in [
        "",
        "?model=",
        "?model=a&model=b",
        "?model=a&call_id=x",
        "?call_id=x",
        "?model=%FF",
    ] {
        assert!(model_param(&uri(bad)).is_err(), "{bad}");
    }
}

#[test]
fn client_events_follow_the_allowlist() {
    let ok = |v: Value| validate(&v.to_string(), 100).unwrap();
    let err = |v: Value| validate(&v.to_string(), 100).unwrap_err();
    for forward in [
        json!({"type":"input_audio_buffer.append","audio":"AAAA","event_id":"e1"}),
        json!({"type":"input_audio_buffer.commit"}),
        json!({"type":"input_audio_buffer.clear"}),
        json!({"type":"output_audio_buffer.clear"}),
        json!({"type":"conversation.item.create","item":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"},{"type":"input_audio","audio":"AAAA"}]}}),
        json!({"type":"conversation.item.create","item":{"type":"function_call_output","call_id":"c","output":"{}"}}),
        json!({"type":"conversation.item.delete","item_id":"item_1"}),
        json!({"type":"conversation.item.retrieve","item_id":"item_1"}),
        json!({"type":"conversation.item.truncate","item_id":"item_1","content_index":0,"audio_end_ms":10}),
        json!({"type":"response.cancel"}),
        json!({"type":"session.update","session":{"type":"realtime","instructions":"x","output_modalities":["text"],"max_output_tokens":100,
            "audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"noise_reduction":null,"transcription":null,
                "turn_detection":{"type":"semantic_vad","create_response":false,"eagerness":"low"}},"output":{"voice":"marin","speed":1.1}},
            "tools":[{"type":"function","name":"f","parameters":{}}],"tool_choice":"auto","truncation":"auto"}}),
        json!({"type":"session.update","session":{"type":"realtime","audio":{"input":{"turn_detection":null}}}}),
    ] {
        let text = forward.to_string();
        assert!(matches!(ok(forward), ClientAction::Forward(t) if t == text));
    }
    // response.create gets an explicit per-response ceiling and an event id.
    match ok(json!({"type":"response.create"})) {
        ClientAction::ResponseCreate { text, event_id } => {
            let v: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(v["response"]["max_output_tokens"], 100);
            assert_eq!(v["event_id"], event_id.as_str());
            assert!(event_id.starts_with("event_gw_"));
        }
        _ => panic!(),
    }
    match ok(
        json!({"type":"response.create","event_id":"mine","response":{"max_output_tokens":7,"conversation":"auto","metadata":{"k":"v"},"audio":{"output":{"voice":"marin"}}}}),
    ) {
        ClientAction::ResponseCreate { text, event_id } => {
            assert_eq!(event_id, "mine");
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap()["response"]["max_output_tokens"],
                7
            );
        }
        _ => panic!(),
    }
    for (event, code) in [
        (
            json!({"type":"transcription_session.update"}),
            "unsupported_event",
        ),
        (
            json!({"type":"response.create","response":{"max_output_tokens":"inf"}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"response.create","response":{"max_output_tokens":101}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"response.create","response":{"conversation":"none"}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"response.create","response":{"input":[]}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"response.create","response":{"tools":[{"type":"mcp","server_url":"https://x"}]}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"session.update","session":{"type":"transcription"}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"session.update","session":{"type":"realtime","model":"gpt-realtime"}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"session.update","session":{"type":"realtime","tracing":"auto"}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"session.update","session":{"type":"realtime","audio":{"input":{"transcription":{"model":"whisper-1"}}}}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"session.update","session":{"type":"realtime","audio":{"input":{"turn_detection":{"type":"server_vad"}}}}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"session.update","session":{"type":"realtime","audio":{"input":{"turn_detection":{"type":"server_vad","create_response":false,"idle_timeout_ms":3000}}}}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"conversation.item.create","item":{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:"}]}}),
            "unsupported_capability",
        ),
        (
            json!({"type":"input_audio_buffer.append","audio":"AA","extra":1}),
            "unsupported_capability",
        ),
        (
            json!({"type":"input_audio_buffer.append"}),
            "invalid_request_error",
        ),
        (
            json!({"type":"response.cancel","event_id":""}),
            "invalid_request_error",
        ),
        (json!(["not an object"]), "invalid_request_error"),
    ] {
        let rejection = err(event.clone());
        assert_eq!(rejection.code, code, "{event}");
        assert!(rejection.fatal);
    }
    let rejection = err(json!({"type":"nope","event_id":"client-1"}));
    assert_eq!(rejection.event_id.as_deref(), Some("client-1"));
    assert!(validate("{", 100).is_err());
}

#[test]
fn session_events_show_the_public_alias_and_errors_are_generic() {
    let event = json!({"type":"session.created","session":{"model":"gpt-realtime-private","type":"realtime"}});
    let text = session_text(event, "company/voice");
    assert!(text.contains("company/voice") && !text.contains("private"));
    let e: Value = serde_json::from_str(&inference_error_text(InferenceError::BudgetExceeded(
        crate::inference::error::LimitScope::Workspace,
    )))
    .unwrap();
    assert_eq!(e["type"], "error");
    assert_eq!(e["error"]["type"], "insufficient_quota");
    assert_eq!(e["error"]["code"], "budget_exceeded");
    assert!(is_output_delta(
        r#"{"type":"response.output_audio.delta","delta":"x"}"#
    ));
    assert!(!is_output_delta(r#"{"type":"response.done"}"#));
}

#[test]
fn realtime_limits_are_bounded() {
    use crate::inference::realtime::RealtimeLimits;
    assert!(RealtimeLimits::default().validate());
    let get = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    };
    let limits = RealtimeLimits::from_lookup(get(&[
        ("GATEWAY_REALTIME_MAX_SESSION_SECONDS", "60"),
        ("GATEWAY_REALTIME_MAX_OUTPUT_TOKENS", "512"),
    ]))
    .unwrap();
    assert_eq!(limits.max_session.as_secs(), 60);
    assert_eq!(limits.max_output_tokens, 512);
    for bad in [
        &[("GATEWAY_REALTIME_MAX_SESSION_SECONDS", "3601")][..],
        &[("GATEWAY_REALTIME_MAX_SESSION_SECONDS", "x")][..],
        &[("GATEWAY_REALTIME_MAX_OUTPUT_TOKENS", "4097")][..],
        &[("GATEWAY_REALTIME_MAX_MESSAGE_BYTES", "10")][..],
        &[("GATEWAY_REALTIME_MAX_EVENTS_PER_SECOND", "0")][..],
        &[("GATEWAY_REALTIME_IDLE_SECONDS", "1")][..],
    ] {
        let pairs: &'static [(&'static str, &'static str)] = bad;
        assert!(RealtimeLimits::from_lookup(get(pairs)).is_err());
    }
}

#[cfg(feature = "integration-tests")]
mod contract;
