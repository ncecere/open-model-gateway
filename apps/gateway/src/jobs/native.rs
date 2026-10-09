//! Native batches (0021): one upstream attempt on a provider batch API,
//! driven by the poller.
//!
//! 1. **Submit** (once): the batch's private copy is streamed line by line,
//!    each line encoded by the adapter (`l<n>` ids, upstream model) and sent
//!    as one batch. The claim (`submit_started_at`) is exclusive with an
//!    early cancel; an interrupted submission is never repeated (it may have
//!    reached the provider): it fails with the hold retained.
//! 2. **Poll**: state, counts and a pending cancel are applied.
//! 3. **Collect** (terminal upstream state): results are decoded into the
//!    client's endpoint shape and written to gateway `batch_output` files,
//!    lines without a result go to the error file, usage is summed per line
//!    (the provider's aggregate when it reports one) and the reservation
//!    settles at its pinned price list. Then the provider's copies and the
//!    private copy are deleted.
use super::{
    batch::settlement,
    io::{LazySink, LineReader, RESULT_LINE_BYTES, Source},
    lines::{self, NotRun},
    store::Finished,
    *,
};
use chrono::Utc;

/// Longer than one submission may take; afterwards an unfinished submission
/// is treated as interrupted.
const SUBMIT_GRACE: TimeDelta = TimeDelta::minutes(40);

impl Jobs {
    /// One poller step for a native engine batch.
    pub(crate) async fn poll_native(&self, job: JobRow) -> JobResult<()> {
        if job.upstream_id.is_none() {
            return match job.submit_started_at {
                None if job.cancel_requested_at.is_none() => self.submit_native(job).await,
                None => Ok(()),
                Some(at) if Utc::now() - at > SUBMIT_GRACE => self.submission_lost(job).await,
                Some(_) => Ok(()),
            };
        }
        let (target, adapter) = self.target(&job).await?;
        let upstream_id = job.upstream().ok_or(InferenceError::Storage)?;
        let mut upstream = call(adapter.retrieve_batch(&target, &upstream_id)).await?;
        if job.cancel_requested_at.is_some()
            && !upstream.status.state().is_terminal()
            && upstream.status != BatchStatus::Cancelling
        {
            upstream = call(adapter.cancel_batch(&target, &upstream_id)).await?;
        }
        if let Some(counts) = upstream.counts {
            store::observe_counts(&self.store, job.id, counts).await?;
        }
        let state = upstream.status.state();
        if !state.is_terminal() {
            if state == JobState::InProgress {
                store::mark_running(&self.store, job.id).await?;
            }
            store::set_status(&self.store, job.id, upstream.status.as_str()).await?;
            store::poll_ok(&self.store, job.id, self.poll_seconds()).await?;
            return Ok(());
        }
        self.collect_native(job, &target, adapter.as_ref(), &upstream)
            .await
    }

