//! Canonical async-job contracts shared by the job service and provider
//! adapters. They describe gateway semantics, not a provider's wire format,
//! and carry no `Debug` on prompt-bearing values.
use std::pin::Pin;

use axum::body::Bytes;
use futures_util::Stream;

use crate::{
    billing::MeterVariant,
    inference::{
        error::InferenceError,
        types::{
            ApiProtocol, ChatRequest, ChatResponse, EmbeddingRequest, EmbeddingResponse, Usage,
        },
    },
};

/// Video prompt cap (bytes), independent of the HTTP body cap.
pub const VIDEO_MAX_PROMPT_BYTES: usize = 32 * 1024;
/// Batch input bounds (OpenAI Batch API limits).
pub const BATCH_MAX_REQUESTS: u32 = 50_000;
pub const BATCH_MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
pub const BATCH_COMPLETION_WINDOW: &str = "24h";

/// Endpoints a batch's lines may target (every line of a batch uses the
/// batch's endpoint). Each is the gateway's normal inference contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BatchEndpoint {
    ChatCompletions,
    Responses,
    Embeddings,
    /// Anthropic Messages shape.
    Messages,
}
impl BatchEndpoint {
    pub const ALL: [BatchEndpoint; 4] = [
        Self::ChatCompletions,
        Self::Responses,
        Self::Embeddings,
        Self::Messages,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
            Self::Embeddings => "/v1/embeddings",
            Self::Messages => "/v1/messages",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == s)
    }
    pub fn protocol(self) -> ApiProtocol {
        match self {
            Self::ChatCompletions => ApiProtocol::ChatCompletions,
            Self::Responses => ApiProtocol::Responses,
            Self::Embeddings => ApiProtocol::Embeddings,
            Self::Messages => ApiProtocol::Messages,
        }
    }
    /// Generation (chat-like) endpoints share the canonical chat request.
    pub fn is_generation(self) -> bool {
        self != Self::Embeddings
    }
}

/// A validated batch line in canonical form. No `Debug`: it is content.
#[derive(Clone)]
pub enum BatchRequest {
    Chat(ChatRequest),
    Embeddings(EmbeddingRequest),
}
impl BatchRequest {
    pub fn model(&self) -> &str {
        match self {
            Self::Chat(r) => &r.model,
            Self::Embeddings(r) => &r.model,
        }
    }
    /// The line's output maximum (0 for embeddings).
    pub fn max_output(&self) -> u32 {
        match self {
            Self::Chat(r) => r.max_output_tokens.unwrap_or(0),
            Self::Embeddings(_) => 0,
        }
    }
}
/// A successful line result in canonical form. No `Debug`: it is content.
pub enum BatchResponse {
    Chat(ChatResponse),
    Embeddings(EmbeddingResponse),
}
impl BatchResponse {
    pub fn usage(&self) -> Usage {
        match self {
            Self::Chat(r) => r.usage,
            Self::Embeddings(r) => r.usage,
        }
    }
}
/// One decoded native batch result line. `custom_id` is the gateway's
/// upstream id of the line (`l<n>`), never the client's.
pub struct NativeResult {
    pub custom_id: String,
    pub outcome: NativeOutcome,
}
pub enum NativeOutcome {
    Succeeded(Box<BatchResponse>),
    /// The provider answered the line with an error (HTTP-like status and a
    /// sanitized code; never the provider's message).
    Failed {
        status: u16,
        code: ErrorCode,
    },
    Cancelled,
    Expired,
}

