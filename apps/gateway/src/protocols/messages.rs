//! Anthropic Messages / Claude Code frontend; adapter-specific field rules.
use super::responses::{Accumulator, Result, event};
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{Engine, error::InferenceError, types::*},
};
use axum::{
    Extension, Json,
    extract::rejection::JsonRejection,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse, sse::KeepAlive},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{convert::Infallible, time::Duration};
mod request;
pub use request::Request;
#[cfg(test)]
use request::parse_request;
pub(crate) const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
fn valid_version(headers: &HeaderMap) -> bool {
    headers.get_all("anthropic-version").iter().count() == 1
        && headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            == Some("2023-06-01")
        && headers
            .get_all("anthropic-beta")
            .iter()
            .all(|v| v.to_str().is_ok())
}
pub async fn handle(
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    input: std::result::Result<Json<Request>, JsonRejection>,
) -> Response {
    if !valid_version(&headers) {
        return error_response(InferenceError::InvalidRequest);
    }
    let wire = match input {
        Ok(Json(v)) => v,
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return http_error(StatusCode::PAYLOAD_TOO_LARGE);
        }
        Err(e) => return rejection(e),
    };
    // Anthropic bounds metadata.user_id to 256 characters.
    if wire
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_ref())
        .is_some_and(|u| u.chars().count() > 256)
    {
        return error_response(InferenceError::InvalidRequest);
    }
    let labels = crate::inference::client::current()
        .with_body_session([wire.metadata.as_ref().and_then(|m| m.user_id.as_deref())]);
    let mut request = match wire.normalize() {
        Ok(v) => v,
        Err(e) => return error_response(e),
    };
    if let Some(options) = &mut request.messages_options {
        options.beta = headers.contains_key("anthropic-beta");
    }
    let model = request.model.clone();
    let id = format!("msg_{}", request_id.0.simple());
    let output = match labels
        .scope(engine.execute_protocol(principal, request, request_id.0, ApiProtocol::Messages))
        .await
    {
        Ok(v) => v,
        Err(e) => return error_response(e),
    };
    render(output, id, model)
}
fn usage_json(usage: Usage) -> Value {
    let mut value = json!({});
    if let Some(n) = usage.input_tokens {
        value["input_tokens"] = n.into();
    }
    if let Some(n) = usage.output_tokens {
        value["output_tokens"] = n.into();
    }
    if let Some(b) = usage.billing {
        if let Some(n) = b.cache_read_input_tokens {
            value["cache_read_input_tokens"] = n.into();
        }
        if let Some(n) = b.cache_write_input_tokens {
            value["cache_creation_input_tokens"] = n.into();
        }
        let mut allocation = json!({});
        if let Some(n) = b.cache_write_5m_input_tokens {
            allocation["ephemeral_5m_input_tokens"] = n.into();
        }
        if let Some(n) = b.cache_write_1h_input_tokens {
            allocation["ephemeral_1h_input_tokens"] = n.into();
        }
        if allocation.as_object().is_some_and(|d| !d.is_empty()) {
            value["cache_creation"] = allocation;
        }
    }
    value
}
/// `/v1/batches` line body (`crate::jobs::lines`, Anthropic shape): the
/// interactive contract, never streamed.
pub(crate) fn batch_request(body: Value) -> Result<ChatRequest> {
    let wire: Request = serde_json::from_value(body).map_err(|_| InferenceError::InvalidRequest)?;
    if wire.stream
        || wire
            .metadata
            .as_ref()
            .and_then(|m| m.user_id.as_ref())
            .is_some_and(|u| u.chars().count() > 256)
    {
        return Err(InferenceError::InvalidRequest);
    }
    wire.normalize()
}
/// `/v1/batches` line result body (an Anthropic `message`).
pub(crate) fn batch_response(id: &str, model: &str, response: &ChatResponse) -> Result<Value> {
    snapshot(id, model, response)
}
/// `/v1/batches` line error body (Anthropic shape).
pub(crate) fn batch_error_body(e: InferenceError) -> Value {
    error_body(e)
}
fn snapshot(id: &str, model: &str, response: &ChatResponse) -> Result<Value> {
    let size = response.content.as_ref().map_or(0, String::len)
        + response
            .tool_calls
            .iter()
            .map(|c| c.id.len() + c.name.len() + c.arguments.len())
            .sum::<usize>();
    if size > OUTPUT_LIMIT {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut content = vec![];
    if let Some(text) = &response.content {
        content.push(json!({"type":"text","text":text}));
    }
    for call in &response.tool_calls {
        let input: Value =
            serde_json::from_str(&call.arguments).map_err(|_| InferenceError::InvalidUpstream)?;
        if !input.is_object() {
            return Err(InferenceError::InvalidUpstream);
        }
        content.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":input}));
    }
    let stop = match response.finish_reason {
        FinishReason::Stop => "end_turn",
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::Refusal => "refusal",
        FinishReason::ContextWindowExceeded => "model_context_window_exceeded",
        FinishReason::ContentFilter => return Err(InferenceError::Unsupported),
    };
    Ok(
        json!({"id":id,"type":"message","role":"assistant","model":model,"content":content,"stop_reason":stop,"stop_sequence":null,"usage":usage_json(response.usage)}),
    )
}
fn render(output: ProviderOutput, id: String, model: String) -> Response {
    match output {
        ProviderOutput::Complete(response) => match snapshot(&id, &model, &response) {
            Ok(v) => Json(v).into_response(),
            Err(e) => error_response(e),
        },
        ProviderOutput::Stream(mut stream) => {
            let events = async_stream::stream! {
                // Sent when the upstream stream opens, before any usage is observed (#23).
                // `input_tokens: 0` is a stream-start placeholder that strict clients require
                // (e.g. Vercel's @ai-sdk/anthropic); message_delta carries the observed input,
                // cache and output counters, which the SDKs apply. `output_tokens` is left out:
                // clients merge usage, so a start value that message_delta never replaces would
                // turn unknown output into zero. Gateway accounting never uses these values.
                yield Ok::<_, Infallible>(event("message_start", json!({"message":{"id":id,"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0}}})));
                // Text streams live in block 0 (text precedes tools in the snapshot). Tool
                // calls stay buffered and are emitted whole after it, so parallel calls never
                // interleave. Every event is validated before anything is forwarded.
                let mut accumulator = Accumulator::with_limit(OUTPUT_LIMIT); let mut done = false; let mut text_open = false;
                while let Some(next) = stream.next().await {
                    let delta = match &next { Ok(ChatEvent::Delta { text: Some(text), .. }) => Some(text.clone()), _ => None };
                    match next.and_then(|e| accumulator.push(e)) {
                        Ok(finished) => {
                            if let Some(delta) = delta {
                                if !text_open {
                                    text_open = true;
                                    yield Ok(event("content_block_start", json!({"index":0,"content_block":{"type":"text","text":""}})));
                                }
                                yield Ok(event("content_block_delta", json!({"index":0,"delta":{"type":"text_delta","text":delta}})));
                            }
                            if finished { done = true; break; }
                        }
                        Err(e) => { yield Ok(event("error", error_body(e))); return; }
                    }
                }
                if !done { yield Ok(event("error", error_body(InferenceError::InvalidUpstream))); return; }
                let terminal = match accumulator.finish().and_then(|r| snapshot(&id, &model, &r)) { Ok(v) => v, Err(e) => { yield Ok(event("error", error_body(e))); return; } };
                for (index, block) in terminal["content"].as_array().unwrap().iter().enumerate() {
                    if block["type"] != "text" {
                        let mut start = block.clone(); start["input"] = json!({});
                        yield Ok(event("content_block_start", json!({"index":index,"content_block":start})));
                        yield Ok(event("content_block_delta", json!({"index":index,"delta":{"type":"input_json_delta","partial_json":block["input"].to_string()}})));
                    }
                    yield Ok(event("content_block_stop", json!({"index":index})));
                }
                yield Ok(event("message_delta", json!({"delta":{"stop_reason":terminal["stop_reason"],"stop_sequence":null},"usage":terminal["usage"]})));
                yield Ok(event("message_stop", json!({})));
            };
            Sse::new(events)
                .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
                .into_response()
        }
    }
}
/// Outer transport rejections must not become plain text/OpenAI errors.
pub(crate) fn http_error(status: StatusCode) -> Response {
    let (kind, message) = if status == StatusCode::PAYLOAD_TOO_LARGE {
        ("request_too_large", "Request body too large")
    } else {
        ("invalid_request_error", "Method not allowed")
    };
    (
        status,
        Json(json!({"type":"error","error":{"type":kind,"message":message}})),
    )
        .into_response()
}
fn rejection(e: JsonRejection) -> Response {
    // Never echo serde's value/type diagnostics (they may include prompt text).
    // Only an identifier from its unknown-field diagnostic is safe to expose.
    let diagnostic = e.body_text();
    let field = diagnostic
        .split("unknown field `")
        .nth(1)
        .and_then(|v| v.split('`').next());
    if let Some(field) = field.filter(|v| {
        !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }) {
        return (StatusCode::BAD_REQUEST, Json(json!({"type":"error","error":{"type":"invalid_request_error","message":format!("Unknown field: {field}")}}))).into_response();
    }
    error_response(InferenceError::InvalidRequest)
}
fn error_body(e: InferenceError) -> Value {
    let kind = match e {
        InferenceError::InvalidRequest
        | InferenceError::InvalidField(_)
        | InferenceError::UnknownField(_)
        | InferenceError::ContextLength
        | InferenceError::UpstreamRejected
        | InferenceError::Unsupported => "invalid_request_error",
        InferenceError::ModelUnavailable => "not_found_error",
        InferenceError::Unauthenticated => "authentication_error",
        // Anthropic has no budget type for HTTP 429; the message distinguishes it.
        InferenceError::Busy
        | InferenceError::BudgetExceeded(_)
        | InferenceError::UnresolvedUsage(_)
        | InferenceError::TokenReservationExceedsLimit(_)
        | InferenceError::JobLimitExceeded(_) => "rate_limit_error",
        InferenceError::UpstreamUnavailable
        | InferenceError::Overloaded
        | InferenceError::RouteCoolingDown(_) => "overloaded_error",
        _ => "api_error",
    };
    let message = match e {
        InferenceError::UnknownField(field) => format!("Unknown field: {}", field.as_str()),
        InferenceError::InvalidField(field) => format!("Unsupported or invalid field: {field}"),
        _ => e.message().to_owned(),
    };
    json!({"type":"error","error":{"type":kind,"message":message}})
}
pub(super) fn status(e: InferenceError) -> StatusCode {
    match e {
        InferenceError::Unsupported => StatusCode::BAD_REQUEST,
        InferenceError::UpstreamUnavailable
        | InferenceError::Overloaded
        | InferenceError::RouteCoolingDown(_) => StatusCode::from_u16(529).unwrap(),
        _ => super::http_status(e),
    }
}
fn error_response(e: InferenceError) -> Response {
    let mut response = super::error_with_body(e, error_body(e));
    *response.status_mut() = status(e);
    response
}
#[cfg(test)]
mod frontend_tests;
#[cfg(test)]
mod tests;
