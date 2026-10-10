//! Realtime sessions (`GET /v1/realtime`): one long-lived upstream WebSocket
//! per session, admitted and accounted as **one upstream attempt** with one
//! durable reservation (see `.local/enterprise-rebuild/realtime-contract.md`
//! and `docs/realtime.md`).
//!
//! Budget model: every response holds one bounded *response window*. Its
//! input bound is the session's actual context ([`context`]: the previous
//! response's usage plus the client input since, capped at the context
//! window); its output bound is the response's `max_output_tokens`, at most
//! [`RealtimeSession::window_output_tokens`]. Admission reserves a minimal
//! window; every `response.create` resizes the reserved window (or adds one)
//! under a fresh budget check before it is forwarded; every `response.done`
//! settles that response with the pinned price (unknown usage keeps its
//! window). Dropping a
//! [`RealtimeSession`] drops the upstream socket synchronously and records the
//! session as cancelled (holds retained).
//!
//! The protocol frontend (`protocols::realtime`) owns client-event validation
//! and the proxy loop; adapters own the upstream wire
//! ([`crate::providers::ProviderAdapter::connect_realtime`]).
use std::{pin::Pin, time::Duration};

use futures_util::{Sink, Stream};
use serde_json::Value;

pub mod context;
pub use context::ResponseBound;
use context::{ContextTracker, EventSize, SessionConfig};

use super::*;
use crate::{
    billing::MeterUsage,
    inference::{
        repository::FinishLabel,
        types::{Deployment, WorkloadKind},
        workload::{OutputReservation, WorkloadAdmission},
    },
};

const MIB: usize = 1024 * 1024;
/// OpenAI's per-response `max_output_tokens` upper bound.
pub const MAX_RESPONSE_OUTPUT_TOKENS: u32 = 4096;
/// Upstream (server → client) message cap; `conversation.item.retrieved`
/// can carry a whole audio item.
pub const MAX_UPSTREAM_MESSAGE_BYTES: usize = 16 * MIB;

/// Server-side realtime limits (environment, never request fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealtimeLimits {
    /// `GATEWAY_REALTIME_MAX_SESSION_SECONDS` (default 900, 10..=3600).
    pub max_session: Duration,
    /// `GATEWAY_REALTIME_IDLE_SECONDS` (default 120, 5..=3600): no frame in
    /// either direction for this long ends the session.
    pub idle: Duration,
    /// `GATEWAY_REALTIME_MAX_OUTPUT_TOKENS` (default 4096, 1..=4096): the
    /// per-response output ceiling (further capped by the price).
    pub max_output_tokens: u32,
    /// `GATEWAY_REALTIME_MAX_MESSAGE_BYTES` (default 1 MiB, 1 KiB..=16 MiB)
    /// per client message.
    pub max_message_bytes: usize,
    /// `GATEWAY_REALTIME_MAX_EVENTS_PER_SECOND` (default 50, 1..=1000), with a
    /// burst of twice the rate.
    pub max_events_per_second: u32,
}
impl Default for RealtimeLimits {
    fn default() -> Self {
        Self {
            max_session: Duration::from_secs(900),
            idle: Duration::from_secs(120),
            max_output_tokens: MAX_RESPONSE_OUTPUT_TOKENS,
            max_message_bytes: MIB,
            max_events_per_second: 50,
        }
    }
}
impl RealtimeLimits {
    pub fn validate(self) -> bool {
        (10..=3600).contains(&self.max_session.as_secs())
            && (5..=3600).contains(&self.idle.as_secs())
            && (1..=MAX_RESPONSE_OUTPUT_TOKENS).contains(&self.max_output_tokens)
            && (1024..=MAX_UPSTREAM_MESSAGE_BYTES).contains(&self.max_message_bytes)
            && (1..=1000).contains(&self.max_events_per_second)
    }
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mut limits = Self::default();
        let parse = |name: &str| -> anyhow::Result<Option<u64>> {
            get(name)
                .map(|v| {
                    v.trim()
                        .parse::<u64>()
                        .map_err(|_| anyhow::anyhow!("{name} must be an integer"))
                })
                .transpose()
        };
        if let Some(n) = parse("GATEWAY_REALTIME_MAX_SESSION_SECONDS")? {
            limits.max_session = Duration::from_secs(n);
        }
        if let Some(n) = parse("GATEWAY_REALTIME_IDLE_SECONDS")? {
            limits.idle = Duration::from_secs(n);
        }
        if let Some(n) = parse("GATEWAY_REALTIME_MAX_OUTPUT_TOKENS")? {
            limits.max_output_tokens = u32::try_from(n).unwrap_or(u32::MAX);
        }
        if let Some(n) = parse("GATEWAY_REALTIME_MAX_MESSAGE_BYTES")? {
            limits.max_message_bytes = usize::try_from(n).unwrap_or(usize::MAX);
        }
        if let Some(n) = parse("GATEWAY_REALTIME_MAX_EVENTS_PER_SECOND")? {
            limits.max_events_per_second = u32::try_from(n).unwrap_or(u32::MAX);
        }
        anyhow::ensure!(limits.validate(), "realtime limits out of range");
        Ok(limits)
    }
    /// Reservation lease: the whole session plus a settlement margin.
    fn lease_seconds(self) -> i64 {
        self.max_session.as_secs() as i64 + 60
    }
}

