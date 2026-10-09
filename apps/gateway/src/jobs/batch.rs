//! `/v1/batches` (0021, see `docs/batches.md`): create (validate the gateway
//! input file, choose the mode, admit the batch's ceiling), retrieve, cancel
//! and list. Native batches are then driven by the poller (`native`),
//! gateway-run batches by the batch runner (`runner`). Batches created by
//! the 0016 passthrough are still polled until settled (`refresh_legacy`).
use chrono::Utc;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    plan::{Issue, PlanError},
    store::{NewBatch, Observation},
    *,
};
use crate::{
    filestore::{Purpose, files::public_id},
    governance::batch::PriceTier,
};

/// Admission lease of a batch until its record exists (then extended).
const ADMISSION_LEASE_SECONDS: i64 = 600;

/// Accounting of a terminal provider observation. `usage` is the provider
/// aggregate or the per-line sum; `None` keeps the hold (unknown).
pub(crate) fn settlement(state: JobState, usage: Option<Usage>) -> Option<Settlement> {
    let usage = usage.unwrap_or_default();
    Some(match state {
        JobState::Completed => Settlement {
            outcome: Outcome::Succeeded,
            error: None,
            usage,
        },
        JobState::Failed => Settlement {
            outcome: Outcome::Failed,
            error: Some(InferenceError::UpstreamRejected),
            usage,
        },
        // Partial work may be billed: record the evidence (a known floor)
        // and keep the hold for reconciliation.
        JobState::Cancelled => Settlement {
            outcome: Outcome::Cancelled,
            error: None,
            usage,
        },
        JobState::Expired => Settlement {
            outcome: Outcome::Failed,
            error: Some(InferenceError::Timeout),
            usage,
        },
        JobState::Queued | JobState::InProgress => return None,
    })
}

/// A create request (`POST /v1/batches`).
pub struct CreateBatch {
    pub input_file_id: String,
    pub endpoint: BatchEndpoint,
    pub metadata: Option<Map<String, Value>>,
    /// `completion_window` in hours (`None`: 24). A window other than 24 h
    /// always runs gateway-side (provider batch APIs only offer 24 h).
    pub completion_window_hours: Option<i16>,
}

/// Why a batch was not created.
#[derive(Debug)]
pub enum CreateError {
    Job(JobError),
    /// The input file failed validation (line-numbered report, ≤ 20 items).
    Invalid(Vec<Issue>),
}
impl From<JobError> for CreateError {
    fn from(e: JobError) -> Self {
        Self::Job(e)
    }
}
impl From<InferenceError> for CreateError {
    fn from(e: InferenceError) -> Self {
        Self::Job(e.into())
    }
}
impl From<sqlx::Error> for CreateError {
    fn from(e: sqlx::Error) -> Self {
        Self::Job(e.into())
    }
}

/// Gateway options in `metadata` (`omg_mode`, `omg_retries`), validated.
fn options(metadata: Option<&Map<String, Value>>) -> Result<(bool, i16), JobError> {
    let invalid = JobError::Invalid(
        "invalid_metadata",
        "metadata must hold at most 16 string values; omg_mode is auto or gateway and omg_retries 0, 1 or 2",
    );
    let Some(m) = metadata else {
        return Ok((false, 0));
    };
    if !valid_metadata(m) {
        return Err(invalid);
    }
    let gateway = match m.get("omg_mode").and_then(Value::as_str) {
        None | Some("auto") => false,
        Some("gateway") => true,
        Some(_) => return Err(invalid),
    };
    let retries = match m.get("omg_retries").and_then(Value::as_str) {
        None | Some("0") => 0,
        Some("1") => 1,
        Some("2") => 2,
        Some(_) => return Err(invalid),
    };
    Ok((gateway, retries))
}

/// The OpenAI batch status shown for a row.
pub fn status(job: &JobRow) -> &str {
    match job.job_state() {
        JobState::Queued | JobState::InProgress => {
            job.upstream_status.as_deref().unwrap_or("validating")
        }
        s => s.as_str(),
    }
}

