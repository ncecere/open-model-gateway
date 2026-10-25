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
            json!({"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}]}),
            "image",
            true,
        ),
        (
            json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"lookup","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"c","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]}]}]}),
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
            if always_refuse && (field != "image" || class != ProviderClass::Anthropic) {
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
        assert!(crate::providers::messages::validate(&r, ProviderClass::Anthropic).is_ok());
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
fn beta_is_accepted_with_bounded_verbatim_values() {
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
    r.messages_options.as_mut().unwrap().beta_headers = beta_headers(&headers).unwrap();
    assert!(crate::providers::messages::validate(&r, ProviderClass::Anthropic).is_ok());
    assert_eq!(
        r.messages_options.as_ref().unwrap().beta_headers,
        vec!["interleaved-thinking-2025-05-14,context-management-2025-06-27"]
    );
    headers.insert("anthropic-beta", "x".repeat(1025).parse().unwrap());
    assert!(!valid_version(&headers));
    headers.remove("anthropic-beta");
    for _ in 0..17 {
        headers.append("anthropic-beta", "x".parse().unwrap());
    }
    assert!(!valid_version(&headers));
    for class in [
        ProviderClass::OpenAi,
        ProviderClass::Bedrock,
        ProviderClass::Local,
    ] {
        assert!(crate::providers::messages::validate(&r, class).is_ok());
    }
}

#[test]
fn beta_denylist_checks_every_token_across_repeated_headers() {
    for beta in [
        "context-1m-2025-08-07",
        "files-api-2025-04-14",
        "code-execution-2025-05-22",
        "web-search-2025-03-05",
        "web-fetch-2025-09-10",
        "mcp-client-2025-11-20",
        "computer-use-2025-01-24",
    ] {
        let mut h = HeaderMap::new();
        h.append(
            "anthropic-beta",
            "interleaved-thinking-2025-05-14".parse().unwrap(),
        );
        h.append("anthropic-beta", beta.parse().unwrap());
        assert!(
            matches!(
                beta_headers(&h),
                Err(crate::providers::messages::BetaHeaderError::Denied(_))
            ),
            "{beta}"
        );
        let mut r = request(json!({})).unwrap();
        r.messages_options.as_mut().unwrap().beta_headers =
            vec![format!("context-management-2025-06-27, {beta}")];
        assert!(matches!(
            crate::providers::messages::validate(&r, ProviderClass::Anthropic),
            Err(InferenceError::InvalidField("anthropic-beta"))
        ));
    }
    for (last_count, accepted) in [(2, true), (3, false)] {
        let mut h = HeaderMap::new();
        for i in 0..16 {
            let value = vec!["context-management-2025-06-27"; if i == 15 { last_count } else { 2 }]
                .join(", ");
            h.append("anthropic-beta", value.parse().unwrap());
        }
        assert_eq!(
            beta_headers(&h).is_ok(),
            accepted,
            "32-token bound is across all repeated headers"
        );
    }
    for beta in [
        "claude-code-20250219",
        "extended-cache-ttl-2025-04-11",
        "tool-search-2025-10-19",
        "effort-2025-11-24",
        "oauth-2025-04-20",
        "per-turn-effort-2026-01-19",
        "structured-outputs-2025-11-13",
        "interleaved-thinking-2025-05-14",
        "context-management-2025-06-27",
        "output-128k-2025-02-19",
        "attacker-beta",
        "future-version",
    ] {
        let mut h = HeaderMap::new();
        h.append("anthropic-beta", beta.parse().unwrap());
        assert_eq!(beta_headers(&h).unwrap(), vec![beta.to_string()]);
    }
}

#[test]
fn malformed_beta_headers_remain_bounded_and_cannot_inject_headers() {
    for beta in [
        "",
        "claude-code-20250219,",
        "claude-code-20250219,,oauth-2025-04-20",
    ] {
        let mut h = HeaderMap::new();
        h.append("anthropic-beta", beta.parse().unwrap());
        assert!(matches!(
            beta_headers(&h),
            Err(crate::providers::messages::BetaHeaderError::Invalid)
        ));
    }
    assert!(
        crate::providers::messages::validate_beta_headers([
            "claude-code-20250219\r\nHost: attacker.invalid"
        ])
        .is_err()
    );
}

#[tokio::test]
async fn beta_recovery_echo_is_bounded_and_never_used_as_a_log_error() {
    let token = format!("context-1m-{}", "x".repeat(1024 - "context-1m-".len()));
    let error =
        crate::providers::messages::validate_beta_headers(std::iter::repeat_n(token.as_str(), 16))
            .unwrap_err();
    assert_eq!(
        format!("{error:?}"),
        "Invalid or denied anthropic-beta header"
    );
    let crate::providers::messages::BetaHeaderError::Denied(tokens) = error else {
        panic!()
    };
    let response = beta_error_response(&tokens);
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.len() <= 17 * 1024);
    assert!(
        message.contains(&tokens.join(", ")),
        "exact rejected names enable recovery; never truncate them"
    );
}

#[tokio::test]
async fn native_thinking_lifecycles_reach_the_client_without_duplicate_text_or_success_on_eof() {
    let blocks = vec![
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque+/=="}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"redacted_thinking","data":"encrypted+/=="}}),
        json!({"type":"content_block_stop","index":2}),
    ];
    for terminal in [false, true] {
        let mut events: Vec<_> = blocks
            .iter()
            .cloned()
            .map(|v| Ok(ChatEvent::MessagesBlock(v)))
            .collect();
        events.push(Ok(ChatEvent::Delta {
            text: Some("answer".into()),
            tool_calls: vec![],
        }));
        events.push(Ok(ChatEvent::Finish(FinishReason::Stop)));
        if terminal {
            events.push(Ok(ChatEvent::Done));
        }
        let response = render(
            ProviderOutput::Stream(Box::pin(futures_util::stream::iter(events))),
            "id".into(),
            "public".into(),
        );
        let bytes = axum::body::to_bytes(response.into_body(), OUTPUT_LIMIT)
            .await
            .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let values: Vec<Value> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        let actual: Vec<_> = values
            .iter()
            .filter(|v| v["type"].as_str().unwrap().starts_with("content_block_"))
            .cloned()
            .collect();
        assert_eq!(actual, blocks);
        assert_eq!(text.matches("answer").count(), 1);
        assert_eq!(text.contains("event: message_stop"), terminal);
        assert_eq!(text.contains("event: error"), !terminal);
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
            // No deployment serves the request: a gateway configuration
            // answer, kept as 501 as on every protocol.
            InferenceError::Unsupported,
            501,
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
