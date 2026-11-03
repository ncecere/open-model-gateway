//! Cancellation evidence is source-backed, not a string that merely looks like
//! a test name. The executed integration tests also prove persisted settlement.
use super::{generation::CANCELLATION_TESTS, tests::generated};
use serde_json::json;

const EXPECTED: &[(&str, &str)] = &[
    (
        "/v1/chat/completions",
        "dropping_http_stream_cancels_transport_and_retains_unknown_hold",
    ),
    (
        "/v1/responses",
        "dropping_responses_http_stream_cancels_transport_and_retains_unknown_hold",
    ),
    (
        "/v1/messages",
        "dropping_messages_http_stream_cancels_transport_and_retains_unknown_hold",
    ),
];
#[test]
fn inference_openapi_generation_cancellation_metadata() {
    assert_eq!(
        CANCELLATION_TESTS, EXPECTED,
        "generation cancellation registry drift"
    );
    let document = generated();
    for (path, test) in EXPECTED {
        assert_eq!(
            document["paths"][path]["post"]["x-omg-cancellation"],
            json!(test),
            "missing cancellation evidence for {path}"
        );
    }
}
#[test]
fn inference_openapi_generation_cancellation_tests_exist() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut functions = std::collections::BTreeSet::new();
    // Only compiled module edges count: an orphan *_tests.rs is not evidence.
    fn collect(path: &std::path::Path, functions: &mut std::collections::BTreeSet<String>) {
        let source = std::fs::read_to_string(path).expect("cancellation test source missing");
        let file = syn::parse_file(&source).expect("cancellation test source syntax");
        for item in file.items {
            match item {
                syn::Item::Fn(f)
                    if f.attrs
                        .iter()
                        .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test")) =>
                {
                    functions.insert(f.sig.ident.to_string());
                }
                syn::Item::Mod(m) if m.ident == "cancellation_tests" => {
                    let attribute = m
                        .attrs
                        .iter()
                        .find(|a| a.path().is_ident("path"))
                        .expect("cancellation module path missing");
                    let syn::Meta::NameValue(meta) = &attribute.meta else {
                        panic!("cancellation module path kind")
                    };
                    let syn::Expr::Lit(lit) = &meta.value else {
                        panic!("cancellation module path expression")
                    };
                    let syn::Lit::Str(relative) = &lit.lit else {
                        panic!("cancellation module path literal")
                    };
                    collect(&path.parent().unwrap().join(relative.value()), functions);
                }
                _ => {}
            }
        }
    }
    collect(&root.join("inference.rs"), &mut functions);
    for (_, test) in EXPECTED {
        assert!(
            functions.contains(*test),
            "missing wired HTTP cancellation test {test}"
        );
    }
}

#[test]
fn inference_openapi_generation_aliases_and_stateless_refusals() {
    let doc = generated();
    for (case, path, body) in [
        (
            "unequal token aliases",
            "/v1/chat/completions",
            json!({"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":1,"max_completion_tokens":2}),
        ),
        (
            "store true",
            "/v1/responses",
            json!({"model":"m","input":"hi","store":true}),
        ),
        (
            "previous response",
            "/v1/responses",
            json!({"model":"m","input":"hi","previous_response_id":"resp_previous"}),
        ),
    ] {
        assert!(
            super::generation::validate_request(&doc, path, &body).is_err(),
            "accepted {case}"
        );
    }
}
#[test]
fn inference_openapi_generation_operations_are_complete() {
    let doc = generated();
    for (path, _) in EXPECTED {
        super::responses::require_complete(&doc, &doc["paths"][path]["post"])
            .unwrap_or_else(|_| panic!("incomplete generation operation {path}"));
    }
}
#[test]
fn inference_openapi_generation_translated_error_results_are_refused() {
    let doc = generated();
    let rows = doc["paths"]["/v1/messages"]["post"]["x-omg-refusals"]
        .as_array()
        .expect("missing Messages refusal rows");
    assert!(
        rows.iter().any(|row| row["status"] == 501
            && row["trigger"] == "tool_result.is_error:true on translated routes"
            && row["envelope"] == "AnthropicErrorEnvelope"),
        "missing translated error-result refusal"
    );
}

