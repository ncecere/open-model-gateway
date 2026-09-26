//! Native, stateless OpenAI Responses transport. Never sends Chat Completions JSON.
use super::{framing::*, openai::OpenAiAdapter};
use crate::inference::{error::InferenceError, types::*};
use futures_util::StreamExt;
use serde_json::{Value, json};

pub(super) async fn execute(
    adapter: &OpenAiAdapter,
    target: &Deployment,
    request: ChatRequest,
) -> Result<ProviderOutput> {
    if target.provider != "openai"
        || target
            .endpoint
            .as_deref()
            .is_some_and(|v| v != "https://api.openai.com/v1" && v != "https://api.openai.com/v1/")
        || target.region.as_deref().is_some_and(|v| !v.is_empty())
    {
        return Err(InferenceError::Configuration);
    }
    let secret = adapter.resolver.resolve(&target.credential_ref)?;
    let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
        .map_err(|_| InferenceError::Configuration)?;
    auth.set_sensitive(true);
    let response = adapter
        .client
        .post(format!("{}/responses", adapter.base))
        .header(reqwest::header::AUTHORIZATION, auth)
        .json(&encode(&target.upstream_model, &request))
        .send()
        .await
        .map_err(transport)?;
    status(response.status())?;
    if !request.stream {
        return Ok(ProviderOutput::Complete(decode(&body(response).await?)?));
    }
    check_sse(&response)?;
    Ok(ProviderOutput::Stream(Box::pin(
        async_stream::try_stream! {
            let frames = frames(response); futures_util::pin_mut!(frames);
            let mut state = State::default();
            while let Some(value) = frames.next().await {
                for event in state.push(value?)? { let done = matches!(event, ChatEvent::Done); yield event; if done { return; } }
            }
        },
    )))
}
fn encode(model: &str, request: &ChatRequest) -> Value {
    let mut input = vec![];
    for message in &request.messages {
        if message.role == Role::Tool {
            input.push(json!({"type":"function_call_output","call_id":message.tool_call_id,"output":message.content.as_deref().unwrap_or("")}));
        } else {
            if let Some(text) = &message.content {
                input.push(json!({"role":message.role,"content":text}));
            }
            for call in &message.tool_calls {
                input.push(json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":call.arguments}));
            }
        }
    }
    let mut value = json!({"model":model,"input":input,"store":false,"stream":request.stream});
    if let Some(n) = request.max_output_tokens {
        value["max_output_tokens"] = n.into();
    }
    if let Some(n) = request.temperature {
        value["temperature"] = json!(n);
    }
    if !request.tools.is_empty() {
        value["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|t| {
                    let mut v = json!({"type":"function","name":t.name,"parameters":t.parameters});
                    if let Some(s) = &t.description {
                        v["description"] = s.clone().into();
                    }
                    if let Some(s) = t.strict {
                        v["strict"] = s.into();
                    }
                    v
                })
                .collect(),
        );
    }
    if let Some(choice) = &request.tool_choice {
        value["tool_choice"] = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Function(name) => json!({"type":"function","name":name}),
        };
    }
    value
}
fn decode(value: &Value) -> Result<ChatResponse> {
    if !value["error"].is_null() {
        return Err(InferenceError::InvalidUpstream);
    }
    let reason = match value["status"].as_str() {
        Some("completed") => FinishReason::Stop,
        Some("incomplete") => match value["incomplete_details"]["reason"].as_str() {
            Some("max_output_tokens") => FinishReason::Length,
            Some("content_filter") => FinishReason::ContentFilter,
            _ => return Err(InferenceError::InvalidUpstream),
        },
        _ => return Err(InferenceError::InvalidUpstream),
    };
    let mut text = String::new();
    let mut calls = vec![];
    let output = value["output"]
        .as_array()
        .ok_or(InferenceError::InvalidUpstream)?;
    if output.len() > 129 {
        return Err(InferenceError::InvalidUpstream);
    }
    for item in output {
        match item["type"].as_str() {
            Some("message") => {
                fields(item, &["type", "id", "status", "role", "content"])?;
                if item["role"] != "assistant" {
                    return Err(InferenceError::InvalidUpstream);
                }
                for part in item["content"]
                    .as_array()
                    .ok_or(InferenceError::InvalidUpstream)?
                {
                    fields(part, &["type", "text", "annotations", "logprobs"])?;
                    if part["type"] != "output_text"
                        || ["annotations", "logprobs"].iter().any(|key| {
                            !part[key].is_null()
                                && part[key].as_array().is_none_or(|v| !v.is_empty())
                        })
                    {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    text.push_str(string(&part["text"])?);
                }
            }
            Some("function_call") => {
                fields(
                    item,
                    &["type", "id", "status", "call_id", "name", "arguments"],
                )?;
                let id = nonempty(&item["call_id"])?;
                if calls.iter().any(|c: &ToolCall| c.id == id) {
                    return Err(InferenceError::InvalidUpstream);
                }
                calls.push(ToolCall {
                    id,
                    name: nonempty(&item["name"])?,
                    arguments: string(&item["arguments"])?.into(),
                });
            }
            _ => return Err(InferenceError::InvalidUpstream),
        }
    }
    Ok(ChatResponse {
        content: if text.is_empty() { None } else { Some(text) },
        finish_reason: if reason == FinishReason::Stop && !calls.is_empty() {
            FinishReason::ToolCalls
        } else {
            reason
        },
        tool_calls: calls,
        usage: usage(&value["usage"])?,
    })
}
// Native output is validated incrementally; the final native snapshot is checked
// against streamed content so unsupported output cannot be silently discarded.
#[derive(Default)]
struct State {
    started: bool,
    items: std::collections::BTreeMap<u64, Value>,
    bytes: usize,
    text: String,
    calls: std::collections::BTreeMap<u64, ToolCall>,
    stopped: std::collections::BTreeSet<u64>,
    parts: std::collections::BTreeSet<u64>,
    closed_parts: std::collections::BTreeSet<u64>,
}
impl State {
    fn push(&mut self, value: Value) -> Result<Vec<ChatEvent>> {
        let kind = string(&value["type"])?;
        let mut events = vec![];
        match kind {
            "response.created" => {
                if self.started {
                    return Err(InferenceError::InvalidUpstream);
                }
                self.started = true;
            }
            "response.in_progress" if self.started => {}
            "response.output_item.added" if self.started => {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or(InferenceError::InvalidUpstream)?;
                if index != self.items.len() as u64 || index >= 129 {
                    return Err(InferenceError::InvalidUpstream);
                }
                let item = &value["item"];
                nonempty(&item["id"])?;
                self.bytes = self
                    .bytes
                    .checked_add(item.to_string().len())
                    .ok_or(InferenceError::InvalidUpstream)?;
                if self.bytes > BODY_LIMIT {
                    return Err(InferenceError::InvalidUpstream);
                }
                match item["type"].as_str() {
                    Some("message") if item["role"] == "assistant" => {
                        fields(item, &["type", "id", "status", "role", "content"])?;
                        if item["content"].as_array().is_none_or(|v| !v.is_empty()) {
                            return Err(InferenceError::InvalidUpstream);
                        }
                    }
                    Some("function_call") => {
                        fields(
                            item,
                            &["type", "id", "status", "call_id", "name", "arguments"],
                        )?;
                        if !string(&item["arguments"])?.is_empty() || self.calls.len() >= 128 {
                            return Err(InferenceError::InvalidUpstream);
                        }
                        let call = ToolCall {
                            id: nonempty(&item["call_id"])?,
                            name: nonempty(&item["name"])?,
                            arguments: String::new(),
                        };
                        events.push(ChatEvent::Delta {
                            text: None,
                            tool_calls: vec![ToolCallDelta {
                                index: self.calls.len() as u32,
                                id: Some(call.id.clone()),
                                name: Some(call.name.clone()),
                                arguments: None,
                            }],
                        });
                        self.calls.insert(index, call);
                    }
                    _ => return Err(InferenceError::InvalidUpstream),
                }
                self.items.insert(index, item.clone());
            }
            "response.output_text.delta" | "response.function_call_arguments.delta"
                if self.started =>
            {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or(InferenceError::InvalidUpstream)?;
                if self.stopped.contains(&index) {
                    return Err(InferenceError::InvalidUpstream);
                }
                let item = self
                    .items
                    .get_mut(&index)
                    .ok_or(InferenceError::InvalidUpstream)?;
                if value["item_id"] != item["id"] {
                    return Err(InferenceError::InvalidUpstream);
                }
                let delta = string(&value["delta"])?;
                self.bytes = self
                    .bytes
                    .checked_add(delta.len())
                    .ok_or(InferenceError::InvalidUpstream)?;
                if self.bytes > BODY_LIMIT {
                    return Err(InferenceError::InvalidUpstream);
                }
                if kind == "response.output_text.delta" {
                    if item["type"] != "message"
                        || value["content_index"].as_u64() != Some(0)
                        || !self.parts.contains(&index)
                        || self.closed_parts.contains(&index)
                    {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    self.text.push_str(delta);
                    events.push(ChatEvent::Delta {
                        text: Some(delta.into()),
                        tool_calls: vec![],
                    });
                } else {
                    if item["type"] != "function_call" {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    let tool_index =
                        self.calls
                            .keys()
                            .position(|key| *key == index)
                            .ok_or(InferenceError::InvalidUpstream)? as u32;
                    self.calls
                        .get_mut(&index)
                        .ok_or(InferenceError::InvalidUpstream)?
                        .arguments
                        .push_str(delta);
                    events.push(ChatEvent::Delta {
                        text: None,
                        tool_calls: vec![ToolCallDelta {
                            index: tool_index,
                            id: None,
                            name: None,
                            arguments: Some(delta.into()),
                        }],
                    });
                }
            }
            "response.content_part.added" | "response.content_part.done" if self.started => {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or(InferenceError::InvalidUpstream)?;
                if value["content_index"].as_u64() != Some(0)
                    || self
                        .items
                        .get(&index)
                        .is_none_or(|item| item["type"] != "message")
                    || self.stopped.contains(&index)
                    || value["part"]["type"] != "output_text"
                {
                    return Err(InferenceError::InvalidUpstream);
                }
                fields(&value["part"], &["type", "text", "annotations", "logprobs"])?;
                string(&value["part"]["text"])?;
                for key in ["annotations", "logprobs"] {
                    if !value["part"][key].is_null()
                        && value["part"][key].as_array().is_none_or(|v| !v.is_empty())
                    {
                        return Err(InferenceError::InvalidUpstream);
                    }
                }
                if kind == "response.content_part.added" {
                    if !self.parts.insert(index) || value["part"]["text"] != "" {
                        return Err(InferenceError::InvalidUpstream);
                    }
                } else if !self.parts.contains(&index) || !self.closed_parts.insert(index) {
                    return Err(InferenceError::InvalidUpstream);
                }
            }
            "response.output_text.done" | "response.function_call_arguments.done"
                if self.started =>
            {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or(InferenceError::InvalidUpstream)?;
                if !self.items.contains_key(&index) {
                    return Err(InferenceError::InvalidUpstream);
                }
            }
            "response.output_item.done" if self.started => {
                let index = value["output_index"]
                    .as_u64()
                    .ok_or(InferenceError::InvalidUpstream)?;
                let item = self
                    .items
                    .get(&index)
                    .ok_or(InferenceError::InvalidUpstream)?;
                decode(&json!({"status":"completed","output":[value["item"]]}))?;
                if value["item"]["id"] != item["id"]
                    || value["item"]["type"] != item["type"]
                    || (item["type"] == "message"
                        && self.parts.contains(&index)
                        && !self.closed_parts.contains(&index))
                    || !self.stopped.insert(index)
                {
                    return Err(InferenceError::InvalidUpstream);
                }
            }
            "response.completed" | "response.incomplete" if self.started => {
                if (kind == "response.completed" && value["response"]["status"] != "completed")
                    || (kind == "response.incomplete"
                        && value["response"]["status"] != "incomplete")
                {
                    return Err(InferenceError::InvalidUpstream);
                }
                let response = decode(&value["response"])?;
                if self.stopped.len() != self.items.len()
                    || value["response"]["output"].as_array().map(Vec::len)
                        != Some(self.items.len())
                    || response.content.as_deref().unwrap_or("") != self.text
                    || response.tool_calls.len() != self.calls.len()
                    || response
                        .tool_calls
                        .iter()
                        .zip(self.calls.values())
                        .any(|(a, b)| {
                            a.id != b.id || a.name != b.name || a.arguments != b.arguments
                        })
                {
                    return Err(InferenceError::InvalidUpstream);
                }
                events.push(ChatEvent::Finish(response.finish_reason));
                events.push(ChatEvent::Usage(response.usage));
                events.push(ChatEvent::Done);
            }
            _ => return Err(InferenceError::InvalidUpstream),
        }
        Ok(events)
    }
}
#[cfg(test)]
mod tests;
