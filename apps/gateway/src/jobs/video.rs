//! Video jobs: create (admission + upstream create), refresh/settle, list,
//! content pass-through and delete. Rendering uses gateway ids only.
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{store::Observation, *};
use crate::inference::{types::WorkloadKind, workload::OutputReservation};

fn observation(v: &UpstreamVideo) -> Observation<'_> {
    Observation {
        state: v.state,
        upstream_status: v.state.as_str(),
        progress: v.progress,
        completed_at: v.completed_at,
        expires_at: v.expires_at,
        error: v.error.as_ref(),
        counts: None,
    }
}

/// Accounting of a terminal video observation.
pub(crate) fn settlement(job: &JobRow, v: &UpstreamVideo) -> Option<Settlement> {
    let requested = job.video_seconds.and_then(|s| u32::try_from(s).ok())?;
    match v.state {
        JobState::Completed => {
            let variant = v.size.or_else(|| {
                job.video_size
                    .as_deref()
                    .and_then(crate::billing::MeterVariant::new)
            });
            let usage = |ms: Option<u64>| Usage {
                meters: Some(video_meters(ms)),
                output_image_variant: variant,
                ..Usage::default()
            };
            Some(match v.seconds {
                // A longer clip than requested is not covered by the hold:
                // keep the evidence and the hold (never settle it).
                Some(s) if s > requested => Settlement {
                    outcome: Outcome::Failed,
                    error: Some(InferenceError::InvalidUpstream),
                    usage: usage(Some(u64::from(s) * 1000)),
                },
                // Unknown duration stays unknown (hold retained).
                seconds => Settlement {
                    outcome: Outcome::Succeeded,
                    error: None,
                    usage: usage(seconds.map(|s| u64::from(s) * 1000)),
                },
            })
        }
        // The provider failed the job; without usage the hold is kept unless
        // the price is entirely free.
        JobState::Failed => Some(Settlement {
            outcome: Outcome::Failed,
            error: Some(InferenceError::UpstreamRejected),
            usage: Usage::default(),
        }),
        _ => None,
    }
}

pub fn render(job: &JobRow) -> Value {
    let unix = |t: Option<chrono::DateTime<Utc>>| t.map(|t| t.timestamp());
    let state = job.job_state();
    json!({
        "id": client_id(JobKind::Video.prefix(), job.id),
        "object": "video",
        "model": job.public_model,
        "status": state.as_str(),
        "progress": job.progress.map(i64::from).unwrap_or(if state == JobState::Completed { 100 } else { 0 }),
        "created_at": job.created_at.timestamp(),
        "completed_at": unix(job.completed_at),
        "expires_at": unix(job.expires_at),
        "seconds": job.video_seconds.map(|s| s.to_string()),
        "size": job.video_size,
        "remixed_from_video_id": Value::Null,
        "error": job.error_code.as_ref().filter(|_| state == JobState::Failed).map(|code| json!({"code": code, "message": "Video generation failed"})),
    })
}

impl Jobs {
    pub async fn create_video(
        &self,
        principal: Principal,
        request: VideoRequest,
        request_id: Uuid,
    ) -> JobResult<JobRow> {
        request.validate()?;
        let model = request.model.clone();
        let (target, adapter) = self
            .select(
                &principal,
                &model,
                ApiProtocol::Videos,
                request_id,
                |a, d| a.supports_video_request(d, &request),
            )
            .await?;
        let admission = WorkloadAdmission {
            kind: WorkloadKind::Videos,
            output: OutputReservation::None,
            unit_ceilings: unit_ceilings(1, u64::from(request.seconds) * 1000),
        };
        let attempt = self
            .admit(principal, request_id, &model, &target, &admission)
            .await?;
        let size = request.size;
        let seconds = request.seconds;
        let upstream = match call(adapter.create_video(&target, request)).await {
            Ok(v) => v,
            Err(e) => return Err(attempt.fail(e).await.into()),
        };
        attempt.accepted();
        let deadline = Utc::now() + VIDEO_POLL_WINDOW;
        let job = store::insert_job(
            &self.store,
            &store::NewJob {
                id: Uuid::new_v4(),
                kind: JobKind::Video,
                workspace_id: principal.workspace_id,
                api_key_id: principal.key_id,
                deployment_id: target.id,
                execution_id: request_id,
                public_model: &model,
                provider: &target.provider,
                upstream_id: &upstream.id,
                poll_deadline_at: deadline,
                video_seconds: Some(seconds as i32),
                video_size: Some(size.as_str()),
                batch_endpoint: None,
            },
        )
        .await
        .inspect_err(|_| {
            tracing::error!(execution_id = %request_id, "video job record failed after upstream acceptance; hold kept until reconciliation");
        })?;
        self.hold_until(request_id, deadline).await;
        self.apply_video(&job, &upstream).await
    }

