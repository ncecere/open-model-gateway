//! OpenAI's wire protocol lives here; no upstream identifiers or error bodies escape.
use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, header};
use serde_json::{Map, Value, json};

use super::{ProviderAdapter, secrets::SecretResolver};
use crate::inference::{error::InferenceError, types::*};

const BASE: &str = "https://api.openai.com/v1";
const BODY_LIMIT: usize = 4 * 1024 * 1024;
const SSE_LIMIT: usize = 1024 * 1024;
const MAX_TOOL_CALLS: usize = 128;

type Result<T> = std::result::Result<T, InferenceError>;

pub struct OpenAiAdapter {
    pub(super) client: Client,
    pub(super) resolver: Arc<dyn SecretResolver>,
    // Only the private test constructor can replace the production origin.
    pub(super) base: String,
}

impl OpenAiAdapter {
    pub fn new(resolver: Arc<dyn SecretResolver>) -> anyhow::Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| anyhow::anyhow!("Unable to initialize provider transport"))?;
        Ok(Self {
            client,
            resolver,
            base: BASE.into(),
        })
    }

    #[cfg(test)]
    pub(super) fn for_test(resolver: Arc<dyn SecretResolver>, base: String) -> Self {
        let url = reqwest::Url::parse(&base).unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        let mut adapter = Self::new(resolver).unwrap();
        adapter.base = base;
        adapter
    }
}

#[async_trait]
impl ProviderAdapter for OpenAiAdapter {
    fn id(&self) -> &'static str {
        "openai"
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
            ApiProtocol::ChatCompletions
                | ApiProtocol::Responses
                | ApiProtocol::Embeddings
                | ApiProtocol::Images
                | ApiProtocol::AudioTranscriptions
                | ApiProtocol::AudioSpeech
        )
    }

    fn supports_transcription_request(&self, _: &Deployment, _: &TranscriptionRequest) -> bool {
        true
    }

    fn supports_speech_request(&self, _: &Deployment, _: &SpeechRequest) -> bool {
        true
    }

    async fn execute_audio_transcription(
        &self,
        target: &Deployment,
        request: TranscriptionRequest,
    ) -> Result<TranscriptionResponse> {
        audio::transcribe(self, target, request).await
    }

    async fn execute_audio_speech(
        &self,
        target: &Deployment,
        request: SpeechRequest,
    ) -> Result<SpeechResponse> {
        audio::speak(self, target, request).await
    }

    fn supports_image_request(&self, target: &Deployment, request: &ImageRequest) -> bool {
        images::supports(target, request)
    }

    async fn execute_images(
        &self,
        target: &Deployment,
        request: ImageRequest,
    ) -> Result<ImageResponse> {
        images::execute(self, target, request).await
    }

    async fn execute_protocol(
        &self,
        target: &Deployment,
        request: ChatRequest,
        protocol: ApiProtocol,
    ) -> Result<ProviderOutput> {
        match protocol {
            ApiProtocol::ChatCompletions => self.execute(target, request).await,
            ApiProtocol::Responses => super::openai_responses::execute(self, target, request).await,
            _ => Err(InferenceError::Unsupported),
        }
    }

    async fn execute_embeddings(
        &self,
        target: &Deployment,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse> {
        super::embeddings::validate(&request)?;
        if target.provider != self.id()
            || target.credential_ref == "none"
            || target
                .endpoint
                .as_deref()
                .is_some_and(|v| v != BASE && v != "https://api.openai.com/v1/")
            || target.region.as_deref().is_some_and(|v| !v.is_empty())
        {
            return Err(InferenceError::Configuration);
        }
        let secret = self.resolver.resolve(&target.credential_ref)?;
        let mut auth = header::HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
            .map_err(|_| InferenceError::Configuration)?;
        auth.set_sensitive(true);
        let response = self
            .client
            .post(format!("{}/embeddings", self.base))
            .header(header::AUTHORIZATION, auth)
            .json(&super::embeddings::encode(&target.upstream_model, &request))
            .send()
            .await
            .map_err(transport_error)?;
        check_status(response.status())?;
        super::embeddings::decode(&super::framing::body(response).await?, &request)
    }

    async fn execute(&self, target: &Deployment, request: ChatRequest) -> Result<ProviderOutput> {
        // Validate before resolving credentials, even when using the test transport.
        if target.provider != self.id()
            || target.credential_ref == "none"
            || target
                .endpoint
                .as_deref()
                .is_some_and(|v| v != BASE && v != "https://api.openai.com/v1/")
            || target.region.as_deref().is_some_and(|v| !v.is_empty())
        {
            return Err(InferenceError::Configuration);
        }
        let secret = self.resolver.resolve(&target.credential_ref)?;
        let mut authorization =
            header::HeaderValue::from_str(&format!("Bearer {}", secret.expose()))
                .map_err(|_| InferenceError::Configuration)?;
        authorization.set_sensitive(true);
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base))
            .header(header::AUTHORIZATION, authorization)
            .json(&encode(&target.upstream_model, &request))
            .send()
            .await
            .map_err(transport_error)?;
        check_status(response.status())?;
        if request.stream {
            let is_sse = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(';')
                        .next()
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
                });
            if !is_sse {
                return Err(InferenceError::InvalidUpstream);
            }
            Ok(ProviderOutput::Stream(decode_stream(response)))
        } else {
            if response
                .content_length()
                .is_some_and(|n| n > BODY_LIMIT as u64)
            {
                return Err(InferenceError::InvalidUpstream);
            }
            let mut body = Vec::new();
            let mut chunks = response.bytes_stream();
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(transport_error)?;
                if chunk.len() > BODY_LIMIT - body.len() {
                    return Err(InferenceError::InvalidUpstream);
                }
                body.extend_from_slice(&chunk);
            }
            let value =
                serde_json::from_slice(&body).map_err(|_| InferenceError::InvalidUpstream)?;
            Ok(ProviderOutput::Complete(decode_complete(&value)?))
        }
    }
}

