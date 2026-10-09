//! Gateway-run batches (0021): each line executes through the normal
//! inference engine (routing, adapters, deadlines, explicit failover policy)
//! with its own durable execution and reservation, transferred out of the
//! batch's ceiling (`governance::batch::admit_line`).
//!
//! - **Workers:** one runner per process claims batches with a renewable
//!   lease (a crashed runner's batches resume elsewhere). Lines run on a
//!   bounded pool (`GATEWAY_BATCH_WORKERS`, default 4) with a per-batch cap
//!   (`GATEWAY_BATCH_CONCURRENCY`, default 2).
//! - **Exactly once:** a line is claimed by inserting its `batch_lines` row
//!   before it runs. A line a crashed runner left `running` becomes
//!   `interrupted` and is never executed again (its result is reported as
//!   unavailable; its hold is reconciled as unknown).
//! - **No implicit retries:** a failed line is recorded failed. A batch may
//!   opt into up to two retries of retryable failures (`omg_retries`), each a
//!   new attempt with its own reservation. Upstream rate limiting (429)
//!   pauses the batch's dispatch with backoff; nothing is resent.
//! - **Stops:** cancel (running lines finish, `cancelled`), the 24 h window
//!   (`expired`) and budget exhaustion (`failed`, `budget_exceeded`). Lines
//!   that never ran are listed in the error file.
//! - **Results:** finished lines are written to encrypted segments
//!   (internal files) as they complete, then merged into the `batch_output`
//!   output and error files. Bodies are never stored in PostgreSQL.
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc};

use super::{
    batch::BATCH_WINDOW,
    io::{LazySink, LineReader, RESULT_LINE_BYTES},
    lines::{self, NotRun},
    store::Finished,
    *,
};
use crate::{
    governance::batch::LineContext,
    inference::{
        repository::AttemptTelemetry,
        types::{ChatRequest, EmbeddingRequest, ProviderOutput},
    },
};

/// Runner lease of a claimed batch, renewed every [`RENEW`].
const LEASE_SECONDS: f64 = 60.0;
const RENEW: Duration = Duration::from_secs(15);
/// How often the runner looks for runnable batches.
const TICK: Duration = Duration::from_secs(2);
/// Batches one process runs at once.
const MAX_ACTIVE: usize = 16;
/// Segment flush thresholds.
const SEGMENT_LINES: usize = 256;
const SEGMENT_BYTES: usize = 8 * 1024 * 1024;
const SEGMENT_AGE: Duration = Duration::from_secs(10);

/// Start the runner when workers are configured and the file store and
/// engine are attached.
pub fn start(jobs: Jobs) -> Option<tokio::task::JoinHandle<()>> {
    if jobs.limits.batch_workers == 0 || jobs.files.is_none() || jobs.engine.is_none() {
        return None;
    }
    let runner = Runner::new(jobs);
    Some(tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if runner.tick().await.is_err() {
                tracing::warn!("batch runner tick incomplete; retrying");
            }
        }
    }))
}

/// One process's gateway-run batch runner.
#[derive(Clone)]
pub struct Runner {
    jobs: Jobs,
    id: Uuid,
    pool: Arc<Semaphore>,
    active: Arc<Mutex<HashSet<Uuid>>>,
}

impl Runner {
    pub fn new(jobs: Jobs) -> Self {
        let workers = jobs.limits.batch_workers.max(1);
        crate::metrics::METRICS.set_batch_workers(workers as u64, 0);
        Self {
            jobs,
            id: Uuid::new_v4(),
            pool: Arc::new(Semaphore::new(workers)),
            active: Arc::default(),
        }
    }