    async fn submit_native(&self, job: JobRow) -> JobResult<()> {
        if !store::claim_submit(&self.store, job.id).await? {
            return Ok(());
        }
        let files = self.files()?.clone();
        let endpoint = job.endpoint().ok_or(InferenceError::Storage)?;
        let work = job.work_file_id.ok_or(InferenceError::Storage)?;
        let (target, adapter) = self.target(&job).await?;
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<axum::body::Bytes, InferenceError>>(8);
        let records: ByteStream = Box::pin(async_stream::stream! {
            while let Some(item) = rx.recv().await {
                yield item;
            }
        });
        let encoder = {
            let adapter = adapter.clone();
            let target = target.clone();
            let workspace = job.workspace_id;
            async move {
                let produced = async {
                    let mut reader =
                        LineReader::stored(&files, work, workspace, BATCH_MAX_LINE_BYTES)
                            .await
                            .map_err(|_| InferenceError::Storage)?;
                    let mut n = 0u32;
                    while let Some(raw) = reader.next_request().await? {
                        let line = lines::parse_line(&raw, endpoint)
                            .map_err(|_| InferenceError::InvalidRequest)?;
                        let record = adapter.encode_native_line(
                            &target,
                            endpoint,
                            &format!("l{n}"),
                            &line.request,
                        )?;
                        tx.send(Ok(record.into()))
                            .await
                            .map_err(|_| InferenceError::UpstreamUnavailable)?;
                        n += 1;
                    }
                    Ok::<_, InferenceError>(n)
                }
                .await;
                if let Err(e) = &produced {
                    // Abort the upstream body: the provider never gets a full batch.
                    let _ = tx.send(Err(*e)).await;
                }
                produced
            }
        };
        let submit = tokio::time::timeout(
            UPLOAD_TIMEOUT,
            adapter.submit_native_batch(&target, endpoint, records),
        );
        let (submitted, produced) = tokio::join!(submit, encoder);
        let outcome = match (submitted, produced) {
            (Ok(Ok(upstream)), Ok(_)) => Ok(upstream),
            (Ok(Err(e)), _) => Err(e),
            (Err(_), _) => Err(InferenceError::Timeout),
            (Ok(Ok(_)), Err(e)) => Err(e),
        };
        match outcome {
            Ok(upstream) => {
                store::set_upstream(&self.store, job.id, &upstream.id).await?;
                if !upstream.status.state().is_terminal() {
                    store::set_status(&self.store, job.id, upstream.status.as_str()).await?;
                }
                crate::metrics::METRICS.observe_batch_submitted(&job.provider);
                Ok(())
            }
            Err(e) => {
                tracing::warn!(job_id = %job.id, "native batch submission failed");
                let job = store::finish_batch(
                    &self.store,
                    job.id,
                    &Finished {
                        state: JobState::Failed,
                        error_code: Some("submission_failed"),
                        output_file: None,
                        error_file: None,
                        counts: None,
                    },
                )
                .await?;
                // The provider may have received part of it: unknown unless free.
                self.settle(
                    &job,
                    Settlement {
                        outcome: Outcome::Failed,
                        error: Some(e),
                        usage: Usage::default(),
                    },
                )
                .await?;
                self.discard_work_file(&job).await;
                crate::metrics::METRICS.observe_batch_finished("native", JobState::Failed.as_str());
                Ok(())
            }
        }
    }

    /// A submission that never recorded its upstream id (crash): never
    /// retried; the batch fails and its hold is retained as unknown.
    async fn submission_lost(&self, job: JobRow) -> JobResult<()> {
        let job = store::finish_batch(
            &self.store,
            job.id,
            &Finished {
                state: JobState::Failed,
                error_code: Some("submission_interrupted"),
                output_file: None,
                error_file: None,
                counts: None,
            },
        )
        .await?;
        self.settle(
            &job,
            Settlement {
                outcome: Outcome::Cancelled,
                error: None,
                usage: Usage::default(),
            },
        )
        .await?;
        self.discard_work_file(&job).await;
        Ok(())
    }

    /// The client `custom_id`s of the batch, in line order.
    async fn custom_ids(&self, job: &JobRow) -> JobResult<Vec<String>> {
        let files = self.files()?;
        let work = job.work_file_id.ok_or(InferenceError::Storage)?;
        let mut reader = LineReader::stored(files, work, job.workspace_id, BATCH_MAX_LINE_BYTES)
            .await
            .map_err(|_| InferenceError::Storage)?;
        let mut ids = Vec::new();
        while let Some(raw) = reader.next_request().await? {
            ids.push(lines::custom_id(&raw).ok_or(InferenceError::Storage)?);
        }
        Ok(ids)
    }

