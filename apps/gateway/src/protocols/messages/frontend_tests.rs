//! #70 frontend contracts. These tests deliberately precede the implementation.
use super::*;
use crate::providers::messages::ProviderClass;

fn request(extra: Value) -> Result<ChatRequest> {
    let mut value = json!({"model":"configured-by-name","max_tokens":8192,"messages":[{"role":"user","content":"hello"}]});
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    parse_request(value)
}

#[test]
fn claude_code_field_rules_by_provider_class() {
    let classes = [
        ProviderClass::Anthropic,
        ProviderClass::OpenAi,
        ProviderClass::Bedrock,
        ProviderClass::Local,
    ];
    let cases = [
        (
            json!({"system":[{"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}}]}),
            "cache_control",
            false,
        ),
        (json!({"thinking":{"type":"adaptive"}}), "thinking", false),
        (
            json!({"thinking":{"type":"enabled","budget_tokens":1024}}),
            "thinking",
            false,
        ),
        (
            json!({"output_config":{"effort":"high"}}),
            "output_config.effort",
            false,
        ),
        (
            json!({"context_management":{"edits":[{"type":"clear_tool_uses_20250919","keep":{"type":"tool_uses","value":3}}]}}),
            "context_management",
            false,
        ),
        (
            json!({"output_config":{"task_budget":{"type":"tokens","total":64000,"remaining":32000}}}),
            "output_config.task_budget",
            false,
        ),
        (
            json!({"tools":[{"name":"lookup","input_schema":{"type":"object"},"defer_loading":true}]}),
            "defer_loading",
            false,
        ),
        (
            json!({"output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}}),
            "output_config.format",
            true,
        ),
        (
            json!({"tools":[{"name":"lookup","input_schema":{"type":"object"},"strict":true}]}),
            "strict",
            true,
        ),
        (
            json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}}]}]}),
            "image",
            true,
        ),
        (
            json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"lookup","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}}]}]}]}),
            "image",
            true,
        ),
        (
            json!({"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"private reasoning","signature":"opaque"},{"type":"redacted_thinking","data":"opaque"},{"type":"text","text":"answer"}]},{"role":"user","content":"continue"}]}),
            "thinking",
            false,
        ),
    ];
    for (body, field, always_refuse) in cases {
        for class in classes {
            let parsed = request(body.clone()).unwrap();
            let verdict = crate::providers::messages::validate(&parsed, class);
            if always_refuse || class == ProviderClass::Anthropic {
                assert!(
                    matches!(verdict, Err(InferenceError::InvalidField(f)) if f == field),
                    "{field}"
                );
            } else {
                assert!(verdict.is_ok(), "{field}");
            }
            assert_eq!(parsed.max_output_tokens, Some(8192));
        }
    }
}

#[test]
fn cache_hints_have_the_same_rule_at_every_supported_position() {
    let cache = json!({"type":"ephemeral","ttl":"1h"});
    for extra in [
        json!({"messages":[{"role":"user","content":[{"type":"text","text":"hi","cache_control":cache}]}]}),
        json!({"tools":[{"name":"lookup","input_schema":{"type":"object"},"cache_control":cache}]}),
        json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"lookup","input":{},"cache_control":cache}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":"result"}]}]}),
        json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"lookup","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","cache_control":cache,"content":[{"type":"text","text":"result","cache_control":cache}]}]}]}),
    ] {
        let r = request(extra).unwrap();
        assert!(
            !r.messages_options
                .as_ref()
                .unwrap()
                .cache_controls
                .is_empty()
        );
        assert!(matches!(
            crate::providers::messages::validate(&r, ProviderClass::Anthropic),
            Err(InferenceError::InvalidField("cache_control"))
        ));
        for class in [
            ProviderClass::OpenAi,
            ProviderClass::Bedrock,
            ProviderClass::Local,
        ] {
            assert!(crate::providers::messages::validate(&r, class).is_ok());
        }
    }
}

#[test]
fn parallel_calls_share_one_assistant_turn_without_collapsing_neighbor_turns() {
    let r = crate::providers::contract::parallel_messages_request();
    assert_eq!(r.messages.len(), 6);
    assert!(r.messages[0].role == Role::User);
    assert_eq!(r.messages[0].content.as_deref(), Some("hello there"));
    assert!(r.messages[1].role == Role::Assistant);
    assert_eq!(r.messages[1].content.as_deref(), Some("checking both!"));
    assert_eq!(r.messages[1].tool_calls.len(), 2);
    assert_eq!(r.messages[1].tool_calls[0].id, "first-call");
    assert_eq!(r.messages[1].tool_calls[1].id, "second-call");
    assert_eq!(r.messages[2].tool_call_id.as_deref(), Some("second-call"));
    assert_eq!(r.messages[3].tool_call_id.as_deref(), Some("first-call"));
    assert!(r.messages[4].role == Role::Assistant);
    assert_eq!(r.messages[4].content.as_deref(), Some("done"));
    assert!(r.messages[5].role == Role::User);
}

#[test]
fn wire_strict_presence_does_not_reinterpret_shared_adapter_tool_settings() {
    let mut r =
        request(json!({"tools":[{"name":"lookup","input_schema":{"type":"object"}}]})).unwrap();
    for strict in [None, Some(false), Some(true)] {
        r.tools[0].strict = strict;
        for class in [
            ProviderClass::Anthropic,
            ProviderClass::OpenAi,
            ProviderClass::Bedrock,
            ProviderClass::Local,
        ] {
            assert!(crate::providers::messages::validate(&r, class).is_ok());
        }
    }
    for strict in [false, true] {
        let r = request(
            json!({"tools":[{"name":"lookup","input_schema":{"type":"object"},"strict":strict}]}),
        )
        .unwrap();
        for class in [
            ProviderClass::Anthropic,
            ProviderClass::OpenAi,
            ProviderClass::Bedrock,
            ProviderClass::Local,
        ] {
            assert!(matches!(
                crate::providers::messages::validate(&r, class),
                Err(InferenceError::InvalidField("strict"))
            ));
        }
    }
}