/// Usage of one realtime response, normalized per modality. All counts are
/// observed; a response whose usage is missing or inconsistent has no
/// `RealtimeUsage` at all (unknown, never zero). Totals include the cached
/// subset (`cached_* <= input_*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RealtimeUsage {
    pub input_text_tokens: u64,
    pub cached_text_tokens: u64,
    pub input_audio_tokens: u64,
    pub cached_audio_tokens: u64,
    pub output_text_tokens: u64,
    pub output_audio_tokens: u64,
}
impl RealtimeUsage {
    pub fn valid(&self) -> bool {
        self.cached_text_tokens <= self.input_text_tokens
            && self.cached_audio_tokens <= self.input_audio_tokens
            && [
                self.input_text_tokens,
                self.input_audio_tokens,
                self.output_text_tokens,
                self.output_audio_tokens,
            ]
            .iter()
            .all(|n| *n <= i64::MAX as u64 / 4)
    }
    pub fn input_tokens(&self) -> u64 {
        self.input_text_tokens + self.input_audio_tokens
    }
    pub fn output_tokens(&self) -> u64 {
        self.output_text_tokens + self.output_audio_tokens
    }
}

/// Final status of a realtime response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseStatus {
    Completed,
    Cancelled,
    Incomplete,
    Failed,
}
impl ResponseStatus {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "incomplete" => Self::Incomplete,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
        }
    }
}

/// One server event as classified by the adapter. Text frames are forwarded
/// verbatim unless noted; no event content is logged or stored.
pub enum UpstreamEvent {
    Forward(String),
    /// `session.created`/`session.updated`. `safe` is false when the
    /// effective configuration would let upstream start billable work the
    /// gateway cannot reserve for (automatic responses, idle-timeout
    /// responses, input transcription). `session.created` precedes the
    /// adapter's enforced configuration, so only updates must be safe. The
    /// frontend rewrites the model id.
    Session {
        event: Value,
        safe: bool,
    },
    ResponseCreated {
        text: String,
        response_id: String,
    },
    ResponseDone {
        text: String,
        response_id: String,
        status: Option<ResponseStatus>,
        usage: Option<RealtimeUsage>,
    },
    /// Upstream `error` event: bounded tokens only (upstream messages may
    /// carry private identifiers and are never forwarded).
    Error {
        kind: Option<String>,
        code: Option<String>,
        param: Option<String>,
        event_id: Option<String>,
    },
    /// Documented as not forwarded (e.g. upstream account rate limits).
    Filtered,
    /// Upstream reports billable work the session cannot account for (for
    /// example input transcription usage): the session fails closed.
    Unaccounted,
}

pub type UpstreamSink = Pin<Box<dyn Sink<String, Error = InferenceError> + Send>>;
pub type UpstreamEvents = Pin<Box<dyn Stream<Item = Result<UpstreamEvent, InferenceError>> + Send>>;
/// A connected, configured upstream session. Dropping it closes the socket.
pub struct RealtimeUpstream {
    pub sink: UpstreamSink,
    pub events: UpstreamEvents,
    /// The acknowledged effective configuration (instructions/tools size,
    /// input audio format) that sizes the first response's hold.
    pub config: SessionConfig,
}

/// Session setup the adapter must enforce before returning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealtimeSetup {
    pub window_output_tokens: u32,
    /// Connect + configuration deadline.
    pub connect_timeout: Duration,
}

