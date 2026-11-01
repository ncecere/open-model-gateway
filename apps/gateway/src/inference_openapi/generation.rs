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
    super::responses::validate_value(document, schema, body)?;
    anyhow::ensure!(
        document["components"]["schemas"]
            [schema["$ref"].as_str().unwrap().rsplit('/').next().unwrap()]["x-omg-request-constraints"]
            == "generation-normalization",
        "missing generation semantic constraint"
    );
    let parsed = match path {
        "/v1/chat/completions" => {
            crate::protocols::chat_completions::openapi_normalize(body.clone())
        }
        "/v1/responses" => crate::protocols::responses::openapi_normalize(body.clone()),
        "/v1/messages" => crate::protocols::messages::openapi_normalize(body.clone()),
        _ => anyhow::bail!("unknown generation route"),
    };
    parsed.map_err(|_| anyhow::anyhow!("generation request semantic constraint"))?;
    if path != "/v1/messages" {
        let metadata = body.get("metadata").or_else(|| body.get(11));
        let metadata = metadata
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value::<crate::inference::client::OpenAiMetadata>(v.clone()))
            .transpose()
            .map_err(|_| anyhow::anyhow!("metadata kind"))?;
        anyhow::ensure!(
            crate::inference::client::valid_openai_metadata(metadata.as_ref()),
            "metadata bounds"
        );
    }
    Ok(())
}

/// Parse frames, validate every data payload against the artifact's event ref,
/// and enforce the protocol's terminal grammar. Never decode-and-discard wire.
pub(super) fn validate_sse(
    document: &serde_json::Value,
    path: &str,
    bytes: &[u8],
    include_usage: bool,
) -> anyhow::Result<super::sse::Outcome> {
    use super::sse::Protocol;
    let protocol = match path {
        "/v1/chat/completions" => Protocol::Chat { include_usage },
        "/v1/responses" => Protocol::Responses,
        "/v1/messages" => Protocol::Messages,
        _ => anyhow::bail!("unknown generation SSE route"),
    };
    let grammar = &document["paths"][path]["post"]["responses"]["200"]["content"]["text/event-stream"]
        ["x-omg-sse"];
    anyhow::ensure!(
        grammar["truncated_is_positive"] == false,
        "missing SSE coverage rule"
    );
    super::sse::validate_with(protocol, bytes, |event, body| {
        let kind = if matches!(protocol, Protocol::Chat { .. }) {
            if body.get("error").is_some() {
                "error"
            } else {
                "message"
            }
        } else {
            event.ok_or_else(|| anyhow::anyhow!("SSE event type missing"))?
        };
        let schema = grammar["events"]
            .get(kind)
            .ok_or_else(|| anyhow::anyhow!("undocumented SSE event kind"))?;
        super::responses::validate_value(document, schema, body)
    })
}
