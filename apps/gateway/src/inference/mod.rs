mod deadline_stream;
pub mod error;
pub mod repository;
pub mod types;

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
use repository::{ExecutionFinish, ExecutionStart, InferenceRepository, Outcome};
use types::{ApiProtocol, ChatEvent, ChatRequest, ProviderOutput, Usage};

#[derive(Clone, Copy)]
pub struct EngineLimits {
    pub max_concurrent: usize,
    pub request_timeout: Duration,
}
impl Default for EngineLimits {
    fn default() -> Self {
        Self {
            max_concurrent: 128,
            request_timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Clone)]
pub struct Engine {
    repository: Arc<dyn InferenceRepository>,
    registry: ProviderRegistry,
    capacity: Arc<Semaphore>,
    limits: EngineLimits,
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
                && !limits.request_timeout.is_zero(),
            "invalid inference limits"
        );
        Ok(Self {
            repository,
            registry,
            capacity: Arc::new(Semaphore::new(limits.max_concurrent)),
            limits,
        })
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
                .map_err(|_| InferenceError::Busy)?,
        )));
        let started = Instant::now();
        let deadline = started + self.limits.request_timeout;
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
                self.registry
                    .get(&deployment.provider)
                    .is_some_and(|adapter| {
                        adapter.supports_protocol(protocol)
                            && adapter.capabilities().supports(&request)
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
            let mut guard = ExecutionGuard {
                repository: self.repository.clone(),
                id: execution_id,
                organization_id: principal.organization_id,
                deployment_id: deployment.id,
                started: attempt_started,
                usage: Usage::default(),
                finished: false,
                health_observation: true,
                _permit: permit.clone(),
            };
            let output = match timeout_at(
                deadline,
                adapter.execute_protocol(deployment, request.clone(), protocol),
            )
            .await
            {
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
                            let next = match timeout_at(deadline, upstream.next()).await {
                                Ok(Some(item)) => item,
                                Ok(None) => Err(InferenceError::InvalidUpstream),
                                Err(_) => Err(InferenceError::Timeout),
                            };
                            if matches!(next,Err(InferenceError::Timeout)) && Instant::now()>=deadline {guard.health_observation=false;}
                            let item = match next {
                                Ok(ChatEvent::Delta { .. }) if finished_choice => Err(InferenceError::InvalidUpstream),
                                Ok(ChatEvent::Finish(_)) if finished_choice => Err(InferenceError::InvalidUpstream),
                                Ok(ChatEvent::Finish(reason)) => { finished_choice = true; Ok(ChatEvent::Finish(reason)) }
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

fn valid_usage(usage: Usage) -> bool {
    [usage.input_tokens, usage.output_tokens]
        .into_iter()
        .flatten()
        .all(|n| n <= i64::MAX as u64)
}

struct ExecutionGuard {
    repository: Arc<dyn InferenceRepository>,
    id: Uuid,
    organization_id: Uuid,
    deployment_id: Uuid,
    started: Instant,
    usage: Usage,
    finished: bool,
    health_observation: bool,
    _permit: SharedPermit,
}
impl ExecutionGuard {
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
            self.repository.finish(&self.record(outcome, error)),
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
                    self.repository
                        .route_result(self.organization_id, self.deployment_id, error)
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
        // Network work is dropped synchronously. Only best-effort accounting is detached.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if !matches!(timeout(Duration::from_secs(3), repository.finish(&record)).await, Ok(Ok(()))) {
                    tracing::error!(execution_id = %record.id, "cancelled inference finalization failed; reconciliation required");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests;
