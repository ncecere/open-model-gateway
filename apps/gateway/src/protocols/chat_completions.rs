use std::{
    convert::Infallible,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Extension, Json,
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::Principal,
    http::RequestId,
    inference::{Engine, client, error::InferenceError, types::*},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(default)]
    stream: bool,
    stream_options: Option<StreamOptions>,
    temperature: Option<f64>,
    max_completion_tokens: Option<u32>,
    n: Option<u32>,
    #[serde(default)]
    tools: Vec<WireTool>,
    tool_choice: Option<Value>,
    /// Client labels only (session id source for Logs); not forwarded upstream.
    user: Option<String>,
    metadata: Option<client::OpenAiMetadata>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamOptions {
    #[serde(default)]
    include_usage: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMessage {
    role: Role,
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
    tool_call_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: WireFunctionCall,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFunctionCall {
    name: String,
    arguments: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTool {
    #[serde(rename = "type")]
    kind: String,
    function: WireFunction,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireFunction {
    name: String,
    description: Option<String>,
    parameters: Value,
    strict: Option<bool>,
}

fn function_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Request {
    fn normalize(self) -> Result<ChatRequest, InferenceError> {
        let invalid = InferenceError::InvalidRequest;
        if self.model.trim().is_empty()
            || self.model.len() > 200
            || self.messages.is_empty()
            || self.messages.len() > 4096
            || self.tools.len() > 128
            || self.n.is_some_and(|n| n != 1)
            || self.temperature.is_some_and(|n| !(0.0..=2.0).contains(&n))
            || self.max_completion_tokens == Some(0)
            || (!self.stream && self.stream_options.is_some())
        {
            return Err(invalid);
        }
        let mut messages = Vec::with_capacity(self.messages.len());
        for message in self.messages {
            if message.role == Role::Tool {
                if message.tool_call_id.as_ref().is_none_or(|id| id.is_empty())
                    || message.content.is_none()
                    || !message.tool_calls.is_empty()
                {
                    return Err(invalid);
                }
            } else if message.tool_call_id.is_some() {
                return Err(invalid);
            }
            if message.role != Role::Assistant
                && (message.content.is_none() || !message.tool_calls.is_empty())
            {
                return Err(invalid);
            }
            if message.content.is_none() && message.tool_calls.is_empty() {
                return Err(invalid);
            }
            let tool_calls = message
                .tool_calls
                .into_iter()
                .map(|call| {
                    if call.kind != "function"
                        || call.id.is_empty()
                        || !function_name(&call.function.name)
                    {
                        return Err(invalid);
                    }
                    Ok(ToolCall {
                        id: call.id,
                        name: call.function.name,
                        arguments: call.function.arguments,
                    })
                })
                .collect::<Result<_, _>>()?;
            messages.push(Message {
                role: message.role,
                content: message.content,
                tool_calls,
                tool_call_id: message.tool_call_id,
            });
        }
        let mut names = std::collections::BTreeSet::new();
        let tools = self
            .tools
            .into_iter()
            .map(|tool| {
                if tool.kind != "function"
                    || !function_name(&tool.function.name)
                    || !tool.function.parameters.is_object()
                    || !names.insert(tool.function.name.clone())
                {
                    return Err(invalid);
                }
                Ok(FunctionTool {
                    name: tool.function.name,
                    description: tool.function.description,
                    parameters: tool.function.parameters,
                    strict: tool.function.strict,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let tool_choice = match self.tool_choice {
            None => None,
            Some(Value::String(s)) => Some(match s.as_str() {
                "auto" => ToolChoice::Auto,
                "none" => ToolChoice::None,
                "required" if !tools.is_empty() => ToolChoice::Required,
                _ => return Err(invalid),
            }),
            Some(value) => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Named {
                    #[serde(rename = "type")]
                    kind: String,
                    function: Name,
                }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Name {
                    name: String,
                }
                let choice: Named = serde_json::from_value(value).map_err(|_| invalid)?;
                if choice.kind != "function" || !names.contains(&choice.function.name) {
                    return Err(invalid);
                }
                Some(ToolChoice::Function(choice.function.name))
            }
        };
        Ok(ChatRequest {
            model: self.model,
            messages,
            tools,
            tool_choice,
            temperature: self.temperature,
            max_output_tokens: self.max_completion_tokens,
            stream: self.stream,
        })
    }
}

pub async fn handle(
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    input: Result<Json<Request>, JsonRejection>,
) -> Response {
    let wire = match input {
        Ok(Json(request)) => request,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(error_body(InferenceError::InvalidRequest)),
            )
                .into_response();
        }
        Err(_) => return error_response(InferenceError::InvalidRequest),
    };
    if !client::valid_openai_metadata(wire.metadata.as_ref()) {
        return error_response(InferenceError::InvalidRequest);
    }
    let labels = client::current().with_body_session(client::openai_session(
        wire.metadata.as_ref(),
        wire.user.as_deref(),
    ));
    let include_usage = wire
        .stream_options
        .as_ref()
        .is_some_and(|options| options.include_usage);
    let request = match wire.normalize() {
        Ok(request) => request,
        Err(error) => return error_response(error),
    };
    let model = request.model.clone();
    let id = format!("chatcmpl-{}", request_id.0.simple());
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let output = match labels
        .scope(engine.execute(principal, request, request_id.0))
        .await
    {
        Ok(output) => output,
        Err(error) => return error_response(error),
    };
    match output {
        ProviderOutput::Complete(response) => {
            Json(complete_body(&id, created, &model, response)).into_response()
        }
        ProviderOutput::Stream(mut upstream) => {
            let events = async_stream::stream! {
                let chunk = |delta: Value, finish: Value| {
                    let mut value = json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,
                        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
                    if include_usage { value["usage"] = Value::Null; }
                    value
                };
                yield Ok::<_,Infallible>(Event::default().data(chunk(json!({"role":"assistant"}), Value::Null).to_string()));
                while let Some(event) = upstream.next().await {
                    let data = match event {
                        Ok(ChatEvent::Delta { text, tool_calls }) => {
                            let mut delta = json!({});
                            if let Some(text) = text { delta["content"] = Value::String(text); }
                            if !tool_calls.is_empty() {
                                delta["tool_calls"] = Value::Array(tool_calls.into_iter().map(|call| {
                                    let mut value = json!({"index":call.index});
                                    if let Some(id) = call.id { value["id"] = id.into(); value["type"] = "function".into(); }
                                    let mut function = json!({});
                                    if let Some(name) = call.name { function["name"] = name.into(); }
                                    if let Some(arguments) = call.arguments { function["arguments"] = arguments.into(); }
                                    if function.as_object().is_some_and(|o| !o.is_empty()) { value["function"] = function; }
                                    value
                                }).collect());
                            }
                            chunk(delta, Value::Null)
                        }
                        Ok(ChatEvent::Finish(reason)) => chunk(json!({}), json!(reason)),
                        Ok(ChatEvent::Usage(usage)) => {
                            if !include_usage { continue; }
                            let Some(usage) = usage_json(usage) else { continue; };
                            json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[],"usage":usage})
                        }
                        Ok(ChatEvent::Done) => { yield Ok(Event::default().data("[DONE]")); break; }
                        Err(error) => { yield Ok(Event::default().data(error_body(error).to_string())); break; }
                    };
                    yield Ok(Event::default().data(data.to_string()));
                }
            };
            Sse::new(events)
                .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
                .into_response()
        }
    }
}

/// A non-streaming `chat.completion` object.
fn complete_body(id: &str, created: u64, model: &str, response: ChatResponse) -> Value {
    let mut message = json!({"role":"assistant", "content": response.content});
    if !response.tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(response.tool_calls.into_iter().map(|call| json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}})).collect());
    }
    let mut body = json!({"id":id,"object":"chat.completion","created":created,"model":model,
        "choices":[{"index":0,"message":message,"finish_reason":response.finish_reason}]});
    if let Some(usage) = usage_json(response.usage) {
        body["usage"] = usage;
    }
    body
}