    /// Claim runnable batches and start them in the background. Returns how
    /// many were started.
    pub async fn tick(&self) -> JobResult<usize> {
        self.observe_queue().await;
        let room = MAX_ACTIVE.saturating_sub(self.active.lock().expect("runner set").len());
        if room == 0 {
            return Ok(0);
        }
        let claimed =
            store::claim_gateway(&self.jobs.store, self.id, LEASE_SECONDS, room as i64).await?;
        let started = claimed.len();
        for job in claimed {
            if !self.active.lock().expect("runner set").insert(job.id) {
                continue;
            }
            let runner = self.clone();
            tokio::spawn(async move {
                let id = job.id;
                if runner.run(job).await.is_err() {
                    tracing::warn!(job_id = %id, "gateway-run batch interrupted; it resumes after its lease");
                }
                runner.active.lock().expect("runner set").remove(&id);
            });
        }
        Ok(started)
    }

    async fn observe_queue(&self) {
        let rows: Result<Vec<(String, i64)>, _> = sqlx::query_as("SELECT coalesce(batch_mode,'native'),count(*) FROM async_jobs WHERE kind='batch' AND settled_at IS NULL GROUP BY 1")
            .fetch_all(&self.jobs.store.pool)
            .await;
        if let Ok(rows) = rows {
            for mode in ["native", "gateway"] {
                let n = rows.iter().find(|r| r.0 == mode).map_or(0, |r| r.1);
                crate::metrics::METRICS.set_batch_queue(mode, n.max(0) as u64);
            }
        }
        let capacity = self.jobs.limits.batch_workers.max(1);
        crate::metrics::METRICS.set_batch_workers(
            capacity as u64,
            capacity.saturating_sub(self.pool.available_permits()) as u64,
        );
    }

    /// Claim and run one batch to completion in the foreground (tests and
    /// operator tooling). `false` when nothing was runnable.
    pub async fn run_next(&self) -> JobResult<bool> {
        let Some(job) = store::claim_gateway(&self.jobs.store, self.id, LEASE_SECONDS, 1)
            .await?
            .pop()
        else {
            return Ok(false);
        };
        self.run(job).await?;
        Ok(true)
    }

    /// Run a claimed batch: resume, dispatch remaining lines, finalize.
    async fn run(&self, job: JobRow) -> JobResult<()> {
        let files = self.jobs.files()?.clone();
        let engine = self
            .jobs
            .engine
            .clone()
            .ok_or(InferenceError::Configuration)?;
        let endpoint = job.endpoint().ok_or(InferenceError::Storage)?;
        let work = job.work_file_id.ok_or(InferenceError::Storage)?;
        let state = Arc::new(RunState::default());
        // Lease renewal (and cancel observation) while the batch runs.
        let renewal = {
            let store = self.jobs.store.clone();
            let state = state.clone();
            let (id, runner) = (job.id, self.id);
            tokio::spawn(async move {
                loop {
                    match store::renew_lease(&store, id, runner, LEASE_SECONDS).await {
                        Ok(Some(cancel)) => {
                            if cancel {
                                state.cancel.store(true, Ordering::SeqCst);
                            }
                        }
                        Ok(None) => {
                            state.lost.store(true, Ordering::SeqCst);
                            return;
                        }
                        Err(_) => {}
                    }
                    tokio::time::sleep(RENEW).await;
                }
            })
        };
        let result = self
            .run_claimed(&job, &files, engine, endpoint, work, state)
            .await;
        renewal.abort();
        if result.is_err() {
            let _ = store::release_runner(&self.jobs.store, job.id, self.id).await;
        }
        result
    }