/// Terminal record of a realtime session.
pub struct RealtimeFinish {
    pub id: Uuid,
    pub outcome: Outcome,
    pub error: Option<InferenceError>,
    pub elapsed_ms: u64,
    pub telemetry: AttemptTelemetry,
    /// A `response.create` was forwarded but neither created nor rejected:
    /// upstream may have started it, so its window is retained as unknown.
    pub unopened_request: bool,
    /// The reserved window of that request.
    pub window: ResponseBound,
}

/// An admitted, connected realtime session (one upstream attempt).
pub struct RealtimeSession {
    repository: Arc<dyn InferenceRepository>,
    principal: Principal,
    id: Uuid,
    deployment_id: Uuid,
    model: String,
    window_output_tokens: u32,
    limits: RealtimeLimits,
    started: Instant,
    dispatched: Instant,
    upstream: Option<RealtimeUpstream>,
    /// One reserved window not yet used by a response, with its bound.
    armed: Option<ResponseBound>,
    /// A `response.create` was forwarded and is neither created nor rejected.
    requested: bool,
    sequence: i32,
    open: Option<(i32, String, ResponseBound)>,
    context: ContextTracker,
    finished: bool,
    first_output_ms: Option<u64>,
    _permit: SharedPermit,
}
impl RealtimeSession {
    pub fn id(&self) -> Uuid {
        self.id
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn limits(&self) -> RealtimeLimits {
        self.limits
    }
    pub fn window_output_tokens(&self) -> u32 {
        self.window_output_tokens
    }
    pub fn take_upstream(&mut self) -> Option<RealtimeUpstream> {
        self.upstream.take()
    }
    /// A response is open (created upstream, not yet done).
    pub fn response_open(&self) -> bool {
        self.open.is_some()
    }
    pub fn first_output(&mut self) {
        if self.first_output_ms.is_none() {
            self.first_output_ms = Some(millis(self.dispatched.elapsed()));
        }
    }
    /// A validated client event about to be forwarded (it may add input).
    pub fn client_event(&mut self, size: EventSize) {
        self.context.client_event(size);
    }
    /// An effective session configuration reported by upstream.
    pub fn session_config(&mut self, config: SessionConfig) {
        self.context.session_config(config);
    }
    /// Before forwarding `response.create` with `max_output_tokens`: reserve
    /// one window sized from the session's context. A reserved but unused
    /// window (admission's, or a rejected request's) is resized; otherwise a
    /// new window is added. Growth is budget-checked; shrinking releases.
    pub async fn reserve_window(&mut self, max_output_tokens: u32) -> Result<(), InferenceError> {
        if !(1..=self.window_output_tokens).contains(&max_output_tokens) {
            return Err(InferenceError::InvalidRequest);
        }
        let bound = self.context.request(max_output_tokens);
        if self.armed != Some(bound) {
            timeout(
                Duration::from_secs(5),
                self.repository.realtime_reserve_window(
                    &self.principal,
                    self.id,
                    &self.model,
                    bound,
                    self.armed,
                ),
            )
            .await
            .map_err(|_| InferenceError::Storage)??;
        }
        self.armed = Some(bound);
        self.requested = true;
        Ok(())
    }
    /// Upstream rejected the forwarded `response.create` (an error event for
    /// its event id): no response exists, the window stays reserved for the
    /// next request.
    pub fn request_rejected(&mut self) {
        self.requested = false;
        self.context.rejected();
    }
    /// `response.created`: the reserved window now belongs to this response.
    /// Without a reserved window (an unsolicited response) the session must
    /// fail closed.
    pub async fn open_response(&mut self, response_id: String) -> Result<(), InferenceError> {
        let (Some(bound), None) = (self.armed, &self.open) else {
            return Err(InferenceError::InvalidUpstream);
        };
        let sequence = self.sequence.checked_add(1).ok_or(InferenceError::Busy)?;
        timeout(
            Duration::from_secs(3),
            self.repository
                .realtime_open_response(self.id, sequence, bound),
        )
        .await
        .map_err(|_| InferenceError::Storage)??;
        self.armed = None;
        self.requested = false;
        self.sequence = sequence;
        self.open = Some((sequence, response_id, bound));
        self.context.opened();
        Ok(())
    }
    /// `response.done`: settle the open response with its (possibly unknown) usage.
    pub async fn settle_response(
        &mut self,
        response_id: &str,
        status: Option<ResponseStatus>,
        usage: Option<RealtimeUsage>,
    ) -> Result<(), InferenceError> {
        let Some((sequence, open_id, bound)) = &self.open else {
            return Err(InferenceError::InvalidUpstream);
        };
        if open_id != response_id {
            return Err(InferenceError::InvalidUpstream);
        }
        let (sequence, bound) = (*sequence, *bound);
        let usage = usage.filter(RealtimeUsage::valid);
        timeout(
            Duration::from_secs(3),
            self.repository
                .realtime_settle_response(self.id, sequence, status, usage, bound),
        )
        .await
        .map_err(|_| InferenceError::Storage)??;
        self.open = None;
        self.context.done(usage);
        Ok(())
    }
    fn record(
        &self,
        outcome: Outcome,
        error: Option<InferenceError>,
        finish: Option<FinishLabel>,
    ) -> RealtimeFinish {
        RealtimeFinish {
            id: self.id,
            outcome,
            error,
            elapsed_ms: millis(self.started.elapsed()),
            telemetry: AttemptTelemetry {
                finish_reason: finish,
                time_to_first_token_ms: self.first_output_ms,
                generation_ms: Some(millis(self.dispatched.elapsed())),
            },
            unopened_request: self.requested,
            window: self
                .armed
                .unwrap_or(ResponseBound::unknown(self.window_output_tokens)),
        }
    }
    /// Durably finish the session. An open response or unopened request keeps
    /// its window as unknown usage.
    pub async fn finish(
        mut self,
        outcome: Outcome,
        error: Option<InferenceError>,
        finish: Option<FinishLabel>,
    ) -> Result<(), InferenceError> {
        // Upstream work is dropped before accounting, never after.
        drop(self.upstream.take());
        self.finished = true;
        let record = self.record(outcome, error, finish);
        let result = timeout(
            Duration::from_secs(3),
            self.repository.realtime_finish(&record),
        )
        .await
        .map_err(|_| InferenceError::Storage)
        .and_then(|r| r);
        if result.is_err() {
            tracing::error!(execution_id = %self.id, "realtime finalization failed; reconciliation required");
        }
        // Health is advisory: only provider-side failures count against it.
        let provider_error = error.filter(|e| {
            matches!(
                e,
                InferenceError::Timeout
                    | InferenceError::UpstreamUnavailable
                    | InferenceError::InvalidUpstream
            )
        });
        if outcome != Outcome::Cancelled
            && !matches!(
                timeout(
                    Duration::from_millis(250),
                    self.repository
                        .route_result(self.deployment_id, provider_error)
                )
                .await,
                Ok(Ok(()))
            )
        {
            tracing::warn!(execution_id = %self.id, "routing health update failed");
        }
        result
    }
}
impl Drop for RealtimeSession {
    fn drop(&mut self) {
        // Network work is dropped synchronously; only accounting is detached.
        drop(self.upstream.take());
        if self.finished {
            return;
        }
        let repository = self.repository.clone();
        let record = self.record(Outcome::Cancelled, None, None);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if !matches!(timeout(Duration::from_secs(3), repository.realtime_finish(&record)).await, Ok(Ok(()))) {
                    tracing::error!(execution_id = %record.id, "cancelled realtime finalization failed; reconciliation required");
                }
            });
        }
    }
}