#[test]
fn inference_openapi_generation_store_true_is_not_accepted() {
    assert!(
        super::generation::validate_request(
            &generated(),
            "/v1/responses",
            &json!({"model":"m","input":"hi","store":true})
        )
        .is_err(),
        "accepted store true"
    );
}
#[test]
fn inference_openapi_generation_previous_response_is_not_accepted() {
    assert!(
        super::generation::validate_request(
            &generated(),
            "/v1/responses",
            &json!({"model":"m","input":"hi","previous_response_id":"resp_previous"})
        )
        .is_err(),
        "accepted previous_response_id"
    );
}

#[test]
fn inference_openapi_generation_named_chat_choice_serde_forms() {
    let doc = generated();
    for choice in [
        json!(["function", ["ping"]]),
        json!({"type":"function","function":["ping"]}),
    ] {
        let body = json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"ping"}}],"tool_choice":choice});
        assert!(
            crate::protocols::chat_completions::openapi_normalize(body.clone()).is_ok(),
            "named choice serde fixture"
        );
        super::generation::validate_request(&doc, "/v1/chat/completions", &body)
            .expect("accepted named choice form missing from contract");
    }
}
#[test]
fn inference_openapi_generation_chat_output_calls_are_objects() {
    let doc = generated();
    let mut body = doc["paths"]["/v1/chat/completions"]["post"]["responses"]["200"]["content"]["application/json"]["example"].clone();
    body["choices"][0]["message"]["tool_calls"] = json!([["call_0", "function", ["ping", "{}"]]]);
    assert!(
        super::responses::validate_body(
            &doc,
            "POST",
            "/v1/chat/completions",
            axum::http::StatusCode::OK,
            &serde_json::to_vec(&body).unwrap()
        )
        .is_err(),
        "accepted request tuple in response"
    );
}
#[test]
fn inference_openapi_generation_responses_event_matches_snapshot_status() {
    let doc = generated();
    let snapshot = doc["paths"]["/v1/responses"]["post"]["responses"]["200"]["content"]["application/json"]["example"].clone();
    let mut wire = String::new();
    for (seq, kind) in [
        "response.created",
        "response.in_progress",
        "response.completed",
    ]
    .iter()
    .enumerate()
    {
        let mut response = snapshot.clone();
        response["status"] = json!("in_progress");
        if seq < 2 {
            response["output"] = json!([]);
        }
        let payload = json!({"type":kind,"sequence_number":seq,"response":response});
        wire.push_str(&format!("event: {kind}\ndata: {payload}\n\n"));
    }
    assert!(
        super::generation::validate_sse(&doc, "/v1/responses", wire.as_bytes(), false).is_err(),
        "completed event accepted non-completed snapshot"
    );
}

#[test]
fn inference_openapi_generation_artifact_truncated_stream_is_not_positive() {
    let doc = generated();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("content-type", "text/event-stream".parse().unwrap());
    headers.insert(
        "x-request-id",
        "00000000-0000-0000-0000-000000000001".parse().unwrap(),
    );
    headers.insert("cache-control", "no-store".parse().unwrap());
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let wire = doc["paths"][path]["post"]["responses"]["200"]["content"]["text/event-stream"]["example"].as_str().unwrap();
        let boundary = wire.trim_end().rfind("\n\n").unwrap() + 2;
        let truncated = &wire.as_bytes()[..boundary];
        assert_eq!(
            super::generation::validate_sse(&doc, path, truncated, true).unwrap(),
            super::sse::Outcome::Truncated,
            "truncated fixture outcome"
        );
        assert!(
            super::responses::validate_documented(
                &doc,
                "POST",
                path,
                axum::http::StatusCode::OK,
                &headers,
                truncated
            )
            .is_err(),
            "truncated SSE would earn artifact positive coverage"
        );
    }
}