    async fn run_claimed(
        &self,
        job: &JobRow,
        files: &crate::filestore::FileStorage,
        engine: Engine,
        endpoint: BatchEndpoint,
        work: Uuid,
        state: Arc<RunState>,
    ) -> JobResult<()> {
        let store = &self.jobs.store;
        if job.job_state().is_terminal() {
            // A runner stopped after recording the results but before
            // settling: only the settlement and cleanup remain.
            return self.settle_finished(job, files).await;
        }
        if job.cancel_requested_at.is_some() {
            state.cancel.store(true, Ordering::SeqCst);
        }
        // Resume: lines a crashed runner left running are never re-executed.
        let interrupted = store::interrupt_running(store, job.id).await?;
        if interrupted > 0 {
            store::progress(store, job.id, 0, interrupted).await?;
        }
        store::mark_running(store, job.id).await?;
        let claimed: HashSet<i32> = store::line_states(store, job.id)
            .await?
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        let repository = Arc::new(LineRepository {
            store: store.clone(),
            batch: LineContext {
                envelope: job.execution_id,
                job: job.id,
            },
        });
        let line = Arc::new(LineRun {
            store: store.clone(),
            engine: engine.with_repository(repository, self.jobs.limits.batch_concurrency),
            job: job.clone(),
            endpoint,
            state: state.clone(),
        });
        let (records, receiver) = mpsc::channel::<Record>(64);
        let writer = tokio::spawn(write_segments(
            store.clone(),
            files.clone(),
            job.clone(),
            receiver,
        ));
        let per_batch = Arc::new(Semaphore::new(self.jobs.limits.batch_concurrency));
        let mut tasks = tokio::task::JoinSet::new();
        let dispatched = async {
            let mut reader =
                LineReader::stored(files, work, job.workspace_id, BATCH_MAX_LINE_BYTES)
                    .await
                    .map_err(|_| InferenceError::Storage)?;
            let mut n: i32 = -1;
            while let Some(raw) = reader.next_request().await? {
                n += 1;
                if claimed.contains(&n) {
                    continue;
                }
                if state.should_stop(job) {
                    break;
                }
                state.wait_pause().await;
                let local = per_batch
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| InferenceError::Storage)?;
                let global = self
                    .pool
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| InferenceError::Storage)?;
                if state.should_stop(job) {
                    break;
                }
                let line = line.clone();
                let records = records.clone();
                tasks.spawn(async move {
                    let _permits = (local, global);
                    line.run(n, raw, records).await;
                });
                while tasks.try_join_next().is_some() {}
            }
            Ok::<_, InferenceError>(())
        }
        .await;
        // Running lines always finish (cancel semantics); nothing is dropped.
        while tasks.join_next().await.is_some() {}
        drop(records);
        drop(line);
        let segments = writer.await.map_err(|_| InferenceError::Storage)?;
        dispatched?;
        segments?;
        if state.lost.load(Ordering::SeqCst) {
            // Another runner owns the batch now; it finalizes.
            return Ok(());
        }
        let stop = state.stop_reason(job);
        self.finalize(job, files, work, stop).await
    }

    async fn finalize(
        &self,
        job: &JobRow,
        files: &crate::filestore::FileStorage,
        work: Uuid,
        stop: Option<Stop>,
    ) -> JobResult<()> {
        let store = &self.jobs.store;
        if job.cancel_requested_at.is_none() && stop != Some(Stop::Cancelled) {
            store::set_status(store, job.id, "finalizing").await?;
        }
        let hex = job.id.simple();
        let template = |suffix: &str| {
            io::batch_file(
                false,
                job.workspace_id,
                job.api_key_id,
                job.user_id,
                format!("batch_{hex}_{suffix}.jsonl"),
            )
        };
        let mut out = LazySink::new(files, template("output"));
        let mut err = LazySink::new(files, template("error"));
        let merged = async {
            let mut written: HashSet<i32> = HashSet::new();
            let segments = store::segments(store, job.id)
                .await
                .map_err(|_| InferenceError::Storage)?;
            for (_, file) in &segments {
                let mut reader =
                    LineReader::stored(files, *file, job.workspace_id, RESULT_LINE_BYTES)
                        .await
                        .map_err(|_| InferenceError::Storage)?;
                while let Some(raw) = reader.next_request().await? {
                    let record: Value =
                        serde_json::from_slice(&raw).map_err(|_| InferenceError::Storage)?;
                    let n = record["n"].as_i64().ok_or(InferenceError::Storage)? as i32;
                    if !written.insert(n) {
                        continue;
                    }
                    if record["e"] == json!(true) {
                        err.line(&record["r"]).await?;
                    } else {
                        out.line(&record["r"]).await?;
                    }
                }
            }
            // Lines without a stored result: never run, or results lost.
            let states: std::collections::HashMap<i32, String> = store::line_states(store, job.id)
                .await
                .map_err(|_| InferenceError::Storage)?
                .into_iter()
                .map(|(n, s, _)| (n, s))
                .collect();
            let unrun = match stop {
                Some(Stop::Cancelled) => NotRun::Cancelled,
                Some(Stop::Expired) => NotRun::Expired,
                Some(Stop::Budget) => NotRun::BudgetExceeded,
                Some(Stop::Lost) | None => NotRun::Failed,
            };
            let mut reader =
                LineReader::stored(files, work, job.workspace_id, BATCH_MAX_LINE_BYTES)
                    .await
                    .map_err(|_| InferenceError::Storage)?;
            let mut n: i32 = -1;
            while let Some(raw) = reader.next_request().await? {
                n += 1;
                if written.contains(&n) {
                    continue;
                }
                let custom_id = lines::custom_id(&raw).ok_or(InferenceError::Storage)?;
                let reason = if states.contains_key(&n) {
                    NotRun::ResultUnavailable
                } else {
                    unrun
                };
                err.line(&lines::not_run_line(&custom_id, reason)).await?;
            }
            Ok::<_, InferenceError>(segments)
        }
        .await;
        let segments = match merged {
            Ok(s) => s,
            Err(e) => {
                out.abort().await;
                err.abort().await;
                return Err(e.into());
            }
        };
        let output_file = out.finish().await?;
        let error_file = err.finish().await?;
        let (completed, failed): (i64, i64) = sqlx::query_as("SELECT count(*) FILTER(WHERE state='succeeded'),count(*) FILTER(WHERE state IN('failed','interrupted')) FROM batch_lines WHERE job_id=$1")
            .bind(job.id)
            .fetch_one(&store.pool)
            .await?;
        let final_state = match stop {
            None => JobState::Completed,
            Some(Stop::Cancelled) => JobState::Cancelled,
            Some(Stop::Expired) => JobState::Expired,
            Some(Stop::Budget) | Some(Stop::Lost) => JobState::Failed,
        };
        let job = store::finish_batch(
            store,
            job.id,
            &Finished {
                state: final_state,
                error_code: (stop == Some(Stop::Budget)).then_some("budget_exceeded"),
                output_file,
                error_file,
                counts: job.request_total.map(|total| RequestCounts {
                    total: total.max(0) as u32,
                    completed: completed.max(0) as u32,
                    failed: failed.max(0) as u32,
                }),
            },
        )
        .await?;
        let _ = segments;
        self.settle_finished(&job, files).await?;
        crate::metrics::METRICS.observe_batch_finished("gateway", final_state.as_str());
        Ok(())
    }

    /// Settle a finished batch (the lease is still held, so no other runner
    /// touches it): release what no line used (lines keep their own
    /// settlement or retained unknown holds), mark it settled, then delete
    /// the segments and the private copy.
    async fn settle_finished(
        &self,
        job: &JobRow,
        files: &crate::filestore::FileStorage,
    ) -> JobResult<()> {
        let store = &self.jobs.store;
        let elapsed = (Utc::now() - job.created_at).num_milliseconds().max(0) as u64;
        if !crate::governance::batch::close_batch(store, job.execution_id, elapsed, false).await? {
            tracing::warn!(job_id = %job.id, "batch envelope no longer pending; its hold is left to reconciliation");
        }
        store::mark_settled(store, job.id).await?;
        for (_, file) in store::segments(store, job.id).await? {
            let _ = files.delete(file, Some(job.workspace_id)).await;
        }
        self.jobs.discard_work_file(job).await;
        Ok(())
    }
}