/// `/v1/batches` line body (`crate::jobs::lines`): the same contract as an
/// interactive request, never streamed. `max_tokens` is accepted as the
/// Batch API's legacy name of `max_completion_tokens` (not both).
pub(crate) fn batch_request(mut body: Value) -> Result<ChatRequest, InferenceError> {
    if let Some(fields) = body.as_object_mut()
        && !fields.contains_key("max_completion_tokens")
        && let Some(max) = fields.remove("max_tokens")
    {
        fields.insert("max_completion_tokens".into(), max);
    }
    let wire: Request = serde_json::from_value(body).map_err(|_| InferenceError::InvalidRequest)?;
    if wire.stream || !client::valid_openai_metadata(wire.metadata.as_ref()) {
        return Err(InferenceError::InvalidRequest);
    }
    wire.normalize()
}
/// `/v1/batches` line result body.
pub(crate) fn batch_response(id: &str, created: u64, model: &str, response: ChatResponse) -> Value {
    complete_body(id, created, model, response)
}

fn usage_json(usage: Usage) -> Option<Value> {
    // OpenAI's three required counts are emitted only when actually known.
    let input = match usage.billing {
        Some(b) => b.total_input_tokens?,
        None => usage.input_tokens?,
    };
    let output = usage.output_tokens?;
    let mut value = json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input.checked_add(output)?});
    if let Some(b) = usage.billing {
        let mut details = json!({});
        if let Some(n) = b.cache_read_input_tokens {
            details["cached_tokens"] = n.into();
        }
        if let Some(n) = b.cache_write_input_tokens {
            details["cache_write_tokens"] = n.into();
        }
        if details.as_object().is_some_and(|d| !d.is_empty()) {
            value["prompt_tokens_details"] = details;
        }
    }
    Some(value)
}