#[test]
fn inference_openapi_generation_headers_are_exact() {
    let good = generated();
    super::generator::verify(&good).expect("generation header declarations");
    let mut headers = axum::http::HeaderMap::new();
    assert!(
        super::generation::validate_request_headers(&good, "/v1/messages", &headers).is_err(),
        "accepted missing version header"
    );
    for version in ["2023-06-01", "2026-10-21"] {
        headers.insert("anthropic-version", version.parse().unwrap());
        super::generation::validate_request_headers(&good, "/v1/messages", &headers)
            .expect("valid protocol headers");
    }
    headers.insert("anthropic-version", "2026-10-22".parse().unwrap());
    assert!(
        super::generation::validate_request_headers(&good, "/v1/messages", &headers).is_err(),
        "accepted an unsupported version"
    );
    headers.insert("anthropic-version", "2023-06-01".parse().unwrap());

    for case in [
        "invented",
        "dropped",
        "wrong kind",
        "duplicate",
        "version literal",
        "version dropped",
        "version invented",
        "version optional",
        "beta allowlist",
        "beta denylist",
        "beta bounds",
    ] {
        let mut bad = good.clone();
        let parameters = bad["paths"]["/v1/messages"]["post"]["parameters"]
            .as_array_mut()
            .unwrap();
        match case {
            "invented" => {
                parameters.push(json!({"in":"header","name":"invented","schema":{"type":"string"}}))
            }
            "dropped" => {
                parameters.remove(0);
            }
            "wrong kind" => parameters[0]["in"] = json!("query"),
            "version literal" => parameters[0]["schema"]["const"] = json!("unsupported"),
            "version dropped" => parameters[0]["schema"]["enum"] = json!(["2023-06-01"]),
            "version invented" => {
                parameters[0]["schema"]["enum"] = json!(["2023-06-01", "2026-10-21", "2026-10-22"])
            }
            "version optional" => parameters[0]["required"] = json!(false),
            "beta allowlist" => parameters[1]["schema"]["enum"] = json!(["only-one-beta"]),
            "beta denylist" => parameters[1]["x-omg-denied-prefixes"] = json!([]),
            "beta bounds" => parameters[1]["x-omg-max-tokens"] = json!(33),
            _ => parameters.push(parameters[0].clone()),
        }
        assert!(
            super::generator::verify(&bad).is_err(),
            "accepted {case} header drift"
        );
    }
}