/// Why a batch stopped dispatching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Cancelled,
    Expired,
    Budget,
    /// Another runner took the lease.
    Lost,
}

#[derive(Default)]
struct RunState {
    cancel: AtomicBool,
    lost: AtomicBool,
    budget: AtomicBool,
    busy_streak: AtomicU32,
    pause_until: Mutex<Option<tokio::time::Instant>>,
}
impl RunState {
    fn stop_reason(&self, job: &JobRow) -> Option<Stop> {
        if self.lost.load(Ordering::SeqCst) {
            Some(Stop::Lost)
        } else if self.budget.load(Ordering::SeqCst) {
            Some(Stop::Budget)
        } else if self.cancel.load(Ordering::SeqCst) {
            Some(Stop::Cancelled)
        } else if Utc::now() >= job.created_at + BATCH_WINDOW {
            Some(Stop::Expired)
        } else {
            None
        }
    }
    fn should_stop(&self, job: &JobRow) -> bool {
        self.stop_reason(job).is_some()
    }
    /// Provider rate limiting: back off this batch's dispatch (1 s doubling
    /// to 60 s); a success resets it.
    fn rate_limited(&self) {
        let streak = self.busy_streak.fetch_add(1, Ordering::SeqCst).min(6);
        let wait = Duration::from_secs(1u64 << streak).min(Duration::from_secs(60));
        *self.pause_until.lock().expect("pause") = Some(tokio::time::Instant::now() + wait);
    }
    fn succeeded(&self) {
        self.busy_streak.store(0, Ordering::SeqCst);
    }
    async fn wait_pause(&self) {
        let until = *self.pause_until.lock().expect("pause");
        if let Some(until) = until {
            tokio::time::sleep_until(until).await;
        }
    }
}

