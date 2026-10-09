//! Batch jobs: streamed input upload (validated line by line, never stored),
//! batch create (admission of the whole file's ceiling), refresh/settle,
//! cancel pass-through, list, and file content pass-through.
use axum::body::Bytes;
use chrono::Utc;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    store::{FileRow, Observation},
    upload::{Lines, StreamingForm, validate_line},
    *,
};
use crate::inference::{types::WorkloadKind, workload::OutputReservation};

/// Streamed upload statistics (the reservation inputs of a later batch).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LineStats {
    pub requests: u32,
    pub output_tokens: u64,
    pub max_line_output: u32,
}

fn observation(b: &UpstreamBatch) -> Observation<'_> {
    Observation {
        state: b.status.state(),
        upstream_status: b.status.as_str(),
        progress: None,
        completed_at: b
            .completed_at
            .or(b.failed_at)
            .or(b.cancelled_at)
            .or(b.expired_at),
        expires_at: b.expires_at,
        error: None,
        counts: b.counts,
    }
}

/// Accounting of a terminal batch observation. `usage` is the provider
/// aggregate or the output-file sum; `None` keeps the hold (unknown).
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

pub fn render_file(f: &FileRow) -> Value {
    json!({
        "id": client_id(FILE_PREFIX, f.id),
        "object": "file",
        "bytes": f.bytes,
        "created_at": f.created_at.timestamp(),
        "filename": match f.purpose.as_str() {
            "batch" => "batch.jsonl",
            "batch_output" => "batch_output.jsonl",
            _ => "batch_errors.jsonl",
        },
        "purpose": f.purpose,
        "status": "processed",
        "expires_at": Value::Null,
        "status_details": Value::Null,
    })
}

/// Batch object from the stored row and gateway file ids, plus the step
/// timestamps and echoed metadata of a fresh provider observation.
pub fn render_batch(
    job: &JobRow,
    files: (Option<Uuid>, Option<Uuid>, Option<Uuid>),
    fresh: Option<&UpstreamBatch>,
) -> Value {
    let file = |id: Option<Uuid>| id.map(|id| client_id(FILE_PREFIX, id));
    let unix = |t: Option<chrono::DateTime<Utc>>| t.map(|t| t.timestamp());
    let status = job
        .upstream_status
        .clone()
        .unwrap_or_else(|| "validating".into());
    let counts = job.request_total.map(|total| {
        json!({"total": total, "completed": job.request_completed.unwrap_or(0), "failed": job.request_failed.unwrap_or(0)})
    });
    let step = |f: fn(&UpstreamBatch) -> Option<i64>| fresh.and_then(f);
    json!({
        "id": client_id(JobKind::Batch.prefix(), job.id),
        "object": "batch",
        "endpoint": job.batch_endpoint,
        "model": job.public_model,
        "errors": Value::Null,
        "input_file_id": file(files.0),
        "completion_window": BATCH_COMPLETION_WINDOW,
        "status": status,
        "output_file_id": file(files.1),
        "error_file_id": file(files.2),
        "created_at": job.created_at.timestamp(),
        "in_progress_at": step(|b| b.in_progress_at),
        "expires_at": unix(job.expires_at),
        "finalizing_at": step(|b| b.finalizing_at),
        "completed_at": if job.job_state() == JobState::Completed { unix(job.completed_at) } else { None },
        "failed_at": if job.job_state() == JobState::Failed { unix(job.completed_at) } else { None },
        "expired_at": if job.job_state() == JobState::Expired { unix(job.completed_at) } else { None },
        "cancelling_at": unix(job.cancel_requested_at),
        "cancelled_at": if job.job_state() == JobState::Cancelled { unix(job.completed_at) } else { None },
        "request_counts": counts,
        "metadata": fresh.and_then(|b| b.metadata.clone()),
    })
}

