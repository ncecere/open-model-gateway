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