fn error_body(error: InferenceError) -> Value {
    super::openai_error_body(error)
}
pub(super) fn error_response(error: InferenceError) -> Response {
    super::error_with_body(error, super::openai_error_body(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(value: Value) -> Result<ChatRequest, InferenceError> {
        serde_json::from_value::<Request>(value)
            .map_err(|_| InferenceError::InvalidRequest)?
            .normalize()
    }
    #[test]
    fn accepts_text_and_tools_without_provider_types_in_engine() {
        let request = parse(json!({"model":"company/smart","messages":[{"role":"user","content":"hi"}],
            "tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}],
            "tool_choice":{"type":"function","function":{"name":"weather"}},"max_completion_tokens":32})).unwrap();
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.max_output_tokens, Some(32));
    }
    #[test]
    fn rejects_unsupported_fields_choices_and_multimodal_inputs() {
        for extra in [
            json!({"response_format":{"type":"json_object"}}),
            json!({"n":2}),
            json!({"temperature":3}),
            json!({"stream_options":{"include_usage":true}}),
            json!({"max_tokens":1,"max_completion_tokens":2}),
        ] {
            let mut request = json!({"model":"test","messages":[{"role":"user","content":"hi"}]});
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(parse(request).is_err());
        }
        assert!(parse(json!({"model":"test","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]})).is_err());
    }
    #[test]
    fn missing_usage_is_not_fabricated_as_zero() {
        assert!(usage_json(Usage::default()).is_none());
    }
    #[test]
    fn exclusive_provider_usage_is_rendered_as_inclusive_chat_numbers() {
        let usage = Usage {
            input_tokens: Some(4),
            output_tokens: Some(3),
            billing: Some(crate::billing::BillingUsage {
                total_input_tokens: Some(34),
                uncached_input_tokens: Some(4),
                cache_read_input_tokens: Some(10),
                cache_write_input_tokens: Some(20),
                cache_write_default_input_tokens: Some(0),
                cache_write_5m_input_tokens: Some(8),
                cache_write_1h_input_tokens: Some(12),
            }),
            ..Default::default()
        };
        let value = usage_json(usage).unwrap();
        assert_eq!(value["prompt_tokens"], 34);
        assert_eq!(value["total_tokens"], 37);
        assert_eq!(value["prompt_tokens_details"]["cache_write_tokens"], 20);
        let usage = Usage {
            billing: Some(crate::billing::BillingUsage {
                total_input_tokens: None,
                ..usage.billing.unwrap()
            }),
            ..usage
        };
        assert!(usage_json(usage).is_none());
    }
}