impl Jobs {
    /// `POST /v1/files` (`purpose=batch`): validate each line while
    /// streaming it to the provider. The first line selects the model's
    /// deployment; the file's ceiling must be priced and bounded before the
    /// upload is allowed to complete (an error aborts the upstream body).
    pub async fn upload_batch_file<S, E>(
        &self,
        principal: Principal,
        request_id: Uuid,
        content_type: &str,
        body: S,
    ) -> JobResult<FileRow>
    where
        S: futures_util::Stream<Item = Result<Bytes, E>> + Unpin + Send,
    {
        let boundary = crate::protocols::audio::multipart::boundary(content_type)?;
        // The file plus form framing.
        let mut form =
            StreamingForm::new(body, &boundary, self.limits.batch_file_bytes + 64 * 1024);
        let purpose = form
            .next_part()
            .await?
            .ok_or(InferenceError::InvalidRequest)?;
        if purpose.name != "purpose" || purpose.has_filename {
            return Err(InferenceError::InvalidRequest.into());
        }
        match form.text(64).await?.as_str() {
            "batch" => {}
            // Other purposes are representable but not served here.
            "assistants" | "fine-tune" | "vision" | "user_data" | "evals" => {
                return Err(InferenceError::Unsupported.into());
            }
            _ => return Err(InferenceError::InvalidRequest.into()),
        }
        let file = form
            .next_part()
            .await?
            .ok_or(InferenceError::InvalidRequest)?;
        if file.name.starts_with("expires_after") {
            return Err(InferenceError::Unsupported.into());
        }
        if file.name != "file" || !file.has_filename {
            return Err(InferenceError::InvalidRequest.into());
        }
        // Read up to the first complete line to pick the deployment.
        let mut lines = Lines::default();
        let mut queue: std::collections::VecDeque<Vec<u8>> = Default::default();
        let mut content_bytes = 0u64;
        let mut part_open = true;
        while queue.is_empty() && part_open {
            match form.chunk().await? {
                Some(data) => {
                    content_bytes += data.len() as u64;
                    queue.extend(lines.push(&data)?);
                }
                None => {
                    part_open = false;
                    queue.extend(lines.finish());
                }
            }
        }
        let first = queue.pop_front().ok_or(InferenceError::InvalidRequest)?;
        let first = validate_line(&first, None)?;
        let model = first.model.clone();
        let (target, adapter) = self
            .select(
                &principal,
                &model,
                ApiProtocol::Batches,
                request_id,
                |_, _| true,
            )
            .await?;
        let upstream_model = target.upstream_model.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, InferenceError>>(4);
        let content: ByteStream = Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }));
        let store = self.store.clone();
        let limit = self.limits.batch_file_bytes;
        let deployment = target.id;
        let line_model = model.clone();
        let produce = async move {
            let mut stats = LineStats::default();
            let accept = |line: super::upload::Line,
                          stats: &mut LineStats|
             -> Result<Vec<u8>, InferenceError> {
                stats.requests = stats
                    .requests
                    .checked_add(1)
                    .filter(|n| *n <= BATCH_MAX_REQUESTS)
                    .ok_or(InferenceError::InvalidRequest)?;
                stats.output_tokens += u64::from(line.max_output_tokens);
                stats.max_line_output = stats.max_line_output.max(line.max_output_tokens);
                line.encode(&upstream_model)
            };
            let send = |bytes: Vec<u8>| {
                let tx = tx.clone();
                async move {
                    tx.send(Ok(Bytes::from(bytes)))
                        .await
                        .map_err(|_| InferenceError::UpstreamUnavailable)
                }
            };
            let result: Result<LineStats, InferenceError> = async {
                send(accept(first, &mut stats)?).await?;
                loop {
                    while let Some(raw) = queue.pop_front() {
                        // Blank lines are not JSONL requests.
                        let line = validate_line(&raw, Some(&line_model))?;
                        send(accept(line, &mut stats)?).await?;
                    }
                    if !part_open {
                        break;
                    }
                    match form.chunk().await? {
                        Some(data) => {
                            content_bytes += data.len() as u64;
                            if content_bytes > limit {
                                return Err(InferenceError::InvalidRequest);
                            }
                            queue.extend(lines.push(&data)?);
                        }
                        None => {
                            part_open = false;
                            queue.extend(lines.finish());
                        }
                    }
                }
                // Exactly `purpose` and `file`.
                if form.next_part().await?.is_some() {
                    return Err(InferenceError::InvalidRequest);
                }
                // The whole file must be priced and bounded before completing.
                let bound = crate::governance::jobs::batch_bound_preview(
                    &store,
                    deployment,
                    stats.requests,
                    stats.output_tokens,
                    stats.max_line_output,
                )
                .await?;
                if bound.is_none() {
                    return Err(InferenceError::Configuration);
                }
                Ok(stats)
            }
            .await;
            if let Err(e) = &result {
                // Abort the upstream body: the provider never sees a full form.
                let _ = tx.send(Err(*e)).await;
            }
            drop(tx);
            result.map(|s| (s, content_bytes))
        };
        let upload =
            tokio::time::timeout(UPLOAD_TIMEOUT, adapter.upload_batch_file(&target, content));
        let (uploaded, produced) = tokio::join!(upload, produce);
        // A validation failure is the client's answer; otherwise the upload's.
        let (stats, bytes) = match produced {
            Ok(v) => v,
            // The upload ended first (a send failed): report its error.
            Err(InferenceError::UpstreamUnavailable) => {
                return Err(match uploaded {
                    Ok(Err(e)) => e,
                    Err(_) => InferenceError::Timeout,
                    Ok(Ok(_)) => InferenceError::UpstreamUnavailable,
                }
                .into());
            }
            Err(e) => return Err(e.into()),
        };
        let file = uploaded.map_err(|_| InferenceError::Timeout)??;
        Ok(store::insert_input_file(
            &self.store,
            &store::NewInputFile {
                id: Uuid::new_v4(),
                workspace_id: principal.workspace_id,
                api_key_id: principal.key_id,
                deployment_id: target.id,
                public_model: &model,
                upstream_id: &file.id,
                bytes: file.bytes.unwrap_or(bytes),
                request_count: stats.requests,
                output_token_sum: stats.output_tokens,
                max_line_output: stats.max_line_output,
            },
        )
        .await?)
    }

    pub async fn get_file(&self, principal: &Principal, id: &str) -> JobResult<FileRow> {
        let id = parse_client_id(FILE_PREFIX, id).ok_or(JobError::NotFound)?;
        store::file(&self.store, principal.workspace_id, id)
            .await?
            .ok_or(JobError::NotFound)
    }

    /// Stream a workspace-owned batch file from the provider (never stored).
    pub async fn file_content(&self, principal: &Principal, id: &str) -> JobResult<ContentStream> {
        let f = self.get_file(principal, id).await?;
        let target = store::target(&self.store, f.deployment_id)
            .await?
            .ok_or(InferenceError::ModelUnavailable)?;
        let adapter = self
            .registry
            .get(&target.provider)
            .ok_or(InferenceError::Configuration)?;
        let upstream = UpstreamId::parse(&f.upstream_id).ok_or(InferenceError::Storage)?;
        Ok(call(adapter.file_content(&target, &upstream)).await?)
    }

    /// `POST /v1/batches`: one attempt reserving the file's whole ceiling.
    pub async fn create_batch(
        &self,
        principal: Principal,
        request_id: Uuid,
        input_file_id: &str,
        metadata: Option<Map<String, Value>>,
    ) -> JobResult<(JobRow, UpstreamBatch)> {
        if metadata.as_ref().is_some_and(|m| !valid_metadata(m)) {
            return Err(InferenceError::InvalidRequest.into());
        }
        let file = self.get_file(&principal, input_file_id).await?;
        if file.purpose != "batch" {
            return Err(InferenceError::InvalidRequest.into());
        }
        let (Some(requests), Some(output), Some(max_line)) = (
            file.request_count.and_then(|n| u32::try_from(n).ok()),
            file.output_token_sum.and_then(|n| u64::try_from(n).ok()),
            file.max_line_output.and_then(|n| u32::try_from(n).ok()),
        ) else {
            return Err(InferenceError::Storage.into());
        };
        if !store::claim_file(&self.store, principal.workspace_id, file.id, request_id).await? {
            return Err(JobError::Conflict(
                "file_already_used",
                "This input file already started a batch; upload it again",
            ));
        }
        let release = || store::release_file(&self.store, file.id, request_id);
        let selected = async {
            let target = store::target(&self.store, file.deployment_id)
                .await
                .map_err(|_| InferenceError::Storage)?
                .filter(|d| d.supported_protocols.iter().any(|p| p == "batches"))
                .ok_or(InferenceError::ModelUnavailable)?;
            let adapter = self
                .registry
                .get(&target.provider)
                .filter(|a| a.supports_protocol(ApiProtocol::Batches))
                .ok_or(InferenceError::Unsupported)?;
            let admission = WorkloadAdmission {
                kind: WorkloadKind::Batches,
                output: OutputReservation::Batch {
                    requests,
                    output_tokens: output,
                    max_line_output: max_line,
                },
                unit_ceilings: unit_ceilings(u64::from(requests), 0),
            };
            let attempt = self
                .admit(
                    principal,
                    request_id,
                    &file.public_model,
                    &target,
                    &admission,
                )
                .await?;
            Ok::<_, InferenceError>((target, adapter, attempt))
        }
        .await;
        let (target, adapter, attempt) = match selected {
            Ok(v) => v,
            Err(e) => {
                // Nothing was sent upstream: the file may be used again.
                release().await?;
                return Err(e.into());
            }
        };
        let upstream_file = UpstreamId::parse(&file.upstream_id).ok_or(InferenceError::Storage)?;
        let upstream = match call(adapter.create_batch(&target, &upstream_file, metadata)).await {
            Ok(b) => b,
            Err(e) => return Err(attempt.fail(e).await.into()),
        };
        attempt.accepted();
        let deadline = Utc::now() + BATCH_POLL_WINDOW;
        let job = store::insert_job(
            &self.store,
            &store::NewJob {
                id: Uuid::new_v4(),
                kind: JobKind::Batch,
                workspace_id: principal.workspace_id,
                api_key_id: principal.key_id,
                deployment_id: target.id,
                execution_id: request_id,
                public_model: &file.public_model,
                provider: &target.provider,
                upstream_id: &upstream.id,
                poll_deadline_at: deadline,
                video_seconds: None,
                video_size: None,
                batch_endpoint: Some(BATCH_ENDPOINT),
            },
        )
        .await
        .inspect_err(|_| {
            tracing::error!(execution_id = %request_id, "batch job record failed after upstream acceptance; hold kept until reconciliation");
        })?;
        store::attach_file(&self.store, file.id, job.id).await?;
        self.hold_until(request_id, deadline).await;
        let job = self.apply_batch(&job, &upstream, false).await?;
        Ok((job, upstream))
    }

    /// Record an observation (and output/error files); settle a terminal
    /// one. `scan` allows reading the output file for usage when the
    /// provider reports no aggregate.
    pub(crate) async fn apply_batch(
        &self,
        job: &JobRow,
        upstream: &UpstreamBatch,
        scan: bool,
    ) -> JobResult<JobRow> {
        let interval = self.poll_seconds();
        let job = store::observe(&self.store, job, &observation(upstream), interval).await?;
        if let Some(f) = &upstream.output_file {
            store::output_file(&self.store, &job, "batch_output", f).await?;
        }
        if let Some(f) = &upstream.error_file {
            store::output_file(&self.store, &job, "batch_error", f).await?;
        }
        let state = job.job_state();
        if !state.is_terminal() || job.settled_at.is_some() {
            return Ok(job);
        }
        let usage = match (upstream.usage, state, &upstream.output_file) {
            (Some(u), _, _) => Some(u),
            (None, JobState::Completed, Some(file)) => {
                if !scan {
                    // Leave the (possibly large) output scan to the poller.
                    return Ok(job);
                }
                self.output_usage(&job, file, upstream.counts).await?
            }
            // Completed without output (nothing succeeded) is still unknown
            // unless the provider reports usage.
            _ => None,
        };
        if let Some(s) = settlement(state, usage) {
            self.settle(&job, s).await?;
        }
        Ok(store::reload(&self.store, job.id).await?)
    }

    /// Per-line usage summed from the output file (bodies discarded). The
    /// line count must match the provider's completed count.
    async fn output_usage(
        &self,
        job: &JobRow,
        file: &UpstreamId,
        counts: Option<RequestCounts>,
    ) -> JobResult<Option<Usage>> {
        let (target, adapter) = self.target(job).await?;
        let parsed = tokio::time::timeout(
            UPLOAD_TIMEOUT,
            adapter.batch_output_usage(&target, file, self.limits.output_scan_bytes),
        )
        .await
        .map_err(|_| InferenceError::Timeout)??;
        Ok(parsed
            .usage
            .filter(|_| counts.is_some_and(|c| u64::from(c.completed) == parsed.lines)))
    }

    async fn own_batch(&self, principal: &Principal, id: &str) -> JobResult<JobRow> {
        let id = parse_client_id(JobKind::Batch.prefix(), id).ok_or(JobError::NotFound)?;
        store::job(&self.store, principal.workspace_id, JobKind::Batch, id)
            .await?
            .ok_or(JobError::NotFound)
    }

    pub(crate) async fn refresh_batch(
        &self,
        job: JobRow,
        scan: bool,
    ) -> JobResult<(JobRow, UpstreamBatch)> {
        let (target, adapter) = self.target(&job).await?;
        let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
        let upstream = call(adapter.retrieve_batch(&target, &upstream_id)).await?;
        let job = self.apply_batch(&job, &upstream, scan).await?;
        Ok((job, upstream))
    }

    pub async fn get_batch(
        &self,
        principal: &Principal,
        id: &str,
    ) -> JobResult<(JobRow, Option<UpstreamBatch>)> {
        let job = self.own_batch(principal, id).await?;
        if job.job_state().is_terminal() && job.settled_at.is_some() {
            return Ok((job, None));
        }
        // Without a poller a client read must be able to settle fully.
        let scan = self.limits.poll_interval.is_none();
        let (job, upstream) = self.refresh_batch(job, scan).await?;
        Ok((job, Some(upstream)))
    }

    pub async fn cancel_batch(
        &self,
        principal: &Principal,
        id: &str,
    ) -> JobResult<(JobRow, UpstreamBatch)> {
        let job = self.own_batch(principal, id).await?;
        if job.job_state().is_terminal() {
            return Err(JobError::Conflict(
                "batch_not_cancellable",
                "This batch has already finished",
            ));
        }
        let (target, adapter) = self.target(&job).await?;
        let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
        let upstream = call(adapter.cancel_batch(&target, &upstream_id)).await?;
        store::mark_cancel_requested(&self.store, job.id).await?;
        let job = store::reload(&self.store, job.id).await?;
        let job = self.apply_batch(&job, &upstream, false).await?;
        Ok((job, upstream))
    }

    pub async fn list_batches(
        &self,
        principal: &Principal,
        after: Option<&str>,
        limit: i64,
    ) -> JobResult<Vec<(JobRow, (Option<Uuid>, Option<Uuid>, Option<Uuid>))>> {
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
            let files = store::job_files(&self.store, row.id).await?;
            out.push((row, files));
        }
        Ok(out)
    }

    pub async fn batch_files(
        &self,
        job: &JobRow,
    ) -> JobResult<(Option<Uuid>, Option<Uuid>, Option<Uuid>)> {
        Ok(store::job_files(&self.store, job.id).await?)
    }
}
