//! Stateless Responses frontend. Streaming snapshots are bounded and committed only on Engine Done.
use crate::{
    auth::Principal,
    http::RequestId,
    inference::{Engine, client, error::InferenceError, types::*},
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
use std::{
    convert::Infallible,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub(super) type Result<T> = std::result::Result<T, InferenceError>;
const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    model: String,
    input: Value,
    instructions: Option<String>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    store: bool,
    max_output_tokens: Option<u32>,
    temperature: Option<f64>,
    #[serde(default)]
    tools: Vec<Function>,
    tool_choice: Option<Value>,
    text: Option<TextConfig>,
    /// Client labels only (session id source for Logs); not forwarded upstream.
    user: Option<String>,
    metadata: Option<client::OpenAiMetadata>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextConfig {
    format: TextFormat,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextFormat {
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Function {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    description: Option<String>,
    parameters: Value,
    strict: Option<bool>,
}
pub(super) fn fields(value: &Value, allowed: &[&str]) -> Result<()> {
    if value
        .as_object()
        .is_none_or(|v| v.keys().any(|k| !allowed.contains(&k.as_str())))
    {
        return Err(InferenceError::InvalidRequest);
    }
    Ok(())
}
pub(super) fn string(value: &Value) -> Result<String> {
    value
        .as_str()
        .map(String::from)
        .ok_or(InferenceError::InvalidRequest)
}
pub(super) fn name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub(super) fn message(role: Role, text: String) -> Message {
    Message {
        role,
        content: Some(text),
        tool_calls: vec![],
        tool_call_id: None,
    }
}
pub(super) fn validate(request: ChatRequest) -> Result<ChatRequest> {
    if request.model.trim().is_empty()
        || request.model.len() > 200
        || request.messages.is_empty()
        || request.messages.len() > 4096
        || request.tools.len() > 128
        || request.max_output_tokens == Some(0)
        || request
            .temperature
            .is_some_and(|t| !(0.0..=2.0).contains(&t))
    {
        return Err(InferenceError::InvalidRequest);
    }
    let mut names = std::collections::BTreeSet::new();
    for tool in &request.tools {
        if !name(&tool.name) || !tool.parameters.is_object() || !names.insert(&tool.name) {
            return Err(InferenceError::InvalidRequest);
        }
    }
    match &request.tool_choice {
        Some(ToolChoice::Required) if names.is_empty() => {
            return Err(InferenceError::InvalidRequest);
        }
        Some(ToolChoice::Function(n)) if !names.contains(n) => {
            return Err(InferenceError::InvalidRequest);
        }
        _ => {}
    }
    Ok(request)
}
impl Request {
    fn normalize(self) -> Result<ChatRequest> {
        if self.store || self.text.is_some_and(|t| t.format.kind != "text") {
            return Err(InferenceError::InvalidRequest);
        }
        let mut messages = vec![];
        if let Some(instructions) = self.instructions {
            messages.push(message(Role::System, instructions));
        }
        match self.input {
            Value::String(text) => messages.push(message(Role::User, text)),
            Value::Array(items) if !items.is_empty() && items.len() <= 4096 => {
                for item in items {
                    match item["type"].as_str() {
                        None | Some("message") => {
                            fields(&item, &["type", "role", "content"])?;
                            if item
                                .get("type")
                                .is_some_and(|v| v.as_str() != Some("message"))
                            {
                                return Err(InferenceError::InvalidRequest);
                            }
                            let role = match item["role"].as_str() {
                                Some("user") => Role::User,
                                Some("assistant") => Role::Assistant,
                                Some("system") => Role::System,
                                Some("developer") => Role::Developer,
                                _ => return Err(InferenceError::InvalidRequest),
                            };
                            let text = match &item["content"] {
                                Value::String(s) => s.clone(),
                                Value::Array(parts) if !parts.is_empty() => {
                                    let mut text = String::new();
                                    for part in parts {
                                        fields(part, &["type", "text"])?;
                                        if part["type"] != "input_text"
                                            && !(role == Role::Assistant
                                                && part["type"] == "output_text")
                                        {
                                            return Err(InferenceError::InvalidRequest);
                                        }
                                        text.push_str(&string(&part["text"])?);
                                    }
                                    text
                                }
                                _ => return Err(InferenceError::InvalidRequest),
                            };
                            messages.push(message(role, text));
                        }
                        Some("function_call") => {
                            fields(&item, &["type", "call_id", "name", "arguments"])?;
                            let id = string(&item["call_id"])?;
                            let n = string(&item["name"])?;
                            if id.is_empty() || !name(&n) {
                                return Err(InferenceError::InvalidRequest);
                            }
                            messages.push(Message {
                                role: Role::Assistant,
                                content: None,
                                tool_call_id: None,
                                tool_calls: vec![ToolCall {
                                    id,
                                    name: n,
                                    arguments: string(&item["arguments"])?,
                                }],
                            });
                        }
                        Some("function_call_output") => {
                            fields(&item, &["type", "call_id", "output"])?;
                            let id = string(&item["call_id"])?;
                            if id.is_empty() {
                                return Err(InferenceError::InvalidRequest);
                            }
                            messages.push(Message {
                                role: Role::Tool,
                                content: Some(string(&item["output"])?),
                                tool_calls: vec![],
                                tool_call_id: Some(id),
                            });
                        }
                        _ => return Err(InferenceError::InvalidRequest),
                    }
                }
            }
            _ => return Err(InferenceError::InvalidRequest),
        }
        let tools = self
            .tools
            .into_iter()
            .map(|t| {
                if t.kind != "function" {
                    return Err(InferenceError::InvalidRequest);
                }
                Ok(FunctionTool {
                    name: t.name,
                    description: t.description,
                    parameters: t.parameters,
                    strict: t.strict,
                })
            })
            .collect::<Result<_>>()?;
        let tool_choice = match self.tool_choice {
            None => None,
            Some(Value::String(s)) => Some(match s.as_str() {
                "auto" => ToolChoice::Auto,
                "none" => ToolChoice::None,
                "required" => ToolChoice::Required,
                _ => return Err(InferenceError::InvalidRequest),
            }),
            Some(v) => {
                fields(&v, &["type", "name"])?;
                if v["type"] != "function" {
                    return Err(InferenceError::InvalidRequest);
                }
                Some(ToolChoice::Function(string(&v["name"])?))
            }
        };
        validate(ChatRequest {
            model: self.model,
            messages,
            tools,
            tool_choice,
            temperature: self.temperature,
            max_output_tokens: self.max_output_tokens,
            stream: self.stream,
        })
    }
}
pub async fn handle(
    Extension(engine): Extension<Engine>,
    Extension(principal): Extension<Principal>,
    Extension(request_id): Extension<RequestId>,
    input: std::result::Result<Json<Request>, JsonRejection>,
) -> Response {
    let wire = match input {
        Ok(Json(v)) => v,
        Err(e) => return rejection(e),
    };
    if !client::valid_openai_metadata(wire.metadata.as_ref()) {
        return error_response(InferenceError::InvalidRequest);
    }
    let labels = client::current().with_body_session(client::openai_session(
        wire.metadata.as_ref(),
        wire.user.as_deref(),
    ));
    let request = match wire.normalize() {
        Ok(v) => v,
        Err(e) => return error_response(e),
    };
    let model = request.model.clone();
    let id = format!("resp_{}", request_id.0.simple());
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let output = match labels
        .scope(engine.execute_protocol(principal, request, request_id.0, ApiProtocol::Responses))
        .await
    {
        Ok(v) => v,
        Err(e) => return error_response(e),
    };
    render(output, id, model, created)
}
/// `/v1/batches` line body (`crate::jobs::lines`): the interactive contract,
/// never streamed.
pub(crate) fn batch_request(body: Value) -> Result<ChatRequest> {
    let wire: Request = serde_json::from_value(body).map_err(|_| InferenceError::InvalidRequest)?;
    if wire.stream || !client::valid_openai_metadata(wire.metadata.as_ref()) {
        return Err(InferenceError::InvalidRequest);
    }
    wire.normalize()
}
/// `/v1/batches` line result body (a completed `response` object).
pub(crate) fn batch_response(
    id: &str,
    created: u64,
    model: &str,
    response: &ChatResponse,
) -> Value {
    snapshot(id, model, created, response)
}
fn snapshot(id: &str, model: &str, created: u64, response: &ChatResponse) -> Value {
    let mut output = vec![];
    if let Some(text) = &response.content {
        output.push(json!({"id":format!("msg_{id}"),"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}));
    }
    for (i, call) in response.tool_calls.iter().enumerate() {
        output.push(json!({"id":format!("fc_{id}_{i}"),"type":"function_call","status":"completed","call_id":call.id,"name":call.name,"arguments":call.arguments}));
    }
    let incomplete = matches!(
        response.finish_reason,
        FinishReason::Length | FinishReason::ContentFilter
    );
    if incomplete {
        for item in &mut output {
            item["status"] = "incomplete".into();
        }
    }
    let mut v = json!({"id":id,"object":"response","created_at":created,"model":model,"status":if incomplete { "incomplete" } else { "completed" },"error":null,"incomplete_details":null,"output":output,"store":false,"usage":null});
    if incomplete {
        v["incomplete_details"] = json!({"reason":if response.finish_reason == FinishReason::Length { "max_output_tokens" } else { "content_filter" }});
    }
    let input = match response.usage.billing {
        Some(b) => b.total_input_tokens,
        None => response.usage.input_tokens,
    };
    if let (Some(input), Some(output)) = (input, response.usage.output_tokens)
        && let Some(total) = input.checked_add(output)
    {
        v["usage"] = json!({"input_tokens":input,"output_tokens":output,"total_tokens":total});
        if let Some(b) = response.usage.billing {
            let mut details = json!({});
            if let Some(n) = b.cache_read_input_tokens {
                details["cached_tokens"] = n.into();
            }
            if let Some(n) = b.cache_write_input_tokens {
                details["cache_write_tokens"] = n.into();
            }
            if details.as_object().is_some_and(|d| !d.is_empty()) {
                v["usage"]["input_tokens_details"] = details;
            }
        }
    }
    v
}
pub(super) fn event(kind: &str, mut value: Value) -> Event {
    value["type"] = kind.into();
    Event::default().event(kind).data(value.to_string())
}
fn numbered(kind: &str, mut value: Value, sequence: &mut u64) -> Event {
    value["sequence_number"] = (*sequence).into();
    *sequence += 1;
    event(kind, value)
}
fn render(output: ProviderOutput, id: String, model: String, created: u64) -> Response {
    match output {
        ProviderOutput::Complete(response) => {
            Json(snapshot(&id, &model, created, &response)).into_response()
        }
        ProviderOutput::Stream(mut stream) => {
            let events = async_stream::stream! {
                let mut seq = 0; let mut start = snapshot(&id, &model, created, &ChatResponse { content: None, tool_calls: vec![], finish_reason: FinishReason::Stop, usage: Usage::default() }); start["status"] = "in_progress".into();
                yield Ok::<_,Infallible>(numbered("response.created", json!({"response":start}), &mut seq));
                yield Ok(numbered("response.in_progress", json!({"response":start}), &mut seq));
                let mut accumulator = Accumulator::default(); let mut done = false;
                while let Some(next) = stream.next().await {
                    match next.and_then(|event| accumulator.push(event)) {
                        Ok(false) => continue,
                        Ok(true) => { done = true; break; }
                        Err(e) => { yield Ok(numbered("error", json!({"code":e.code(),"message":e.message(),"param":null}), &mut seq)); return; }
                    }
                }
                if !done { let e = InferenceError::InvalidUpstream; yield Ok(numbered("error", json!({"code":e.code(),"message":e.message(),"param":null}), &mut seq)); return; }
                let response = match accumulator.finish() { Ok(r) => r, Err(e) => { yield Ok(numbered("error", json!({"code":e.code(),"message":e.message(),"param":null}), &mut seq)); return; } };
                let terminal = snapshot(&id, &model, created, &response);
                for (i, item) in terminal["output"].as_array().unwrap().iter().enumerate() {
                    let mut initial = item.clone(); initial["status"] = "in_progress".into();
                    if item["type"] == "message" { initial["content"] = json!([]); } else { initial["arguments"] = "".into(); }
                    yield Ok(numbered("response.output_item.added", json!({"output_index":i,"item":initial}), &mut seq));
                    if item["type"] == "message" {
                        let part = &item["content"][0]; let mut empty = part.clone(); empty["text"] = "".into();
                        yield Ok(numbered("response.content_part.added", json!({"output_index":i,"item_id":item["id"],"content_index":0,"part":empty}), &mut seq));
                        yield Ok(numbered("response.output_text.delta", json!({"output_index":i,"item_id":item["id"],"content_index":0,"delta":part["text"]}), &mut seq));
                        yield Ok(numbered("response.output_text.done", json!({"output_index":i,"item_id":item["id"],"content_index":0,"text":part["text"]}), &mut seq));
                        yield Ok(numbered("response.content_part.done", json!({"output_index":i,"item_id":item["id"],"content_index":0,"part":part}), &mut seq));
                    } else {
                        yield Ok(numbered("response.function_call_arguments.delta", json!({"output_index":i,"item_id":item["id"],"delta":item["arguments"]}), &mut seq));
                        yield Ok(numbered("response.function_call_arguments.done", json!({"output_index":i,"item_id":item["id"],"arguments":item["arguments"],"name":item["name"]}), &mut seq));
                    }
                    yield Ok(numbered("response.output_item.done", json!({"output_index":i,"item":item}), &mut seq));
                }
                let kind = if terminal["status"] == "completed" { "response.completed" } else { "response.incomplete" };
                yield Ok(numbered(kind, json!({"response":terminal}), &mut seq));
            };
            Sse::new(events)
                .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
                .into_response()
        }
    }
}
/// Bounded canonical output buffer shared by native frontends. No network task is spawned.
#[derive(Default)]
pub(super) struct Accumulator {
    text: Option<String>,
    tools: std::collections::BTreeMap<u32, ToolCall>,
    reason: Option<FinishReason>,
    usage: Usage,
    bytes: usize,
}
impl Accumulator {
    pub(super) fn push(&mut self, event: ChatEvent) -> Result<bool> {
        match event {
            ChatEvent::Delta { text, tool_calls } if self.reason.is_none() => {
                let size = text.as_ref().map_or(0, String::len)
                    + tool_calls
                        .iter()
                        .map(|t| {
                            t.id.as_ref().map_or(0, String::len)
                                + t.name.as_ref().map_or(0, String::len)
                                + t.arguments.as_ref().map_or(0, String::len)
                        })
                        .sum::<usize>();
                self.bytes = self
                    .bytes
                    .checked_add(size)
                    .ok_or(InferenceError::InvalidUpstream)?;
                if self.bytes > OUTPUT_LIMIT {
                    return Err(InferenceError::InvalidUpstream);
                }
                if let Some(text) = text {
                    self.text.get_or_insert_with(String::new).push_str(&text);
                }
                for delta in tool_calls {
                    if delta.index >= 128 {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    let call = self.tools.entry(delta.index).or_insert_with(|| ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                    if let Some(id) = delta.id {
                        call.id.push_str(&id);
                    }
                    if let Some(name) = delta.name {
                        call.name.push_str(&name);
                    }
                    if let Some(args) = delta.arguments {
                        call.arguments.push_str(&args);
                    }
                }
            }
            ChatEvent::Finish(reason) if self.reason.is_none() => self.reason = Some(reason),
            ChatEvent::Usage(usage) => self.usage = usage,
            ChatEvent::Done if self.reason.is_some() => return Ok(true),
            _ => return Err(InferenceError::InvalidUpstream),
        }
        Ok(false)
    }
    pub(super) fn finish(self) -> Result<ChatResponse> {
        let mut ids = std::collections::BTreeSet::new();
        for tool in self.tools.values() {
            if tool.id.is_empty() || !name(&tool.name) || !ids.insert(&tool.id) {
                return Err(InferenceError::InvalidUpstream);
            }
        }
        Ok(ChatResponse {
            content: self.text,
            tool_calls: self.tools.into_values().collect(),
            finish_reason: self.reason.ok_or(InferenceError::InvalidUpstream)?,
            usage: self.usage,
        })
    }
}
pub(super) fn status(error: InferenceError) -> StatusCode {
    super::http_status(error)
}
fn error_body(e: InferenceError) -> Value {
    super::openai_error_body(e)
}
fn error_response(e: InferenceError) -> Response {
    super::error_with_body(e, error_body(e))
}
fn rejection(e: JsonRejection) -> Response {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(error_body(InferenceError::InvalidRequest)),
        )
            .into_response()
    } else {
        error_response(InferenceError::InvalidRequest)
    }
}
#[cfg(test)]
mod tests;