    async fn collect_native(
        &self,
        job: JobRow,
        target: &Deployment,
        adapter: &dyn ProviderAdapter,
        upstream: &UpstreamBatch,
    ) -> JobResult<()> {
        let files = self.files()?.clone();
        let endpoint = job.endpoint().ok_or(InferenceError::Storage)?;
        store::set_status(&self.store, job.id, "finalizing").await?;
        let ids = self.custom_ids(&job).await?;
        let hex = job.id.simple();
        let mut out = LazySink::new(
            &files,
            io::batch_file(
                false,
                job.workspace_id,
                job.api_key_id,
                job.user_id,
                format!("batch_{hex}_output.jsonl"),
            ),
        );
        let mut err = LazySink::new(
            &files,
            io::batch_file(
                false,
                job.workspace_id,
                job.api_key_id,
                job.user_id,
                format!("batch_{hex}_error.jsonl"),
            ),
        );
        let state = upstream.status.state();
        let written = async {
            let results = call(adapter.native_batch_results(target, upstream)).await?;
            let mut reader = LineReader::new(Source::Upstream(results), RESULT_LINE_BYTES);
            let mut seen = vec![false; ids.len()];
            let mut usage: Option<Usage> = None;
            let mut usage_known = true;
            let mut succeeded = 0u32;
            while let Some(raw) = reader.next_request().await? {
                let Ok(result) = adapter.decode_native_result(target, endpoint, &raw) else {
                    usage_known = false;
                    continue;
                };
                let index = result
                    .custom_id
                    .strip_prefix('l')
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|n| *n < ids.len() && !seen[*n]);
                let Some(index) = index else {
                    usage_known = false;
                    continue;
                };
                seen[index] = true;
                let custom_id = &ids[index];
                match result.outcome {
                    NativeOutcome::Succeeded(response) => {
                        succeeded += 1;
                        let u = response.usage();
                        if u.input_tokens.is_none() || u.output_tokens.is_none() {
                            usage_known = false;
                        }
                        usage = Some(usage.map_or(u, |a| add_usage(a, u)));
                        let rid = Uuid::new_v4();
                        match lines::response_body(endpoint, rid, &job.public_model, *response) {
                            Ok(body) => {
                                out.line(&lines::success_line(custom_id, None, body))
                                    .await?
                            }
                            Err(e) => {
                                err.line(&lines::failure_line(custom_id, None, endpoint, e))
                                    .await?
                            }
                        }
                    }
                    NativeOutcome::Failed { status, code } => {
                        err.line(&lines::provider_failure_line(
                            custom_id, endpoint, status, &code,
                        ))
                        .await?
                    }
                    NativeOutcome::Cancelled => {
                        err.line(&lines::not_run_line(custom_id, NotRun::Cancelled))
                            .await?
                    }
                    NativeOutcome::Expired => {
                        err.line(&lines::not_run_line(custom_id, NotRun::Expired))
                            .await?
                    }
                }
            }
            let missing = match state {
                JobState::Cancelled => NotRun::Cancelled,
                JobState::Expired => NotRun::Expired,
                JobState::Failed => NotRun::Failed,
                _ => NotRun::ResultUnavailable,
            };
            for (index, custom_id) in ids.iter().enumerate() {
                if !seen[index] {
                    err.line(&lines::not_run_line(custom_id, missing)).await?;
                }
            }
            Ok::<_, InferenceError>((usage.filter(|_| usage_known), succeeded))
        }
        .await;
        let (summed, succeeded) = match written {
            Ok(v) => v,
            Err(e) => {
                out.abort().await;
                err.abort().await;
                return Err(e.into());
            }
        };
        let output_file = out.finish().await?;
        let error_file = err.finish().await?;
        let total = ids.len() as u32;
        let counts = RequestCounts {
            total,
            completed: succeeded,
            failed: upstream
                .counts
                .map_or(total - succeeded.min(total), |c| c.failed),
        };
        crate::metrics::METRICS.observe_batch_lines(
            "native",
            &job.provider,
            u64::from(succeeded),
            u64::from(counts.failed),
        );
        let job = store::finish_batch(
            &self.store,
            job.id,
            &Finished {
                state,
                error_code: (state == JobState::Failed).then_some("batch_failed"),
                output_file,
                error_file,
                counts: Some(counts),
            },
        )
        .await?;
        // The provider's aggregate when it reports one, else the per-line sum
        // when every succeeded line reported usage; otherwise unknown.
        let usage = upstream.usage.or(summed);
        if let Some(s) = settlement(state, usage) {
            self.settle(&job, s).await?;
        }
        if call(adapter.delete_native_batch(target, upstream))
            .await
            .is_err()
        {
            tracing::warn!(job_id = %job.id, "provider batch copies not deleted");
        }
        self.discard_work_file(&job).await;
        crate::metrics::METRICS.observe_batch_finished("native", state.as_str());
        Ok(())
    }
}
