//! Anthropic's fixed-origin Messages transport, restricted to text and function tools.
use super::{ProviderAdapter, framing::*, secrets::SecretResolver};
use crate::inference::{error::InferenceError, types::*};
use crate::jobs::types::{
    BatchEndpoint, BatchRequest, ByteStream, NativeResult, UpstreamBatch, UpstreamId,
};
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
                // Every resolved address must pass the egress policy (#33).
                .dns_resolver(crate::egress::resolver(crate::egress::Purpose::Provider))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| anyhow::anyhow!("Unable to initialize provider transport"))?,
            resolver,
            base: BASE.into(),
        })
    }
    #[cfg(test)]
    pub(crate) fn for_test(resolver: Arc<dyn SecretResolver>, base: String) -> Self {
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
        super::capabilities::serves(self.id(), protocol)
    }
    /// Reuses the real, pure request-encoding conversion (`encode`) so
    /// pre-admission validation can never drift from what `execute` would
    /// actually reject: the model id is irrelevant to validation (it is only
    /// ever copied verbatim into the payload), so a placeholder is safe and
    /// the encoded payload itself is discarded.
    fn validate_chat_request(&self, request: &ChatRequest) -> Result<()> {
        encode("", request).map(|_| ())
    }
    fn native_batch(&self, target: &Deployment, endpoint: BatchEndpoint) -> bool {
        batches::native_batch(target, endpoint)
    }
    fn encode_native_line(
        &self,
        target: &Deployment,
        endpoint: BatchEndpoint,
        custom_id: &str,
        request: &BatchRequest,
    ) -> Result<Vec<u8>> {
        batches::encode_native_line(target, endpoint, custom_id, request)
    }
    async fn submit_native_batch(
        &self,
        target: &Deployment,
        endpoint: BatchEndpoint,
        records: ByteStream,
    ) -> Result<UpstreamBatch> {
        batches::submit_native_batch(self, target, endpoint, records).await
    }
    async fn retrieve_batch(
        &self,
        target: &Deployment,
        batch: &UpstreamId,
    ) -> Result<UpstreamBatch> {
        batches::batch_action(self, target, batch, false).await
    }
    async fn cancel_batch(&self, target: &Deployment, batch: &UpstreamId) -> Result<UpstreamBatch> {
        batches::batch_action(self, target, batch, true).await
    }
    async fn native_batch_results(
        &self,
        target: &Deployment,
        batch: &UpstreamBatch,
    ) -> Result<ByteStream> {
        batches::native_batch_results(self, target, batch).await
    }
    fn decode_native_result(
        &self,
        _target: &Deployment,
        endpoint: BatchEndpoint,
        line: &[u8],
    ) -> Result<NativeResult> {
        batches::decode_native_result(endpoint, line)
    }
    async fn delete_native_batch(&self, target: &Deployment, batch: &UpstreamBatch) -> Result<()> {
        batches::delete_native_batch(self, target, batch).await
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
        let mut outbound = self
            .client
            .post(format!("{}/messages", self.base))
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", key);
        if let Some(options) = &request.messages_options {
            // Header name is fixed; append each original value, never synthesize
            // betas or copy arbitrary headers (especially inference credentials).
            for beta in &options.beta_headers {
                outbound = outbound.header("anthropic-beta", beta);
            }
        }
        let response = outbound.json(&payload).send().await.map_err(transport)?;
        let response = if request.messages_options.is_some() {
            super::framing::anthropic_generation_status(response).await?
        } else {
            status(response.status())?;
            response
        };
        if !request.stream {
            return Ok(ProviderOutput::Complete(decode_for(
                &super::framing::body_limit(response, request.response_limit()).await?,
                request.messages_options.is_some(),
            )?));
        }
        check_sse(&response)?;
        Ok(ProviderOutput::Stream(Box::pin(
            async_stream::try_stream! {
                let frames = frames(response); futures_util::pin_mut!(frames); let mut state = State { limit: Some(request.response_limit()), messages: request.messages_options.is_some(), ..State::default() };
                while let Some(value) = frames.next().await {
                    for event in state.push(value?)? { let done = matches!(event, ChatEvent::Done); yield event; if done { return; } }
                }
            },
        )))
    }
}
fn encode(model: &str, request: &ChatRequest) -> Result<Value> {
    super::messages::validate(request, super::messages::ProviderClass::Anthropic)?;
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
    if let Some(options) = &request.messages_options {
        if let Some(p) = options.top_p {
            value["top_p"] = json!(p);
        }
        if let Some(k) = options.top_k {
            value["top_k"] = json!(k);
        }
        if let Some(stops) = &options.stop_sequences {
            value["stop_sequences"] = json!(stops);
        }
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
    if let Some(options) = &request.messages_options {
        // Only fully grammar-validated envelopes retained by the Messages
        // normalizer; metadata.user_id was intentionally never retained here.
        if !options.native_messages.is_empty() {
            value["messages"] = json!(options.native_messages);
        }
        if let Some(system) = &options.system {
            value["system"] = system.clone();
        }
        if !options.native_tools.is_empty() {
            value["tools"] = json!(options.native_tools);
        }
        if let Some(thinking) = &options.thinking {
            value["thinking"] = thinking.clone();
        }
        if let Some(output) = &options.output_config {
            value["output_config"] = output.clone();
        }
        if let Some(context) = &options.context_management {
            value["context_management"] = context.clone();
        }
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
        Some("refusal") => Ok(FinishReason::Refusal),
        Some("model_context_window_exceeded") => Ok(FinishReason::ContextWindowExceeded),
        _ => Err(InferenceError::InvalidUpstream),
    }
}
/// Structural failure still fails, but a valid usage object is kept as evidence.
#[cfg(test)]
fn decode(value: &Value) -> Result<ChatResponse> {
    decode_for(value, false)
}
fn decode_for(value: &Value, messages: bool) -> Result<ChatResponse> {
    crate::inference::evidence::preserve(decode_shape(value, messages), || {
        value["usage"].is_object().then(|| {
            usage(&value["usage"]).map(|u| super::metering::with_model(u, &value["model"]))
        })
    })
}
fn decode_shape(value: &Value, messages: bool) -> Result<ChatResponse> {
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
            Some("thinking") if messages => {
                fields(block, &["type", "thinking", "signature"])?;
                string(&block["thinking"])?;
                string(&block["signature"])?;
            }
            Some("redacted_thinking") if messages => {
                fields(block, &["type", "data"])?;
                string(&block["data"])?;
            }
            _ => return Err(InferenceError::InvalidUpstream),
        }
    }
    Ok(ChatResponse {
        messages_content: messages.then(|| blocks.clone()),
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls: calls,
        finish_reason: finish(&value["stop_reason"])?,
        usage: super::metering::with_model(usage(&value["usage"])?, &value["model"]),
    })
}
#[derive(Default)]
struct State {
    messages: bool,
    started: bool,
    finished: bool,
    done: bool,
    active: Option<(u64, BlockKind)>,
    blocks: u64,
    tools: u32,
    usage: Usage,
    arguments: String,
    native_bytes: usize,
    limit: Option<usize>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Tool,
    Thinking,
    Redacted,
}
impl State {
    fn push(&mut self, value: Value) -> Result<Vec<ChatEvent>> {
        if self.done {
            return Err(InferenceError::InvalidUpstream);
        }
        let mut events = vec![];
        if value["type"] == "error" {
            return Err(if self.messages {
                super::framing::anthropic_error(&value).unwrap_or(InferenceError::InvalidUpstream)
            } else {
                InferenceError::InvalidUpstream
            });
        }
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
                self.usage =
                    super::metering::with_model(usage(&message["usage"])?, &message["model"]);
                // Start counts are preliminary; only cumulative message_delta usage
                // is evidence of generated output. Do not settle a missing final
                // output count (or its thinking breakdown) using a start-event zero.
                self.usage.output_tokens = None;
                self.usage.reasoning_tokens = None;
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
                let kind = match block["type"].as_str() {
                    Some("text") => {
                        fields(block, &["type", "text"])?;
                        let text = string(&block["text"])?;
                        if !text.is_empty() {
                            events.push(ChatEvent::Delta {
                                text: Some(text.into()),
                                tool_calls: vec![],
                            });
                        }
                        BlockKind::Text
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
                        BlockKind::Tool
                    }
                    Some("thinking") if self.messages => {
                        fields(block, &["type", "thinking", "signature"])?;
                        string(&block["thinking"])?;
                        string(&block["signature"])?;
                        BlockKind::Thinking
                    }
                    Some("redacted_thinking") if self.messages => {
                        fields(block, &["type", "data"])?;
                        string(&block["data"])?;
                        BlockKind::Redacted
                    }
                    _ => return Err(InferenceError::InvalidUpstream),
                };
                self.active = Some((index, kind));
                self.blocks += 1;
            }
            Some("content_block_delta") if self.started && !self.finished => {
                let (index, kind) = self.active.ok_or(InferenceError::InvalidUpstream)?;
                if value["index"].as_u64() != Some(index) {
                    return Err(InferenceError::InvalidUpstream);
                }
                let delta = &value["delta"];
                if kind == BlockKind::Tool {
                    fields(delta, &["type", "partial_json"])?;
                    if delta["type"] != "input_json_delta" {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    let partial = string(&delta["partial_json"])?;
                    if partial.len()
                        > self
                            .limit
                            .unwrap_or(BODY_LIMIT)
                            .saturating_sub(self.arguments.len())
                    {
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
                } else if kind == BlockKind::Thinking {
                    match delta["type"].as_str() {
                        Some("thinking_delta") => {
                            fields(delta, &["type", "thinking"])?;
                            string(&delta["thinking"])?;
                        }
                        Some("signature_delta") => {
                            fields(delta, &["type", "signature"])?;
                            string(&delta["signature"])?;
                        }
                        _ => return Err(InferenceError::InvalidUpstream),
                    }
                } else if kind == BlockKind::Text {
                    fields(delta, &["type", "text"])?;
                    if delta["type"] != "text_delta" {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    events.push(ChatEvent::Delta {
                        text: Some(string(&delta["text"])?.into()),
                        tool_calls: vec![],
                    });
                } else {
                    return Err(InferenceError::InvalidUpstream);
                }
            }
            Some("content_block_stop") if self.active.is_some() => {
                if value["index"].as_u64() != self.active.map(|v| v.0) {
                    return Err(InferenceError::InvalidUpstream);
                }
                if self.active.is_some_and(|v| v.1 == BlockKind::Tool) {
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
        // Preserve only the reviewed content-block lifecycle, after structural
        // validation. Native start precedes canonical deltas so the codec knows
        // to use the native view without duplicating text or tools.
        if self.messages
            && matches!(
                value["type"].as_str(),
                Some("content_block_start" | "content_block_delta" | "content_block_stop")
            )
        {
            let allowed = match value["type"].as_str() {
                Some("content_block_start") => &["type", "index", "content_block"][..],
                Some("content_block_delta") => &["type", "index", "delta"][..],
                _ => &["type", "index"][..],
            };
            fields(&value, allowed)?;
            self.native_bytes = self
                .native_bytes
                .checked_add(value.to_string().len())
                .ok_or(InferenceError::InvalidUpstream)?;
            if self.native_bytes > self.limit.unwrap_or(BODY_LIMIT) {
                return Err(InferenceError::InvalidUpstream);
            }
            events.insert(0, ChatEvent::MessagesBlock(value));
        }
        Ok(events)
    }
}
/// Native Message Batches (`jobs::native`).
mod batches;
#[cfg(test)]
mod fidelity_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod terminal_tests;
#[cfg(test)]
mod tests;
