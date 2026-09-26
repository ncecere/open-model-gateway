use std::pin::Pin;

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::InferenceError;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ApiProtocol {
    ChatCompletions,
    Responses,
    Messages,
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