#[test]
fn system_text_and_sampling_are_not_lost() {
    let r = request(json!({"system":[{"type":"text","text":"one"},{"type":"text","text":"two"}],"top_p":0.8,"top_k":40,"stop_sequences":["STOP"],"metadata":{"user_id":"client-label"}})).unwrap();
    assert_eq!(r.messages[0].content.as_deref(), Some("one"));
    assert_eq!(r.messages[1].content.as_deref(), Some("two"));
    let options = r.messages_options.as_ref().unwrap();
    assert_eq!(options.top_p, Some(0.8));
    assert_eq!(options.top_k, Some(40));
    assert_eq!(
        options.stop_sequences.as_deref(),
        Some(&["STOP".to_string()][..])
    );
    assert!(crate::providers::messages::validate(&r, ProviderClass::Anthropic).is_ok());
    for class in [
        ProviderClass::OpenAi,
        ProviderClass::Bedrock,
        ProviderClass::Local,
    ] {
        assert!(matches!(
            crate::providers::messages::validate(&r, class),
            Err(InferenceError::InvalidField("top_k"))
        ));
    }
}

#[test]
fn server_tools_unknown_and_malformed_fields_are_refused() {
    for tools in [
        json!([{"type":"web_search_20250305","name":"web_search","max_uses":1}]),
        json!([{"type":"code_execution_20250825","name":"code_execution"}]),
    ] {
        assert!(matches!(
            request(json!({"tools":tools})),
            Err(InferenceError::InvalidField("tools.type"))
        ));
    }
    for extra in [
        json!({"future_field":null}),
        json!({"thinking":{"type":"adaptive","unknown":true}}),
        json!({"output_config":{"effort":"high","unknown":true}}),
        json!({"system":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral","ttl":"2h"}}]}),
        json!({"thinking":{"type":"enabled","budget_tokens":8193}}),
        json!({"top_p":2}),
        json!({"top_k":0}),
    ] {
        assert!(request(extra).is_err());
    }
}

#[test]
fn beta_is_accepted_but_anthropic_forwarding_is_not_faked() {
    let mut headers = HeaderMap::new();
    headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
    headers.append(
        "anthropic-beta",
        "interleaved-thinking-2025-05-14,context-management-2025-06-27"
            .parse()
            .unwrap(),
    );
    assert!(valid_version(&headers));
    let mut r = request(json!({})).unwrap();
    r.messages_options.as_mut().unwrap().beta = true;
    assert!(matches!(
        crate::providers::messages::validate(&r, ProviderClass::Anthropic),
        Err(InferenceError::InvalidField("anthropic-beta"))
    ));
    for class in [
        ProviderClass::OpenAi,
        ProviderClass::Bedrock,
        ProviderClass::Local,
    ] {
        assert!(crate::providers::messages::validate(&r, class).is_ok());
    }
}

#[tokio::test]
async fn messages_errors_are_anthropic_bounded_and_have_correct_status() {
    for (error, status, kind, message) in [
        (
            InferenceError::ContextLength,
            400,
            "invalid_request_error",
            "prompt is too long",
        ),
        (
            InferenceError::Overloaded,
            529,
            "overloaded_error",
            "The provider is overloaded",
        ),
        (
            InferenceError::Busy,
            429,
            "rate_limit_error",
            InferenceError::Busy.message(),
        ),
        (
            InferenceError::InvalidField("image"),
            400,
            "invalid_request_error",
            "image",
        ),
        (
            InferenceError::Unsupported,
            400,
            "invalid_request_error",
            InferenceError::Unsupported.message(),
        ),
    ] {
        let response = error_response(error);
        assert_eq!(response.status().as_u16(), status);
        let bytes = axum::body::to_bytes(response.into_body(), 2048)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["type"], kind);
        let rendered = value["error"]["message"].as_str().unwrap();
        assert!(rendered.contains(message));
        assert!(rendered.len() <= 1024);
    }
}

#[tokio::test]
async fn messages_output_bound_is_sixteen_mib() {
    for (size, success) in [
        (4 * 1024 * 1024 + 1, true),
        (16 * 1024 * 1024, true),
        (16 * 1024 * 1024 + 1, false),
    ] {
        let response = render(
            ProviderOutput::Stream(Box::pin(futures_util::stream::iter(vec![
                Ok(ChatEvent::Delta {
                    text: Some("x".repeat(size)),
                    tool_calls: vec![],
                }),
                Ok(ChatEvent::Finish(FinishReason::Stop)),
                Ok(ChatEvent::Done),
            ]))),
            "msg_bound".into(),
            "m".into(),
        );
        let bytes = axum::body::to_bytes(response.into_body(), 40 * 1024 * 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(body.contains("event: message_stop"), success);
        assert_eq!(body.contains("event: error"), !success);
    }
}

#[test]
fn beta_query_uses_the_same_exact_route_policy() {
    // Axum matches paths independently of queries; never register a '?' template.
    use crate::route_policy::{Access, ROUTES, TokenPolicy};
    let entries: Vec<_> = ROUTES.iter().filter(|r| r.path == "/v1/messages").collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].methods, "POST");
    assert_eq!(entries[0].access, Access::InferenceKey);
    assert_eq!(entries[0].token_policy, TokenPolicy::Denied);
    assert!(ROUTES.iter().all(|r| !r.path.contains('?')));
}