impl Engine {
    pub fn realtime_limits(&self) -> RealtimeLimits {
        self.realtime
    }
    /// Replace the realtime limits (composition root / tests).
    pub fn with_realtime_limits(mut self, limits: RealtimeLimits) -> anyhow::Result<Self> {
        anyhow::ensure!(limits.validate(), "invalid realtime limits");
        self.realtime = limits;
        Ok(self)
    }
    /// Admit and connect one realtime session. Failover follows the route
    /// plan only for failures before the session is returned (admission or
    /// upstream connect/configuration); every attempt is admitted separately.
    pub async fn open_realtime(
        &self,
        principal: Principal,
        model: &str,
        request_id: Uuid,
    ) -> Result<RealtimeSession, InferenceError> {
        if model.trim().is_empty() || model.len() > 200 {
            return Err(InferenceError::InvalidRequest);
        }
        let permit = Arc::new(Mutex::new(Some(
            self.capacity
                .clone()
                .try_acquire_owned()
                .map_err(|_| crate::metrics::capacity_denied())?,
        )));
        let limits = self.realtime;
        let deadline = Instant::now() + self.limits.request_timeout;
        let labels = super::client::current();
        let deployments = timeout_at(deadline, self.repository.deployments(&principal, model))
            .await
            .map_err(|_| InferenceError::Timeout)??;
        if deployments.is_empty() {
            return Err(InferenceError::ModelUnavailable);
        }
        let protocol = ApiProtocol::Realtime;
        let candidates: Vec<Deployment> = deployments
            .into_iter()
            .filter(|d| {
                d.supported_protocols.iter().any(|p| p == protocol.as_str())
                    && self
                        .registry
                        .get(&d.provider)
                        .is_some_and(|a| a.supports_protocol(protocol))
            })
            .collect();
        if candidates.is_empty() {
            return Err(InferenceError::Unsupported);
        }
        let plan = timeout_at(
            deadline,
            self.repository
                .route_plan(&principal, model, &candidates, request_id),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if !(1..=3).contains(&plan.max_attempts) {
            return Err(InferenceError::Configuration);
        }
        for (attempt, id) in plan
            .deployment_ids
            .iter()
            .take(plan.max_attempts)
            .enumerate()
        {
            let target = candidates
                .iter()
                .find(|d| d.id == *id)
                .ok_or(InferenceError::Configuration)?;
            let adapter = self
                .registry
                .get(&target.provider)
                .ok_or(InferenceError::Configuration)?;
            let execution_id = if attempt == 0 {
                request_id
            } else {
                Uuid::now_v7()
            };
            let started = Instant::now();
            let window = timeout_at(
                deadline,
                self.repository
                    .realtime_window_output(target.id, limits.max_output_tokens),
            )
            .await
            .map_err(|_| InferenceError::Timeout)??;
            if !(1..=MAX_RESPONSE_OUTPUT_TOKENS).contains(&window) {
                return Err(InferenceError::Configuration);
            }
            timeout_at(
                deadline,
                self.repository.admit_workload(
                    &ExecutionStart {
                        id: execution_id,
                        root_request_id: request_id,
                        attempt_number: (attempt + 1) as i32,
                        principal,
                        deployment_id: target.id,
                        provider: target.provider.clone(),
                        model: model.to_owned(),
                        streamed: true,
                        upstream_model: super::upstream_snapshot(&target.upstream_model),
                        client: labels.clone(),
                    },
                    &WorkloadAdmission {
                        kind: WorkloadKind::Realtime,
                        output: OutputReservation::Requested(Some(window)),
                        unit_ceilings: MeterUsage {
                            requests: Some(1),
                            ..MeterUsage::default()
                        },
                    },
                    limits.lease_seconds(),
                    target,
                ),
            )
            .await
            .map_err(|_| InferenceError::Timeout)??;
            // From here the attempt is durable: every exit records it.
            let mut session = RealtimeSession {
                repository: self.repository.clone(),
                principal,
                id: execution_id,
                deployment_id: target.id,
                model: model.to_owned(),
                window_output_tokens: window,
                limits,
                started,
                dispatched: Instant::now(),
                upstream: None,
                armed: Some(ResponseBound::admission(window)),
                requested: false,
                sequence: 0,
                open: None,
                context: ContextTracker::new(SessionConfig::default()),
                finished: false,
                first_output_ms: None,
                _permit: permit.clone(),
            };
            let setup = RealtimeSetup {
                window_output_tokens: window,
                connect_timeout: Duration::from_secs(10)
                    .min(deadline.saturating_duration_since(Instant::now())),
            };
            let connected = timeout(
                setup.connect_timeout,
                adapter.connect_realtime(target, &setup),
            )
            .await
            .unwrap_or(Err(InferenceError::Timeout));
            match connected {
                Ok(upstream) => {
                    session.context = ContextTracker::new(upstream.config);
                    session.upstream = Some(upstream);
                    session.dispatched = Instant::now();
                    return Ok(session);
                }
                Err(error) => {
                    // No client event was ever forwarded, so no response ran:
                    // the session settles with zero responses.
                    let _ = session.finish(Outcome::Failed, Some(error), None).await;
                    if attempt + 1 < plan.max_attempts
                        && attempt + 1 < plan.deployment_ids.len()
                        && crate::routing::may_failover(&plan, error)
                    {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Err(InferenceError::Busy)
    }
}

#[cfg(test)]
mod tests;