/// One finished line for the segment writer.
struct Record {
    line: i32,
    error: bool,
    value: Value,
}

struct LineRun {
    store: Store,
    engine: Engine,
    job: JobRow,
    endpoint: BatchEndpoint,
    state: Arc<RunState>,
}
impl LineRun {
    async fn emit(&self, records: &mpsc::Sender<Record>, line: i32, error: bool, value: Value) {
        // A closed writer means the batch is stopping; the line is then
        // reported as unavailable at finalization.
        let _ = records.send(Record { line, error, value }).await;
    }
    async fn finished(&self, line: i32, ok: bool, status: Option<u16>, code: Option<&str>) {
        if store::finish_line(&self.store, self.job.id, line, ok, status, code)
            .await
            .is_err()
        {
            tracing::warn!(job_id = %self.job.id, "batch line state not recorded");
        }
        let (c, f) = if ok { (1, 0) } else { (0, 1) };
        let _ = store::progress(&self.store, self.job.id, c, f).await;
        crate::metrics::METRICS.observe_batch_lines(
            "gateway",
            &self.job.provider,
            c as u64,
            f as u64,
        );
    }

    async fn run(&self, n: i32, raw: Vec<u8>, records: mpsc::Sender<Record>) {
        let mut execution = Uuid::new_v4();
        match store::claim_line(
            &self.store,
            self.job.id,
            self.job.workspace_id,
            n,
            execution,
        )
        .await
        {
            Ok(true) => {}
            // Already claimed elsewhere, or not recorded: never run it.
            _ => return,
        }
        let line = match lines::parse_line(&raw, self.endpoint) {
            Ok(l) => l,
            Err(_) => {
                let custom_id = lines::custom_id(&raw).unwrap_or_default();
                let error = InferenceError::InvalidRequest;
                self.finished(n, false, Some(400), Some("invalid_request_error"))
                    .await;
                let value = lines::failure_line(&custom_id, Some(execution), self.endpoint, error);
                self.emit(&records, n, true, value).await;
                return;
            }
        };
        let model = line.request.model().to_owned();
        let mut attempt: i16 = 1;
        loop {
            let result = self.execute(line.request.clone(), execution).await;
            match result {
                Ok(response) => {
                    self.state.succeeded();
                    match lines::response_body(self.endpoint, execution, &model, response) {
                        Ok(body) => {
                            self.finished(n, true, Some(200), None).await;
                            let value = lines::success_line(&line.custom_id, Some(execution), body);
                            self.emit(&records, n, false, value).await;
                        }
                        Err(e) => self.fail(n, &line.custom_id, execution, e, &records).await,
                    }
                    return;
                }
                Err(e) if e.is_budget_denial() => {
                    // Admission refused: the line never ran. The batch stops.
                    self.state.budget.store(true, Ordering::SeqCst);
                    self.finished(n, false, None, Some("budget_exceeded")).await;
                    let value = lines::not_run_line(&line.custom_id, NotRun::BudgetExceeded);
                    self.emit(&records, n, true, value).await;
                    return;
                }
                Err(e) => {
                    if e == InferenceError::Busy {
                        self.state.rate_limited();
                    }
                    let retryable = matches!(
                        e,
                        InferenceError::Busy
                            | InferenceError::UpstreamUnavailable
                            | InferenceError::Timeout
                    );
                    if retryable
                        && attempt <= self.job.retry_limit
                        && !self.state.should_stop(&self.job)
                    {
                        // Explicit retry policy only: a new attempt with its
                        // own execution and reservation.
                        let (status, _) = crate::protocols::batch_line_error(false, e);
                        let _ = store::finish_line(
                            &self.store,
                            self.job.id,
                            n,
                            false,
                            Some(status),
                            Some(e.code()),
                        )
                        .await;
                        tokio::time::sleep(Duration::from_secs(1u64 << attempt)).await;
                        let next = Uuid::new_v4();
                        if matches!(
                            store::retry_line(&self.store, self.job.id, n, attempt, next).await,
                            Ok(true)
                        ) {
                            attempt += 1;
                            execution = next;
                            continue;
                        }
                        // Could not claim the retry: the failure stands.
                        let _ = store::progress(&self.store, self.job.id, 0, 1).await;
                        let value =
                            lines::failure_line(&line.custom_id, Some(execution), self.endpoint, e);
                        self.emit(&records, n, true, value).await;
                        return;
                    }
                    self.fail(n, &line.custom_id, execution, e, &records).await;
                    return;
                }
            }
        }
    }

