use std::pin::Pin;

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::InferenceError;
pub use super::workload_types::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ApiProtocol {
    ChatCompletions,
    Responses,
    Messages,
    Embeddings,
    /// Non-generation workloads; see `inference::workload`. Images, audio
    /// (`inference::audio`), rerank and System One are served.
    Images,
    AudioTranscriptions,
    AudioSpeech,
    Rerank,
    Systemone,
    /// `GET /v1/realtime` WebSocket sessions (`inference::realtime`).
    Realtime,
    /// Async jobs (`jobs`): `POST /v1/videos` and `/v1/files` + `/v1/batches`.
    Videos,
    Batches,
}

impl ApiProtocol {
    pub const ALL: [ApiProtocol; 12] = [
        Self::ChatCompletions,
        Self::Responses,
        Self::Messages,
        Self::Embeddings,
        Self::Images,
        Self::AudioTranscriptions,
        Self::AudioSpeech,
        Self::Rerank,
        Self::Systemone,
        Self::Realtime,
        Self::Videos,
        Self::Batches,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
            Self::Embeddings => "embeddings",
            Self::Images => "images",
            Self::AudioTranscriptions => "audio_transcriptions",
            Self::AudioSpeech => "audio_speech",
            Self::Rerank => "rerank",
            Self::Systemone => "systemone",
            Self::Realtime => "realtime",
            Self::Videos => "videos",
            Self::Batches => "batches",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
    /// Execution workload; protocols of different workloads never share a model.
    pub fn workload(self) -> WorkloadKind {
        match self {
            Self::ChatCompletions | Self::Responses | Self::Messages => WorkloadKind::Generation,
            Self::Embeddings => WorkloadKind::Embeddings,
            Self::Images => WorkloadKind::Images,
            Self::AudioTranscriptions => WorkloadKind::AudioTranscriptions,
            Self::AudioSpeech => WorkloadKind::AudioSpeech,
            Self::Rerank => WorkloadKind::Rerank,
            Self::Systemone => WorkloadKind::Systemone,
            Self::Realtime => WorkloadKind::Realtime,
            Self::Videos => WorkloadKind::Videos,
            Self::Batches => WorkloadKind::Batches,
        }
    }
    /// A valid model protocol set is nonempty, distinct and within one workload.
    pub fn valid_set<S: AsRef<str>>(protocols: &[S]) -> bool {
        let Some(parsed) = protocols
            .iter()
            .map(|p| Self::parse(p.as_ref()))
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        let distinct = parsed
            .iter()
            .map(|p| p.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        !parsed.is_empty()
            && distinct == parsed.len()
            && parsed.iter().all(|p| p.workload() == parsed[0].workload())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadKind {
    Generation,
    Embeddings,
    Images,
    AudioTranscriptions,
    AudioSpeech,
    Rerank,
    Systemone,
    Realtime,
    Videos,
    Batches,
}
impl WorkloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generation => "generation",
            Self::Embeddings => "embeddings",
            Self::Images => "images",
            Self::AudioTranscriptions => "audio_transcriptions",
            Self::AudioSpeech => "audio_speech",
            Self::Rerank => "rerank",
            Self::Systemone => "systemone",
            Self::Realtime => "realtime",
            Self::Videos => "videos",
            Self::Batches => "batches",
        }
    }
}

#[derive(Clone)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
    pub dimensions: Option<u32>,
}

pub struct EmbeddingResponse {
    pub embeddings: Vec<Vec<f32>>,
    pub usage: Usage,
}

// These types describe gateway semantics, not any provider's HTTP payload.
// No Debug on prompt-bearing or credential-reference-bearing structures.
#[derive(Clone)]
pub struct ChatRequest {
    /// Validated Messages semantics, never serialized as an upstream payload.
    pub messages_options: Option<Box<MessagesOptions>>,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<FunctionTool>,
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f64>,
    pub max_output_tokens: Option<u32>,
    pub stream: bool,
}