/// A provider job/file id: 1..=128 of `[A-Za-z0-9._:-]`. Never returned to
/// clients (gateway ids replace it).
#[derive(Clone, PartialEq, Eq)]
pub struct UpstreamId(String);
impl UpstreamId {
    pub fn parse(s: &str) -> Option<Self> {
        (!s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')))
        .then(|| Self(s.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for UpstreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UpstreamId(..)")
    }
}

/// Gateway job state (DB-enforced forward-only machine, migration 0016).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    InProgress,
    Completed,
    Failed,
    Cancelled,
    Expired,
}
impl JobState {
    pub const ALL: [JobState; 6] = [
        Self::Queued,
        Self::InProgress,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
        Self::Expired,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::InProgress)
    }
    /// Forward-only transitions; an observed regression is ignored.
    pub fn may_become(self, next: Self) -> bool {
        match self {
            Self::Queued => next != Self::Queued,
            Self::InProgress => next.is_terminal(),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Video,
    Batch,
}
impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Batch => "batch",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "video" => Some(Self::Video),
            "batch" => Some(Self::Batch),
            _ => None,
        }
    }
    /// Client id prefix (`video_…`, `batch_…`).
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Video => "video_",
            Self::Batch => "batch_",
        }
    }
}
/// Client-facing id: kind prefix + 32 lowercase hex digits of the gateway UUID.
pub fn client_id(prefix: &str, id: uuid::Uuid) -> String {
    format!("{prefix}{}", id.simple())
}
/// Parse a client-facing id with the expected prefix; anything else is unknown.
pub fn parse_client_id(prefix: &str, s: &str) -> Option<uuid::Uuid> {
    let hex = s.strip_prefix(prefix)?;
    (hex.len() == 32
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then(|| uuid::Uuid::parse_str(hex).ok())
    .flatten()
}
pub const FILE_PREFIX: &str = "file-";

// ---------------------------------------------------------------- Video ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoSize {
    P720x1280,
    P1280x720,
    P1024x1792,
    P1792x1024,
}
impl VideoSize {
    pub const DEFAULT: Self = Self::P720x1280;
    pub fn as_str(self) -> &'static str {
        match self {
            Self::P720x1280 => "720x1280",
            Self::P1280x720 => "1280x720",
            Self::P1024x1792 => "1024x1792",
            Self::P1792x1024 => "1792x1024",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::P720x1280,
            Self::P1280x720,
            Self::P1024x1792,
            Self::P1792x1024,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }
    pub fn variant(self) -> MeterVariant {
        MeterVariant::new(self.as_str()).expect("static variant")
    }
}
/// Allowed clip lengths in seconds (OpenAI: 4, 8, 12; default 4).
pub const VIDEO_SECONDS: [u32; 3] = [4, 8, 12];
pub const VIDEO_DEFAULT_SECONDS: u32 = 4;

