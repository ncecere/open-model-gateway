use std::fmt;

/// Policy layer that denied admission. Carries no identifiers or amounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitScope {
    Installation,
    Workspace,
    ApiKey,
}

/// Safe errors only. Never embed upstream bodies, URLs, credentials or prompts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InferenceError {
    InvalidRequest,
    ModelUnavailable,
    Unsupported,
    Configuration,
    Busy,
    /// Admission would exceed a budget (in its current day/week/month window) at this scope.
    BudgetExceeded(LimitScope),
    /// Unresolved unbounded-cost usage blocks budgeted admission at this scope.
    UnresolvedUsage(LimitScope),
    /// The pinned price's per-attempt token reservation (input ceiling plus
    /// output reservation) alone exceeds a tokens-per-minute limit at this
    /// scope, so the request can never be admitted until configuration changes.
    TokenReservationExceedsLimit(LimitScope),
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
            Self::BudgetExceeded(_) => "budget_exceeded",
            Self::UnresolvedUsage(_) => "unresolved_usage",
            Self::TokenReservationExceedsLimit(_) => "token_reservation_exceeds_limit",
            Self::Timeout => "timeout_error",
            Self::UpstreamRejected => "upstream_rejected",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::InvalidUpstream => "invalid_upstream_response",
            Self::Storage => "accounting_unavailable",
        }
    }
    /// Budget/accounting denials are not transient; SDKs must not auto-retry them.
    pub fn is_budget_denial(self) -> bool {
        matches!(self, Self::BudgetExceeded(_) | Self::UnresolvedUsage(_))
    }
    /// Admission denials that retrying cannot fix (`x-should-retry: false`).
    pub fn is_non_retryable_denial(self) -> bool {
        self.is_budget_denial() || matches!(self, Self::TokenReservationExceedsLimit(_))
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "Invalid or unsupported chat request fields",
            Self::ModelUnavailable => "Model not found or not available to this workspace",
            Self::Unsupported => "No registered deployment supports the requested capabilities",
            Self::Configuration => "Provider configuration is unavailable",
            Self::Busy => "Gateway or provider concurrency/rate limit reached",
            Self::BudgetExceeded(LimitScope::ApiKey) => {
                "Budget for this API key would be exceeded in its current period"
            }
            Self::BudgetExceeded(LimitScope::Workspace) => {
                "Budget for this workspace would be exceeded in its current period"
            }
            Self::BudgetExceeded(LimitScope::Installation) => {
                "Installation-wide budget cannot admit this request in its current period"
            }
            Self::UnresolvedUsage(LimitScope::ApiKey) => {
                "Unresolved usage with unbounded cost blocks budgeted admission for this API key until reconciled"
            }
            Self::UnresolvedUsage(_) => {
                "Unresolved usage with unbounded cost blocks budgeted admission for this workspace until reconciled"
            }
            Self::TokenReservationExceedsLimit(LimitScope::ApiKey) => {
                "The model's input+output token ceiling exceeds this API key's tokens-per-minute limit; lower the price ceilings or raise the limit"
            }
            Self::TokenReservationExceedsLimit(LimitScope::Workspace) => {
                "The model's input+output token ceiling exceeds this workspace's tokens-per-minute limit; lower the price ceilings or raise the limit"
            }
            Self::TokenReservationExceedsLimit(LimitScope::Installation) => {
                "The model's input+output token ceiling exceeds the installation-wide tokens-per-minute limit; lower the price ceilings or raise the limit"
            }
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
