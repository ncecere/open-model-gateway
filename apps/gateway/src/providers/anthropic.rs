//! Anthropic's fixed-origin Messages transport, restricted to text and function tools.
use super::{ProviderAdapter, framing::*, secrets::SecretResolver};
use crate::inference::{error::InferenceError, types::*};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
const BASE: &str = "https://api.anthropic.com/v1";
pub struct AnthropicAdapter {
    client: reqwest::Client,
    resolver: Arc<dyn SecretResolver>,
    base: String,
}
impl AnthropicAdapter {
    pub fn new(resolver: Arc<dyn SecretResolver>) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| anyhow::anyhow!("Unable to initialize provider transport"))?,
            resolver,
            base: BASE.into(),
        })
    }
    #[cfg(test)]
    fn for_test(resolver: Arc<dyn SecretResolver>, base: String) -> Self {
        let url = reqwest::Url::parse(&base).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        let mut adapter = Self::new(resolver).unwrap();
        adapter.base = base;
        adapter
    }
}
#[async_trait]
impl ProviderAdapter for AnthropicAdapter {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            text_chat: true,
            streaming: true,
            tools: true,
        }
    }
    fn supports_protocol(&self, protocol: ApiProtocol) -> bool {
        matches!(
            protocol,
            ApiProtocol::ChatCompletions | ApiProtocol::Messages
        )
    }
    async fn execute(&self, target: &Deployment, request: ChatRequest) -> Result<ProviderOutput> {
        if target.provider != "anthropic"
            || target.credential_ref == "none"
            || target.endpoint.is_some()
            || target.region.as_deref().is_some_and(|v| !v.is_empty())
        {
            return Err(InferenceError::Configuration);
        }
        let payload = encode(&target.upstream_model, &request)?;
        let secret = self.resolver.resolve(&target.credential_ref)?;
        let mut key = reqwest::header::HeaderValue::from_str(secret.expose())
            .map_err(|_| InferenceError::Configuration)?;
        key.set_sensitive(true);
        let response = self
            .client
            .post(format!("{}/messages", self.base))
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", key)
            .json(&payload)
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
                let frames = frames(response); futures_util::pin_mut!(frames); let mut state = State::default();
                while let Some(value) = frames.next().await {
                    for event in state.push(value?)? { let done = matches!(event, ChatEvent::Done); yield event; if done { return; } }
                }
            },
        )))
    }
}
fn encode(model: &str, request: &ChatRequest) -> Result<Value> {
    if request
        .temperature
        .is_some_and(|t| !(0.0..=1.0).contains(&t))
        || request.tools.iter().any(|t| t.strict == Some(true))
    {
        return Err(InferenceError::Unsupported);
    }
    let mut system = vec![];
    let mut messages = vec![];
    let mut conversation = false;
    for message in &request.messages {
        if matches!(message.role, Role::System | Role::Developer) {
            if conversation || !message.tool_calls.is_empty() {
                return Err(InferenceError::Unsupported);
            }
            if let Some(text) = &message.content {
                system.push(json!({"type":"text","text":text}));
            }
            continue;
        }
        conversation = true;
        let mut content = vec![];
        if message.role == Role::Tool {
            content.push(json!({"type":"tool_result","tool_use_id":message.tool_call_id,"content":message.content}));
        } else {
            if let Some(text) = &message.content {
                content.push(json!({"type":"text","text":text}));
            }
            for call in &message.tool_calls {
                let input: Value = serde_json::from_str(&call.arguments)
                    .map_err(|_| InferenceError::InvalidRequest)?;
                if !input.is_object() {
                    return Err(InferenceError::InvalidRequest);
                }
                content
                    .push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":input}));
            }
        }
        messages.push(json!({"role":if message.role == Role::Assistant { "assistant" } else { "user" },"content":content}));
    }
    let mut value = json!({"model":model,"messages":messages,"max_tokens":request.max_output_tokens.unwrap_or(1024),"stream":request.stream});
    if !system.is_empty() {
        value["system"] = system.into();
    }
    if let Some(t) = request.temperature {
        value["temperature"] = json!(t);
    }
    if !request.tools.is_empty() {
        value["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|t| {
                    let mut v = json!({"name":t.name,"input_schema":t.parameters});
                    if let Some(d) = &t.description {
                        v["description"] = d.clone().into();
                    }
                    v
                })
                .collect(),
        );
    }
    if let Some(choice) = &request.tool_choice {
        value["tool_choice"] = match choice {
            ToolChoice::Auto => json!({"type":"auto"}),
            ToolChoice::None => json!({"type":"none"}),
            ToolChoice::Required => json!({"type":"any"}),
            ToolChoice::Function(name) => json!({"type":"tool","name":name}),
        };
    }
    Ok(value)
}
fn finish(value: &Value) -> Result<FinishReason> {
    match value.as_str() {
        Some("end_turn" | "stop_sequence") => Ok(FinishReason::Stop),
        Some("max_tokens") => Ok(FinishReason::Length),
        Some("tool_use") => Ok(FinishReason::ToolCalls),
        _ => Err(InferenceError::InvalidUpstream),
    }
}
/// Structural failure still fails, but a valid usage object is kept as evidence.
fn decode(value: &Value) -> Result<ChatResponse> {
    crate::inference::evidence::preserve(decode_shape(value), || {
        value["usage"].is_object().then(|| usage(&value["usage"]))
    })
}
fn decode_shape(value: &Value) -> Result<ChatResponse> {
    fields(
        value,
        &[
            "id",
            "type",
            "role",
            "model",
            "content",
            "stop_reason",
            "stop_sequence",
            "usage",
        ],
    )?;
    if value["type"] != "message" || value["role"] != "assistant" {
        return Err(InferenceError::InvalidUpstream);
    }
    let mut text = String::new();
    let mut calls = vec![];
    let blocks = value["content"]
        .as_array()
        .ok_or(InferenceError::InvalidUpstream)?;
    if blocks.len() > 256 {
        return Err(InferenceError::InvalidUpstream);
    }
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                fields(block, &["type", "text"])?;
                text.push_str(string(&block["text"])?);
            }
            Some("tool_use") => {
                fields(block, &["type", "id", "name", "input"])?;
                if !block["input"].is_object() {
                    return Err(InferenceError::InvalidUpstream);
                }
                let id = nonempty(&block["id"])?;
                if calls.iter().any(|c: &ToolCall| c.id == id) || calls.len() == 128 {
                    return Err(InferenceError::InvalidUpstream);
                }
                calls.push(ToolCall {
                    id,
                    name: nonempty(&block["name"])?,
                    arguments: block["input"].to_string(),
                });
            }
            _ => return Err(InferenceError::InvalidUpstream),
        }
    }
    Ok(ChatResponse {
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls: calls,
        finish_reason: finish(&value["stop_reason"])?,
        usage: usage(&value["usage"])?,
    })
}
#[derive(Default)]
struct State {
    started: bool,
    finished: bool,
    done: bool,
    active: Option<(u64, bool)>,
    blocks: u64,
    tools: u32,
    usage: Usage,
    arguments: String,
}
impl State {
    fn push(&mut self, value: Value) -> Result<Vec<ChatEvent>> {
        if self.done {
            return Err(InferenceError::InvalidUpstream);
        }
        let mut events = vec![];
        match value["type"].as_str() {
            Some("ping") => {}
            Some("message_start") if !self.started => {
                let message = &value["message"];
                fields(
                    message,
                    &[
                        "id",
                        "type",
                        "role",
                        "model",
                        "content",
                        "stop_reason",
                        "stop_sequence",
                        "usage",
                    ],
                )?;
                if message["type"] != "message"
                    || message["role"] != "assistant"
                    || message["content"].as_array().is_none_or(|v| !v.is_empty())
                    || !message["stop_reason"].is_null()
                {
                    return Err(InferenceError::InvalidUpstream);
                }
                self.started = true;
                self.usage = usage(&message["usage"])?;
                // Start counts are preliminary; only cumulative message_delta usage
                // is evidence of generated output. Do not settle a missing final
                // output count using the usual start-event zero.
                self.usage.output_tokens = None;
            }
            Some("content_block_start")
                if self.started && !self.finished && self.active.is_none() =>
            {
                let index = value["index"]
                    .as_u64()
                    .ok_or(InferenceError::InvalidUpstream)?;
                if index != self.blocks || index >= 256 {
                    return Err(InferenceError::InvalidUpstream);
                }
                let block = &value["content_block"];
                let tool = match block["type"].as_str() {
                    Some("text") => {
                        fields(block, &["type", "text"])?;
                        let text = string(&block["text"])?;
                        if !text.is_empty() {
                            events.push(ChatEvent::Delta {
                                text: Some(text.into()),
                                tool_calls: vec![],
                            });
                        }
                        false
                    }
                    Some("tool_use") => {
                        fields(block, &["type", "id", "name", "input"])?;
                        if self.tools >= 128
                            || block["input"].as_object().is_none_or(|v| !v.is_empty())
                        {
                            return Err(InferenceError::InvalidUpstream);
                        }
                        events.push(ChatEvent::Delta {
                            text: None,
                            tool_calls: vec![ToolCallDelta {
                                index: self.tools,
                                id: Some(nonempty(&block["id"])?),
                                name: Some(nonempty(&block["name"])?),
                                arguments: None,
                            }],
                        });
                        self.tools += 1;
                        true
                    }
                    _ => return Err(InferenceError::InvalidUpstream),
                };
                self.active = Some((index, tool));
                self.blocks += 1;
            }
            Some("content_block_delta") if self.started && !self.finished => {
                let (index, tool) = self.active.ok_or(InferenceError::InvalidUpstream)?;
                if value["index"].as_u64() != Some(index) {
                    return Err(InferenceError::InvalidUpstream);
                }
                let delta = &value["delta"];
                if tool {
                    fields(delta, &["type", "partial_json"])?;
                    if delta["type"] != "input_json_delta" {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    let partial = string(&delta["partial_json"])?;
                    if partial.len() > BODY_LIMIT - self.arguments.len() {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    self.arguments.push_str(partial);
                    events.push(ChatEvent::Delta {
                        text: None,
                        tool_calls: vec![ToolCallDelta {
                            index: self.tools - 1,
                            id: None,
                            name: None,
                            arguments: Some(partial.into()),
                        }],
                    });
                } else {
                    fields(delta, &["type", "text"])?;
                    if delta["type"] != "text_delta" {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    events.push(ChatEvent::Delta {
                        text: Some(string(&delta["text"])?.into()),
                        tool_calls: vec![],
                    });
                }
            }
            Some("content_block_stop") if self.active.is_some() => {
                if value["index"].as_u64() != self.active.map(|v| v.0) {
                    return Err(InferenceError::InvalidUpstream);
                }
                if self.active.is_some_and(|v| v.1) {
                    if self.arguments.is_empty() {
                        events.push(ChatEvent::Delta {
                            text: None,
                            tool_calls: vec![ToolCallDelta {
                                index: self.tools - 1,
                                id: None,
                                name: None,
                                arguments: Some("{}".into()),
                            }],
                        });
                    } else if !serde_json::from_str::<Value>(&self.arguments)
                        .map_err(|_| InferenceError::InvalidUpstream)?
                        .is_object()
                    {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    self.arguments.clear();
                }
                self.active = None;
            }
            Some("message_delta") if self.started && !self.finished && self.active.is_none() => {
                fields(&value["delta"], &["stop_reason", "stop_sequence"])?;
                self.usage = super::metering::merge(self.usage, usage(&value["usage"])?)?;
                if !value["delta"]["stop_reason"].is_null() {
                    self.finished = true;
                    events.push(ChatEvent::Finish(finish(&value["delta"]["stop_reason"])?));
                }
            }
            Some("message_stop") if self.finished => {
                self.done = true;
                events.push(ChatEvent::Usage(self.usage));
                events.push(ChatEvent::Done);
            }
            _ => return Err(InferenceError::InvalidUpstream),
        }
        Ok(events)
    }
}
#[cfg(test)]
mod tests;
