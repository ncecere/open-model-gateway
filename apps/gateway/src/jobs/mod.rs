//! Async jobs: video generation and batches (see `docs/async-jobs.md`).
//!
//! A job is **one upstream attempt** with one durable execution and
//! reservation, admitted through the ordinary `governance` functions when the
//! job is created and settled through `governance::finish` when the provider
//! reports a terminal state (by the background poller or a client read).
//! Settlement is idempotent; unknown usage keeps the hold. The job table
//! (`async_jobs`, migration 0016) holds metadata only: never prompts, batch
//! lines, outputs, metadata values or messages.
//!
//! Ownership: a job belongs to the creating workspace. Every lookup filters
//! by the principal's workspace, so another workspace's id is "not found".
//! Client ids are gateway ids; upstream ids never leave the gateway.
pub mod batch;
pub mod poller;
mod store;
pub mod types;
pub(crate) mod upload;
pub mod video;

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use crate::{
    auth::Principal,
    billing::MeterUsage,
    inference::{
        Engine,
        error::InferenceError,
        repository::{ExecutionFinish, ExecutionStart, InferenceRepository, Outcome},
        types::{ApiProtocol, Deployment, Usage},
        workload::WorkloadAdmission,
    },
    providers::{ProviderAdapter, ProviderRegistry},
    store::Store,
};
pub use store::JobRow;
use types::*;

/// Video jobs are polled (and their lease kept) for at most this long.
pub const VIDEO_POLL_WINDOW: TimeDelta = TimeDelta::hours(6);
/// Batches: the 24 h completion window plus finalization.
pub const BATCH_POLL_WINDOW: TimeDelta = TimeDelta::hours(26);
/// Lease grace beyond the poll deadline before reconciliation marks the
/// reservation unknown (hold retained).
const LEASE_GRACE: TimeDelta = TimeDelta::minutes(10);
/// Upstream deadline of one job call (create, retrieve, cancel, delete).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Deadline of one streamed batch upload.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Server configuration of async jobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobLimits {
    /// `GATEWAY_JOB_POLL_INTERVAL_SECONDS` (default 30, `0` disables, ≤ 3600).
    pub poll_interval: Option<Duration>,
    /// `GATEWAY_MAX_BODY_BYTES_VIDEOS` (default 2 MiB, 1 KiB–64 MiB).
    pub video_body_bytes: usize,
    /// `GATEWAY_MAX_BATCH_FILE_BYTES` (default 200 MiB, 1 KiB–512 MiB).
    pub batch_file_bytes: u64,
    /// `GATEWAY_BATCH_MAX_OUTPUT_SCAN_BYTES` (default 1 GiB, 1 MiB–16 GiB).
    pub output_scan_bytes: u64,
}
const MIB: u64 = 1024 * 1024;
impl Default for JobLimits {
    fn default() -> Self {
        Self {
            poll_interval: Some(Duration::from_secs(30)),
            video_body_bytes: 2 * MIB as usize,
            batch_file_bytes: 200 * MIB,
            output_scan_bytes: 1024 * MIB,
        }
    }
}
impl JobLimits {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let mut limits = Self::default();
        let int = |name: &str| -> anyhow::Result<Option<u64>> {
            get(name)
                .map(|v| {
                    v.trim()
                        .parse::<u64>()
                        .map_err(|_| anyhow::anyhow!("{name} must be a non-negative integer"))
                })
                .transpose()
        };
        if let Some(n) = int("GATEWAY_JOB_POLL_INTERVAL_SECONDS")? {
            anyhow::ensure!(
                n <= 3600,
                "GATEWAY_JOB_POLL_INTERVAL_SECONDS must be 0..=3600"
            );
            limits.poll_interval = (n > 0).then(|| Duration::from_secs(n));
        }
        if let Some(n) = int("GATEWAY_MAX_BODY_BYTES_VIDEOS")? {
            anyhow::ensure!(
                (1024..=64 * MIB).contains(&n),
                "GATEWAY_MAX_BODY_BYTES_VIDEOS out of range"
            );
            limits.video_body_bytes = n as usize;
        }
        if let Some(n) = int("GATEWAY_MAX_BATCH_FILE_BYTES")? {
            anyhow::ensure!(
                (1024..=512 * MIB).contains(&n),
                "GATEWAY_MAX_BATCH_FILE_BYTES out of range"
            );
            limits.batch_file_bytes = n;
        }
        if let Some(n) = int("GATEWAY_BATCH_MAX_OUTPUT_SCAN_BYTES")? {
            anyhow::ensure!(
                (MIB..=16 * 1024 * MIB).contains(&n),
                "GATEWAY_BATCH_MAX_OUTPUT_SCAN_BYTES out of range"
            );
            limits.output_scan_bytes = n;
        }
        Ok(limits)
    }
}
static LIMITS: OnceLock<JobLimits> = OnceLock::new();
/// Set the process configuration once at startup (before serving).
pub fn configure(limits: JobLimits) -> anyhow::Result<()> {
    LIMITS
        .set(limits)
        .map_err(|_| anyhow::anyhow!("job limits already configured"))
}
pub fn limits() -> JobLimits {
    LIMITS.get().copied().unwrap_or_default()
}

