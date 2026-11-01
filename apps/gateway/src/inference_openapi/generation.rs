//! Generation contract evidence registry (#82). These are executable tests,
//! never a claim that cancellation is observable in the SSE bytes.
pub(super) const CANCELLATION_TESTS: &[(&str, &str)] = &[
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

pub(super) fn validate_request(
    document: &serde_json::Value,
    path: &str,
    body: &serde_json::Value,
) -> anyhow::Result<()> {
    let schema = &document["paths"][path]["post"]["x-omg-request-schema"];
    super::responses::validate_value(document, schema, body)
}