#[test]
fn inference_openapi_generation_request_dto_field_inventory() {
    use crate::management::openapi::requests::vectors_in;
    let doc = generated();
    // Normalization refines every field's serde shape; field INVENTORY remains
    // independent and exact, so a DTO field addition/removal still fails here.
    fn refined(doc: &serde_json::Value, name: &str) -> Vec<String> {
        doc["components"]["schemas"][name]["oneOf"][0]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(|key| format!("/{key}"))
            .collect()
    }
    let chat = refined(&doc, "post_v1_chat_completionsRequest");
    vectors_in::<crate::protocols::chat_completions::Request>(
        &doc,
        "POST",
        "/v1/chat/completions",
        json!({"model":"m","messages":[{"role":"user","content":"hi"}]}),
        &chat.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .expect("Chat DTO inventory");
    let responses = refined(&doc, "post_v1_responsesRequest");
    vectors_in::<crate::protocols::responses::Request>(
        &doc,
        "POST",
        "/v1/responses",
        json!({"model":"m","input":"hi"}),
        &responses.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .expect("Responses DTO inventory");
    let messages = refined(&doc, "post_v1_messagesRequest");
    vectors_in::<crate::protocols::messages::Request>(
        &doc,
        "POST",
        "/v1/messages",
        json!({"model":"m","max_tokens":32,"messages":[{"role":"user","content":"hi"}]}),
        &messages.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .expect("Messages DTO inventory");
}

#[test]
fn inference_openapi_generation_validator_cache_is_contract_scoped() {
    let document = super::responses::artifact();
    let schema = json!({"$ref":"#/components/schemas/get_v1_modelsResponse"});
    let first = crate::management::openapi::responses::validator(document, &schema).unwrap();
    let again = crate::management::openapi::responses::validator(document, &schema).unwrap();
    let body = json!({"object":"list","data":[]});
    assert!(
        first.is_valid(&body) && again.is_valid(&body),
        "cache control fixture"
    );
    let mut mutated = document.clone();
    mutated["components"]["schemas"]["get_v1_modelsResponse"] = json!({"const":false});
    let bad = crate::management::openapi::responses::validator(&mutated, &schema).unwrap();
    assert!(!bad.is_valid(&body), "mutated schema reused trusted cache");
    assert!(
        crate::management::openapi::responses::validator(
            crate::management::openapi::responses::artifact(),
            &schema
        )
        .is_err(),
        "inference refs leaked into management cache"
    );
}

#[test]
fn inference_openapi_generation_sse_payload_refs_are_complete() {
    let doc = generated();
    for (path, terminals, count) in [
        ("/v1/chat/completions", json!(["[DONE]"]), 2),
        (
            "/v1/responses",
            json!(["response.completed", "response.incomplete"]),
            13,
        ),
        ("/v1/messages", json!(["message_stop"]), 7),
    ] {
        let grammar = &doc["paths"][path]["post"]["responses"]["200"]["content"]["text/event-stream"]
            ["x-omg-sse"];
        assert_eq!(
            grammar["terminal"], terminals,
            "generation terminal metadata drift"
        );
        assert_eq!(
            grammar["error_event"],
            json!("error"),
            "generation error metadata drift"
        );
        assert_eq!(
            grammar["events"].as_object().unwrap().len(),
            count,
            "generation event vocabulary drift"
        );
        for schema in grammar["events"].as_object().unwrap().values() {
            let operation = json!({"responses":{"200":{"description":"SSE frame payload", "content":{"application/json":{"schema":schema}}}}});
            crate::management::openapi::quality::operation(&doc, &operation)
                .expect("SSE payload schema quality");
        }
    }
}

pub(super) fn integer_float_cases() -> Vec<(&'static str, serde_json::Value)> {
    let chat = json!({"model":"company/smart","messages":[{"role":"user","content":"text"}]});
    let responses = json!({"model":"company/smart","input":"text"});
    let messages = json!({"model":"company/smart","max_tokens":2048,"messages":[{"role":"user","content":"text"}]});
    let mut cases = vec![];
    for (path, base, extra) in [
        ("/v1/chat/completions", &chat, json!({"max_tokens":2.0})),
        (
            "/v1/chat/completions",
            &chat,
            json!({"max_completion_tokens":2.0}),
        ),
        ("/v1/chat/completions", &chat, json!({"n":1.0})),
        (
            "/v1/responses",
            &responses,
            json!({"max_output_tokens":2.0}),
        ),
        ("/v1/messages", &messages, json!({"max_tokens":2048.0})),
        ("/v1/messages", &messages, json!({"top_k":2.0})),
        (
            "/v1/messages",
            &messages,
            json!({"thinking":{"type":"enabled","budget_tokens":1024.0}}),
        ),
        (
            "/v1/messages",
            &messages,
            json!({"output_config":{"task_budget":{"type":"tokens","total":2.0,"remaining":1}}}),
        ),
        (
            "/v1/messages",
            &messages,
            json!({"output_config":{"task_budget":{"type":"tokens","total":2,"remaining":1.0}}}),
        ),
    ] {
        let mut body = base.clone();
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        cases.push((path, body));
    }
    for field in ["trigger", "keep", "clear_at_least"] {
        let mut body = messages.clone();
        let mut edit = json!({"type":"clear_tool_uses_20250919"});
        edit[field] = json!({"type":"tool_uses","value":1.0});
        body["context_management"] = json!({"edits":[edit]});
        cases.push(("/v1/messages", body));
    }
    cases
}
#[test]
fn inference_openapi_generation_integer_wire_refinement_is_explicit() {
    let doc = generated();
    for (path, body) in integer_float_cases() {
        // JSON Schema integers include 2.0, whereas serde and Value::as_u64
        // reject the float wire kind (#431). The explicit semantic extension
        // must reject it too; do not misrepresent this as a JSON Schema bound.
        let schema = &doc["paths"][path]["post"]["x-omg-request-schema"];
        super::responses::validate_value(&doc, schema, &body)
            .expect("mathematical integer fixture");
        assert!(
            super::generation::validate_request(&doc, path, &body).is_err(),
            "accepted floating-point integer wire kind"
        );
    }
}

#[test]
fn inference_openapi_generation_protocol_profiles_are_source_derived() {
    use crate::{inference::types::ApiProtocol as P, providers::capabilities::PROFILES};
    let good = generated();
    for (path, protocol) in [
        ("/v1/chat/completions", P::ChatCompletions),
        ("/v1/responses", P::Responses),
        ("/v1/messages", P::Messages),
    ] {
        let expected = PROFILES
            .iter()
            .filter(|(_, protocols)| protocols.contains(&protocol))
            .map(|(name, _)| json!(name))
            .collect::<Vec<_>>();
        let actual = good["paths"][path]["post"]["x-omg-protocol-profiles"]
            .as_array()
            .expect("missing generation protocol profiles");
        assert!(actual == &expected, "generation protocol-profile drift");
        for profiles in [serde_json::Value::Null, json!(["invented-profile"])] {
            let mut bad = good.clone();
            bad["paths"][path]["post"]["x-omg-protocol-profiles"] = profiles;
            assert!(
                super::generator::verify(&bad).is_err(),
                "accepted protocol-profile drift"
            );
        }
    }
}