/// Job operation failure: unknown/foreign ids are `NotFound`, operations
/// invalid for the job's state are `Conflict` (code, message).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobError {
    NotFound,
    Conflict(&'static str, &'static str),
    Inference(InferenceError),
}
impl From<InferenceError> for JobError {
    fn from(e: InferenceError) -> Self {
        Self::Inference(e)
    }
}
impl From<sqlx::Error> for JobError {
    fn from(_: sqlx::Error) -> Self {
        Self::Inference(InferenceError::Storage)
    }
}
pub type JobResult<T> = Result<T, JobError>;

/// The job service: storage, adapters and deadlines.
#[derive(Clone)]
pub struct Jobs {
    pub(crate) store: Store,
    pub(crate) registry: ProviderRegistry,
    pub(crate) limits: JobLimits,
}
impl Jobs {
    pub fn new(store: Store, engine: &Engine) -> Self {
        Self::with(store, engine.registry().clone(), limits())
    }
    pub fn with(store: Store, registry: ProviderRegistry, limits: JobLimits) -> Self {
        Self {
            store,
            registry,
            limits,
        }
    }

    /// Route a new job: live catalog/key filtering, protocol and adapter
    /// capability gating, then the route plan's first candidate (async jobs
    /// never fail over: a job is one attempt).
    pub(crate) async fn select(
        &self,
        principal: &Principal,
        model: &str,
        protocol: ApiProtocol,
        request_id: Uuid,
        supports: impl Fn(&dyn ProviderAdapter, &Deployment) -> bool,
    ) -> Result<(Deployment, Arc<dyn ProviderAdapter>), InferenceError> {
        let deployments = self.store.deployments(principal, model).await?;
        if deployments.is_empty() {
            return Err(InferenceError::ModelUnavailable);
        }
        let candidates: Vec<_> = deployments
            .into_iter()
            .filter(|d| {
                d.supported_protocols.iter().any(|p| p == protocol.as_str())
                    && self
                        .registry
                        .get(&d.provider)
                        .is_some_and(|a| a.supports_protocol(protocol) && supports(a.as_ref(), d))
            })
            .collect();
        if candidates.is_empty() {
            return Err(InferenceError::Unsupported);
        }
        let plan = self
            .store
            .route_plan(principal, model, &candidates, request_id)
            .await?;
        let id = plan
            .deployment_ids
            .first()
            .ok_or(InferenceError::ModelUnavailable)?;
        let target = candidates
            .into_iter()
            .find(|d| d.id == *id)
            .ok_or(InferenceError::Configuration)?;
        let adapter = self
            .registry
            .get(&target.provider)
            .ok_or(InferenceError::Configuration)?;
        Ok((target, adapter))
    }

    /// Durable admission of one job attempt (execution + reservation), with a
    /// short lease that is extended once the provider accepted the job.
    pub(crate) async fn admit(
        &self,
        principal: Principal,
        execution: Uuid,
        model: &str,
        target: &Deployment,
        admission: &WorkloadAdmission,
    ) -> Result<Attempt, InferenceError> {
        let upstream_model = (1..=512)
            .contains(&target.upstream_model.chars().count())
            .then(|| target.upstream_model.clone());
        // Covers the create call; extended to the poll deadline on acceptance.
        let lease_seconds = CALL_TIMEOUT.as_secs() as i64 + 60;
        crate::governance::admit_workload_for_deployment(
            &self.store,
            &ExecutionStart {
                id: execution,
                root_request_id: execution,
                attempt_number: 1,
                principal,
                deployment_id: target.id,
                provider: target.provider.clone(),
                model: model.to_owned(),
                streamed: false,
                upstream_model,
                client: crate::inference::client::current(),
            },
            admission,
            lease_seconds,
            target,
        )
        .await?;
        Ok(Attempt {
            store: self.store.clone(),
            id: execution,
            started: std::time::Instant::now(),
            done: false,
        })
    }

