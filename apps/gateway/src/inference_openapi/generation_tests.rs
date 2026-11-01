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
