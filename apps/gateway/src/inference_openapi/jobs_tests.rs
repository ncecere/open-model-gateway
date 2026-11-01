//! Job contract regression controls (#82 PR3b).
use super::{responses::validate_value, tests::generated};
use serde_json::json;

#[test]
fn inference_openapi_batch_endpoint_is_closed() {
    let doc = generated();
    let schema = &doc["components"]["schemas"]["post_v1_batchesRequest"];
    for endpoint in ["/v1/chat/completions", "/v1/responses", "/v1/embeddings", "/v1/messages"] {
        let mut body = json!({"input_file_id":"file-00000000000000000000000000000000","endpoint":endpoint,"completion_window":"24h"});
        assert!(validate_value(&doc, schema, &body).is_ok(), "allowed batch endpoint");
        body["endpoint"] = json!("/v1/audio/speech");
        assert!(validate_value(&doc, schema, &body).is_err(), "unsupported batch endpoint accepted");
    }
}

#[test]
fn inference_openapi_video_create_is_refusal_only() {
    let doc = generated();
    let responses = &doc["paths"]["/v1/videos"]["post"]["responses"];
    assert!(responses.get("200").is_none(), "video creation promises success");
    assert_eq!(responses["400"]["x-omg-positive-kind"], "refusal");
    assert_eq!(doc["paths"]["/v1/videos"]["post"]["x-omg-refusals"][0]["code"], "unsupported_capability");
}

#[test]
fn inference_openapi_batch_errors_have_line_reports() {
    let doc = generated();
    let schema = &doc["components"]["schemas"]["BatchInvalidInput"];
    let valid = json!({"error":{"message":"Invalid lines","type":"invalid_request_error","code":"invalid_batch_input","param":"input_file_id"},"errors":{"object":"list","data":[{"code":"invalid_request_error","message":"Invalid line","line":1,"param":null}]}});
    assert!(validate_value(&doc, schema, &valid).is_ok(), "line report rejected");
    let mut bad = valid;
    bad["errors"]["data"] = json!({});
    assert!(validate_value(&doc, schema, &bad).is_err(), "non-array line report accepted");
}