    /// The pinned deployment of an existing job. Client-initiated calls and
    /// polling require the connection to still be enabled (an operator's
    /// kill switch); the provider kind must not have changed.
    pub(crate) async fn target(
        &self,
        job: &JobRow,
    ) -> Result<(Deployment, Arc<dyn ProviderAdapter>), InferenceError> {
        let target = store::target(&self.store, job.deployment_id)
            .await
            .map_err(|_| InferenceError::Storage)?
            .filter(|d| d.provider == job.provider)
            .ok_or(InferenceError::ModelUnavailable)?;
        let adapter = self
            .registry
            .get(&target.provider)
            .ok_or(InferenceError::Configuration)?;
        Ok((target, adapter))
    }

    /// Keep the reservation pending until the job's poll deadline.
    pub(crate) async fn hold_until(&self, execution: Uuid, deadline: DateTime<Utc>) {
        let until = deadline + LEASE_GRACE;
        if !matches!(
            crate::governance::jobs::extend_lease(&self.store, execution, until).await,
            Ok(true)
        ) {
            tracing::error!(execution_id = %execution, "job lease extension failed; reconciliation will keep the hold as unknown");
        }
    }

    /// Record a terminal observation's accounting exactly once. A replay of an
    /// identical finish is success. A reservation that is no longer pending
    /// (lease expired, reconciled) is left untouched.
    pub(crate) async fn settle(&self, job: &JobRow, finish: Settlement) -> JobResult<()> {
        if job.settled_at.is_some() {
            return Ok(());
        }
        let pending = store::reservation_pending(&self.store, job.execution_id).await?;
        if pending {
            let elapsed = (Utc::now() - job.created_at).num_milliseconds().max(0) as u64;
            crate::governance::finish(
                &self.store,
                &ExecutionFinish {
                    id: job.execution_id,
                    outcome: finish.outcome,
                    error: finish.error,
                    usage: finish.usage,
                    elapsed_ms: elapsed,
                },
            )
            .await?;
        } else {
            tracing::warn!(job_id = %job.id, "job reservation no longer pending; settlement left to reconciliation");
        }
        store::mark_settled(&self.store, job.id).await?;
        Ok(())
    }
}

/// Accounting of a terminal job observation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settlement {
    pub outcome: Outcome,
    pub error: Option<InferenceError>,
    pub usage: Usage,
}

/// Meter usage of a non-text job: every non-video meter is a semantic zero,
/// one upstream request, and the observed video duration (or unknown).
pub(crate) fn video_meters(video_ms: Option<u64>) -> MeterUsage {
    MeterUsage {
        output_images: Some(0),
        input_characters: Some(0),
        input_audio_seconds_ms: Some(0),
        output_audio_seconds_ms: Some(0),
        search_units: Some(0),
        requests: Some(1),
        output_video_seconds_ms: video_ms,
    }
}
/// Request-derived unit ceilings (`requests`, video ms; other meters 0).
pub(crate) fn unit_ceilings(requests: u64, video_ms: u64) -> MeterUsage {
    MeterUsage {
        requests: Some(requests),
        output_video_seconds_ms: Some(video_ms),
        ..video_meters(Some(video_ms))
    }
}

/// An admitted attempt whose upstream create call is in flight. Dropping it
/// unfinished (client disconnect) records a cancelled attempt; the hold is
/// kept because the provider may still have accepted the job.
pub(crate) struct Attempt {
    store: Store,
    id: Uuid,
    started: std::time::Instant,
    done: bool,
}
impl Attempt {
    fn record(&self, outcome: Outcome, error: Option<InferenceError>) -> ExecutionFinish {
        ExecutionFinish {
            id: self.id,
            outcome,
            error,
            usage: Usage::default(),
            elapsed_ms: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        }
    }
    /// The provider did not accept the job: record the failed attempt.
    pub(crate) async fn fail(mut self, error: InferenceError) -> InferenceError {
        self.done = true;
        let record = self.record(Outcome::Failed, Some(error));
        if crate::governance::finish(&self.store, &record)
            .await
            .is_err()
        {
            tracing::error!(execution_id = %self.id, "job attempt finalization failed; reconciliation required");
        }
        error
    }
    /// The provider accepted the job; settlement follows its terminal state.
    pub(crate) fn accepted(mut self) {
        self.done = true;
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let store = self.store.clone();
        let record = self.record(Outcome::Cancelled, None);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if crate::governance::finish(&store, &record).await.is_err() {
                    tracing::error!(execution_id = %record.id, "cancelled job attempt finalization failed; reconciliation required");
                }
            });
        }
    }
}

/// Bounded upstream call.
pub(crate) async fn call<T>(
    future: impl std::future::Future<Output = Result<T, InferenceError>>,
) -> Result<T, InferenceError> {
    tokio::time::timeout(CALL_TIMEOUT, future)
        .await
        .map_err(|_| InferenceError::Timeout)?
}

#[cfg(all(test, feature = "integration-tests"))]
mod db_tests;
#[cfg(test)]
mod tests;
