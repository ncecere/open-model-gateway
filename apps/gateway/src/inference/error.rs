use std::fmt;

/// Safe errors only. Never embed upstream bodies, URLs, credentials or prompts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InferenceError {
    InvalidRequest,
    ModelUnavailable,
    Unsupported,
    Configuration,
    Busy,
    Timeout,
    UpstreamRejected,
    UpstreamUnavailable,
    InvalidUpstream,
    Storage,
}

impl InferenceError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request_error",
            Self::ModelUnavailable => "model_not_found",
            Self::Unsupported => "unsupported_capability",
            Self::Configuration => "provider_configuration_error",
            Self::Busy => "rate_limit_error",
            Self::Timeout => "timeout_error",
            Self::UpstreamRejected => "upstream_rejected",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::InvalidUpstream => "invalid_upstream_response",
            Self::Storage => "accounting_unavailable",
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "Invalid or unsupported chat request fields",
            Self::ModelUnavailable => "Model not found or not available to this workspace",
            Self::Unsupported => "No registered deployment supports the requested capabilities",
            Self::Configuration => "Provider configuration is unavailable",
            Self::Busy => "Gateway or provider concurrency/rate limit reached",
            Self::Timeout => "Inference deadline exceeded",
            Self::UpstreamRejected => "The provider rejected the request",
            Self::UpstreamUnavailable => "The provider is unavailable",
            Self::InvalidUpstream => "The provider returned an invalid or incomplete response",
            Self::Storage => "Inference accounting is unavailable",
        }
    }
}

impl fmt::Display for InferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}
impl std::error::Error for InferenceError {}