fn transport_error(error: reqwest::Error) -> InferenceError {
    if error.is_timeout() {
        InferenceError::Timeout
    } else {
        InferenceError::UpstreamUnavailable
    }
}

fn check_status(status: StatusCode) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    Err(match status.as_u16() {
        401 | 403 => InferenceError::Configuration,
        429 => InferenceError::Busy,
        408 => InferenceError::Timeout,
        400 | 422 => InferenceError::UpstreamRejected,
        500..=599 => InferenceError::UpstreamUnavailable,
        300..=399 => InferenceError::Configuration,
        400..=499 => InferenceError::UpstreamRejected,
        _ => InferenceError::InvalidUpstream,
    })
}

pub(super) fn encode(model: &str, request: &ChatRequest) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            let mut value = json!({"role": message.role, "content": message.content});
            if let Some(id) = &message.tool_call_id {
                value["tool_call_id"] = json!(id);
            }
            if !message.tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    message
                        .tool_calls
                        .iter()
                        .map(|tool| {
                            json!({
                                "id": tool.id, "type": "function",
                                "function": { "name": tool.name, "arguments": tool.arguments }
                            })
                        })
                        .collect(),
                );
            }
            value
        })
        .collect();
    let mut value = json!({"model": model, "messages": messages, "stream": request.stream});
    if request.stream {
        value["stream_options"] = json!({"include_usage": true});
    }
    if let Some(temperature) = request.temperature {
        value["temperature"] = json!(temperature);
    }
    if let Some(tokens) = request.max_output_tokens {
        value["max_completion_tokens"] = json!(tokens);
    }
    if !request.tools.is_empty() {
        value["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    let mut function = json!({"name": tool.name, "parameters": tool.parameters});
                    if let Some(description) = &tool.description {
                        function["description"] = json!(description);
                    }
                    if let Some(strict) = tool.strict {
                        function["strict"] = json!(strict);
                    }
                    json!({"type": "function", "function": function})
                })
                .collect(),
        );
    }
    if let Some(choice) = &request.tool_choice {
        value["tool_choice"] = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Function(name) => json!({"type": "function", "function": {"name": name}}),
        };
    }
    value
}

fn object(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().ok_or(InferenceError::InvalidUpstream)
}

fn array(value: &Value) -> Result<&Vec<Value>> {
    value.as_array().ok_or(InferenceError::InvalidUpstream)
}

fn string(value: &Value) -> Result<&str> {
    value.as_str().ok_or(InferenceError::InvalidUpstream)
}