/// The OpenAI `batch` object of a row. Metadata is not stored, so it is
/// never echoed (`null`).
pub fn render_batch(
    job: &JobRow,
    legacy: Option<(Option<Uuid>, Option<Uuid>, Option<Uuid>)>,
) -> Value {
    let unix = |t: Option<chrono::DateTime<Utc>>| t.map(|t| t.timestamp());
    let file = |id: Option<Uuid>| id.map(public_id);
    let (input, output, error) = match legacy {
        Some(files) => files,
        None => (job.input_file_id, job.output_file_id, job.error_file_id),
    };
    let state = job.job_state();
    let terminal = |s: JobState| {
        if state == s {
            unix(job.completed_at)
        } else {
            None
        }
    };
    let counts = job.request_total.map(|total| {
        json!({"total": total, "completed": job.request_completed.unwrap_or(0), "failed": job.request_failed.unwrap_or(0)})
    });
    json!({
        "id": client_id(JobKind::Batch.prefix(), job.id),
        "object": "batch",
        "endpoint": job.batch_endpoint,
        "model": job.public_model,
        "errors": Value::Null,
        "input_file_id": file(input),
        "completion_window": completion_window_name(job.completion_window_hours.unwrap_or(24)),
        "status": status(job),
        "output_file_id": file(output),
        "error_file_id": file(error),
        "created_at": job.created_at.timestamp(),
        "in_progress_at": unix(job.in_progress_at),
        "expires_at": job.expires_at.map(|t| t.timestamp()).or(Some((job.created_at + job.completion_window()).timestamp())),
        "finalizing_at": unix(job.finalizing_at),
        "completed_at": terminal(JobState::Completed),
        "failed_at": terminal(JobState::Failed),
        "expired_at": terminal(JobState::Expired),
        "cancelling_at": unix(job.cancel_requested_at),
        "cancelled_at": terminal(JobState::Cancelled),
        "request_counts": counts,
        "metadata": Value::Null,
    })
}

/// The 24 h completion window.
pub const BATCH_WINDOW: chrono::TimeDelta = chrono::TimeDelta::hours(24);