/// Prompt-bearing options intentionally have no Debug/Serialize implementation.
#[derive(Clone, Default)]
pub struct MessagesOptions {
    /// `strict` was present on a Messages tool definition. Do not infer wire
    /// presence from shared tools that adapters/canonical callers may modify.
    pub strict_tools: bool,
    /// Repeated header values retained verbatim, never interpreted as headers.
    pub beta_headers: Vec<String>,
    /// Validated native message/tool envelopes; no caller transport or metadata.
    pub native_messages: Vec<serde_json::Value>,
    pub native_tools: Vec<serde_json::Value>,
    pub error_results: bool,
    pub system: Option<serde_json::Value>,
    pub cache_controls: Vec<serde_json::Value>,
    pub thinking: Option<serde_json::Value>,
    pub history_thinking: Vec<serde_json::Value>,
    pub images: Vec<serde_json::Value>,
    pub output_config: Option<serde_json::Value>,
    pub context_management: Option<serde_json::Value>,
    pub defer_loading: Vec<serde_json::Value>,
    pub task_budget: Option<serde_json::Value>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub stop_sequences: Option<Vec<String>>,
}
impl ChatRequest {
    pub fn response_limit(&self) -> usize {
        if self.messages_options.is_some() {
            16 * 1024 * 1024
        } else {
            4 * 1024 * 1024
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Clone)]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

#[derive(Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone)]
pub struct FunctionTool {
    pub name: String,
    pub description: Option<String>,
    pub parameters: serde_json::Value,
    pub strict: Option<bool>,
}

#[derive(Clone)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Function(String),
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    /// Native refusal, distinct from generic filtering.
    Refusal,
    /// The model context window, not the requested output maximum, was reached.
    ContextWindowExceeded,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Usage {
    // None means unknown, never a fabricated zero or estimate.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub billing: Option<crate::billing::BillingUsage>,
    /// Non-token meters; `None` means no meter observation at all.
    pub meters: Option<crate::billing::MeterUsage>,
    pub output_image_variant: Option<crate::billing::MeterVariant>,
    /// Provider-reported charge, evidence only; never the gateway charge.
    pub provider_cost_microusd: Option<i64>,
    /// Provider-reported reasoning tokens (a subset of output tokens).
    /// Telemetry only: never charged separately; `None` = not reported.
    pub reasoning_tokens: Option<u64>,
    /// Model id the provider reports having served (telemetry only; `None` =
    /// not reported or not a valid bounded id). Never fails a request.
    pub reported_model: Option<ReportedModel>,
}

/// A provider-reported upstream model id: 1..=[`ReportedModel::MAX`] bytes of
/// printable, non-space ASCII. Stored inline so [`Usage`] stays `Copy`.
/// Anything else (absent, empty, too long, whitespace/control/non-ASCII,
/// non-string) is unknown, never an error and never truncated.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ReportedModel {
    len: u16,
    bytes: [u8; ReportedModel::MAX],
}
impl ReportedModel {
    pub const MAX: usize = 256;
    pub fn parse(value: &str) -> Option<Self> {
        let raw = value.as_bytes();
        if raw.is_empty() || raw.len() > Self::MAX || !raw.iter().all(u8::is_ascii_graphic) {
            return None;
        }
        let mut bytes = [0; Self::MAX];
        bytes[..raw.len()].copy_from_slice(raw);
        Some(Self {
            len: raw.len() as u16,
            bytes,
        })
    }
    /// A JSON string value, else unknown.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        value.as_str().and_then(Self::parse)
    }
    pub fn as_str(&self) -> &str {
        // Only validated ASCII is ever stored.
        std::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or_default()
    }
}
impl std::fmt::Debug for ReportedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ReportedModel")
            .field(&self.as_str())
            .finish()
    }
}

pub struct ChatResponse {
    /// Validated native Anthropic blocks, preserving order and opaque signatures.
    /// Used only by the Messages codec, never logged as telemetry.
    pub messages_content: Option<Vec<serde_json::Value>>,
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Usage,
}

#[derive(Clone)]
pub struct ToolCallDelta {
    pub index: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
}

// `Usage` is a copyable, fixed-size metering record; events are short-lived.
#[allow(clippy::large_enum_variant)]
pub enum ChatEvent {
    /// Validated native Anthropic content-block lifecycle event. Canonical
    /// text/tool deltas accompany it for accounting/telemetry and other codecs.
    MessagesBlock(serde_json::Value),
    Delta {
        text: Option<String>,
        tool_calls: Vec<ToolCallDelta>,
    },
    Finish(FinishReason),
    Usage(Usage),
    /// Exactly one Done after Finish. EOF without Done is failure, not success.
    Done,
}

pub type EventStream = Pin<Box<dyn Stream<Item = Result<ChatEvent, InferenceError>> + Send>>;

#[allow(clippy::large_enum_variant)]
pub enum ProviderOutput {
    Complete(ChatResponse),
    Stream(EventStream),
}

/// Longest text a unique index on `models.public_name` can hold (PostgreSQL's
/// B-tree entry limit): a longer name can never have been stored.
const MAX_STORED_MODEL_NAME_BYTES: usize = 2704;

/// Whether a request's `model` is worth looking up: not empty, no control
/// characters (PostgreSQL rejects NUL in text, which would be a 5xx) and not
/// longer than a stored name can be. Everything else is looked up by exact
/// equality, so names inserted by SQL with other characters keep resolving.
pub fn model_name_is_queryable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_STORED_MODEL_NAME_BYTES
        && !name.chars().any(char::is_control)
}

#[derive(Clone, sqlx::FromRow)]
pub struct Deployment {
    pub id: Uuid,
    pub provider: String,
    pub upstream_model: String,
    pub credential_ref: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    #[sqlx(default)]
    pub supported_protocols: Vec<String>,
}

#[derive(Clone, Copy)]
pub struct Capabilities {
    pub text_chat: bool,
    pub streaming: bool,
    pub tools: bool,
}

impl Capabilities {
    pub fn supports(self, request: &ChatRequest) -> bool {
        self.text_chat
            && (!request.stream || self.streaming)
            && ((request.tools.is_empty()
                && request.tool_choice.is_none()
                && request
                    .messages
                    .iter()
                    .all(|m| m.tool_calls.is_empty() && m.role != Role::Tool))
                || self.tools)
    }
}