/// `POST /v1/videos` subset. No `Debug`: the prompt is content.
#[derive(Clone)]
pub struct VideoRequest {
    pub model: String,
    pub prompt: String,
    /// Always explicit upstream (default applied by the frontend) so the
    /// billed duration is known.
    pub seconds: u32,
    pub size: VideoSize,
}
impl VideoRequest {
    pub fn validate(&self) -> Result<(), InferenceError> {
        if self.model.trim().is_empty()
            || self.model.len() > 200
            || self.prompt.trim().is_empty()
            || self.prompt.len() > VIDEO_MAX_PROMPT_BYTES
            || !VIDEO_SECONDS.contains(&self.seconds)
        {
            return Err(InferenceError::InvalidRequest);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoAsset {
    Video,
    Thumbnail,
    Spritesheet,
}
impl VideoAsset {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "video" => Some(Self::Video),
            "thumbnail" => Some(Self::Thumbnail),
            "spritesheet" => Some(Self::Spritesheet),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Thumbnail => "thumbnail",
            Self::Spritesheet => "spritesheet",
        }
    }
}

/// A sanitized provider error code (`[a-z_]{1,64}`), never a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorCode(String);
impl ErrorCode {
    pub fn parse(s: &str) -> Self {
        let valid = !s.is_empty()
            && s.len() <= 64
            && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
        Self(if valid { s } else { "upstream_failed" }.to_owned())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Provider observation of a video job. Unknown fields are `None`, never guessed.
#[derive(Clone, Debug)]
pub struct UpstreamVideo {
    pub id: UpstreamId,
    pub state: JobState,
    pub progress: Option<u8>,
    /// Generated duration in whole seconds (`seconds`), when reported.
    pub seconds: Option<u32>,
    pub size: Option<MeterVariant>,
    pub completed_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub error: Option<ErrorCode>,
}

// ---------------------------------------------------------------- Batch ----

/// Provider batch status vocabulary (OpenAI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchStatus {
    Validating,
    Failed,
    InProgress,
    Finalizing,
    Completed,
    Expired,
    Cancelling,
    Cancelled,
}
impl BatchStatus {
    pub const ALL: [BatchStatus; 8] = [
        Self::Validating,
        Self::Failed,
        Self::InProgress,
        Self::Finalizing,
        Self::Completed,
        Self::Expired,
        Self::Cancelling,
        Self::Cancelled,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validating => "validating",
            Self::Failed => "failed",
            Self::InProgress => "in_progress",
            Self::Finalizing => "finalizing",
            Self::Completed => "completed",
            Self::Expired => "expired",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
    pub fn state(self) -> JobState {
        match self {
            Self::Validating => JobState::Queued,
            Self::InProgress | Self::Finalizing | Self::Cancelling => JobState::InProgress,
            Self::Completed => JobState::Completed,
            Self::Failed => JobState::Failed,
            Self::Expired => JobState::Expired,
            Self::Cancelled => JobState::Cancelled,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestCounts {
    pub total: u32,
    pub completed: u32,
    pub failed: u32,
}

/// Provider observation of a batch. `usage` is the provider-reported
/// aggregate (unknown when absent); timestamps are Unix seconds.
#[derive(Clone, Debug)]
pub struct UpstreamBatch {
    pub id: UpstreamId,
    pub status: BatchStatus,
    /// The provider copy of the input (deleted after the results are stored).
    pub input_file: Option<UpstreamId>,
    pub output_file: Option<UpstreamId>,
    pub error_file: Option<UpstreamId>,
    pub counts: Option<RequestCounts>,
    pub usage: Option<Usage>,
    pub created_at: Option<i64>,
    pub in_progress_at: Option<i64>,
    pub finalizing_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub failed_at: Option<i64>,
    pub expired_at: Option<i64>,
    pub cancelling_at: Option<i64>,
    pub cancelled_at: Option<i64>,
    pub expires_at: Option<i64>,
    /// Client metadata echoed by the provider (passed through, never stored).
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Validated `metadata` (≤ 16 string pairs, key ≤ 64, value ≤ 512 bytes).
pub fn valid_metadata(v: &serde_json::Map<String, serde_json::Value>) -> bool {
    v.len() <= 16
        && v.iter().all(|(k, v)| {
            !k.is_empty() && k.len() <= 64 && v.as_str().is_some_and(|s| s.len() <= 512)
        })
}

/// Sum of two usage observations; any unknown counter stays unknown.
pub fn add_usage(a: Usage, b: Usage) -> Usage {
    let sum = |x: Option<u64>, y: Option<u64>| x.zip(y).and_then(|(x, y)| x.checked_add(y));
    let billing = a
        .billing
        .zip(b.billing)
        .map(|(x, y)| crate::billing::BillingUsage {
            total_input_tokens: sum(x.total_input_tokens, y.total_input_tokens),
            uncached_input_tokens: sum(x.uncached_input_tokens, y.uncached_input_tokens),
            cache_read_input_tokens: sum(x.cache_read_input_tokens, y.cache_read_input_tokens),
            cache_write_input_tokens: sum(x.cache_write_input_tokens, y.cache_write_input_tokens),
            cache_write_default_input_tokens: sum(
                x.cache_write_default_input_tokens,
                y.cache_write_default_input_tokens,
            ),
            cache_write_5m_input_tokens: sum(
                x.cache_write_5m_input_tokens,
                y.cache_write_5m_input_tokens,
            ),
            cache_write_1h_input_tokens: sum(
                x.cache_write_1h_input_tokens,
                y.cache_write_1h_input_tokens,
            ),
        });
    // A side without billing metadata makes the aggregate's billing unknown.
    let billing = match (a.billing.is_some(), b.billing.is_some()) {
        (false, false) => None,
        (true, true) => billing,
        _ => None,
    };
    Usage {
        input_tokens: sum(a.input_tokens, b.input_tokens),
        output_tokens: sum(a.output_tokens, b.output_tokens),
        billing,
        reasoning_tokens: sum(a.reasoning_tokens, b.reasoning_tokens),
        ..Usage::default()
    }
}

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, InferenceError>> + Send>>;

/// A provider body passed through unbuffered (video assets, batch files).
pub struct ContentStream {
    /// Allowlisted media type.
    pub content_type: &'static str,
    pub content_length: Option<u64>,
    pub body: ByteStream,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_machine_is_forward_only() {
        use JobState::*;
        assert!(Queued.may_become(InProgress));
        assert!(Queued.may_become(Completed));
        assert!(InProgress.may_become(Expired));
        assert!(!InProgress.may_become(Queued));
        assert!(!InProgress.may_become(InProgress));
        for t in [Completed, Failed, Cancelled, Expired] {
            assert!(t.is_terminal());
            for n in JobState::ALL {
                assert!(!t.may_become(n));
            }
        }
        assert_eq!(BatchStatus::Finalizing.state(), InProgress);
        assert_eq!(BatchStatus::Cancelling.state(), InProgress);
        assert_eq!(BatchStatus::Validating.state(), Queued);
    }
    #[test]
    fn ids_are_strict() {
        let id = uuid::Uuid::new_v4();
        let c = client_id("video_", id);
        assert_eq!(parse_client_id("video_", &c), Some(id));
        assert_eq!(parse_client_id("batch_", &c), None);
        assert_eq!(parse_client_id("video_", &c.to_uppercase()), None);
        assert_eq!(parse_client_id("video_", "video_123"), None);
        assert!(UpstreamId::parse("video_68d7512d07848190b3e45da0ecbebcde").is_some());
        assert!(UpstreamId::parse("../x").is_none());
        assert!(UpstreamId::parse("").is_none());
        assert_eq!(ErrorCode::parse("Some Message").as_str(), "upstream_failed");
        assert_eq!(
            ErrorCode::parse("moderation_blocked").as_str(),
            "moderation_blocked"
        );
        assert_eq!(
            format!("{:?}", UpstreamId::parse("abc").unwrap()),
            "UpstreamId(..)"
        );
    }
}