impl Jobs {
    /// `POST /v1/batches`: validate the input, choose the mode and admit the
    /// batch's ceiling (one "Jobs at once" slot). Nothing is sent upstream
    /// here; the runner/poller starts the work.
    pub async fn create_batch(
        &self,
        principal: Principal,
        request_id: Uuid,
        request: CreateBatch,
    ) -> Result<JobRow, CreateError> {
        let (force_gateway, retries) = options(request.metadata.as_ref())?;
        let window_hours = request.completion_window_hours.unwrap_or(24);
        if completion_window_hours(completion_window_name(window_hours)) != Some(window_hours) {
            return Err(JobError::Invalid(
                "invalid_completion_window",
                "completion_window must be 24h, 48h, 72h or 168h",
            )
            .into());
        }
        // Provider batch APIs only offer 24 h: longer windows run gateway-side.
        let force_gateway = force_gateway || window_hours != 24;
        let files = self.files()?;
        if !files
            .accepts(Purpose::BatchOutput)
            .await
            .map_err(|_| InferenceError::Storage)?
        {
            return Err(JobError::Invalid(
                "batch_files_disabled",
                "Batch files are turned off in this installation's storage settings",
            )
            .into());
        }
        let id = crate::filestore::files::parse_public_id(&request.input_file_id)
            .ok_or(JobError::NotFound)?;
        let input = files
            .get(id, Some(principal.workspace_id))
            .await
            .map_err(|_| InferenceError::Storage)?
            .ok_or(JobError::NotFound)?;
        if input.purpose != Purpose::BatchInput {
            return Err(JobError::Invalid(
                "invalid_input_file",
                "input_file_id must be a file uploaded with purpose batch",
            )
            .into());
        }
        let plan = match self
            .plan(
                &principal,
                request_id,
                &input,
                request.endpoint,
                force_gateway,
            )
            .await
        {
            Ok(p) => p,
            Err(PlanError::Invalid(issues)) => return Err(CreateError::Invalid(issues)),
            Err(PlanError::Job(e)) => return Err(e.into()),
        };
        let first = &plan.groups[0].deployment;
        let provider = if plan.groups.len() == 1 {
            first.provider.clone()
        } else {
            "mixed".to_owned()
        };
        let record = ExecutionStart {
            id: request_id,
            root_request_id: request_id,
            attempt_number: 1,
            principal,
            deployment_id: first.id,
            provider: provider.clone(),
            model: plan.model.clone(),
            streamed: false,
            upstream_model: plan
                .native
                .as_ref()
                .map(|(d, _)| d.upstream_model.clone())
                .filter(|m| (1..=512).contains(&m.chars().count())),
            client: crate::inference::client::current(),
        };
        let admitted = crate::governance::batch::admit_batch(
            &self.store,
            &record,
            &plan.groups,
            plan.native.is_some(),
            ADMISSION_LEASE_SECONDS,
        )
        .await;
        let hold = match admitted {
            Ok(h) => h,
            Err(e) => {
                let _ = files
                    .delete(plan.work_file, Some(principal.workspace_id))
                    .await;
                return Err(e.into());
            }
        };
        // The window plus finalization (26 h for the default 24 h window).
        let deadline = Utc::now() + BATCH_POLL_WINDOW - BATCH_WINDOW
            + chrono::TimeDelta::hours(i64::from(window_hours));
        let mode = if plan.native.is_some() {
            BatchMode::Native
        } else {
            BatchMode::Gateway
        };
        let job = store::insert_batch(
            &self.store,
            &NewBatch {
                id: Uuid::new_v4(),
                workspace_id: principal.workspace_id,
                api_key_id: principal.key_id,
                user_id: principal.user_id,
                deployment_id: first.id,
                execution_id: request_id,
                public_model: &plan.model,
                provider: &provider,
                poll_deadline_at: deadline,
                endpoint: request.endpoint,
                mode,
                input_file_id: input.id,
                work_file_id: plan.work_file,
                price_tier: (mode == BatchMode::Native).then_some(hold.tier.as_str()),
                retry_limit: retries,
                requests: plan.requests as i32,
                window_hours,
            },
        )
        .await
        .inspect_err(|_| {
            tracing::error!(execution_id = %request_id, "batch record failed after admission; hold kept until reconciliation");
        })?;
        self.hold_until(request_id, deadline).await;
        crate::metrics::METRICS.observe_batch_created(mode.as_str());
        Ok(job)
    }

    async fn own_batch(&self, principal: &Principal, id: &str) -> JobResult<JobRow> {
        let id = parse_client_id(JobKind::Batch.prefix(), id).ok_or(JobError::NotFound)?;
        store::job(&self.store, principal.workspace_id, JobKind::Batch, id)
            .await?
            .ok_or(JobError::NotFound)
    }

    /// The batch object (from gateway records; the runner and poller keep
    /// them current).
    pub async fn get_batch(&self, principal: &Principal, id: &str) -> JobResult<Value> {
        let job = self.own_batch(principal, id).await?;
        self.batch_object(&job).await
    }

    pub async fn batch_object(&self, job: &JobRow) -> JobResult<Value> {
        let legacy = match job.mode() {
            None => Some(store::legacy_files(&self.store, job.id).await?),
            Some(_) => None,
        };
        Ok(render_batch(job, legacy))
    }

    /// `POST /v1/batches/{id}/cancel`. A gateway-run batch stops starting
    /// lines, lets running lines finish and ends `cancelled`; a native batch
    /// is cancelled upstream (or at once when it was never submitted).
    pub async fn cancel_batch(&self, principal: &Principal, id: &str) -> JobResult<Value> {
        let job = self.own_batch(principal, id).await?;
        let job = self.cancel(job).await?;
        self.batch_object(&job).await
    }