    async fn fail(
        &self,
        n: i32,
        custom_id: &str,
        execution: Uuid,
        error: InferenceError,
        records: &mpsc::Sender<Record>,
    ) {
        let (status, _) = crate::protocols::batch_line_error(false, error);
        self.finished(n, false, Some(status), Some(error.code()))
            .await;
        let value = lines::failure_line(custom_id, Some(execution), self.endpoint, error);
        self.emit(records, n, true, value).await;
    }

    async fn execute(
        &self,
        request: BatchRequest,
        execution: Uuid,
    ) -> Result<BatchResponse, InferenceError> {
        let principal = self.job.principal();
        match request {
            BatchRequest::Chat(r) => match self
                .engine
                .execute_protocol(principal, r, execution, self.endpoint.protocol())
                .await?
            {
                ProviderOutput::Complete(response) => Ok(BatchResponse::Chat(response)),
                ProviderOutput::Stream(_) => Err(InferenceError::InvalidUpstream),
            },
            BatchRequest::Embeddings(r) => self
                .engine
                .execute_embeddings(principal, r, execution)
                .await
                .map(BatchResponse::Embeddings),
        }
    }
}

/// Store finished lines in encrypted segments as they complete.
async fn write_segments(
    store: Store,
    files: crate::filestore::FileStorage,
    job: JobRow,
    mut records: mpsc::Receiver<Record>,
) -> Result<(), InferenceError> {
    let mut next_seq = store::segments(&store, job.id)
        .await
        .map_err(|_| InferenceError::Storage)?
        .last()
        .map_or(0, |s| s.0 + 1);
    let mut buffer: Vec<u8> = Vec::new();
    let mut lines: Vec<i32> = Vec::new();
    let mut open = true;
    while open {
        let deadline = tokio::time::sleep(SEGMENT_AGE);
        tokio::pin!(deadline);
        let flush = tokio::select! {
            record = records.recv() => match record {
                Some(r) => {
                    let mut bytes = serde_json::to_vec(&json!({"n": r.line, "e": r.error, "r": r.value}))
                        .map_err(|_| InferenceError::Storage)?;
                    bytes.push(b'\n');
                    buffer.extend_from_slice(&bytes);
                    lines.push(r.line);
                    lines.len() >= SEGMENT_LINES || buffer.len() >= SEGMENT_BYTES
                }
                None => {
                    open = false;
                    true
                }
            },
            _ = &mut deadline => true,
        };
        if !flush || lines.is_empty() {
            continue;
        }
        let body = std::mem::take(&mut buffer);
        let held = std::mem::take(&mut lines);
        let mut sink = LazySink::new(
            &files,
            io::batch_file(
                true,
                job.workspace_id,
                job.api_key_id,
                job.user_id,
                format!("batch_{}_part_{next_seq}.jsonl", job.id.simple()),
            ),
        );
        let stored = async {
            sink.raw(body).await?;
            sink.finish().await
        }
        .await;
        match stored {
            Ok(Some(file)) => {
                if store::insert_segment(&store, job.id, job.workspace_id, next_seq, file, &held)
                    .await
                    .is_err()
                {
                    let _ = files.delete(file, Some(job.workspace_id)).await;
                    tracing::warn!(job_id = %job.id, "batch segment not recorded; its lines are reported unavailable");
                } else {
                    next_seq += 1;
                }
            }
            _ => {
                tracing::warn!(job_id = %job.id, "batch segment not stored; its lines are reported unavailable");
            }
        }
    }
    Ok(())
}

