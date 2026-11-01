//! Official clients consuming documented examples without a database or network.
use super::{generation::validate_request, tests::generated};
use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn inference_openapi_generation_sdk_wire_without_database() {
    let doc = generated();
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let content = &doc["paths"][path]["post"]["responses"]["200"]["content"];
        super::responses::validate_body(
            &doc,
            "POST",
            path,
            axum::http::StatusCode::OK,
            &serde_json::to_vec(&content["application/json"]["example"]).unwrap(),
        )
        .expect("SDK JSON example contract");
        let wire = content["text/event-stream"]["example"]
            .as_str()
            .expect("SDK SSE example missing");
        assert_eq!(
            super::generation::validate_sse(&doc, path, wire.as_bytes(), true)
                .expect("SDK SSE example contract"),
            super::sse::Outcome::Completed,
            "SDK SSE example terminal"
        );
    }
    let requests = sdk_requests(&doc).expect("SDK wire runner failure");
    assert_eq!(requests.len(), 6, "SDK JSON/SSE request inventory");
    let mut seen = std::collections::BTreeSet::new();
    for request in requests {
        let path = request["path"].as_str().expect("SDK request path missing");
        let mut headers = axum::http::HeaderMap::new();
        for (name, value) in request["headers"]
            .as_object()
            .expect("SDK protocol headers missing")
        {
            let name =
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("SDK header name kind");
            let value =
                axum::http::HeaderValue::from_str(value.as_str().expect("SDK header value kind"))
                    .expect("SDK header representation");
            headers.insert(name, value);
        }
        assert!(
            headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v
                    .split(';')
                    .next()
                    .unwrap()
                    .trim()
                    .eq_ignore_ascii_case("application/json")),
            "SDK JSON request media type"
        );
        super::generation::validate_request_headers(&doc, path, &headers)
            .expect("SDK protocol header contract");
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

fn sdk_requests(doc: &serde_json::Value) -> anyhow::Result<Vec<serde_json::Value>> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new("node")
        .arg(root.join("tests/inference-sdk-wire.mjs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let written = child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(doc)?);
    let output = child.wait_with_output()?;
    anyhow::ensure!(written.is_ok(), "SDK wire input transport failure");
    anyhow::ensure!(output.status.success(), "SDK wire runner status failure");
    let requests: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
        .map_err(|_| anyhow::anyhow!("SDK wire output kind"))?;
    Ok(requests)
}

#[test]
fn inference_openapi_generation_sdk_rejects_unbalanced_documented_blocks() {
    let mut doc = generated();
    let example = &mut doc["paths"]["/v1/messages"]["post"]["responses"]["200"]["content"]["text/event-stream"]
        ["example"];
    let wire = example
        .as_str()
        .unwrap()
        .split("\n\n")
        .filter(|frame| !frame.starts_with("event: content_block_start\n"))
        .collect::<Vec<_>>()
        .join("\n\n");
    *example = serde_json::json!(wire);
    assert!(
        sdk_requests(&doc).is_err(),
        "official SDK helper accepted unbalanced blocks"
    );
}
