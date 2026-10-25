use std::fmt;

/// Policy layer that denied admission. Carries no identifiers or amounts.
/// There is no installation scope: installation-wide limits were removed
/// (migration 0026); every limit belongs to a workspace or a key lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitScope {
    Workspace,
    ApiKey,
}
impl LimitScope {
    /// The scope of a policy layer of the admitting workspace: the key
    /// lineage layer when it carries a key, otherwise a workspace layer.
    pub fn of<T>(key: Option<T>) -> Self {
        if key.is_some() {
            Self::ApiKey
        } else {
            Self::Workspace
        }
    }
}

/// A bounded request-field identifier, not a value or a serde diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldName {
    bytes: [u8; 64],
    len: u8,
}
impl FieldName {
    pub fn parse(name: &str) -> Option<Self> {
        if name.is_empty()
            || name.len() > 64
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return None;
        }
        let mut bytes = [0; 64];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Some(Self {
            bytes,
            len: name.len() as u8,
        })
    }
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or_default()
    }
}

/// Safe errors only. Never embed upstream bodies, URLs, credentials or prompts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InferenceError {
    InvalidRequest,
    /// Static, reviewed field names only; never client values or upstream text.
    InvalidField(&'static str),
    UnknownField(FieldName),
    ContextLength,
    Overloaded,
    /// The key is no longer valid (revoked, disabled, expired, its user,
    /// membership, service account or workspace no longer active), found by a
    /// live re-check after authentication (for example a per-replica key
    /// cache hit just before invalidation arrived). Rendered exactly like the
    /// authentication layer's refusal of an unknown key (HTTP 401
    /// `authentication_error`), never as `model_not_found`.
    Unauthenticated,
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
    /// The pinned pricing-v3 price cannot bound this attempt's cost (a meter
    /// it may use has no line, is explicitly unknown, or has no `max_units`),
    /// so no budget can be enforced. A configuration problem, never a budget
    /// denial; refused before dispatch whenever a budget applies.
    PriceUnbounded,
    /// The "jobs at once" limit (concurrent active video/batch jobs) at this
    /// scope is reached. Retryable once a job finishes or is cancelled.
    JobLimitExceeded(LimitScope),
    /// Every route that could serve the model is in its circuit-breaker
    /// cooldown after consecutive provider failures. Transient: retry after
    /// the carried number of seconds (at least 1). The model exists; this is
    /// never reported as `model_not_found`.
    RouteCoolingDown(u32),
    Timeout,
    UpstreamRejected,
    UpstreamUnavailable,
    InvalidUpstream,
    Storage,
}

impl InferenceError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest | Self::InvalidField(_) | Self::UnknownField(_) => {
                "invalid_request_error"
            }
            Self::ContextLength => "context_length_exceeded",
            Self::Overloaded => "overloaded_error",
            Self::Unauthenticated => "authentication_error",
            Self::ModelUnavailable => "model_not_found",
            Self::Unsupported => "unsupported_capability",
            Self::Configuration => "provider_configuration_error",
            Self::PriceUnbounded => "price_unbounded",
            Self::Busy => "rate_limit_error",
            Self::BudgetExceeded(_) => "budget_exceeded",
            Self::UnresolvedUsage(_) => "unresolved_usage",
            Self::TokenReservationExceedsLimit(_) => "token_reservation_exceeds_limit",
            Self::JobLimitExceeded(_) => "job_limit_exceeded",
            Self::RouteCoolingDown(_) => "model_temporarily_unavailable",
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
    /// Permanent capability/admission refusals (`x-should-retry: false`).
    pub fn is_non_retryable_denial(self) -> bool {
        self.is_budget_denial()
            || matches!(
                self,
                Self::Unsupported
                    | Self::InvalidField(_)
                    | Self::UnknownField(_)
                    | Self::ContextLength
                    | Self::TokenReservationExceedsLimit(_)
                    | Self::PriceUnbounded
            )
    }
    /// Seconds a client should wait before retrying (`Retry-After`), if known.
    pub fn retry_after_seconds(self) -> Option<u32> {
        match self {
            Self::RouteCoolingDown(seconds) => Some(seconds.max(1)),
            _ => None,
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "Invalid or unsupported chat request fields",
            Self::InvalidField(field) => field,
            Self::UnknownField(_) => "Unknown request field",
            Self::ContextLength => "prompt is too long",
            Self::Overloaded => "The provider is overloaded",
            // The authentication layer's message (`http::unauthorized`).
            Self::Unauthenticated => "Invalid or missing API key",
            Self::ModelUnavailable => "Model not found or not available to this workspace",
            Self::Unsupported => "No registered deployment supports the requested capabilities",
            Self::Configuration => "Provider configuration is unavailable",
            Self::PriceUnbounded => {
                "The model's price cannot bound this request's cost (a meter it may use is unknown or has no per-request maximum), so budgeted keys cannot use it until a Platform Admin publishes a complete price"
            }
            Self::Busy => "Gateway or provider concurrency/rate limit reached",
            Self::BudgetExceeded(LimitScope::ApiKey) => {
                "Budget for this API key would be exceeded in its current period"
            }
            Self::BudgetExceeded(LimitScope::Workspace) => {
                "Budget for this workspace would be exceeded in its current period"
            }
            Self::UnresolvedUsage(LimitScope::ApiKey) => {
                "Unresolved usage with unbounded cost blocks budgeted admission for this API key until reconciled"
            }
            Self::UnresolvedUsage(LimitScope::Workspace) => {
                "Unresolved usage with unbounded cost blocks budgeted admission for this workspace until reconciled"
            }
            Self::TokenReservationExceedsLimit(LimitScope::ApiKey) => {
                "The model's input+output token ceiling exceeds this API key's tokens-per-minute limit; lower the price ceilings or raise the limit"
            }
            Self::TokenReservationExceedsLimit(LimitScope::Workspace) => {
                "The model's input+output token ceiling exceeds this workspace's tokens-per-minute limit; lower the price ceilings or raise the limit"
            }
            Self::JobLimitExceeded(LimitScope::ApiKey) => {
                "Too many jobs are running for this API key (jobs at once limit); wait for one to finish or cancel one"
            }
            Self::JobLimitExceeded(LimitScope::Workspace) => {
                "Too many jobs are running for this workspace (jobs at once limit); wait for one to finish or cancel one"
            }
            Self::RouteCoolingDown(_) => {
                "The model is temporarily unavailable after repeated provider failures; retry after the indicated delay"
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