    /// Apply an observation and settle a terminal one (idempotent).
    async fn apply_video(&self, job: &JobRow, upstream: &UpstreamVideo) -> JobResult<JobRow> {
        let interval = self.poll_seconds();
        let job = store::observe(&self.store, job, &observation(upstream), interval).await?;
        if job.job_state().is_terminal()
            && job.settled_at.is_none()
            && let Some(s) = settlement(&job, upstream)
        {
            self.settle(&job, s).await?;
            return Ok(store::reload(&self.store, job.id).await?);
        }
        Ok(job)
    }

    pub(crate) fn poll_seconds(&self) -> f64 {
        self.limits.poll_interval.map_or(30.0, |d| d.as_secs_f64())
    }

    async fn own_video(&self, principal: &Principal, id: &str) -> JobResult<JobRow> {
        let id = parse_client_id(JobKind::Video.prefix(), id).ok_or(JobError::NotFound)?;
        store::job(&self.store, principal.workspace_id, JobKind::Video, id)
            .await?
            .ok_or(JobError::NotFound)
    }

    /// Current state, refreshed from the provider while not terminal.
    pub async fn get_video(&self, principal: &Principal, id: &str) -> JobResult<JobRow> {
        let job = self.own_video(principal, id).await?;
        if job.job_state().is_terminal() && job.settled_at.is_some() {
            return Ok(job);
        }
        self.refresh_video(job).await
    }

    pub(crate) async fn refresh_video(&self, job: JobRow) -> JobResult<JobRow> {
        let (target, adapter) = self.target(&job).await?;
        let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
        let upstream = call(adapter.retrieve_video(&target, &upstream_id)).await?;
        self.apply_video(&job, &upstream).await
    }

    pub async fn list_videos(
        &self,
        principal: &Principal,
        after: Option<&str>,
        limit: i64,
        ascending: bool,
    ) -> JobResult<Vec<JobRow>> {
        let after = after
            .map(|a| parse_client_id(JobKind::Video.prefix(), a).ok_or(JobError::NotFound))
            .transpose()?;
        Ok(store::list(
            &self.store,
            principal.workspace_id,
            JobKind::Video,
            after,
            limit,
            ascending,
        )
        .await?)
    }

    pub async fn video_content(
        &self,
        principal: &Principal,
        id: &str,
        asset: VideoAsset,
    ) -> JobResult<ContentStream> {
        let job = self.own_video(principal, id).await?;
        if job.job_state() != JobState::Completed {
            return Err(JobError::Conflict(
                "video_not_ready",
                "Video content is available once the video is completed",
            ));
        }
        if job.deleted_at.is_some() {
            return Err(JobError::NotFound);
        }
        let (target, adapter) = self.target(&job).await?;
        let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
        Ok(call(adapter.video_content(&target, &upstream_id, asset)).await?)
    }

    /// Delete a finished video's provider assets. In-progress jobs cannot be
    /// deleted (their accounting is still open).
    pub async fn delete_video(&self, principal: &Principal, id: &str) -> JobResult<JobRow> {
        let job = self.own_video(principal, id).await?;
        if job.deleted_at.is_some() {
            return Err(JobError::NotFound);
        }
        if !job.job_state().is_terminal() {
            return Err(JobError::Conflict(
                "video_in_progress",
                "A video can be deleted once it is completed or failed",
            ));
        }
        let (target, adapter) = self.target(&job).await?;
        let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
        call(adapter.delete_video(&target, &upstream_id)).await?;
        store::mark_deleted(&self.store, job.id).await?;
        Ok(job)
    }
}
