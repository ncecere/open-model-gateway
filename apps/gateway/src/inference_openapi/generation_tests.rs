//! Cancellation evidence is source-backed, not a string that merely looks like
//! a test name. The executed integration tests also prove persisted settlement.
use super::tests::generated;
use serde_json::json;

const DROPS: &[(&str, &str)] = &[
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
    let document = generated();
    for (path, test) in DROPS {
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
    fn collect(path: &std::path::Path, functions: &mut std::collections::BTreeSet<String>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, functions);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let source = std::fs::read_to_string(&path).unwrap();
                let file = syn::parse_file(&source).unwrap();
                for item in file.items {
                    if let syn::Item::Fn(f) = item
                        && f.attrs
                            .iter()
                            .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test"))
                    {
                        functions.insert(f.sig.ident.to_string());
                    }
                }
            }
        }
    }
    collect(&root, &mut functions);
    for (_, test) in DROPS {
        assert!(
            functions.contains(*test),
            "missing HTTP cancellation test {test}"
        );
    }
}
