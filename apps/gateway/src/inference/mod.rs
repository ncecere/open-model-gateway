mod attempt;
pub mod audio;
pub mod client;
mod deadline_stream;
mod embeddings;
pub mod error;
pub(crate) mod evidence;
pub mod images;
pub mod realtime;
pub mod repository;
mod request_telemetry;
pub mod scheduling;
pub mod types;
pub mod workload;
pub mod workload_types;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
type SharedPermit = Arc<Mutex<Option<OwnedSemaphorePermit>>>;

use futures_util::StreamExt;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Instant, timeout, timeout_at},
};
use uuid::Uuid;

use crate::{auth::Principal, providers::ProviderRegistry, telemetry};
use attempt::{AttemptEnd, RequestScope, record_routing_span};
use error::InferenceError;
use repository::{AttemptTelemetry, ExecutionFinish, ExecutionStart, InferenceRepository, Outcome};
use request_telemetry::*;
use types::{ApiProtocol, ChatEvent, ChatRequest, Deployment, ProviderOutput, Usage};

/// Opaque proof that a trace id came from this module's own
/// shared-admission-proven `log_trace_id` provenance (#98, #294): computed
/// only right after a shared-workspace admission has already committed for
/// this request (see the call site below), never for a personal workspace
/// or while telemetry is disabled. No crate-wide raw constructor: the
/// private `from_provenance` is reachable only from this module and its
/// submodules, never from `ops_log`, `leases`, `jobs` or any other module.
/// `as_str` is read-only and does not let a caller mint a new value from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogTraceId(String);
impl LogTraceId {
    fn from_provenance(hex: String) -> Self {
        Self(hex)
    }
    /// Test fixtures only: compiled out of every production build, so this
    /// is never a production forgery path.
    #[cfg(test)]
    pub(crate) fn for_test(hex: &str) -> Self {
        Self(hex.to_owned())
    }
    /// Read-only: the existing hexadecimal value, for stderr diagnostics and
    /// the wire `traceId` field. Never lets a caller mint a new value.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy)]
pub struct EngineLimits {
    pub max_concurrent: usize,
    pub request_timeout: Duration,
    pub workloads: workload::WorkloadLimits,
    pub audio: audio::AudioLimits,
}
impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            max_concurrent: 128,
            request_timeout: Duration::from_secs(120),
            workloads: workload::WorkloadLimits::default(),
            audio: audio::AudioLimits::default(),
        }
    }
}

#[derive(Clone)]
pub struct Engine {
    repository: Arc<dyn InferenceRepository>,
    registry: ProviderRegistry,
    capacity: Arc<Semaphore>,
    limits: EngineLimits,
    /// Realtime session limits (`Engine::with_realtime_limits`).
    realtime: realtime::RealtimeLimits,
    /// Bounded trace recorder (`Engine::with_telemetry`); None disables
    /// tracing, in which case the request path never allocates a
    /// RequestTelemetry context or hashes a request id.
    recorder: Option<telemetry::Recorder>,
}