fn nonempty_string(value: &Value) -> Result<String> {
    let value = string(value)?;
    if value.is_empty() {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(value.to_owned())
}

fn optional_string(value: &Value) -> Result<Option<String>> {
    if value.is_null() {
        Ok(None)
    } else {
        Ok(Some(string(value)?.to_owned()))
    }
}

fn empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

// Content-bearing objects must not silently lose newly introduced modalities.
fn known_fields(value: &Value, allowed: &[&str]) -> Result<()> {
    if object(value)?
        .iter()
        .any(|(key, value)| !allowed.contains(&key.as_str()) && !empty(value))
    {
        return Err(InferenceError::InvalidUpstream);
    }
    Ok(())
}

fn finish(value: &Value) -> Result<FinishReason> {
    match string(value)? {
        "stop" => Ok(FinishReason::Stop),
        "length" => Ok(FinishReason::Length),
        "tool_calls" => Ok(FinishReason::ToolCalls),
        "content_filter" => Ok(FinishReason::ContentFilter),
        _ => Err(InferenceError::InvalidUpstream),
    }
}

fn usage(value: &Value) -> Result<Usage> {
    super::metering::inclusive(
        value,
        "prompt_tokens",
        "completion_tokens",
        "prompt_tokens_details",
        "cache_write_tokens",
    )
}
pub(super) fn local_usage(value: &Value, profile: &str) -> Result<Usage> {
    super::metering::inclusive(
        value,
        "prompt_tokens",
        "completion_tokens",
        "prompt_tokens_details",
        if profile == "vllm" {
            "created_cache_tokens"
        } else {
            "cache_write_tokens"
        },
    )
}

pub(super) fn decode_complete(value: &Value) -> Result<ChatResponse> {
    decode_complete_profile(value, None)
}
/// Structural failure still fails, but a valid usage object is kept as evidence.
pub(super) fn decode_complete_profile(
    value: &Value,
    profile: Option<&str>,
) -> Result<ChatResponse> {
    crate::inference::evidence::preserve(decode_shape(value, profile), || {
        value["usage"].is_object().then(|| match profile {
            Some(profile) => local_usage(&value["usage"], profile),
            None => usage(&value["usage"]),
        })
    })
}
fn decode_shape(value: &Value, profile: Option<&str>) -> Result<ChatResponse> {
    object(value)?;
    if !empty(&value["error"]) {
        return Err(InferenceError::InvalidUpstream);
    }
    let choices = array(&value["choices"])?;
    if choices.len() != 1 {
        return Err(InferenceError::InvalidUpstream);
    }
    let choice = &choices[0];
    if choice["index"].as_u64() != Some(0) {
        return Err(InferenceError::InvalidUpstream);
    }
    known_fields(choice, &["index", "message", "finish_reason", "logprobs"])?;
    let message = &choice["message"];
    known_fields(message, &["role", "content", "tool_calls"])?;
    if message["role"].as_str() != Some("assistant") {
        return Err(InferenceError::InvalidUpstream);
    }
    let content = optional_string(&message["content"])?;
    let mut tool_calls = Vec::new();
    if !message["tool_calls"].is_null() {
        let calls = array(&message["tool_calls"])?;
        if calls.len() > MAX_TOOL_CALLS {
            return Err(InferenceError::InvalidUpstream);
        }
        for tool in calls {
            known_fields(tool, &["id", "type", "function"])?;
            if tool["type"].as_str() != Some("function") {
                return Err(InferenceError::InvalidUpstream);
            }
            let function = &tool["function"];
            known_fields(function, &["name", "arguments"])?;
            let id = nonempty_string(&tool["id"])?;
            if tool_calls.iter().any(|t: &ToolCall| t.id == id) {
                return Err(InferenceError::InvalidUpstream);
            }
            tool_calls.push(ToolCall {
                id,
                name: nonempty_string(&function["name"])?,
                arguments: string(&function["arguments"])?.into(),
            });
        }
    }
    Ok(ChatResponse {
        content,
        tool_calls,
        finish_reason: finish(&choice["finish_reason"])?,
        usage: match profile {
            Some(profile) => local_usage(&value["usage"], profile)?,
            None => usage(&value["usage"])?,
        },
    })
}

#[derive(Default)]
pub(super) struct StreamState {
    profile: Option<&'static str>,
    finished: bool,
    used: bool,
}

impl StreamState {
    pub(super) fn decode(&mut self, data: &[u8]) -> Result<Vec<ChatEvent>> {
        if data == b"[DONE]" {
            if !self.finished {
                return Err(InferenceError::InvalidUpstream);
            }
            return Ok(vec![ChatEvent::Done]);
        }
        let value: Value =
            serde_json::from_slice(data).map_err(|_| InferenceError::InvalidUpstream)?;
        object(&value)?;
        if !empty(&value["error"]) {
            return Err(InferenceError::InvalidUpstream);
        }
        let choices = array(&value["choices"])?;
        if choices.len() > 1 {
            return Err(InferenceError::InvalidUpstream);
        }
        let mut events = Vec::new();
        if let Some(choice) = choices.first() {
            if self.finished || choice["index"].as_u64() != Some(0) {
                return Err(InferenceError::InvalidUpstream);
            }
            known_fields(choice, &["index", "delta", "finish_reason", "logprobs"])?;
            let delta = &choice["delta"];
            known_fields(delta, &["role", "content", "tool_calls"])?;
            if !delta["role"].is_null() && delta["role"].as_str() != Some("assistant") {
                return Err(InferenceError::InvalidUpstream);
            }
            let text = optional_string(&delta["content"])?;
            let mut tool_calls = Vec::new();
            if !delta["tool_calls"].is_null() {
                for tool in array(&delta["tool_calls"])? {
                    known_fields(tool, &["index", "id", "type", "function"])?;
                    if !tool["type"].is_null() && tool["type"].as_str() != Some("function") {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    let index = tool["index"]
                        .as_u64()
                        .and_then(|v| u32::try_from(v).ok())
                        .ok_or(InferenceError::InvalidUpstream)?;
                    if index as usize >= MAX_TOOL_CALLS
                        || tool_calls.iter().any(|t: &ToolCallDelta| t.index == index)
                    {
                        return Err(InferenceError::InvalidUpstream);
                    }
                    let function = &tool["function"];
                    if !function.is_null() {
                        known_fields(function, &["name", "arguments"])?;
                    }
                    tool_calls.push(ToolCallDelta {
                        index,
                        id: optional_string(&tool["id"])?,
                        name: optional_string(&function["name"])?,
                        arguments: optional_string(&function["arguments"])?,
                    });
                }
            }
            if text.is_some() || !tool_calls.is_empty() {
                events.push(ChatEvent::Delta { text, tool_calls });
            }
            if !choice["finish_reason"].is_null() {
                events.push(ChatEvent::Finish(finish(&choice["finish_reason"])?));
                self.finished = true;
            }
        } else if value["usage"].is_null() {
            return Err(InferenceError::InvalidUpstream);
        }
        if !value["usage"].is_null() {
            if self.used || !self.finished {
                return Err(InferenceError::InvalidUpstream);
            }
            events.push(ChatEvent::Usage(match self.profile {
                Some(profile) => local_usage(&value["usage"], profile)?,
                None => usage(&value["usage"])?,
            }));
            self.used = true;
        }
        Ok(events)
    }
}

/// Incremental byte framing: UTF-8 is decoded only after a complete line, so a
/// codepoint, CRLF pair, or event may span any number of network chunks. The
/// entire frame (including comments and ignored fields) has a hard byte bound.
#[derive(Default)]
pub(super) struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    event_bytes: usize,
    skip_lf: bool,
    first_line: bool,
}

impl SseDecoder {
    pub(super) fn new() -> Self {
        Self {
            first_line: true,
            ..Self::default()
        }
    }

    pub(super) fn push(&mut self, byte: u8) -> Result<Option<Vec<u8>>> {
        if self.skip_lf {
            self.skip_lf = false;
            if byte == b'\n' {
                return Ok(None);
            }
        }
        self.event_bytes += 1;
        if self.event_bytes > SSE_LIMIT {
            return Err(InferenceError::InvalidUpstream);
        }
        if byte != b'\n' && byte != b'\r' {
            if self.line.len() == SSE_LIMIT {
                return Err(InferenceError::InvalidUpstream);
            }
            self.line.push(byte);
            return Ok(None);
        }
        self.skip_lf = byte == b'\r';
        let line = std::str::from_utf8(&self.line).map_err(|_| InferenceError::InvalidUpstream)?;
        let line = if self.first_line {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        } else {
            line
        };
        self.first_line = false;
        if line.is_empty() {
            self.line.clear();
            self.event_bytes = 0;
            if self.data.is_empty() {
                return Ok(None);
            }
            self.data.pop(); // SSE joins consecutive data fields with a newline.
            return Ok(Some(std::mem::take(&mut self.data)));
        }
        if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            if field == "data" {
                if self.data.len() + value.len() + 1 > SSE_LIMIT {
                    return Err(InferenceError::InvalidUpstream);
                }
                self.data.extend_from_slice(value.as_bytes());
                self.data.push(b'\n');
            }
        }
        self.line.clear();
        Ok(None)
    }
}

fn decode_stream(response: reqwest::Response) -> EventStream {
    decode_stream_profile(response, None)
}
pub(super) fn decode_stream_profile(
    response: reqwest::Response,
    profile: Option<&'static str>,
) -> EventStream {
    Box::pin(async_stream::try_stream! {
        let mut chunks = response.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut state = StreamState { profile, ..StreamState::default() };
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(transport_error)?;
            for &byte in chunk.iter() {
                if let Some(data) = decoder.push(byte)? {
                    for event in state.decode(&data)? {
                        let done = matches!(event, ChatEvent::Done);
                        yield event;
                        if done { return; }
                    }
                }
            }
        }
        // A valid finish chunk is insufficient; only [DONE] proves completion.
        Err(InferenceError::InvalidUpstream)?;
    })
}

mod audio;
mod images;
#[cfg(test)]
mod tests;
