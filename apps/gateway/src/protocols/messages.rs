//! Anthropic Messages version 2023-06-01: stateless text and function tools only.
use super::responses::{Accumulator, Result, event, fields, message, name, string, validate};
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
use serde::Deserialize;
use serde_json::{Value, json};
use std::{convert::Infallible, time::Duration};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    model: String,
    messages: Vec<WireMessage>,
    system: Option<Value>,
    max_tokens: u32,
    #[serde(default)]
    stream: bool,
    temperature: Option<f64>,
    #[serde(default)]
    tools: Vec<Tool>,
    tool_choice: Option<Value>,
    /// Client label only (session id source for Logs); not forwarded upstream.
    metadata: Option<WireMetadata>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMetadata {
    user_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMessage {
    role: String,
    content: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tool {
    name: String,
    description: Option<String>,
    input_schema: Value,
}
fn text(value: &Value) -> Result<String> {
    match value {
        Value::String(v) => Ok(v.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                fields(part, &["type", "text"])?;
                if part["type"] != "text" {
                    return Err(InferenceError::InvalidRequest);
                }
                text.push_str(&string(&part["text"])?);
            }
            Ok(text)
        }
        _ => Err(InferenceError::InvalidRequest),
    }
}
impl Request {
    fn normalize(self) -> Result<ChatRequest> {
        if self.messages.is_empty()
            || self.messages.len() > 4096
            || self.temperature.is_some_and(|v| !(0.0..=1.0).contains(&v))
        {
            return Err(InferenceError::InvalidRequest);
        }
        let mut messages = vec![];
        if let Some(system) = self.system {
            messages.push(message(Role::System, text(&system)?));
        }
        for wire in self.messages {
            let role = match wire.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => return Err(InferenceError::InvalidRequest),
            };
            if let Value::String(s) = wire.content {
                messages.push(message(role, s));
                continue;
            }
            let blocks = wire
                .content
                .as_array()
                .ok_or(InferenceError::InvalidRequest)?;
            if blocks.is_empty() || blocks.len() > 256 {
                return Err(InferenceError::InvalidRequest);
            }
            // Preserve block order by representing each block as one typed message.
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        fields(block, &["type", "text"])?;
                        messages.push(message(role, string(&block["text"])?));
                    }
                    Some("tool_use") if role == Role::Assistant => {
                        fields(block, &["type", "id", "name", "input"])?;
                        let id = string(&block["id"])?;
                        let n = string(&block["name"])?;
                        if id.is_empty() || !name(&n) || !block["input"].is_object() {
                            return Err(InferenceError::InvalidRequest);
                        }
                        messages.push(Message {
                            role,
                            content: None,
                            tool_call_id: None,
                            tool_calls: vec![ToolCall {
                                id,
                                name: n,
                                arguments: block["input"].to_string(),
                            }],
                        });
                    }
                    Some("tool_result") if role == Role::User => {
                        fields(block, &["type", "tool_use_id", "content"])?;
                        let id = string(&block["tool_use_id"])?;
                        if id.is_empty() {
                            return Err(InferenceError::InvalidRequest);
                        }
                        messages.push(Message {
                            role: Role::Tool,
                            content: Some(text(&block["content"])?),
                            tool_call_id: Some(id),
                            tool_calls: vec![],
                        });
                    }
                    _ => return Err(InferenceError::InvalidRequest),
                }
            }
        }
        let tool_choice = match self.tool_choice {
            None => None,
            Some(v) => {
                fields(&v, &["type", "name"])?;
                Some(match v["type"].as_str() {
                    Some("auto") if v.get("name").is_none() => ToolChoice::Auto,
                    Some("none") if v.get("name").is_none() => ToolChoice::None,
                    Some("any") if v.get("name").is_none() => ToolChoice::Required,
                    Some("tool") => ToolChoice::Function(string(&v["name"])?),
                    _ => return Err(InferenceError::InvalidRequest),
                })
            }
        };
        validate(ChatRequest {
            model: self.model,
            messages,
            tools: self
                .tools
                .into_iter()
                .map(|t| FunctionTool {
                    name: t.name,
                    description: t.description,
                    parameters: t.input_schema,
                    strict: None,
                })
                .collect(),
            tool_choice,
            max_output_tokens: Some(self.max_tokens),
            temperature: self.temperature,
            stream: self.stream,
        })
    }
}
fn valid_version(headers: &HeaderMap) -> bool {
    headers.get_all("anthropic-version").iter().count() == 1
        && headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            == Some("2023-06-01")
        && !headers.contains_key("anthropic-beta")
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
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(error_body(InferenceError::InvalidRequest)),
            )
                .into_response();
        }
        Err(_) => return error_response(InferenceError::InvalidRequest),
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
    let request = match wire.normalize() {
        Ok(v) => v,
        Err(e) => return error_response(e),
    };
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
/// `message_start` usage: observed input-side counters only. `output_tokens` is
/// the vendor's cumulative count at stream start (0) and is emitted only when the
/// final count is known, because `message_delta` must then replace it; unknown
/// output stays absent rather than becoming an SDK-visible zero.
fn start_usage(terminal: &Value) -> Value {
    let mut usage = terminal.clone();
    if let Some(fields) = usage.as_object_mut()
        && fields.contains_key("output_tokens")
    {
        fields.insert("output_tokens".into(), 0.into());
    }
    usage
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
                let mut accumulator = Accumulator::default(); let mut done = false;
                while let Some(next) = stream.next().await {
                    match next.and_then(|e| accumulator.push(e)) {
                        Ok(false) => continue, Ok(true) => { done = true; break; }
                        Err(e) => { yield Ok::<_, Infallible>(event("error", error_body(e))); return; }
                    }
                }
                if !done { yield Ok(event("error", error_body(InferenceError::InvalidUpstream))); return; }
                let terminal = match accumulator.finish().and_then(|r| snapshot(&id, &model, &r)) { Ok(v) => v, Err(e) => { yield Ok(event("error", error_body(e))); return; } };
                // Output is buffered, so message_start follows validated completion
                // and carries the observed input-side usage (vendor shape).
                yield Ok(event("message_start", json!({"message":{"id":id,"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":start_usage(&terminal["usage"])}})));
                // Serial blocks avoid illegal interleaving of parallel function argument deltas.
                for (index, block) in terminal["content"].as_array().unwrap().iter().enumerate() {
                    let mut start = block.clone();
                    let delta = if block["type"] == "text" { start["text"] = "".into(); json!({"type":"text_delta","text":block["text"]}) } else { start["input"] = json!({}); json!({"type":"input_json_delta","partial_json":block["input"].to_string()}) };
                    yield Ok(event("content_block_start", json!({"index":index,"content_block":start})));
                    yield Ok(event("content_block_delta", json!({"index":index,"delta":delta})));
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
fn error_body(e: InferenceError) -> Value {
    let kind = match e {
        InferenceError::InvalidRequest
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
        InferenceError::UpstreamUnavailable | InferenceError::RouteCoolingDown(_) => {
            "overloaded_error"
        }
        _ => "api_error",
    };
    json!({"type":"error","error":{"type":kind,"message":e.message()}})
}
fn error_response(e: InferenceError) -> Response {
    super::error_with_body(e, error_body(e))
}
#[cfg(test)]
mod tests;