impl Engine {
    pub fn new(
        repository: Arc<dyn InferenceRepository>,
        registry: ProviderRegistry,
        limits: EngineLimits,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            limits.max_concurrent > 0
                && limits.max_concurrent <= Semaphore::MAX_PERMITS
                && !limits.request_timeout.is_zero()
                && limits.workloads.validate()
                && limits.audio.validate(),
            "invalid inference limits"
        );
        Ok(Self {
            repository,
            registry,
            capacity: Arc::new(Semaphore::new(limits.max_concurrent)),
            limits,
            realtime: realtime::RealtimeLimits::default(),
            recorder: None,
        })
    }

    /// Attaches the bounded trace recorder (#98). None disables tracing for
    /// every request through this engine.
    pub fn with_telemetry(mut self, recorder: Option<telemetry::Recorder>) -> Self {
        self.recorder = recorder;
        self
    }

    /// The same engine (adapters, limits, deadlines) with another repository
    /// and its own capacity, for background work such as gateway-run batch
    /// lines (`jobs::runner`), whose admission differs from interactive
    /// requests but whose execution path must not.
    pub fn with_repository(
        &self,
        repository: Arc<dyn InferenceRepository>,
        max_concurrent: usize,
    ) -> Self {
        Self {
            repository,
            registry: self.registry.clone(),
            capacity: Arc::new(Semaphore::new(
                max_concurrent.clamp(1, Semaphore::MAX_PERMITS),
            )),
            limits: self.limits,
            realtime: self.realtime,
            // Background work (gateway-run batch lines) is out of scope for
            // interactive-generation tracing: never wrapped in telemetry,
            // same as every other non-interactive admission path.
            recorder: None,
        }
    }

    pub async fn execute(
        &self,
        principal: Principal,
        request: ChatRequest,
        request_id: Uuid,
    ) -> Result<ProviderOutput, InferenceError> {
        self.execute_protocol(principal, request, request_id, ApiProtocol::ChatCompletions)
            .await
    }

    pub async fn execute_protocol(
        &self,
        principal: Principal,
        request: ChatRequest,
        request_id: Uuid,
        protocol: ApiProtocol,
    ) -> Result<ProviderOutput, InferenceError> {
        let permit = Arc::new(Mutex::new(Some(
            self.capacity
                .clone()
                .try_acquire_owned()
                .map_err(|_| crate::metrics::capacity_denied())?,
        )));
        let started = Instant::now();
        let deadline = started + self.limits.request_timeout;
        let labels = client::current();
        // Reserved before anything else: routing/admission/attempt/
        // settlement spans all name this as `parent`. Export never happens
        // without a later shared-workspace admission commit (#98).
        let mut telemetry = RequestTelemetry::new(
            self.recorder.as_ref(),
            request_id,
            operation_of(protocol),
            request.stream,
        );
        let routing_start = telemetry.active().then(telemetry::now_unix_nanos);
        let deployments = timeout_at(
            deadline,
            self.repository
                .candidate_deployments(&principal, &request.model, request_id),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if deployments.is_empty() {
            return Err(InferenceError::ModelUnavailable);
        }
        let (candidates, refused) = self.eligible_candidates(deployments, &request, protocol);
        if candidates.is_empty() {
            if refused.is_empty() {
                return Err(InferenceError::Unsupported);
            }
            return self
                .refuse(&principal, &request, &refused, request_id, deadline)
                .await;
        }
        let plan = timeout_at(
            deadline,
            self.repository
                .route_plan(&principal, &request.model, &candidates, request_id),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if !(1..=3).contains(&plan.max_attempts) {
            return Err(InferenceError::Configuration);
        }
        record_routing_span(&mut telemetry, routing_start, protocol);
        let streamed = request.stream;
        let scope = RequestScope {
            principal,
            request: &request,
            request_id,
            deadline,
            labels: &labels,
            permit: &permit,
            plan: &plan,
            protocol,
            streamed,
        };
        for (attempt, id) in plan
            .deployment_ids
            .iter()
            .take(plan.max_attempts)
            .enumerate()
        {
            let deployment = match candidates.iter().find(|d| d.id == *id) {
                Some(deployment) => deployment,
                None => {
                    // An early return must still finish telemetry
                    // explicitly, exactly like every other terminal path of
                    // an attempt (`Engine::attempt`), so Drop never
                    // mislabels a real denial `cancelled` with no
                    // `error.type`.
                    telemetry.finish(
                        telemetry::Outcome::Error,
                        Some(InferenceError::Configuration),
                    );
                    return Err(InferenceError::Configuration);
                }
            };
            let adapter = match self.registry.get(&deployment.provider) {
                Some(adapter) => adapter,
                None => {
                    telemetry.finish(
                        telemetry::Outcome::Error,
                        Some(InferenceError::Configuration),
                    );
                    return Err(InferenceError::Configuration);
                }
            };
            telemetry = match self
                .attempt(&scope, telemetry, attempt, deployment, adapter)
                .await
            {
                AttemptEnd::Failover(telemetry) => telemetry,
                AttemptEnd::Return(result) => return result,
            };
        }
        telemetry.finish(telemetry::Outcome::Error, Some(InferenceError::Busy));
        Err(InferenceError::Busy)
    }
}

/// The configured upstream model id as stored (1..=512 characters), else none.
fn upstream_snapshot(model: &str) -> Option<String> {
    (1..=512)
        .contains(&model.chars().count())
        .then(|| model.to_owned())
}

fn valid_usage(usage: Usage) -> bool {
    usage.meters.is_none_or(|m| m.validate().is_ok())
        && usage.provider_cost_microusd.is_none_or(|n| n >= 0)
        && valid_token_usage(usage)
}
fn valid_token_usage(usage: Usage) -> bool {
    [usage.input_tokens, usage.output_tokens]
        .into_iter()
        .flatten()
        .all(|n| n <= i64::MAX as u64)
        && u128::from(usage.input_tokens.unwrap_or(0))
            + u128::from(usage.output_tokens.unwrap_or(0))
            <= i64::MAX as u128
        && usage.billing.is_none_or(|billing| {
            billing.input_lower_bound().is_ok_and(|n| {
                u128::from(n) + u128::from(usage.output_tokens.unwrap_or(0)) <= i64::MAX as u128
            }) && billing.validate().is_ok()
                && match (billing.total_input_tokens, usage.input_tokens) {
                    (Some(total), Some(raw)) => total >= raw,
                    _ => true,
                }
        })
}

/// Settles one attempt's durable write (wrapping its real phase boundaries
/// as a `settlement` span, via [`begin_phase_group`]/[`end_phase_group`])
/// and records its `upstream.attempt` span, regardless of whether the
/// settlement write itself succeeded. Returns settlement's own result
/// unchanged, so callers keep deciding failover/return exactly as before.
#[allow(clippy::too_many_arguments)]
async fn settle_attempt(
    telemetry: &mut RequestTelemetry,
    guard: &mut ExecutionGuard,
    outcome: Outcome,
    error: Option<InferenceError>,
    attempt_number: usize,
    attempt_wall_start: Option<u64>,
    attempt_id: Uuid,
    provider: &str,
    streamed: bool,
) -> Result<(), InferenceError> {
    let telemetry_outcome = match outcome {
        Outcome::Succeeded => telemetry::Outcome::Completed,
        Outcome::Failed => telemetry::Outcome::Error,
        Outcome::Cancelled => telemetry::Outcome::Cancelled,
    };
    let usage = guard.usage;
    let timings = guard.telemetry_now();
    // Captured here, before settlement's own (possibly slow, up to its 3s
    // timeout) write: `upstream.attempt` ends at the terminal upstream
    // outcome, never inflated by settlement's own duration. `settlement`
    // is recorded separately below with its own real
    // start/end from `begin_phase_group`/`end_phase_group`.
    let attempt_end = telemetry::now_unix_nanos();
    let group = begin_phase_group(telemetry);
    let buffer = group.as_ref().map(|(b, _, _)| b.clone());
    let result = guard.finish(outcome, error, buffer).await;
    end_phase_group(
        telemetry,
        group,
        telemetry::SpanName::Settlement,
        attempt_number,
    );
    if let Some(start) = attempt_wall_start
        && let Some(span) = attempt_span(
            telemetry,
            start,
            attempt_end,
            attempt_number,
            provider,
            streamed,
            attempt_id,
            telemetry_outcome,
            error,
            usage,
            &timings,
        )
    {
        telemetry.record(span);
    }
    result
}

/// How long a settlement may take: the one the request waits for and the
/// detached one of a cancelled request alike.
pub(crate) const SETTLEMENT_DEADLINE: Duration = Duration::from_secs(3);
struct ExecutionGuard {
    repository: Arc<dyn InferenceRepository>,
    id: Uuid,
    deployment_id: Uuid,
    started: Instant,
    /// Upstream dispatch (after admission): the origin of attempt timings.
    dispatched: Instant,
    usage: Usage,
    telemetry: AttemptTelemetry,
    finished: bool,
    health_observation: bool,
    _permit: SharedPermit,
    /// The request's correlation id (#98), for the reconciliation log lines
    /// below only: `Some` only once a shared-workspace admission has
    /// already committed for this request, `None` for a personal workspace
    /// (never logged) or while telemetry is disabled. Never affects
    /// accounting; `x-request-id` is unchanged.
    log_trace_id: Option<LogTraceId>,
    #[cfg(test)]
    diagnostics: Option<crate::db::diagnostics::Handle>,
}
fn millis(d: Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}
impl ExecutionGuard {
    /// Created right after durable admission, immediately before dispatch.
    fn new(
        repository: Arc<dyn InferenceRepository>,
        id: Uuid,
        deployment_id: Uuid,
        started: Instant,
        permit: SharedPermit,
        log_trace_id: Option<LogTraceId>,
    ) -> Self {
        Self {
            repository,
            id,
            deployment_id,
            started,
            dispatched: Instant::now(),
            usage: Usage::default(),
            telemetry: AttemptTelemetry::default(),
            finished: false,
            health_observation: true,
            _permit: permit,
            log_trace_id,
            #[cfg(test)]
            diagnostics: crate::db::diagnostics::current(),
        }
    }
    /// Streams: the first delta carrying text or a tool call sets time to first token.
    fn first_token(&mut self, text: Option<&str>, tool_calls: &[types::ToolCallDelta]) {
        if self.telemetry.time_to_first_token_ms.is_none()
            && (text.is_some_and(|t| !t.is_empty()) || !tool_calls.is_empty())
        {
            self.telemetry.time_to_first_token_ms = Some(millis(self.dispatched.elapsed()));
        }
    }
    /// Telemetry as of now: generation time ends when the attempt finishes.
    fn telemetry_now(&self) -> AttemptTelemetry {
        AttemptTelemetry {
            generation_ms: self
                .telemetry
                .generation_ms
                .or(Some(millis(self.dispatched.elapsed()))),
            ..self.telemetry
        }
    }
    fn record(&self, outcome: Outcome, error: Option<InferenceError>) -> ExecutionFinish {
        ExecutionFinish {
            id: self.id,
            outcome,
            error,
            usage: self.usage,
            elapsed_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        }
    }
    async fn finish(
        &mut self,
        outcome: Outcome,
        error: Option<InferenceError>,
        phases: Option<Arc<Mutex<telemetry::PhaseBuffer>>>,
    ) -> Result<(), InferenceError> {
        // Mark attempted finalization first: failed writes remain 'started' for reconciliation.
        self.finished = true;
        let record = self.record(outcome, error);
        let attempt_telemetry = self.telemetry_now();
        let repository = self.repository.clone();
        let settlement = timeout(SETTLEMENT_DEADLINE, async move {
            let settle = repository.finish_attempt(&record, &attempt_telemetry);
            match phases {
                Some(buffer) => telemetry::with_phases(buffer, settle).await,
                None => settle.await,
            }
        });
        let settlement = crate::db::accounting_failure::explicit_settlement(settlement);
        #[cfg(test)]
        let settlement = crate::db::diagnostics::settlement(
            self.diagnostics.clone(),
            crate::db::diagnostics::Stage::ExplicitSettlement,
            settlement,
        );
        let result = settlement
            .await
            .map_err(|_| {
                // A write that fails fast counts itself as held inside
                // `finish_attempt`; one that times out never gets that far (#136).
                crate::metrics::METRICS.observe_settlement(crate::metrics::Settlement::Held);
                InferenceError::Storage
            })
            .and_then(|result| result);
        if result.is_err() {
            tracing::error!(execution_id = %self.id, trace_id = self.log_trace_id.as_ref().map(LogTraceId::as_str), "inference finalization failed; reconciliation required");
            // Only generation finalization (this call) may carry a trace id,
            // from the same already shared-admission-proven provenance; the
            // value is already the opaque type, never re-minted here.
            crate::ops_log::emit(crate::ops_log::OpsEvent::FinalizationNeedsReconciliation {
                finalizer: crate::ops_log::Finalizer::GenerationFinish,
                trace: self.log_trace_id.clone(),
            });
        }
        // Health is advisory; a failed health write never discards a durably accounted response.
        if self.health_observation
            && outcome != Outcome::Cancelled
            && !matches!(
                timeout(
                    Duration::from_millis(250),
                    self.repository.route_result(self.deployment_id, error)
                )
                .await,
                Ok(Ok(()))
            )
        {
            tracing::warn!(execution_id=%self.id, trace_id = self.log_trace_id.as_ref().map(LogTraceId::as_str), "routing health update failed");
        }
        result
    }
}
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let repository = self.repository.clone();
        let record = self.record(Outcome::Cancelled, None);
        let telemetry = self.telemetry_now();
        let log_trace_id = self.log_trace_id.clone();
        // Network work is dropped synchronously. Only best-effort accounting is detached.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let execution_id = record.id;
            // Construct only the future here; its unchanged deadline starts inside the owner task.
            let settlement = async move {
                timeout(
                    SETTLEMENT_DEADLINE,
                    repository.finish_attempt(&record, &telemetry),
                )
                .await
            };
            let settlement = crate::db::accounting_failure::cancellation_settlement(settlement);
            #[cfg(test)]
            let settlement = crate::db::diagnostics::settlement(
                self.diagnostics.clone(),
                crate::db::diagnostics::Stage::CancellationSettlement,
                settlement,
            );
            runtime.spawn(async move {
                let finished = settlement.await;
                if finished.is_err() {
                    crate::metrics::METRICS.observe_settlement(crate::metrics::Settlement::Held);
                }
                if !matches!(finished, Ok(Ok(()))) {
                    tracing::error!(execution_id = %execution_id, trace_id = log_trace_id.as_ref().map(LogTraceId::as_str), "cancelled inference finalization failed; reconciliation required");
                    // Cancellation never carries a trace id (#294): only the
                    // explicit `finish` path above is generation-finalization-proven.
                    crate::ops_log::emit(crate::ops_log::OpsEvent::FinalizationNeedsReconciliation {
                        finalizer: crate::ops_log::Finalizer::GenerationCancel,
                        trace: None,
                    });
                }
            });
        }
    }
}

#[cfg(all(test, feature = "integration-tests"))]
pub(crate) mod admission_validation_tests;
#[cfg(all(test, feature = "integration-tests"))]
mod steering_tests;
#[cfg(test)]
mod telemetry_tests;
#[cfg(test)]
mod tests;
