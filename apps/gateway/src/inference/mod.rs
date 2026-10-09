pub mod audio;
pub mod client;
mod deadline_stream;
mod embeddings;
pub mod error;
pub(crate) mod evidence;
pub mod images;
pub mod realtime;
pub mod repository;
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

use crate::{auth::Principal, providers::ProviderRegistry};
use error::InferenceError;
use repository::{AttemptTelemetry, ExecutionFinish, ExecutionStart, InferenceRepository, Outcome};
use types::{ApiProtocol, ChatEvent, ChatRequest, ProviderOutput, Usage};

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
        })
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
        let deployments = timeout_at(
            deadline,
            self.repository.deployments(&principal, &request.model),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        if deployments.is_empty() {
            return Err(InferenceError::ModelUnavailable);
        }
        let candidates: Vec<_> = deployments
            .into_iter()
            .filter(|deployment| {
                deployment
                    .supported_protocols
                    .iter()
                    .any(|p| p == protocol.as_str())
                    && self
                        .registry
                        .get(&deployment.provider)
                        .is_some_and(|adapter| {
                            adapter.supports_protocol(protocol)
                                && adapter.supports_chat_request(&request)
                        })
            })
            .collect();
        if candidates.is_empty() {
            return Err(InferenceError::Unsupported);
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
        let streamed = request.stream;
        for (attempt, id) in plan
            .deployment_ids
            .iter()
            .take(plan.max_attempts)
            .enumerate()
        {
            let deployment = candidates
                .iter()
                .find(|d| d.id == *id)
                .ok_or(InferenceError::Configuration)?;
            let adapter = self
                .registry
                .get(&deployment.provider)
                .ok_or(InferenceError::Configuration)?;
            let execution_id = if attempt == 0 {
                request_id
            } else {
                Uuid::new_v4()
            };
            let attempt_started = Instant::now();
            timeout_at(
                deadline,
                self.repository.admit(
                    &ExecutionStart {
                        id: execution_id,
                        root_request_id: request_id,
                        attempt_number: (attempt + 1) as i32,
                        principal,
                        deployment_id: deployment.id,
                        provider: deployment.provider.clone(),
                        model: request.model.clone(),
                        streamed,
                        upstream_model: upstream_snapshot(&deployment.upstream_model),
                        client: labels.clone(),
                    },
                    &request,
                    self.limits
                        .request_timeout
                        .as_secs()
                        .min(i64::MAX as u64 - 5) as i64
                        + 5,
                    deployment,
                ),
            )
            .await
            .map_err(|_| InferenceError::Timeout)??;
            let mut guard = ExecutionGuard::new(
                self.repository.clone(),
                execution_id,
                deployment.id,
                attempt_started,
                permit.clone(),
            );
            let (output, invalid_body_usage) = evidence::capture(timeout_at(
                deadline,
                adapter.execute_protocol(deployment, request.clone(), protocol),
            ))
            .await;
            // A body that failed validation still fails, but its valid usage
            // object is retained as observed evidence (the hold is kept).
            if let Some(observed) = invalid_body_usage.filter(|u| valid_usage(*u)) {
                guard.usage = observed;
            }
            let output = match output {
                Ok(result) => result,
                Err(_) => {
                    guard.health_observation = false;
                    Err(InferenceError::Timeout)
                }
            };
            match output {
                Err(error) => {
                    guard.finish(Outcome::Failed, Some(error)).await?;
                    if attempt + 1 < plan.max_attempts
                        && attempt + 1 < plan.deployment_ids.len()
                        && crate::routing::may_failover(&plan, error)
                    {
                        continue;
                    }
                    return Err(error);
                }
                Ok(ProviderOutput::Complete(response)) if !streamed => {
                    if !valid_usage(response.usage) {
                        guard
                            .finish(Outcome::Failed, Some(InferenceError::InvalidUpstream))
                            .await?;
                        return Err(InferenceError::InvalidUpstream);
                    }
                    guard.usage = response.usage;
                    guard.telemetry.finish_reason = Some(response.finish_reason.into());
                    guard.finish(Outcome::Succeeded, None).await?;
                    return Ok(ProviderOutput::Complete(response));
                }
                Ok(ProviderOutput::Stream(upstream)) if streamed => {
                    // The watchdog drops transport at the deadline even if a slow
                    // downstream never polls the returned stream again.
                    let mut upstream = deadline_stream::DeadlineStream::new(
                        upstream,
                        deadline,
                        Some(permit.clone()),
                    );
                    let tracked = async_stream::stream! {
                        // Capturing guard outside the generator accounts for drops even before first poll.
                        let mut guard = guard;
                        let mut finished_choice = false;
                        let mut reported_usage = false;
                        loop {
                            let (polled, invalid_body_usage) = evidence::capture(timeout_at(deadline, upstream.next())).await;
                            if let Some(observed) = invalid_body_usage.filter(|u| !reported_usage && valid_usage(*u)) {
                                guard.usage = observed;
                            }
                            let next = match polled {
                                Ok(Some(item)) => item,
                                Ok(None) => Err(InferenceError::InvalidUpstream),
                                Err(_) => Err(InferenceError::Timeout),
                            };
                            if matches!(next,Err(InferenceError::Timeout)) && Instant::now()>=deadline {guard.health_observation=false;}
                            let item = match next {
                                Ok(ChatEvent::Delta { .. }) if finished_choice => Err(InferenceError::InvalidUpstream),
                                Ok(ChatEvent::Delta { text, tool_calls }) => { guard.first_token(text.as_deref(), &tool_calls); Ok(ChatEvent::Delta { text, tool_calls }) }
                                Ok(ChatEvent::Finish(_)) if finished_choice => Err(InferenceError::InvalidUpstream),
                                Ok(ChatEvent::Finish(reason)) => { finished_choice = true; guard.telemetry.finish_reason = Some(reason.into()); Ok(ChatEvent::Finish(reason)) }
                                Ok(ChatEvent::Usage(usage)) if reported_usage || !valid_usage(usage) => Err(InferenceError::InvalidUpstream),
                                Ok(ChatEvent::Usage(usage)) => { reported_usage = true; guard.usage = usage; Ok(ChatEvent::Usage(usage)) }
                                Ok(ChatEvent::Done) if !finished_choice => Err(InferenceError::InvalidUpstream),
                                other => other,
                            };
                            match item {
                                Ok(ChatEvent::Done) => {
                                    match guard.finish(Outcome::Succeeded, None).await {
                                        Ok(()) => yield Ok(ChatEvent::Done),
                                        Err(error) => yield Err(error),
                                    }
                                    break;
                                }
                                Ok(event) => yield Ok(event),
                                Err(error) => {
                                    let error = guard.finish(Outcome::Failed, Some(error)).await.err().unwrap_or(error);
                                    yield Err(error);
                                    break;
                                }
                            }
                        }
                    };
                    return Ok(ProviderOutput::Stream(Box::pin(tracked)));
                }
                Ok(_) => {
                    guard
                        .finish(Outcome::Failed, Some(InferenceError::InvalidUpstream))
                        .await?;
                    return Err(InferenceError::InvalidUpstream);
                }
            }
        }
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
    ) -> Result<(), InferenceError> {
        // Mark attempted finalization first: failed writes remain 'started' for reconciliation.
        self.finished = true;
        let result = timeout(
            Duration::from_secs(3),
            self.repository
                .finish_attempt(&self.record(outcome, error), &self.telemetry_now()),
        )
        .await
        .map_err(|_| InferenceError::Storage)
        .and_then(|result| result);
        if result.is_err() {
            tracing::error!(execution_id = %self.id, "inference finalization failed; reconciliation required");
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
            tracing::warn!(execution_id=%self.id,"routing health update failed");
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
        // Network work is dropped synchronously. Only best-effort accounting is detached.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if !matches!(timeout(Duration::from_secs(3), repository.finish_attempt(&record, &telemetry)).await, Ok(Ok(()))) {
                    tracing::error!(execution_id = %record.id, "cancelled inference finalization failed; reconciliation required");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests;