    /// Cancel a batch the caller is allowed to manage.
    pub async fn cancel(&self, job: JobRow) -> JobResult<JobRow> {
        if job.job_state().is_terminal() {
            return Err(JobError::Conflict(
                "batch_not_cancellable",
                "This batch has already finished",
            ));
        }
        match job.mode() {
            Some(BatchMode::Native) if job.upstream_id.is_none() => {
                if store::cancel_unsubmitted(&self.store, job.id).await? {
                    // Nothing reached the provider: release the ceiling.
                    let elapsed = (Utc::now() - job.created_at).num_milliseconds().max(0) as u64;
                    crate::governance::batch::close_batch(
                        &self.store,
                        job.execution_id,
                        elapsed,
                        true,
                    )
                    .await?;
                    store::mark_settled(&self.store, job.id).await?;
                    self.discard_work_file(&job).await;
                } else {
                    // The submission is in flight: the poller cancels after it.
                    store::mark_cancel_requested(&self.store, job.id).await?;
                    store::set_status(&self.store, job.id, "cancelling").await?;
                }
            }
            Some(BatchMode::Gateway) => {
                store::mark_cancel_requested(&self.store, job.id).await?;
                store::set_status(&self.store, job.id, "cancelling").await?;
            }
            Some(BatchMode::Native) | None => {
                let (target, adapter) = self.target(&job).await?;
                let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
                let upstream = call(adapter.cancel_batch(&target, &upstream_id)).await?;
                store::mark_cancel_requested(&self.store, job.id).await?;
                if !upstream.status.state().is_terminal() {
                    store::set_status(&self.store, job.id, upstream.status.as_str()).await?;
                }
            }
        }
        Ok(store::reload(&self.store, job.id).await?)
    }

    pub async fn list_batches(
        &self,
        principal: &Principal,
        after: Option<&str>,
        limit: i64,
    ) -> JobResult<Vec<Value>> {
        let after = after
            .map(|a| parse_client_id(JobKind::Batch.prefix(), a).ok_or(JobError::NotFound))
            .transpose()?;
        let rows = store::list(
            &self.store,
            principal.workspace_id,
            JobKind::Batch,
            after,
            limit,
            false,
        )
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(self.batch_object(&row).await?);
        }
        Ok(out)
    }

    /// Best-effort removal of the batch's private copy once it is no longer
    /// needed (retention removes it otherwise).
    pub(crate) async fn discard_work_file(&self, job: &JobRow) {
        if let (Some(files), Some(id)) = (&self.files, job.work_file_id)
            && files.delete(id, Some(job.workspace_id)).await.is_err()
        {
            tracing::warn!(job_id = %job.id, "batch work file not deleted; retention will remove it");
        }
    }

    /// Poller step of a 0016 passthrough batch: record the provider's state
    /// and settle a terminal one from the provider's aggregate usage (without
    /// it the hold is retained as unknown; output files are no longer read).
    pub(crate) async fn refresh_legacy(&self, job: JobRow) -> JobResult<JobRow> {
        let (target, adapter) = self.target(&job).await?;
        let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
        let upstream = call(adapter.retrieve_batch(&target, &upstream_id)).await?;
        let job = store::observe(
            &self.store,
            &job,
            &Observation {
                state: upstream.status.state(),
                upstream_status: upstream.status.as_str(),
                progress: None,
                completed_at: upstream
                    .completed_at
                    .or(upstream.failed_at)
                    .or(upstream.cancelled_at)
                    .or(upstream.expired_at),
                expires_at: upstream.expires_at,
                error: None,
                counts: upstream.counts,
            },
            self.poll_seconds(),
        )
        .await?;
        let state = job.job_state();
        if state.is_terminal()
            && job.settled_at.is_none()
            && let Some(s) = settlement(state, upstream.usage)
        {
            self.settle(&job, s).await?;
        }
        Ok(store::reload(&self.store, job.id).await?)
    }
}

/// Whether a native batch row applied the batch price list.
pub fn batch_priced(job: &JobRow) -> Option<bool> {
    job.price_tier
        .as_deref()
        .and_then(PriceTier::parse)
        .map(|t| t == PriceTier::Batch)
}
