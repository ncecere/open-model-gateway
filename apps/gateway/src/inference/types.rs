use std::pin::Pin;

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::InferenceError;
pub use super::workload_types::*;

#[derive(Clone, Copy, PartialEq, Eq)]
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
}

impl ApiProtocol {
    pub const ALL: [ApiProtocol; 9] = [
        Self::ChatCompletions,
        Self::Responses,
        Self::Messages,
        Self::Embeddings,
        Self::Images,
        Self::AudioTranscriptions,
        Self::AudioSpeech,
        Self::Rerank,
        Self::Systemone,
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
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<FunctionTool>,
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f64>,
    pub max_output_tokens: Option<u32>,
    pub stream: bool,
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
}

pub struct ChatResponse {
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
