//! Official clients consuming documented examples without a database or network.
use super::{generation::validate_request, tests::generated};
use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn inference_openapi_generation_sdk_wire_without_database() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let doc = generated();
    let mut child = Command::new("node")
        .arg(root.join("tests/inference-sdk-wire.mjs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("SDK wire runner unavailable");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&doc).unwrap())
        .unwrap();
    let output = child.wait_with_output().expect("SDK wire runner failed");
    assert!(output.status.success(), "SDK wire runner status failure");
    let requests: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("SDK wire output kind"));
    assert_eq!(requests.len(), 6, "SDK JSON/SSE request inventory");
    let mut seen = std::collections::BTreeSet::new();
    for request in requests {
        let path = request["path"].as_str().expect("SDK request path missing");
        let stream = request["body"]["stream"].as_bool().unwrap_or(false);
        assert!(seen.insert((path.to_owned(), stream)), "duplicate SDK case");
        validate_request(&doc, path, &request["body"])
            .unwrap_or_else(|e| panic!("SDK request schema mismatch: {e:#}"));
    }
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        for stream in [false, true] {
            assert!(
                seen.contains(&(path.to_owned(), stream)),
                "SDK request case missing"
            );
        }
    }
}