/// Repository of a gateway-run batch's line attempts: everything is the
/// normal store, except that admission transfers each line's hold out of
/// the batch's ceiling (`governance::batch::admit_line`).
struct LineRepository {
    store: Store,
    batch: LineContext,
}
#[async_trait]
impl InferenceRepository for LineRepository {
    async fn deployments(
        &self,
        principal: &Principal,
        model: &str,
    ) -> Result<Vec<Deployment>, InferenceError> {
        self.store.deployments(principal, model).await
    }
    async fn start(&self, _record: &ExecutionStart) -> Result<(), InferenceError> {
        // Every line attempt is admitted (never started unreserved).
        Err(InferenceError::Configuration)
    }
    async fn finish(&self, record: &ExecutionFinish) -> Result<(), InferenceError> {
        self.store.finish(record).await
    }
    async fn finish_attempt(
        &self,
        record: &ExecutionFinish,
        telemetry: &AttemptTelemetry,
    ) -> Result<(), InferenceError> {
        self.store.finish_attempt(record, telemetry).await
    }
    async fn admit(
        &self,
        record: &ExecutionStart,
        request: &ChatRequest,
        lease_seconds: i64,
        deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        crate::governance::batch::admit_line(
            &self.store,
            self.batch,
            record,
            &WorkloadAdmission {
                kind: crate::inference::types::WorkloadKind::Generation,
                output: crate::inference::workload::OutputReservation::Requested(
                    request.max_output_tokens,
                ),
                unit_ceilings: MeterUsage::default(),
            },
            lease_seconds,
            deployment,
        )
        .await
    }
    async fn admit_embeddings(
        &self,
        record: &ExecutionStart,
        _request: &EmbeddingRequest,
        lease_seconds: i64,
        deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        crate::governance::batch::admit_line(
            &self.store,
            self.batch,
            record,
            &WorkloadAdmission {
                kind: crate::inference::types::WorkloadKind::Embeddings,
                output: crate::inference::workload::OutputReservation::None,
                unit_ceilings: MeterUsage::default(),
            },
            lease_seconds,
            deployment,
        )
        .await
    }
    async fn admit_workload(
        &self,
        record: &ExecutionStart,
        admission: &WorkloadAdmission,
        lease_seconds: i64,
        deployment: &Deployment,
    ) -> Result<(), InferenceError> {
        crate::governance::batch::admit_line(
            &self.store,
            self.batch,
            record,
            admission,
            lease_seconds,
            deployment,
        )
        .await
    }
    async fn route_plan(
        &self,
        principal: &Principal,
        model: &str,
        candidates: &[Deployment],
        request_id: Uuid,
    ) -> Result<crate::routing::RoutePlan, InferenceError> {
        self.store
            .route_plan(principal, model, candidates, request_id)
            .await
    }
    async fn route_result(
        &self,
        deployment: Uuid,
        error: Option<InferenceError>,
    ) -> Result<(), InferenceError> {
        self.store.route_result(deployment, error).await
    }
}
