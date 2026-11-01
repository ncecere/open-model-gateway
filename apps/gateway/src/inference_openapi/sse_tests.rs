use super::sse::{Outcome, Protocol, validate};
use serde_json::{Value, json};

fn frame(event: Option<&str>, body: Value) -> String {
    format!(
        "{}data: {body}\n\n",
        event.map(|e| format!("event: {e}\n")).unwrap_or_default()
    )
}
fn role() -> String {
    frame(
        None,
        json!({"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}),
    )
}
fn response(event: &str, seq: u64) -> String {
    frame(Some(event), json!({"type":event,"sequence_number":seq}))
}
fn message(event: &str, extra: Value) -> String {
    let mut body = extra;
    body["type"] = event.into();
    frame(Some(event), body)
}
fn responses() -> String {
    response("response.created", 0)
        + &response("response.in_progress", 1)
        + &response("response.completed", 2)
}
fn messages() -> String {
    message("message_start", json!({}))
        + &message("content_block_start", json!({"index":0}))
        + &message("content_block_delta", json!({"index":0}))
        + &message("content_block_stop", json!({"index":0}))
        + &message("message_delta", json!({}))
        + &message("message_stop", json!({}))
}
#[test]
fn inference_openapi_sse_completed_and_truncated() {
    for (protocol, wire) in [
        (
            Protocol::Chat {
                include_usage: false,
            },
            role() + "data: [DONE]\n\n",
        ),
        (Protocol::Responses, responses()),
        (Protocol::Messages, messages()),
    ] {
        assert_eq!(
            validate(protocol, wire.as_bytes()).unwrap(),
            Outcome::Completed
        );
        assert!(validate(protocol, wire.as_bytes()).unwrap().positive());
        let boundary = wire.trim_end().rfind("\n\n").unwrap() + 2;
        let truncated = validate(protocol, &wire.as_bytes()[..boundary]).unwrap();
        assert_eq!(
            truncated,
            Outcome::Truncated,
            "missing terminal must truncate"
        );
        assert!(
            !truncated.positive(),
            "truncated stream must not earn coverage"
        );
    }
}
#[test]
fn inference_openapi_sse_terminal_mutations() {
    for (protocol, wire, terminal) in [
        (
            Protocol::Chat {
                include_usage: false,
            },
            role() + "data: [DONE]\n\n",
            "data: [DONE]\n\n".to_owned(),
        ),
        (
            Protocol::Responses,
            responses(),
            response("response.completed", 2),
        ),
        (
            Protocol::Messages,
            messages(),
            message("message_stop", json!({})),
        ),
    ] {
        for (case, bad) in [
            ("duplicate terminal", wire.clone() + &terminal),
            ("terminal first", terminal.clone() + &wire),
            (
                "event after terminal",
                wire.clone() + &frame(Some("invented"), json!({})),
            ),
            ("invented event", frame(Some("invented"), json!({})) + &wire),
        ] {
            assert!(
                validate(protocol, bad.as_bytes()).is_err(),
                "accepted {case}"
            );
        }
    }
}
#[test]
fn inference_openapi_sse_responses_sequence_and_incomplete() {
    let prefix = response("response.created", 0) + &response("response.in_progress", 1);
    let gap = prefix.clone() + &response("response.completed", 3);
    assert!(
        validate(Protocol::Responses, gap.as_bytes()).is_err(),
        "accepted sequence gap"
    );
    let incomplete = prefix + &response("response.incomplete", 2);
    assert_eq!(
        validate(Protocol::Responses, incomplete.as_bytes()).unwrap(),
        Outcome::Incomplete
    );
}
#[test]
fn inference_openapi_sse_error_is_terminal() {
    for (protocol, prefix, error, terminal) in [
        (
            Protocol::Chat {
                include_usage: false,
            },
            role(),
            frame(
                None,
                json!({"error":{"message":"unavailable","type":"server_error","code":"invalid_upstream_response","param":null}}),
            ),
            "data: [DONE]\n\n".to_owned(),
        ),
        (
            Protocol::Responses,
            response("response.created", 0) + &response("response.in_progress", 1),
            frame(
                Some("error"),
                json!({"type":"error","sequence_number":2,"code":"invalid_upstream_response","message":"unavailable","param":null}),
            ),
            response("response.completed", 3),
        ),
        (
            Protocol::Messages,
            message("message_start", json!({})),
            message(
                "error",
                json!({"error":{"type":"api_error","message":"unavailable"}}),
            ),
            message("message_stop", json!({})),
        ),
    ] {
        let wire = prefix + &error;
        assert_eq!(
            validate(protocol, wire.as_bytes()).unwrap(),
            Outcome::Errored
        );
        assert!(
            validate(protocol, (wire + &terminal).as_bytes()).is_err(),
            "accepted success after error"
        );
    }
}
#[test]
fn inference_openapi_sse_chat_usage_and_first_role() {
    let usage = frame(
        None,
        json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
    );
    let wire = role() + &usage + "data: [DONE]\n\n";
    assert!(
        validate(
            Protocol::Chat {
                include_usage: false
            },
            wire.as_bytes()
        )
        .is_err(),
        "accepted unrequested usage"
    );
    assert_eq!(
        validate(
            Protocol::Chat {
                include_usage: true
            },
            wire.as_bytes()
        )
        .unwrap(),
        Outcome::Completed
    );
    assert!(
        validate(
            Protocol::Chat {
                include_usage: false
            },
            b"data: [DONE]\n\n"
        )
        .is_err(),
        "accepted missing role"
    );
}
#[test]
fn inference_openapi_sse_messages_balanced_blocks() {
    let prefix = message("message_start", json!({}));
    for (case, tail) in [
        (
            "delta without start",
            message("content_block_delta", json!({"index":0})),
        ),
        (
            "stop without start",
            message("content_block_stop", json!({"index":0})),
        ),
        (
            "unclosed block",
            message("content_block_start", json!({"index":0}))
                + &message("message_delta", json!({})),
        ),
        ("stop without delta", message("message_stop", json!({}))),
    ] {
        assert!(
            validate(Protocol::Messages, (prefix.clone() + &tail).as_bytes()).is_err(),
            "accepted {case}"
        );
    }
}
#[test]
fn inference_openapi_sse_parser_comments_multiline_and_trailing_bytes() {
    let wire = ": keep-alive\r\n\r\ndata: {\r\ndata: \"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\r\n\r\ndata: [DONE]\r\n\r\n";
    assert_eq!(
        validate(
            Protocol::Chat {
                include_usage: false
            },
            wire.as_bytes()
        )
        .unwrap(),
        Outcome::Completed
    );
    let truncated = role() + "data: [DONE]";
    assert_eq!(
        validate(
            Protocol::Chat {
                include_usage: false
            },
            truncated.as_bytes()
        )
        .unwrap(),
        Outcome::Truncated
    );
    assert!(
        validate(
            Protocol::Responses,
            b"event: response.created\ndata: not-json\n\n"
        )
        .is_err(),
        "accepted malformed JSON"
    );
}
